//! panel.rs — egui 轻量面板窗口（`--panel` 模式）
//!
//! 托盘的第二形态：macOS 菜单栏拥挤被系统隐藏、或想要常驻桌面小组件时用。
//! 从托盘菜单"打开面板"唤起，或打成 .app 从 Dock/Spotlight 启动。
//! 卡片式布局：配额卡片 + 趋势图 + 每日堆叠柱状图 + token 构成条 + 全模型族表。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::TimeZone;
use eframe::egui;
use egui_plot::{Bar, BarChart, Legend, Plot};

use crate::{
    build_report, cost, load_prices, load_stats, now, open_db, paint_icon, price_of,
    spawn_collect, tok, Agg, ModelRow, Stats,
};

const RELOAD: Duration = Duration::from_secs(60);
const COLLECT_DELAY: Duration = Duration::from_secs(8);

// 配色
const GREEN: egui::Color32 = egui::Color32::from_rgb(52, 199, 89);
const AMBER: egui::Color32 = egui::Color32::from_rgb(255, 190, 90);
const RED: egui::Color32 = egui::Color32::from_rgb(255, 90, 90);
const C_IN: egui::Color32 = egui::Color32::from_rgb(91, 143, 249);
const C_OUT: egui::Color32 = egui::Color32::from_rgb(97, 221, 170);
const C_CR: egui::Color32 = egui::Color32::from_rgb(246, 189, 22);
const C_CW: egui::Color32 = egui::Color32::from_rgb(114, 98, 253);

pub fn run() -> eframe::Result<()> {
    // 先读一次数据：窗口图标要用真实配额决定状态点颜色（与托盘一致）
    let st0 = load_stats(None);
    let pct = st0.quota.as_ref().map(|q| q.weekly_pct);
    let px = paint_icon(pct, 64);
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("AI 用量")
            .with_inner_size([460.0, 780.0])
            .with_min_inner_size([380.0, 560.0])
            .with_icon(Arc::new(egui::IconData {
                rgba: px,
                width: 64,
                height: 64,
            })),
        ..Default::default()
    };
    eframe::run_native(
        "AI 用量",
        opts,
        Box::new(|cc| {
            load_cjk_font(&cc.egui_ctx);
            Ok(Box::new(Panel::new(st0)))
        }),
    )
}

/// egui 自带字体不含 CJK，从系统字体目录补一个（找不到就显示方框，不致命）
fn load_cjk_font(ctx: &egui::Context) {
    const PATHS: &[&str] = &[
        "/System/Library/Fonts/PingFang.ttc",   // macOS
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/Library/Fonts/Arial Unicode.ttf",
        r"C:\Windows\Fonts\msyh.ttc",            // Windows
        r"C:\Windows\Fonts\simhei.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", // Linux
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    ];
    for p in PATHS {
        if let Ok(bytes) = std::fs::read(p) {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("cjk".into(), Arc::new(egui::FontData::from_owned(bytes)));
            for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts.families.entry(fam).or_default().push("cjk".into());
            }
            ctx.set_fonts(fonts);
            return;
        }
    }
}

fn quota_color(pct: f64) -> egui::Color32 {
    if pct >= 50.0 {
        GREEN
    } else if pct >= 20.0 {
        AMBER
    } else {
        RED
    }
}

/// 模型族：去掉尾部档位词（-medium/-high/-xhigh/-max/-low/-none/-priority）
/// swe-2-max→swe-2 · gpt-6-astra-medium-priority→gpt-6-astra · kimi-k3-high→kimi-k3
fn family_of(m: &str) -> String {
    const TIERS: &[&str] = &["priority", "low", "medium", "high", "xhigh", "max", "none"];
    let mut parts: Vec<&str> = m.split('-').collect();
    while parts.len() > 1 && TIERS.contains(parts.last().unwrap()) {
        parts.pop();
    }
    parts.join("-")
}

// ---------------------------------------------------------------- 图表数据

struct DayRow {
    ymd: String,   // 2026-09-17（查明细用）
    label: String, // 09-17（轴标签用）
    vals: [f64; 4], // [in, out, cr, cw]
}

#[derive(Default)]
struct Charts {
    daily: Vec<DayRow>,
}

/// days=0 → 全部历史；app="devin" 走 local_sessions，其他应用走 usage_events；
/// device=Some → 只看该设备
fn load_charts(days: i64, app: &str, device: Option<&str>) -> Charts {
    let mut c = Charts::default();
    let Some(conn) = open_db() else { return c };
    let t0 = if days > 0 { now() - days * 86400 } else { 0 };
    // 各列分开 sum：整行相加遇 NULL 会整行变 NULL（老会话无 metrics）
    let sql = match app {
        // devin/全部 都走 usage_events：消息级时间戳归日，
        // 跨天会话不再把历史 token 全堆在创建日
        "all" =>
            "SELECT day, substr(day,6) md,
                    sum(tok_in), sum(tok_out), sum(tok_cache_read), sum(tok_cache_write)
             FROM usage_events WHERE ts >= ?1 AND (?3 IS NULL OR device=?3)
             GROUP BY day ORDER BY day",
        _ =>
            "SELECT day, substr(day,6) md,
                    sum(tok_in), sum(tok_out), sum(tok_cache_read), sum(tok_cache_write)
             FROM usage_events WHERE ts >= ?1 AND app = ?2 AND (?3 IS NULL OR device=?3)
             GROUP BY day ORDER BY day",
    };
    if let Ok(mut s) = conn.prepare(sql) {
        let parse = |r: &rusqlite::Row| -> rusqlite::Result<DayRow> {
            Ok(DayRow {
                ymd: r.get::<_, String>(0)?,
                label: r.get::<_, String>(1)?,
                vals: [
                    r.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
                    r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                    r.get::<_, Option<f64>>(4)?.unwrap_or(0.0),
                    r.get::<_, Option<f64>>(5)?.unwrap_or(0.0),
                ],
            })
        };
        let rows = match app {
            "all" => s.query_map(rusqlite::params![t0, "x", device], parse).ok(),
            _ => s.query_map(rusqlite::params![t0, app, device], parse).ok(),
        };
        if let Some(rows) = rows {
            c.daily = rows.flatten().collect();
        }
    }
    // 补齐缺数据的日期：GROUP BY 只返回有记录的日子，缺日被压缩掉会让
    // 日期间隔失真（如 devin tab 14 天只有 5 根柱，09-19 与 09-23 看着相邻）。
    // 用日历天补零，起点 = 选定区间起点，终点 = 今天。
    if !c.daily.is_empty() {
        let first = c.daily.first().unwrap().ymd.clone();
        let last_real = c.daily.last().unwrap().ymd.clone();
        let today = crate::now();
        let today_ymd = chrono::Local
            .timestamp_opt(today, 0)
            .single()
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| last_real.clone());
        let end = if today_ymd > last_real { today_ymd } else { last_real };
        let start = if days > 0 {
            let s = chrono::Local
                .timestamp_opt(today - (days - 1) * 86400, 0)
                .single()
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| first.clone());
            if s < first { s } else { first }
        } else {
            first
        };
        let parse = |s: &str| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok();
        if let (Some(mut d0), Some(d1)) = (parse(&start), parse(&end)) {
            let mut filled = Vec::new();
            let mut it = c.daily.into_iter().peekable();
            while d0 <= d1 {
                let ymd = d0.format("%Y-%m-%d").to_string();
                if it.peek().map(|r| r.ymd == ymd).unwrap_or(false) {
                    filled.push(it.next().unwrap());
                } else {
                    filled.push(DayRow {
                        label: ymd[5..].to_string(),
                        ymd,
                        vals: [0.0; 4],
                    });
                }
                d0 += chrono::Duration::days(1);
            }
            filled.extend(it);
            c.daily = filled;
        }
    }
    c
}

/// 模型明细（事件级）：跟随选中日期 > 天数区间 > 全部。
/// devin 行用 kind(app/cli) 作来源标签，其他应用用 app 名；
/// 时长取 meta.gen_ms（devin 有），无则 0。
fn load_models(
    app: &str,
    device: Option<&str>,
    ymd: Option<&str>,
    days: i64,
) -> Vec<ModelRow> {
    let mut out = Vec::new();
    let Some(conn) = open_db() else { return out };
    let t0 = if ymd.is_none() && days > 0 { now() - days * 86400 } else { 0 };
    let app_f = if app == "all" { None } else { Some(app) };
    let rules = load_prices(&conn);
    if let Ok(mut s) = conn.prepare(
        "SELECT CASE WHEN app='devin' THEN COALESCE(kind,app) ELSE app END src,
                model,
                count(distinct COALESCE(session_id,'e'||rowid)),
                count(*),
                sum(tok_in), sum(tok_out), sum(tok_cache_read), sum(tok_cache_write),
                sum(json_extract(meta,'$.gen_ms')),
                sum(cost_usd)
         FROM usage_events
         WHERE (?1 IS NULL OR day=?1)
           AND ts >= ?2
           AND (?3 IS NULL OR device=?3)
           AND (?4 IS NULL OR app=?4)
         GROUP BY src, model
         ORDER BY sum(ifnull(tok_in,0)+ifnull(tok_out,0)
                    +ifnull(tok_cache_read,0)+ifnull(tok_cache_write,0)) DESC",
    ) {
        if let Ok(rows) = s.query_map(
            rusqlite::params![ymd, t0, device, app_f],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                    r.get::<_, Option<f64>>(9)?.unwrap_or(0.0),
                ))
            },
        ) {
            for row in rows.flatten() {
                let (src, model, n_sess, n_ev, ti, to, tcr, tcw, gms, rc) = row;
                // antigravity 偶有多模型合并串 "?,gemini-3.8-flash" —— 取末段
                let model = model
                    .rsplit(',')
                    .next()
                    .unwrap_or(&model)
                    .trim_start_matches('?')
                    .to_string();
                let a = Agg {
                    sessions: n_sess,
                    msgs: n_ev,
                    tin: ti,
                    tout: to,
                    tcr,
                    tcw,
                    hours: gms as f64 / 3_600_000.0,
                    ..Default::default()
                };
                let usd = if rc > 0.0 { rc } else { cost(&a, price_of(&model, &rules)) };
                out.push(ModelRow {
                    source: src,
                    model,
                    all: a,
                    d7: Agg::default(),
                    usd_all: usd,
                    usd_7d: 0.0,
                });
            }
        }
    }
    out
}

/// 某一天的明细：该日记录数 + 分模型行（token 降序）。
/// devin 系按会话创建日；cursor/antigravity 按事件日（更贴近真实使用日）。
fn load_day_detail(
    ymd: &str,
    app: &str,
    device: Option<&str>,
) -> (i64, Vec<(String, i64, i64, i64, i64, i64)>) {
    let mut n_sess = 0i64;
    let mut rows = Vec::new();
    let Some(conn) = open_db() else {
        return (0, rows);
    };
    let (sql, p2): (&str, Option<&str>) = match app {
        "all" =>
            ("SELECT model, count(distinct session_id), count(*),
                    sum(tok_in), sum(tok_out), sum(tok_cache_read)
              FROM (SELECT app||'·'||model model, session_id,
                           tok_in, tok_out, tok_cache_read
                      FROM usage_events
                     WHERE day=?1 AND (?3 IS NULL OR device=?3))
             GROUP BY model
             ORDER BY sum(ifnull(tok_in,0)+ifnull(tok_out,0)
                        +ifnull(tok_cache_read,0)) DESC",
             None),
        _ =>
            ("SELECT model, count(distinct session_id), count(*),
                    sum(tok_in), sum(tok_out), sum(tok_cache_read)
             FROM usage_events WHERE day=?1 AND app=?2 AND (?3 IS NULL OR device=?3)
             GROUP BY model
             ORDER BY sum(tok_in)+sum(tok_out)+sum(tok_cache_read) DESC",
             Some(app)),
    };
    if let Ok(mut s) = conn.prepare(sql) {
        let parse = |r: &rusqlite::Row| -> rusqlite::Result<(String, i64, i64, i64, i64, i64)> {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                r.get::<_, Option<i64>>(5)?.unwrap_or(0),
            ))
        };
        let it = if let Some(a) = p2 {
            s.query_map(rusqlite::params![ymd, a, device], parse).ok()
        } else {
            s.query_map(rusqlite::params![ymd, "x", device], parse).ok()
        };
        if let Some(it) = it {
            for row in it.flatten() {
                n_sess += row.1;
                rows.push(row);
            }
        }
    }
    (n_sess, rows)
}

/// 小额美元不显示成 $0.00：>=0.01 用两位，再小用四位，0 显示 —
fn usd(v: f64) -> String {
    if v >= 0.01 {
        format!("${:.2}", v)
    } else if v > 0.0 {
        format!("${:.4}", v)
    } else {
        "—".into()
    }
}

/// 中文量级：572.7M→5.7亿，3.5M→350万，更直观（面板专用，托盘/报告仍用 M/k）
fn tok_zh(v: i64) -> String {
    let v = v as f64;
    if v.abs() >= 1e8 {
        format!("{:.2}亿", v / 1e8)
    } else if v.abs() >= 1e4 {
        format!("{:.1}万", v / 1e4)
    } else {
        format!("{}", v as i64)
    }
}

// ---------------------------------------------------------------- 面板

/// 当日明细（选中日 ymd + load_day_detail 的会话数与模型行）
type DetailRows = (String, i64, Vec<(String, i64, i64, i64, i64, i64)>);
/// 后台刷新产物：统计 + 日图 + 模型表 + 当日明细
type RefreshOut = (Stats, Charts, Vec<ModelRow>, Option<DetailRows>);
/// 视图键：(tab, device, days, sel_day)——任一变化即触发后台重载
type ViewKey = (&'static str, Option<String>, i64, Option<String>);

struct Panel {
    st: Stats,
    charts: Charts,
    models: Vec<ModelRow>,          // 模型族表缓存（上次后台加载结果）
    detail: Option<DetailRows>,     // 选中日明细缓存
    days: i64, // 图表回看范围：7/14/30/90，0=全部
    tab: &'static str, // "devin" | "cursor" | ... 各应用套餐独立
    device: Option<String>, // 设备过滤：None=全部设备
    report: String,
    logo: Option<egui::TextureHandle>,
    sel_day: Option<String>, // 图表中选中的日期（点柱子/下拉）
    pending: Option<std::sync::mpsc::Receiver<RefreshOut>>,
    loaded_key: ViewKey,
    reloaded: Instant,
    collecting: Option<Instant>,
    on_top: bool,
}

/// 卡片容器
fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

/// 一行配额：名称 + 进度条 + 剩余%/用尽 + used/limit + 重置倒计时
fn quota_line(
    ui: &mut egui::Ui,
    name: &str,
    pct: f64,
    resets: Option<i64>,
    used: Option<f64>,
    lim: Option<f64>,
) {
    // 尾巴文本（用量 · 重置）先算出来，宽度按字符估：ASCII≈6.5px，CJK≈13px
    let mut tail = String::new();
    if let (Some(u), Some(l)) = (used, lim) {
        tail += &format!("{}/{}", u as i64, l as i64);
    }
    if let Some(r) = resets {
        let left = r - now();
        if !tail.is_empty() {
            tail.push_str(" · ");
        }
        tail += &format!(
            "重置 {}",
            if left <= 0 {
                "待刷新".to_string()
            } else if left >= 86400 {
                format!("{}d{}h", left / 86400, left % 86400 / 3600)
            } else {
                format!("{}h", left / 3600)
            }
        );
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.add_sized(
            [92.0, 16.0],
            egui::Label::new(egui::RichText::new(name).small()).truncate(),
        );
        let frac = (pct / 100.0).clamp(0.0, 1.0) as f32;
        // 条固定全长=100%：剩余彩色，已消耗留白（白底+描边）
        // 列宽：名92 + 条 + %34 + 尾150（右对齐），各行条宽一致
        let bar_w =
            (ui.available_width() - 34.0 - 150.0 - 12.0).clamp(40.0, 400.0);
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(bar_w, 10.0),
            egui::Sense::hover(),
        );
        let painter = ui.painter();
        painter.rect_filled(rect, 5.0, egui::Color32::WHITE);
        if frac > 0.0 {
            painter.rect_filled(
                egui::Rect::from_min_size(
                    rect.min,
                    egui::vec2(rect.width() * frac, rect.height()),
                ),
                5.0,
                quota_color(pct),
            );
        }
        painter.rect_stroke(
            rect,
            5.0,
            egui::Stroke::new(1.0_f32, egui::Color32::from_gray(200)),
            egui::StrokeKind::Inside,
        );
        ui.add_sized(
            [34.0, 16.0],
            egui::Label::new(
                if pct <= 0.0 {
                    egui::RichText::new("用尽").color(RED).small()
                } else {
                    egui::RichText::new(format!("{pct:.0}%"))
                        .color(quota_color(pct))
                        .small()
                },
            )
            .truncate(),
        );
        // 尾巴固定宽右对齐，保证各行条宽一致
        ui.allocate_ui_with_layout(
            egui::vec2(150.0, 16.0),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.label(egui::RichText::new(&tail).weak().small());
            },
        );
    });
}

impl Panel {
    fn new(st: Stats) -> Self {
        let report = build_report(&st);
        Self {
            st,
            charts: Charts::default(),
            models: Vec::new(),
            detail: None,
            days: 14,
            tab: "devin",
            device: None,
            report,
            logo: None,
            sel_day: None,
            pending: None,
            // 初始键与首帧真实键必不同 → 首帧自动起后台加载
            loaded_key: ("", None, -1, None),
            reloaded: Instant::now(),
            collecting: None,
            on_top: false,
        }
    }

    /// 当前 tab 的报告文本（devin 用完整报告，其他应用生成简版）
    fn current_report(&self) -> String {
        if self.tab == "devin" {
            return self.report.clone();
        }
        if self.tab == "all" {
            let mut s = format!("{}\n", self.report);
            for (app_key, ap) in &self.st.apps {
                s += &format!(
                    "\n[{}] 会话 {} | 请求 {} | in {} out {} cacheR {} | ≈${:.2}\n",
                    app_key, ap.sessions_all, ap.total_all.sessions,
                    ap.total_all.tin, ap.total_all.tout, ap.total_all.tcr,
                    ap.usd_all,
                );
            }
            return s;
        }
        let name = match self.tab {
            "cursor" => "Cursor",
            "antigravity" => "Antigravity",
            "zcode" => "ZCode",
            "grok" => "Grok",
            "claude" => "Claude",
            "codex" => "Codex",
            x => x,
        };
        let Some(ap) = self.st.apps.get(self.tab) else {
            return format!("{name} 暂无数据");
        };
        let mut s = format!(
            "{name} 用量报告\n会话 {}（7d {}） | 请求 {}\ntoken: in {} out {} cacheR {} cacheW {}\n估算成本: 7d ${:.2} | 累计 ${:.2}\n",
            ap.sessions_all,
            ap.sessions_7d,
            ap.total_all.sessions,
            ap.total_all.tin,
            ap.total_all.tout,
            ap.total_all.tcr,
            ap.total_all.tcw,
            ap.usd_7d,
            ap.usd_all,
        );
        if ap.real_usd_all > 0.0 {
            s += &format!("订阅外实扣: ${:.2}\n", ap.real_usd_all);
        }
        if !ap.plan.is_empty() {
            s += &format!("套餐: {}\n", ap.plan);
        }
        for (label, pct, resets, used, lim) in &ap.quota_rows {
            let mut l = if *pct <= 0.0 {
                format!("配额 {}: 已用尽", label.trim_start_matches('_'))
            } else {
                format!("配额 {}: 剩 {:.0}%", label.trim_start_matches('_'), pct)
            };
            if let (Some(u), Some(lm)) = (used, lim) {
                l += &format!(" ({}/{})", *u as i64, *lm as i64);
            }
            if let Some(r) = resets {
                let left = r - now();
                l += &format!(
                    " 重置 {}",
                    if left <= 0 {
                        "待刷新".to_string()
                    } else {
                        format!("{}d{}h后", left / 86400, left % 86400 / 3600)
                    }
                );
            }
            s += &l;
            s.push('\n');
        }
        for m in &ap.models {
            s += &format!(
                "  {}·{}  req {}  in {}  out {}  cacheR {}  ~${:.4}\n",
                m.source, m.model, m.all.sessions, m.all.tin, m.all.tout, m.all.tcr, m.usd_all
            );
        }
        s
    }

    /// x 轴标签抽稀；首尾必标（最右一定是今天，抽稀会把它吞掉导致误读）
    fn x_fmt(
        labels: &[String],
        max: usize,
    ) -> impl Fn(egui_plot::GridMark, &std::ops::RangeInclusive<f64>) -> String {
        let owned = labels.to_vec();
        let n = owned.len();
        let step = (n / max).max(1);
        move |mark, _| {
            let i = mark.value.round() as usize;
            if i % step == 0 || i + 1 == n {
                owned.get(i).cloned().unwrap_or_default()
            } else {
                String::new()
            }
        }
    }

    /// 每日 token 堆叠柱状图：单位按最大值自适应（亿/M/k），柱顶标总量，点柱子选日期
    fn daily_plot(&mut self, ui: &mut egui::Ui) {
        if self.charts.daily.is_empty() {
            return;
        }
        let labels: Vec<String> =
            self.charts.daily.iter().map(|d| d.label.clone()).collect();
        // 自适应单位：按最大日总量选亿/M/k，轴与悬停同单位
        let max_tot: f64 = self
            .charts
            .daily
            .iter()
            .map(|d| d.vals.iter().sum::<f64>())
            .fold(0.0, f64::max);
        let (div, unit): (f64, &str) = if max_tot >= 1e8 {
            (1e8, "亿")
        } else if max_tot >= 1e6 {
            (1e6, "M")
        } else {
            (1e3, "k")
        };
        let series: [(&str, usize, egui::Color32); 4] = [
            ("输入", 0, C_IN),
            ("输出", 1, C_OUT),
            ("缓存读", 2, C_CR),
            ("缓存写", 3, C_CW),
        ];
        // 手动堆叠：每个序列的 base_offset = 前面所有序列之和
        let charts: Vec<BarChart> = series
            .iter()
            .map(|(name, idx, color)| {
                BarChart::new(
                    *name,
                    self.charts
                        .daily
                        .iter()
                        .enumerate()
                        .map(|(i, d)| {
                            let off: f64 = (0..*idx).map(|j| d.vals[j] / div).sum();
                            let mut b = Bar::new(i as f64, d.vals[*idx] / div)
                                .width(0.6)
                                .fill(*color);
                            b.base_offset = Some(off);
                            b
                        })
                        .collect::<Vec<Bar>>(),
                )
            })
            .collect();
        // 柱顶总量标签（<=40 天才画，多了会糊成一团）
        let tops: Vec<(f64, f64, String)> = if self.charts.daily.len() <= 40 {
            self.charts
                .daily
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let tot: f64 = d.vals.iter().sum::<f64>() / div;
                    let txt = if tot >= 100.0 {
                        format!("{:.0}", tot)
                    } else if tot >= 10.0 {
                        format!("{:.1}", tot)
                    } else {
                        format!("{:.2}", tot)
                    };
                    (i as f64, tot, format!("{txt}{unit}"))
                })
                .collect()
        } else {
            Vec::new()
        };
        let meta: Vec<(String, [f64; 4])> = self
            .charts
            .daily
            .iter()
            .map(|d| (d.label.clone(), d.vals))
            .collect();
        let ymax = max_tot / div;
        let presp = Plot::new("daily")
            .height(120.0)
            .allow_drag(false)
            .allow_zoom(false)
            .allow_scroll(false)
            .include_y(0.0)
            .include_y(ymax * 1.12)            // 给柱顶标签留位
            .x_axis_formatter(Self::x_fmt(&labels, 7))
            .y_axis_formatter(move |m, _| format!("{:.0}{unit}", m.value))
            .label_formatter(move |name, p| {
                let i = p.x.round() as usize;
                if let Some((day, v)) = meta.get(i) {
                    let idx = ["输入", "输出", "缓存读", "缓存写"]
                        .iter()
                        .position(|n| *n == name)
                        .unwrap_or(0);
                    let tot: f64 = v.iter().sum();
                    format!("{day} {name} {:.2}{unit} / 合计 {:.2}{unit}",
                            v[idx] / div, tot / div)
                } else {
                    format!("{name} {:.2}{unit}", p.y)
                }
            })
            .legend(Legend::default().position(egui_plot::Corner::LeftTop))
            .show(ui, |pui| {
                for c in charts {
                    pui.bar_chart(c);
                }
                for (x, y, t) in &tops {
                    pui.text(egui_plot::Text::new(
                        "",
                        egui_plot::PlotPoint::new(*x, *y * 1.05),
                        egui::RichText::new(t).size(9.0).weak(),
                    ));
                }
            });
        // 点击柱子 → 选中/取消该天，下方出明细
        if presp.response.clicked() {
            if let Some(pos) = presp.response.interact_pointer_pos() {
                let p = presp.transform.value_from_position(pos);
                let i = p.x.round() as usize;
                if let Some(d) = self.charts.daily.get(i) {
                    let ymd = d.ymd.clone();
                    self.sel_day = if self.sel_day.as_deref() == Some(ymd.as_str()) {
                        None
                    } else {
                        Some(ymd)
                    };
                }
            }
        }
    }

    /// 选中某天的明细块（分模型）——读后台加载的缓存，不在 UI 线程查库
    fn day_detail(&self, ui: &mut egui::Ui) {
        let Some(ymd) = &self.sel_day else { return };
        let Some((d_ymd, n_sess, rows)) = &self.detail else { return };
        if d_ymd != ymd {
            return; // 缓存是旧选日期的，新结果在路上
        }
        ui.add_space(4.0);
        ui.separator();
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(format!("{} · {} 条记录", ymd, n_sess)).strong(),
        );
        if rows.is_empty() {
            ui.label(egui::RichText::new("当日无会话").weak().small());
            return;
        }
        let rows = rows.clone();
        egui::Grid::new("daydetail")
            .num_columns(6)
            .spacing([10.0, 3.0])
            .striped(true)
            .show(ui, |ui| {
                for h in ["模型", "会话", "msg", "输入", "输出", "缓存读"] {
                    ui.label(egui::RichText::new(h).weak().small());
                }
                ui.end_row();
                for (m, s, nmsg, tin, tout, tcr) in &rows {
                    ui.label(egui::RichText::new(m).small());
                    ui.monospace(format!("{s}"));
                    ui.monospace(format!("{nmsg}"));
                    ui.monospace(tok_zh(*tin));
                    ui.monospace(tok_zh(*tout));
                    ui.monospace(tok_zh(*tcr));
                    ui.end_row();
                }
            });
    }

    /// token 构成比例条（累计 in/out/缓存读/缓存写）
    fn mix_strip(&self, ui: &mut egui::Ui, t: &Agg) {
        let sum = t.tin + t.tout + t.tcr + t.tcw;
        if sum == 0 {
            return;
        }
        let total = sum as f64;
        let parts: [(&str, i64, egui::Color32); 4] = [
            ("输入", t.tin, C_IN),
            ("输出", t.tout, C_OUT),
            ("缓存读", t.tcr, C_CR),
            ("缓存写", t.tcw, C_CW),
        ];
        let w = ui.available_width();
        let (rect, resp) =
            ui.allocate_exact_size(egui::vec2(w, 14.0_f32), egui::Sense::hover());
        let mut x = rect.left();
        for (_, v, c) in &parts {
            let sw = (*v as f64 / total) as f32 * rect.width();
            if sw > 0.0 {
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(
                        egui::pos2(x, rect.top()),
                        egui::vec2(sw, rect.height()),
                    ),
                    0.0,
                    *c,
                );
            }
            x += sw;
        }
        resp.on_hover_ui(|ui| {
            for (name, v, _) in &parts {
                ui.monospace(format!(
                    "{name}: {} ({:.1}%)",
                    tok(*v),
                    *v as f64 / total * 100.0
                ));
            }
        });
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            for (name, v, c) in &parts {
                let (r, _) =
                    ui.allocate_exact_size(egui::vec2(8.0_f32, 8.0_f32), egui::Sense::hover());
                ui.painter().rect_filled(r, 2.0, *c);
                ui.label(
                    egui::RichText::new(format!(
                        "{name} {} ({:.0}%)",
                        tok_zh(*v),
                        *v as f64 / total * 100.0
                    ))
                    .small(),
                );
            }
        });
    }

    /// 模型族汇总表（全部族）+ 可折叠的具体模型明细
    fn model_table(&self, ui: &mut egui::Ui, models: &[ModelRow], note: &str) {
        struct Fam {
            sessions: i64,
            tin: i64,
            tout: i64,
            tcr: i64,
            usd: f64,
            hours: f64,
        }
        let mut fams: BTreeMap<String, Fam> = BTreeMap::new();
        for m in models {
            let f = fams.entry(family_of(&m.model)).or_insert(Fam {
                sessions: 0,
                tin: 0,
                tout: 0,
                tcr: 0,
                usd: 0.0,
                hours: 0.0,
            });
            f.sessions += m.all.sessions;
            f.tin += m.all.tin;
            f.tout += m.all.tout;
            f.tcr += m.all.tcr;
            f.usd += m.usd_all;
            f.hours += m.all.hours;
        }
        let mut list: Vec<(String, Fam)> = fams.into_iter().collect();
        // 按消耗量（token 总量）降序
        list.sort_by(|a, b| {
            (b.1.tin + b.1.tout + b.1.tcr).cmp(&(a.1.tin + a.1.tout + a.1.tcr))
        });
        if list.is_empty() {
            return;
        }
        let has_hours = list.iter().any(|(_, f)| f.hours > 0.0);
        egui::ScrollArea::horizontal()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Grid::new("fams")
                    .num_columns(if has_hours { 7 } else { 6 })
                    .spacing([10.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("模型族").weak().small());
                        for h in ["会话", "输入", "输出", "缓存读"] {
                            ui.label(egui::RichText::new(h).weak().small());
                        }
                        if has_hours {
                            ui.label(egui::RichText::new("时长").weak().small());
                        }
                        ui.label(egui::RichText::new("≈$").weak().small());
                        ui.end_row();
                        for (name, f) in &list {
                            ui.label(name);
                            ui.monospace(format!("{}", f.sessions));
                            ui.monospace(tok_zh(f.tin));
                            ui.monospace(tok_zh(f.tout));
                            ui.monospace(tok_zh(f.tcr));
                            if has_hours {
                                ui.monospace(format!("{:.1}h", f.hours));
                            }
                            ui.monospace(usd(f.usd));
                            ui.end_row();
                        }
                    });
            });

        // 具体型号全列表（可折叠；型号名长，包横向滚动条）
        egui::CollapsingHeader::new(format!("具体型号（{} 个）", models.len()))
            .default_open(false)
            .show(ui, |ui| {
                if !note.is_empty() {
                    ui.label(egui::RichText::new(note).weak().small());
                }
                egui::ScrollArea::horizontal()
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        egui::Grid::new("models_all")
                            .num_columns(6)
                            .spacing([10.0, 3.0])
                            .striped(true)
                            .show(ui, |ui| {
                                for h in ["型号", "会话/请求", "输入", "输出", "缓存读", "≈$"] {
                                    ui.label(egui::RichText::new(h).weak().small());
                                }
                                ui.end_row();
                                for m in models {
                                    ui.label(
                                        egui::RichText::new(format!("{}·{}", m.source, m.model))
                                            .small(),
                                    );
                                    ui.monospace(format!("{}", m.all.sessions));
                                    ui.monospace(tok_zh(m.all.tin));
                                    ui.monospace(tok_zh(m.all.tout));
                                    ui.monospace(tok_zh(m.all.tcr));
                                    ui.monospace(usd(m.usd_all));
                                    ui.end_row();
                                }
                            });
                    });
            });
    }
}

impl eframe::App for Panel {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 收后台刷新结果（SQL 全在工作线程跑，UI 永不阻塞）
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok((st, ch, ms, dt)) => {
                    self.report = build_report(&st);
                    self.st = st;
                    self.charts = ch;
                    self.models = ms;
                    self.detail = dt;
                    self.reloaded = Instant::now();
                    self.collecting = None;
                    self.pending = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.pending = None,
            }
        }
        // 视图键变化（tab/设备/天数/选中日）或定时/采集后延迟到点 → 后台重载
        let key: ViewKey = (
            self.tab,
            self.device.clone(),
            self.days,
            self.sel_day.clone(),
        );
        let collect_due = self
            .collecting
            .is_some_and(|t| t.elapsed() >= COLLECT_DELAY);
        if self.pending.is_none()
            && (key != self.loaded_key
                || self.reloaded.elapsed() >= RELOAD
                || collect_due)
        {
            self.loaded_key = key.clone();
            self.collecting = None;
            let (tx, rx) = std::sync::mpsc::channel();
            self.pending = Some(rx);
            let (tab, dev, days, sel) = (key.0, key.1, key.2, key.3);
            let ctx2 = ctx.clone();
            std::thread::spawn(move || {
                let st = load_stats(dev.as_deref());
                let ch = load_charts(days, tab, dev.as_deref());
                let ms = load_models(tab, dev.as_deref(), sel.as_deref(), days);
                let dt = sel.as_deref().map(|y| {
                    let (n, rows) = load_day_detail(y, tab, dev.as_deref());
                    (y.to_string(), n, rows)
                });
                let _ = tx.send((st, ch, ms, dt));
                ctx2.request_repaint();
            });
        }
        if self.logo.is_none() {
            if let Ok(img) = image::load_from_memory(crate::LOGO_PNG) {
                let rgba = img.to_rgba8();
                let (w, h) = (rgba.width() as usize, rgba.height() as usize);
                self.logo = Some(ctx.load_texture(
                    "logo",
                    egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba),
                    egui::TextureOptions::LINEAR,
                ));
            }
        }

        // 底栏：等效成本 + 操作按钮（固定可见，不随内容滚动）
        egui::TopBottomPanel::bottom("actions").show(ctx, |ui| {
            ui.add_space(4.0);
            match self.tab {
                "devin" => ui.label(format!(
                    "Devin 等效成本（公开 API 价折算）: 近7天 ${:.2} · SWE-2 ${:.2} · 累计 ${:.2}",
                    self.st.usd_7d, self.st.swe2_usd_7d, self.st.usd_all
                )),
                "all" => {
                    let usd7: f64 = self.st.usd_7d
                        + self.st.apps.values().map(|a| a.usd_7d).sum::<f64>();
                    let usda: f64 = self.st.usd_all
                        + self.st.apps.values().map(|a| a.usd_all).sum::<f64>();
                    let real: f64 =
                        self.st.apps.values().map(|a| a.real_usd_all).sum();
                    let mut s = format!(
                        "全部应用估算合计: 近7天 ${:.2} · 累计 ${:.2}",
                        usd7, usda
                    );
                    if real > 0.0 {
                        s += &format!(" · 其中实扣 ${:.2}", real);
                    }
                    ui.label(s)
                }
                key => {
                    if let Some(ap) = self.st.apps.get(key) {
                        let an = match key {
                            "cursor" => "Cursor",
                            "antigravity" => "Antigravity",
                            "zcode" => "ZCode",
                            "grok" => "Grok",
                            "claude" => "Claude",
            "codex" => "Codex",
                            x => x,
                        };
                        let mut s = format!(
                            "{} 估算成本: 近7天 ${:.2} · 累计 ${:.2}",
                            an, ap.usd_7d, ap.usd_all
                        );
                        if ap.real_usd_all > 0.0 {
                            s += &format!(" · 订阅外实扣 ${:.2}", ap.real_usd_all);
                        }
                        ui.label(s)
                    } else {
                        ui.label("暂无该应用数据")
                    }
                }
            };
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let label = if self.collecting.is_some() {
                    "采集中…"
                } else {
                    "立即采集"
                };
                if ui
                    .add_enabled(self.collecting.is_none(), egui::Button::new(label))
                    .clicked()
                    && spawn_collect()
                {
                    self.collecting = Some(Instant::now());
                }
                if ui.button("复制报告").clicked() {
                    ctx.copy_text(self.current_report());
                }
                if ui
                    .button(if self.on_top { "取消置顶" } else { "置顶" })
                    .clicked()
                {
                    self.on_top = !self.on_top;
                    ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                        if self.on_top {
                            egui::WindowLevel::AlwaysOnTop
                        } else {
                            egui::WindowLevel::Normal
                        },
                    ));
                }
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            // 顶栏：logo + 标题 + 采集时间
            ui.horizontal(|ui| {
                if let Some(t) = &self.logo {
                    ui.image((t.id(), egui::vec2(22.0, 22.0)));
                }
                ui.heading("AI 用量");
                ui.label(
                    egui::RichText::new(concat!("v", env!("CARGO_PKG_VERSION"), "+", env!("GIT_HASH")))
                        .weak()
                        .small(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!("采集于 {}", self.st.last_collect_ago))
                            .weak()
                            .small(),
                    );
                });
            });
            ui.add_space(4.0);

            if !self.st.has_db {
                ui.add_space(16.0);
                ui.label("无数据 — 先跑 python devin_usage.py collect");
                return;
            }

            // 应用切换：各应用套餐/计费独立，不混计
            ui.horizontal(|ui| {
                for (key, label) in [
                    ("all", "总览"),
                    ("devin", "Devin"),
                    ("cursor", "Cursor"),
                    ("antigravity", "Antigravity"),
                    ("zcode", "ZCode"),
                    ("grok", "Grok"),
                    ("claude", "Claude"),
                    ("codex", "Codex"),
                ] {
                    if ui.selectable_label(self.tab == key, label).clicked()
                        && self.tab != key
                    {
                        self.tab = key;
                        self.sel_day = None;
                    }
                }
            });
            // 设备切换（多设备同步后才有 >1 个选项；wrapped 防超宽溢出）
            if self.st.devices.len() > 1 {
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new("设备").weak().small());
                    if ui
                        .selectable_label(self.device.is_none(), "全部")
                        .clicked()
                        && self.device.is_some()
                    {
                        self.device = None;
                        self.sel_day = None;
                    }
                    for d in self.st.devices.clone() {
                        let sel = self.device.as_deref() == Some(d.as_str());
                        if ui.selectable_label(sel, &d).clicked() && !sel {
                            self.device = Some(d.clone());
                            self.sel_day = None;
                        }
                    }
                });
            }
            ui.add_space(2.0);

            egui::ScrollArea::vertical().show(ui, |ui| {
                // ---- 配额卡片（Devin 专属）
                if self.tab == "devin" {
                    if let Some(q) = &self.st.quota {
                    card(ui, |ui| {
                        let c = quota_color(q.weekly_pct);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(format!("{} 周配额", q.plan)).strong());
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(format!("剩 {:.0}%", q.weekly_pct))
                                            .size(22.0)
                                            .color(c)
                                            .strong(),
                                    );
                                },
                            );
                        });
                        ui.add(
                            egui::ProgressBar::new(
                                (q.weekly_pct / 100.0).clamp(0.0, 1.0) as f32,
                            )
                            .fill(c)
                            .desired_height(10.0),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "超额余额 ${:.2} · 日重置 {}h 后 · 周重置 {:.1} 天后",
                                q.overage_usd,
                                (q.daily_reset - now()).max(0) / 3600,
                                (q.weekly_reset - now()).max(0) as f64 / 86400.0
                            ))
                            .small()
                            .weak(),
                        );
                    });
                    ui.add_space(6.0);
                    }
                }

                // ---- 趋势卡片（含历史回看选择器）
                card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("趋势").strong());
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                for (label, d) in [
                                    ("全部", 0i64),
                                    ("90天", 90),
                                    ("30天", 30),
                                    ("14天", 14),
                                    ("7天", 7),
                                ] {
                                    if ui
                                        .selectable_label(self.days == d, label)
                                        .clicked()
                                        && self.days != d
                                    {
                                        self.days = d;
                                        self.sel_day = None;
                                    }
                                }
                            },
                        );
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("每日 token · 点柱子看当日").small().weak(),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                let cur = self.sel_day.clone().unwrap_or_else(|| "选择日期".into());
                                egui::ComboBox::from_id_salt("daypick")
                                    .selected_text(cur)
                                    .show_ui(ui, |ui| {
                                        if ui
                                            .selectable_label(self.sel_day.is_none(), "不选")
                                            .clicked()
                                        {
                                            self.sel_day = None;
                                        }
                                        for d in self.charts.daily.iter().rev() {
                                            ui.selectable_value(
                                                &mut self.sel_day,
                                                Some(d.ymd.clone()),
                                                &d.ymd,
                                            );
                                        }
                                    });
                            },
                        );
                    });
                    self.daily_plot(ui);
                    if self.charts.daily.is_empty() {
                        ui.label(egui::RichText::new("暂无记录").weak().small());
                    } else {
                        ui.label(
                            egui::RichText::new(
                                "缓存读占大头是常态：KV cache 命中，不等于消耗配额",
                            )
                            .weak()
                            .small(),
                        );
                        self.day_detail(ui);
                    }
                });
                ui.add_space(6.0);

                // ---- SWE-2 卡片（紧凑数字列，不溢出）
                if self.tab == "devin" {
                    card(ui, |ui| {
                        ui.label(egui::RichText::new("SWE-2").strong());
                        egui::Grid::new("swe2")
                            .num_columns(6)
                            .spacing([10.0, 4.0])
                            .show(ui, |ui| {
                                for h in ["", "会话", "msg", "tool", "输出", "缓存读"] {
                                    ui.label(egui::RichText::new(h).weak().small());
                                }
                                ui.end_row();
                                for (label, a) in
                                    [("近7天", &self.st.swe2_7d), ("累计", &self.st.swe2_all)]
                                {
                                    ui.label(label);
                                    ui.monospace(format!("{}", a.sessions));
                                    ui.monospace(format!("{}", a.msgs));
                                    ui.monospace(format!("{}", a.tools));
                                    ui.monospace(tok_zh(a.tout));
                                    ui.monospace(tok_zh(a.tcr));
                                    ui.end_row();
                                }
                            });
                    });
                    ui.add_space(6.0);
                }

                // ---- 总览：所有应用配额一览 + 各应用摘要
                if self.tab == "all" {
                    card(ui, |ui| {
                        ui.label(egui::RichText::new("各应用配额").strong());
                        if let Some(q) = &self.st.quota {
                            quota_line(ui, "Devin 周配额", q.weekly_pct,
                                       Some(q.weekly_reset), None, None);
                        }
                        for (app_key, ap) in &self.st.apps {
                            let an = match app_key.as_str() {
                                "cursor" => "Cursor",
                                "antigravity" => "Antigravity",
                                "zcode" => "ZCode",
                                "grok" => "Grok",
                                "claude" => "Claude",
            "codex" => "Codex",
                                x => x,
                            };
                            if ap.quota_rows.is_empty() {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{an} · 无配额口径（本地统计）"
                                    ))
                                    .weak()
                                    .small(),
                                );
                            } else {
                                for (label, pct, resets, used, lim) in
                                    &ap.quota_rows
                                {
                                    quota_line(
                                        ui,
                                        &format!(
                                            "{an}·{}",
                                            label.trim_start_matches('_')
                                        ),
                                        *pct,
                                        *resets,
                                        *used,
                                        *lim,
                                    );
                                }
                            }
                        }
                    });
                    ui.add_space(6.0);
                    card(ui, |ui| {
                        ui.label(egui::RichText::new("各应用摘要").strong());
                        egui::Grid::new("overview")
                            .num_columns(4)
                            .spacing([14.0, 4.0])
                            .show(ui, |ui| {
                                for h in ["应用", "会话/请求", "Token 总量", "≈成本"] {
                                    ui.label(egui::RichText::new(h).weak().small());
                                }
                                ui.end_row();
                                let t = &self.st.total_all;
                                let sum = t.tin + t.tout + t.tcr + t.tcw;
                                ui.label("Devin");
                                ui.monospace(format!("{} 会话", t.sessions));
                                ui.monospace(tok_zh(sum));
                                ui.monospace(usd(self.st.usd_all));
                                ui.end_row();
                                for (app_key, ap) in &self.st.apps {
                                    let an = match app_key.as_str() {
                                        "cursor" => "Cursor",
                                        "antigravity" => "Antigravity",
                                        "zcode" => "ZCode",
                                        "grok" => "Grok",
                                        "claude" => "Claude",
            "codex" => "Codex",
                                        x => x,
                                    };
                                    let a = &ap.total_all;
                                    let asum = a.tin + a.tout + a.tcr + a.tcw;
                                    ui.label(an);
                                    ui.monospace(format!(
                                        "{} 会话 · {} 请求",
                                        ap.sessions_all, a.sessions
                                    ));
                                    ui.monospace(tok_zh(asum));
                                    ui.monospace(usd(ap.usd_all));
                                    ui.end_row();
                                }
                            });
                    });
                    ui.add_space(6.0);
                }

                // ---- 非 Devin 应用：配额卡（套餐配额）+ 概要卡
                if self.tab != "devin" && self.tab != "all" {
                    card(ui, |ui| {
                        if let Some(ap) = self.st.apps.get(self.tab) {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("配额").strong());
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if !ap.plan.is_empty() {
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "套餐 {}",
                                                    ap.plan
                                                ))
                                                .weak()
                                                .small(),
                                            );
                                        }
                                    },
                                );
                            });
                            if ap.quota_rows.is_empty() {
                                let hint = if self.tab == "antigravity" {
                                    "暂无配额数据 — Antigravity 需在 IDE 运行时采集"
                                } else {
                                    "暂无配额数据 — 先跑一次 collect"
                                };
                                ui.label(egui::RichText::new(hint).weak().small());
                            } else {
                                for (label, pct, resets, used, lim) in
                                    &ap.quota_rows
                                {
                                    quota_line(
                                        ui,
                                        label.trim_start_matches('_'),
                                        *pct,
                                        *resets,
                                        *used,
                                        *lim,
                                    );
                                }
                            }
                            ui.add_space(2.0);
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} 会话（7天 {}）· {} 次请求",
                                    ap.sessions_all,
                                    ap.sessions_7d,
                                    ap.total_all.sessions
                                ))
                                .weak()
                                .small(),
                            );
                            if !ap.extra.is_empty() {
                                ui.label(
                                    egui::RichText::new(&ap.extra).weak().small(),
                                );
                            }
                        } else {
                            ui.label("暂无该应用的数据 — 先跑一次 collect");
                        }
                    });
                    ui.add_space(6.0);
                }

                // ---- Token 构成卡片（跟随选中日期 > 时间区间）
                card(ui, |ui| {
                    let (rt, label) = if let Some(ymd) = &self.sel_day {
                        // 点中某天 → 构成卡变成那天的
                        let mut rt = Agg::default();
                        if let Some(d) = self.charts.daily.iter().find(|d| &d.ymd == ymd)
                        {
                            rt.tin = d.vals[0] as i64;
                            rt.tout = d.vals[1] as i64;
                            rt.tcr = d.vals[2] as i64;
                            rt.tcw = d.vals[3] as i64;
                        }
                        let sum = rt.tin + rt.tout + rt.tcr + rt.tcw;
                        (rt, format!("{ymd} 当天 {}", tok_zh(sum)))
                    } else {
                        let mut rt = Agg::default();
                        for d in &self.charts.daily {
                            rt.tin += d.vals[0] as i64;
                            rt.tout += d.vals[1] as i64;
                            rt.tcr += d.vals[2] as i64;
                            rt.tcw += d.vals[3] as i64;
                        }
                        let sum = rt.tin + rt.tout + rt.tcr + rt.tcw;
                        let label = if self.days > 0 {
                            format!("近{}天 {}", self.days, tok_zh(sum))
                        } else {
                            format!("累计 {}", tok_zh(sum))
                        };
                        (rt, label)
                    };
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Token 构成").strong());
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.label(egui::RichText::new(label).strong());
                            },
                        );
                    });
                    self.mix_strip(ui, &rt);
                });
                ui.add_space(6.0);

                // ---- 模型族卡片（跟随选中日期/区间；读后台缓存）
                card(ui, |ui| {
                    let models = &self.models;
                    let scope = self.sel_day.clone().unwrap_or_else(|| {
                        if self.days > 0 {
                            format!("近{}天", self.days)
                        } else {
                            "全部".into()
                        }
                    });
                    let note = match self.tab {
                        "devin" =>
                            "gpt-6-astra / gpt-5-6-* 等为 Devin 内部模型，无公开价，按 gpt 档折算",
                        "all" =>
                            "全部应用合并视图；各应用计费/订阅独立，成本仅作量级参考",
                        "cursor" =>
                            "Cost=Included 为订阅内用量；估算按 API 刊例价折算",
                        _ => "本地记录的 token 统计；按模型 API 价折算",
                    };
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(format!("模型族（{scope}）")).strong());
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    egui::RichText::new(format!("共 {} 个模型", models.len()))
                                        .weak()
                                        .small(),
                                );
                            },
                        );
                    });
                    if models.is_empty() {
                        ui.label(
                            egui::RichText::new("暂无该应用的模型记录 — 先跑一次 collect")
                                .weak(),
                        );
                    } else {
                        self.model_table(ui, models, note);
                    }
                });
                ui.add_space(6.0);
            });
        });
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}
