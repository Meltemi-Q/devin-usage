#![windows_subsystem = "windows"]
//! devin-usage-tray — Windows 托盘图标，实时显示 Devin 用量
//!
//! 只读 `data/usage.db`（由同目录 devin_usage.py collect 产出），不写任何 Devin 文件。
//! 菜单每 60s 重建刷新；"立即采集"调用 python devin_usage.py collect。

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

mod panel;

const REFRESH: Duration = Duration::from_secs(60);
const COLLECT_REFRESH_DELAY: Duration = Duration::from_secs(8);

// ---------------------------------------------------------------- 数据

#[derive(Default)]
pub struct Stats {
    pub quota: Option<Quota>,
    pub swe2_7d: Agg,
    pub swe2_all: Agg,
    pub per_model: Vec<ModelRow>,
    pub total_7d: Agg,
    pub total_all: Agg,
    pub usd_7d: f64,
    pub usd_all: f64,
    pub swe2_usd_7d: f64,
    pub swe2_usd_all: f64,
    pub last_collect_ago: String,
    pub has_db: bool,
    /// 全模型行（面板表格用；per_model 仍是 swe-2 专供托盘菜单）
    pub all_models: Vec<ModelRow>,
}

pub struct Quota {
    pub plan: String,
    pub weekly_pct: f64,
    pub overage_usd: f64,
    pub daily_reset: i64,
    pub weekly_reset: i64,
}

#[derive(Default, Clone)]
pub struct Agg {
    pub sessions: i64,
    pub msgs: i64,
    pub tools: i64,
    pub hours: f64,
    pub tin: i64,   // input tokens
    pub tout: i64,  // output tokens
    pub tcr: i64,   // cache-read tokens
    pub tcw: i64,   // cache-write tokens
}

pub struct ModelRow {
    pub source: String,
    pub model: String,
    pub all: Agg,
    pub d7: Agg,
    pub usd_all: f64,
    pub usd_7d: f64,
}

/// 每 1M token 美元价格规则（prefix 首个命中；"" 兜底）——与 devin_usage.py 同源，
/// 主数据在 db 的 model_prices 表（collect 时写入，含 data/prices.json 覆盖）
#[derive(Clone)]
pub struct PriceRule(f64, f64, f64, f64);

pub fn load_prices(conn: &Connection) -> Vec<(String, PriceRule)> {
    let mut v = Vec::new();
    if let Ok(mut s) = conn.prepare(
        "SELECT prefix, in_per_1m, out_per_1m, cr_per_1m, cw_per_1m
         FROM model_prices ORDER BY prio",
    ) {
        if let Ok(rows) = s.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                PriceRule(r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?),
            ))
        }) {
            v.extend(rows.flatten());
        }
    }
    if v.is_empty() {
        // 兜底内置（collect 还没写过表时）
        v.push(("swe-2".into(), PriceRule(3.0, 15.0, 0.30, 3.75)));
        v.push(("claude".into(), PriceRule(3.0, 15.0, 0.30, 3.75)));
        v.push(("gpt".into(), PriceRule(1.25, 10.0, 0.125, 1.25)));
        v.push(("".into(), PriceRule(3.0, 15.0, 0.30, 3.75)));
    }
    v
}

pub fn price_of<'a>(model: &str, rules: &'a [(String, PriceRule)]) -> &'a PriceRule {
    let m = model.to_lowercase();
    rules
        .iter()
        .find(|(p, _)| m.starts_with(p.as_str()))
        .or_else(|| rules.last())
        .map(|(_, r)| r)
        .unwrap()
}

pub fn cost(a: &Agg, p: &PriceRule) -> f64 {
    (a.tin as f64 * p.0 + a.tout as f64 * p.1 + a.tcr as f64 * p.2
        + a.tcw as f64 * p.3)
        / 1e6
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 从 exe 向上找含 devin_usage.py 的目录（exe 在 devin-usage-tray/target/{profile}/ 下）
pub fn project_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    for anc in exe.ancestors().skip(1).take(6) {
        if anc.join("devin_usage.py").is_file() {
            return Some(anc.to_path_buf());
        }
    }
    None
}

pub fn open_db() -> Option<Connection> {
    let dir = project_dir()?;
    let db = dir.join("data").join("usage.db");
    if !db.is_file() {
        return None;
    }
    Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

pub fn agg(r: &rusqlite::Row, base: usize) -> rusqlite::Result<Agg> {
    Ok(Agg {
        sessions: r.get::<_, Option<i64>>(base)?.unwrap_or(0),
        msgs: r.get::<_, Option<i64>>(base + 1)?.unwrap_or(0),
        tools: r.get::<_, Option<i64>>(base + 2)?.unwrap_or(0),
        hours: r.get::<_, Option<i64>>(base + 3)?.unwrap_or(0) as f64 / 3600.0,
        tin: r.get::<_, Option<i64>>(base + 4)?.unwrap_or(0),
        tout: r.get::<_, Option<i64>>(base + 5)?.unwrap_or(0),
        tcr: r.get::<_, Option<i64>>(base + 6)?.unwrap_or(0),
        tcw: r.get::<_, Option<i64>>(base + 7)?.unwrap_or(0),
    })
}

/// token 人性化: 1234→1.2k 4500000→4.5M
pub fn tok(v: i64) -> String {
    let v = v as f64;
    for (u, d) in [("B", 1e9), ("M", 1e6), ("k", 1e3)] {
        if v.abs() >= d {
            return format!("{:.1}{}", v / d, u);
        }
    }
    format!("{}", v as i64)
}

pub fn load_stats() -> Stats {
    let mut st = Stats {
        last_collect_ago: "从未".into(),
        ..Stats::default()
    };
    let Some(conn) = open_db() else {
        return st;
    };
    st.has_db = true;
    let n = now();
    let t7 = n - 7 * 86400;

    st.quota = conn
        .query_row(
            "SELECT plan_name, weekly_quota_remaining_pct, overage_balance_micros,
                    daily_reset, weekly_reset
             FROM quota_snapshots ORDER BY ts DESC LIMIT 1",
            [],
            |r| {
                Ok(Quota {
                    plan: r.get::<_, Option<String>>(0)?.unwrap_or_else(|| "?".into()),
                    weekly_pct: r.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
                    overage_usd: r.get::<_, Option<i64>>(2)?.unwrap_or(0) as f64 / 1e6,
                    daily_reset: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    weekly_reset: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                })
            },
        )
        .ok();

    if let Ok(ts) = conn.query_row(
        "SELECT MAX(ts) FROM collect_runs WHERE status='ok'",
        [],
        |r| r.get::<_, Option<i64>>(0),
    ) {
        if let Some(ts) = ts {
            let d = (n - ts).max(0);
            st.last_collect_ago = if d < 90 {
                format!("{d}秒前")
            } else if d < 5400 {
                format!("{}分钟前", d / 60)
            } else {
                format!("{}小时前", d / 3600)
            };
        }
    }

    const SEL: &str = "SELECT count(*), sum(n_user), sum(n_tool_calls),
                       sum(last_activity_at - created_at),
                       sum(tok_in), sum(tok_out), sum(tok_cache_read), sum(tok_cache_write)
                       FROM local_sessions";

    st.swe2_all = conn
        .query_row(&format!("{SEL} WHERE model LIKE 'swe-2%'"), [], |r| {
            agg(r, 0)
        })
        .unwrap_or_default();
    st.swe2_7d = conn
        .query_row(
            &format!("{SEL} WHERE model LIKE 'swe-2%' AND created_at>=?1"),
            [t7],
            |r| agg(r, 0),
        )
        .unwrap_or_default();
    st.total_7d = conn
        .query_row(&format!("{SEL} WHERE created_at>=?1"), [t7], |r| {
            agg(r, 0)
        })
        .unwrap_or_default();
    st.total_all = conn
        .query_row(SEL, [], |r| agg(r, 0))
        .unwrap_or_default();

    // 等效成本：db 里的 model_prices 规则表（首个 prefix 命中）
    let rules = load_prices(&conn);
    st.swe2_usd_7d = cost(&st.swe2_7d, price_of("swe-2", &rules));
    st.swe2_usd_all = cost(&st.swe2_all, price_of("swe-2", &rules));
    // 全模型成本：逐模型行按各自价格累计
    for (sql, args, dst) in [
        (format!("{SELM} GROUP BY model"), vec![], &mut st.usd_all),
        (format!("{SELM} WHERE created_at>=?1 GROUP BY model"), vec![t7], &mut st.usd_7d),
    ] {
        if let Ok(mut s) = conn.prepare(&sql) {
            if let Ok(rows) = s.query_map(rusqlite::params_from_iter(args), |r| {
                Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), agg(r, 1)?))
            }) {
                for (mdl, a) in rows.flatten() {
                    *dst += cost(&a, price_of(&mdl, &rules));
                }
            }
        }
    }

    const SELM: &str = "SELECT model, count(*), sum(n_user), sum(n_tool_calls),
                        sum(last_activity_at - created_at),
                        sum(tok_in), sum(tok_out), sum(tok_cache_read), sum(tok_cache_write)
                        FROM local_sessions";
    if let Ok(mut stmt) = conn.prepare(&format!(
        "{SELM} WHERE model LIKE 'swe-2%' GROUP BY model ORDER BY 2 DESC"
    )) {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), agg(r, 1)?))
        }) {
            for row in rows.flatten().take(6) {
                let usd = cost(&row.1, price_of(&row.0, &rules));
                st.per_model.push(ModelRow {
                    source: String::new(),
                    model: row.0,
                    all: row.1,
                    d7: Agg::default(),
                    usd_all: usd,
                    usd_7d: 0.0,
                });
            }
        }
    }
    if let Ok(mut stmt) = conn.prepare(&format!(
        "{SELM} WHERE model LIKE 'swe-2%' AND created_at>=?1 GROUP BY model"
    )) {
        if let Ok(rows) = stmt.query_map([t7], |r| {
            Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), agg(r, 1)?))
        }) {
            for (model, a) in rows.flatten() {
                if let Some(m) = st.per_model.iter_mut().find(|m| m.model == model) {
                    m.usd_7d = cost(&a, price_of(&model, &rules));
                    m.d7 = a;
                }
            }
        }
    }

    // 全模型行：面板用（50+ 个 Cascade 内部模型也在里面），按 token 总量排序
    const SELSM: &str = "SELECT source, model, count(*), sum(n_user), sum(n_tool_calls),
                        sum(last_activity_at - created_at),
                        sum(tok_in), sum(tok_out), sum(tok_cache_read), sum(tok_cache_write)
                        FROM local_sessions";
    if let Ok(mut stmt) = conn.prepare(&format!(
        "{SELSM} GROUP BY source, model
         ORDER BY sum(ifnull(tok_in,0)+ifnull(tok_out,0)+ifnull(tok_cache_read,0)+ifnull(tok_cache_write,0)) DESC
         LIMIT 200"
    )) {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                agg(r, 2)?,
            ))
        }) {
            for (src, mdl, a) in rows.flatten() {
                st.all_models.push(ModelRow {
                    source: src,
                    model: mdl.clone(),
                    all: a.clone(),
                    d7: Agg::default(),
                    usd_all: cost(&a, price_of(&mdl, &rules)),
                    usd_7d: 0.0,
                });
            }
        }
    }
    if let Ok(mut stmt) =
        conn.prepare(&format!("{SELSM} WHERE created_at>=?1 GROUP BY source, model"))
    {
        if let Ok(rows) = stmt.query_map([t7], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                agg(r, 2)?,
            ))
        }) {
            for (src, model, a) in rows.flatten() {
                if let Some(m) = st
                    .all_models
                    .iter_mut()
                    .find(|m| m.model == model && m.source == src)
                {
                    m.usd_7d = cost(&a, price_of(&model, &rules));
                    m.d7 = a;
                }
            }
        }
    }
    st
}

// ---------------------------------------------------------------- 图标

const LOGO_PNG: &[u8] = include_bytes!("../assets/devin-logo-1024.png");

/// 托盘图标像素（size×size，Retina 建议 64）：白底圆角卡片 + Devin 黑色标志
/// + 右下配额状态点装饰。quota_pct: 周配额剩余 %（None→灰点）
pub fn paint_icon(quota_pct: Option<f64>, size: usize) -> Vec<u8> {
    let (w, h) = (size, size);
    let s = size as f32 / 32.0; // 相对 32px 设计稿的缩放
    let mut px = vec![0u8; w * h * 4];
    let put = |px: &mut [u8], x: i32, y: i32, c: [u8; 4]| {
        if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
            let i = (y as usize * w + x as usize) * 4;
            // 简单 alpha 叠加
            let a = c[3] as u16;
            let ia = 255 - a;
            px[i] = ((c[0] as u16 * a + px[i] as u16 * ia) / 255) as u8;
            px[i + 1] = ((c[1] as u16 * a + px[i + 1] as u16 * ia) / 255) as u8;
            px[i + 2] = ((c[2] as u16 * a + px[i + 2] as u16 * ia) / 255) as u8;
            px[i + 3] = 255;
        }
    };
    // 白底圆角卡片
    let cr = (6.0 * s).round() as i32;       // 圆角半径
    let shade_y = (26.0 * s).round() as i32; // 底部微阴影分界
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let (dx, dy) = (
                if x < cr { cr - x } else { (x - (w as i32 - 1 - cr)).max(0) },
                if y < cr { cr - y } else { (y - (h as i32 - 1 - cr)).max(0) },
            );
            if dx * dx + dy * dy <= cr * cr {
                let shade = if y > shade_y { 232 } else { 245 };
                put(&mut px, x, y, [shade, shade, shade + 3, 255]);
            }
        }
    }
    // Devin logo 缩放到 26s×26s 居中贴上
    let ls = (26.0 * s).round() as u32;
    let off = ((size as u32 - ls) / 2) as i32;
    if let Ok(img) = image::load_from_memory(LOGO_PNG) {
        let logo = image::imageops::resize(
            &img.to_rgba8(),
            ls,
            ls,
            image::imageops::FilterType::Lanczos3,
        );
        for (lx, ly, p) in logo.enumerate_pixels() {
            let (x, y) = (lx as i32 + off, ly as i32 + off);
            if p[3] > 0 {
                put(&mut px, x, y, [p[0], p[1], p[2], p[3]]);
            }
        }
    }
    // 右下状态点：白圈 + 配额色芯
    let dot = match quota_pct {
        None => [150, 150, 150, 255],
        Some(p) if p >= 50.0 => [52, 199, 89, 255],   // 绿
        Some(p) if p >= 20.0 => [255, 190, 90, 255],  // 琥珀
        Some(_) => [255, 90, 90, 255],                // 红
    };
    let (cx, cy, r) = ((24.0 * s) as i32, (24.0 * s) as i32, (6.0 * s) as i32);
    let bw = (2.0 * s).max(1.0) as i32; // 描边宽
    for y in (cy - r - bw)..=(cy + r + bw) {
        for x in (cx - r - bw)..=(cx + r + bw) {
            let d2 = (x - cx) * (x - cx) + (y - cy) * (y - cy);
            if d2 <= r * r {
                put(&mut px, x, y, dot);
            } else if d2 <= (r + bw) * (r + bw) {
                put(&mut px, x, y, [255, 255, 255, 255]);
            }
        }
    }
    px
}

pub fn make_icon(quota_pct: Option<f64>) -> Icon {
    let px = paint_icon(quota_pct, 64); // 64px：Retina @2x 清晰
    Icon::from_rgba(px, 64, 64).expect("icon")
}

// ---------------------------------------------------------------- UI

struct App {
    tray: tray_icon::TrayIcon,
    id_collect: MenuId,
    id_panel: MenuId,
    id_copy: MenuId,
    id_quit: MenuId,
    next_refresh: Instant,
    refresh_after_collect: Option<Instant>,
    last_report: String,
}

impl App {
    fn new() -> Self {
        let menu = Menu::new();
        menu.append(&MenuItem::new("Devin 用量 · 加载中…", false, None))
            .ok();
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Devin 用量")
            .with_icon(make_icon(None))
            .build()
            .expect("tray icon");
        let mut app = App {
            tray,
            id_collect: MenuId::new("collect"),
            id_panel: MenuId::new("panel"),
            id_copy: MenuId::new("copy"),
            id_quit: MenuId::new("quit"),
            next_refresh: Instant::now(),
            refresh_after_collect: None,
            last_report: String::new(),
        };
        app.refresh();
        app
    }

    fn item(id: &str, text: impl AsRef<str>, enabled: bool) -> MenuItem {
        MenuItem::with_id(MenuId::new(id), text, enabled, None)
    }

    fn refresh(&mut self) {
        let st = load_stats();
        let menu = Menu::new();
        let add = |m: &Menu, text: String| {
            menu.append(&MenuItem::new(text, false, None)).ok();
            let _ = m;
        };

        if !st.has_db {
            menu.append(&MenuItem::new("Devin 用量 — 无数据", false, None))
                .ok();
            menu.append(&MenuItem::new(
                "先跑: python devin_usage.py collect",
                false,
                None,
            ))
            .ok();
        } else {
            add(&menu, format!("Devin 用量 · 采集于 {}", st.last_collect_ago));
            if let Some(q) = &st.quota {
                add(
                    &menu,
                    format!(
                        "配额[{}]: 周剩 {:.0}% · 超额 ${:.2}",
                        q.plan, q.weekly_pct, q.overage_usd
                    ),
                );
                let dl = (q.daily_reset - now()).max(0);
                let wl = (q.weekly_reset - now()).max(0);
                add(
                    &menu,
                    format!("重置: 日 {}h后 · 周 {:.1}天后", dl / 3600, wl as f64 / 86400.0),
                );
            }
            menu.append(&PredefinedMenuItem::separator()).ok();
            add(
                &menu,
                format!(
                    "SWE-2 近7天: {}会话 · 入{} · 出{} · 缓{}",
                    st.swe2_7d.sessions,
                    tok(st.swe2_7d.tin),
                    tok(st.swe2_7d.tout),
                    tok(st.swe2_7d.tcr + st.swe2_7d.tcw)
                ),
            );
            add(
                &menu,
                format!(
                    "全部模型近7天: 入{} · 出{} · 缓{}",
                    tok(st.total_7d.tin),
                    tok(st.total_7d.tout),
                    tok(st.total_7d.tcr + st.total_7d.tcw)
                ),
            );
            add(
                &menu,
                format!(
                    "等效成本: 7天 ${:.2} · SWE-2 ${:.2} · 累计 ${:.2}",
                    st.usd_7d, st.swe2_usd_7d, st.usd_all
                ),
            );
            for m in &st.per_model {
                add(
                    &menu,
                    format!(
                        "  {}  7d:{}会话 出{} ≈${:.2} | 总:{}会话 ≈${:.2}",
                        m.model, m.d7.sessions, tok(m.d7.tout), m.usd_7d,
                        m.all.sessions, m.usd_all
                    ),
                );
            }
            self.last_report = build_report(&st);
            let tip = format!(
                "Devin {}% · SWE-2 {}会话 出{}/7d",
                st.quota.as_ref().map(|q| q.weekly_pct as i64).unwrap_or(0),
                st.swe2_7d.sessions,
                tok(st.swe2_7d.tout)
            );
            let _ = self.tray.set_tooltip(Some(&tip[..tip.len().min(120)]));
            let _ = self
                .tray
                .set_icon(Some(make_icon(st.quota.as_ref().map(|q| q.weekly_pct))));
        }

        menu.append(&PredefinedMenuItem::separator()).ok();
        let it_collect = Self::item("collect", "立即采集", true);
        let it_panel = Self::item("panel", "打开面板", true);
        let it_copy = Self::item("copy", "复制文本报告", true);
        let it_quit = Self::item("quit", "退出", true);
        self.id_collect = it_collect.id().clone();
        self.id_panel = it_panel.id().clone();
        self.id_copy = it_copy.id().clone();
        self.id_quit = it_quit.id().clone();
        menu.append(&it_collect).ok();
        menu.append(&it_panel).ok();
        menu.append(&it_copy).ok();
        menu.append(&PredefinedMenuItem::separator()).ok();
        menu.append(&it_quit).ok();
        self.tray.set_menu(Some(Box::new(menu)));
    }

    fn collect_now(&mut self) {
        if spawn_collect() {
            self.refresh_after_collect = Some(Instant::now() + COLLECT_REFRESH_DELAY);
        }
    }
}

/// 后台起 python devin_usage.py collect（托盘与面板共用）
pub fn spawn_collect() -> bool {
    let Some(dir) = project_dir() else { return false };
    let script = dir.join("devin_usage.py");
    #[cfg(target_os = "macos")]
    let pys = ["python3", "/usr/bin/python3"];
    #[cfg(not(target_os = "macos"))]
    let pys = [
        "python",
        "py",
        r"C:\Users\meltemi\scoop\apps\miniconda3\current\python.exe",
    ];
    pys.iter().any(|py| {
        Command::new(py)
            .arg(&script)
            .arg("collect")
            .current_dir(&dir)
            .spawn()
            .is_ok()
    })
}

/// 文本报告（托盘"复制报告"与面板共用）
pub fn build_report(st: &Stats) -> String {
    let mut r = String::new();
    if let Some(q) = &st.quota {
        r.push_str(&format!(
            "Devin [{}] 周配额剩余 {:.0}% · 超额余额 ${:.2}\n",
            q.plan, q.weekly_pct, q.overage_usd
        ));
    }
    r.push_str(&format!(
        "SWE-2 近7天: {}会话 {}msg {}tool | in={} out={} cache_read={} cache_write={}\n",
        st.swe2_7d.sessions, st.swe2_7d.msgs, st.swe2_7d.tools,
        st.swe2_7d.tin, st.swe2_7d.tout, st.swe2_7d.tcr, st.swe2_7d.tcw
    ));
    r.push_str(&format!(
        "SWE-2 全部: {}会话 {}msg {}tool {:.1}h | in={} out={} cr={} cw={}\n",
        st.swe2_all.sessions, st.swe2_all.msgs, st.swe2_all.tools,
        st.swe2_all.hours, st.swe2_all.tin, st.swe2_all.tout,
        st.swe2_all.tcr, st.swe2_all.tcw
    ));
    r.push_str(&format!(
        "等效$近7天: ${:.2} (SWE-2 ${:.2}) | 累计 ${:.2} (SWE-2 ${:.2})  [公开API价折算]\n",
        st.usd_7d, st.swe2_usd_7d, st.usd_all, st.swe2_usd_all
    ));
    r.push_str(&format!(
        "本地总计近7天: {}会话 {}msg {}tool | in={} out={} cr={} cw={}\n",
        st.total_7d.sessions, st.total_7d.msgs, st.total_7d.tools,
        st.total_7d.tin, st.total_7d.tout, st.total_7d.tcr, st.total_7d.tcw
    ));
    for m in &st.per_model {
        r.push_str(&format!(
            "  {}  7d:{}会话 out={} ≈${:.2} | all:{}会话 {}msg {}tool in={} out={} cr={} cw={} ≈${:.2} {:.1}h\n",
            m.model, m.d7.sessions, m.d7.tout, m.usd_7d, m.all.sessions, m.all.msgs,
            m.all.tools, m.all.tin, m.all.tout, m.all.tcr, m.all.tcw, m.usd_all, m.all.hours
        ));
    }
    r
}

impl ApplicationHandler for App {
    fn resumed(&mut self, _el: &ActiveEventLoop) {}
    fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, _e: WindowEvent) {}
    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == self.id_collect {
                self.collect_now();
            } else if ev.id == self.id_panel {
                if let Ok(exe) = std::env::current_exe() {
                    let _ = Command::new(exe).arg("--panel").spawn();
                }
            } else if ev.id == self.id_copy {
                if let Ok(mut cb) = arboard::Clipboard::new() {
                    let _ = cb.set_text(self.last_report.clone());
                }
            } else if ev.id == self.id_quit {
                el.exit();
                return;
            }
        }
        if Instant::now() >= self.next_refresh {
            self.refresh();
            self.next_refresh = Instant::now() + REFRESH;
        }
        if let Some(t) = self.refresh_after_collect {
            if Instant::now() >= t {
                self.refresh_after_collect = None;
                self.refresh();
            }
        }
        el.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(250),
        ));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--dump-icon") {
        for (name, pct) in [("icon-green", Some(75.0)), ("icon-amber", Some(30.0)),
                            ("icon-red", Some(5.0)), ("icon-gray", None)] {
            let px = paint_icon(pct, 64);
            image::save_buffer(
                format!("{name}.png"), &px, 64, 64, image::ColorType::Rgba8,
            )
            .unwrap();
        }
        println!("icons dumped");
        return;
    }
    if args.iter().any(|a| a == "--panel") {
        if let Err(e) = panel::run() {
            eprintln!("panel: {e}");
            std::process::exit(1);
        }
        return;
    }
    #[allow(unused_mut)]
    let mut builder = EventLoop::builder();
    #[cfg(target_os = "macos")]
    {
        // 纯托盘：不占 Dock 位（菜单栏拥挤时面板走 .app 启动）
        use winit::platform::macos::EventLoopBuilderExtMacOS;
        builder.with_activation_policy(winit::platform::macos::ActivationPolicy::Accessory);
    }
    let el = builder.build().expect("event loop");
    let mut app = App::new();
    el.run_app(&mut app).expect("run");
}
