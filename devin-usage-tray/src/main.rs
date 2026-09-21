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
/// 托盘自身定时采集：进程活着就会每 15min 采一轮（不依赖任务计划/cron，
/// 笔记本睡醒后下个周期自然恢复）；首次在启动 20s 后
const COLLECT_INTERVAL: Duration = Duration::from_secs(15 * 60);
const FIRST_COLLECT_DELAY: Duration = Duration::from_secs(20);

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
    /// 口径：仅 devin 系（app/cli/unknown 源），其他应用走 apps
    pub all_models: Vec<ModelRow>,
    /// 其他应用独立统计（套餐各自独立，不混入 devin 数字）
    pub apps: std::collections::BTreeMap<String, AppStats>,
}

#[derive(Default)]
pub struct AppStats {
    pub total_7d: Agg,
    pub total_all: Agg,
    pub usd_7d: f64,
    pub usd_all: f64,
    pub real_usd_7d: f64,  // 服务端实扣（cursor 订阅外消耗）
    pub real_usd_all: f64,
    pub models: Vec<ModelRow>,
    pub sessions_7d: i64,
    pub sessions_all: i64,
    pub plan: String,      // 如 cursor 的 ultra
    pub extra: String,     // 附加说明行（如 tab 采纳行数）
    /// 配额快照：(label, 剩余%, 重置unix, used, limit)
    /// cursor: plan/auto/api；antigravity: _plan + 每模型一行
    pub quota_rows: Vec<(String, f64, Option<i64>, Option<f64>, Option<f64>)>,
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
    // devin 系总计（排除 cursor/antigravity——各应用套餐独立，不混计）
    const DEVIN_SRC: &str = "source NOT IN ('cursor','antigravity')";
    st.total_7d = conn
        .query_row(
            &format!("{SEL} WHERE {DEVIN_SRC} AND created_at>=?1"),
            [t7],
            |r| agg(r, 0),
        )
        .unwrap_or_default();
    st.total_all = conn
        .query_row(&format!("{SEL} WHERE {DEVIN_SRC}"), [], |r| agg(r, 0))
        .unwrap_or_default();

    // 等效成本：db 里的 model_prices 规则表（首个 prefix 命中）
    let rules = load_prices(&conn);
    st.swe2_usd_7d = cost(&st.swe2_7d, price_of("swe-2", &rules));
    st.swe2_usd_all = cost(&st.swe2_all, price_of("swe-2", &rules));
    // 全模型成本：逐模型行按各自价格累计（仅 devin 系）
    for (sql, args, dst) in [
        (format!("{SELM} WHERE {DEVIN_SRC} GROUP BY model"), vec![], &mut st.usd_all),
        (format!("{SELM} WHERE {DEVIN_SRC} AND created_at>=?1 GROUP BY model"), vec![t7], &mut st.usd_7d),
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
        "{SELSM} WHERE source NOT IN ('cursor','antigravity') GROUP BY source, model
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
    // 其他应用：独立统计（各应用套餐独立，绝不混入 devin 数字）
    for app in ["cursor", "antigravity"] {
        st.apps.insert(app.to_string(), load_app_stats(&conn, app, t7, &rules));
    }
    if let Ok(mut stmt) =
        conn.prepare(&format!("{SELSM} WHERE source NOT IN ('cursor','antigravity') AND created_at>=?1 GROUP BY source, model"))
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

/// 其他应用（cursor/antigravity）独立统计：
/// token 账本=usage_events（真实口径），会话数=local_sessions(source=app)
fn load_app_stats(
    conn: &Connection,
    app: &str,
    t7: i64,
    rules: &[(String, PriceRule)],
) -> AppStats {
    let mut a = AppStats::default();
    a.sessions_all = conn
        .query_row(
            "SELECT count(*) FROM local_sessions WHERE source=?1",
            [app],
            |r| r.get(0),
        )
        .unwrap_or(0);
    a.sessions_7d = conn
        .query_row(
            "SELECT count(*) FROM local_sessions WHERE source=?1 AND created_at>=?2",
            rusqlite::params![app, t7],
            |r| r.get(0),
        )
        .unwrap_or(0);
    for (all, since) in [(true, 0i64), (false, t7)] {
        let row = conn.query_row(
            "SELECT count(*), sum(tok_in), sum(tok_out), sum(tok_cache_read),
                    sum(tok_cache_write), sum(cost_usd)
             FROM usage_events WHERE app=?1 AND ts>=?2",
            rusqlite::params![app, since],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    r.get::<_, Option<f64>>(5)?.unwrap_or(0.0),
                ))
            },
        );
        if let Ok((n, i, o, cr, cw, rc)) = row {
            let (dst, real) = if all {
                (&mut a.total_all, &mut a.real_usd_all)
            } else {
                (&mut a.total_7d, &mut a.real_usd_7d)
            };
            dst.sessions += n; // 应用页"会话"列口径=请求数
            dst.tin = i;
            dst.tout = o;
            dst.tcr = cr;
            dst.tcw = cw;
            *real = rc;
        }
    }
    // 每模型明细（all + 7d 各一遍）
    for (is7d, since) in [(false, 0i64), (true, t7)] {
        if let Ok(mut s) = conn.prepare(
            "SELECT model, count(*), sum(tok_in), sum(tok_out), sum(tok_cache_read),
                    sum(tok_cache_write), sum(cost_usd)
             FROM usage_events WHERE app=?1 AND ts>=?2 GROUP BY model
             ORDER BY 3 DESC",
        ) {
            if let Ok(rows) = s.query_map(rusqlite::params![app, since], |r| {
                let mut ag = Agg::default();
                ag.sessions = r.get::<_, Option<i64>>(1)?.unwrap_or(0);
                ag.tin = r.get::<_, Option<i64>>(2)?.unwrap_or(0);
                ag.tout = r.get::<_, Option<i64>>(3)?.unwrap_or(0);
                ag.tcr = r.get::<_, Option<i64>>(4)?.unwrap_or(0);
                ag.tcw = r.get::<_, Option<i64>>(5)?.unwrap_or(0);
                Ok((
                    r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    ag,
                    r.get::<_, Option<f64>>(6)?.unwrap_or(0.0),
                ))
            }) {
                for (mdl, ag, rc) in rows.flatten() {
                    // antigravity 偶有多模型合并串 "?,gemini-3.8-flash" —— 取末段
                    let mdl = mdl
                        .rsplit(',')
                        .next()
                        .unwrap_or(&mdl)
                        .trim_start_matches('?')
                        .to_string();
                    let usd = if rc > 0.0 {
                        rc
                    } else {
                        cost(&ag, price_of(&mdl, rules))
                    };
                    if is7d {
                        if let Some(m) = a.models.iter_mut().find(|m| m.model == mdl) {
                            m.d7.sessions += ag.sessions;
                            m.d7.msgs += ag.msgs;
                            m.d7.tools += ag.tools;
                            m.d7.hours += ag.hours;
                            m.d7.tin += ag.tin;
                            m.d7.tout += ag.tout;
                            m.d7.tcr += ag.tcr;
                            m.d7.tcw += ag.tcw;
                            m.usd_7d += usd;
                        }
                    } else if let Some(m) =
                        a.models.iter_mut().find(|m| m.model == mdl)
                    {
                        // 规范化后重名的变体合并进同一行
                        m.all.sessions += ag.sessions;
                        m.all.msgs += ag.msgs;
                        m.all.tools += ag.tools;
                        m.all.hours += ag.hours;
                        m.all.tin += ag.tin;
                        m.all.tout += ag.tout;
                        m.all.tcr += ag.tcr;
                        m.all.tcw += ag.tcw;
                        m.usd_all += usd;
                    } else {
                        a.models.push(ModelRow {
                            source: app.to_string(),
                            model: mdl,
                            all: ag,
                            d7: Agg::default(),
                            usd_all: usd,
                            usd_7d: 0.0,
                        });
                    }
                }
            }
        }
    }
    // 估算成本：优先服务端实扣，否则按每模型行求和（各模型价不同，不能拿单一规则套总量）
    a.usd_all = if a.real_usd_all > 0.0 {
        a.real_usd_all
    } else {
        a.models.iter().map(|m| m.usd_all).sum()
    };
    a.usd_7d = if a.real_usd_7d > 0.0 {
        a.real_usd_7d
    } else {
        a.models.iter().map(|m| m.usd_7d).sum()
    };
    // 配额快照：每 label 取最新一条
    if let Ok(mut s) = conn.prepare(
        "SELECT label, pct_remaining, resets_at, used, lim FROM app_quota q
         WHERE app=?1 AND ts=(SELECT max(ts) FROM app_quota
                              WHERE app=q.app AND label=q.label)
         ORDER BY CASE label WHEN 'plan' THEN 0 WHEN '_plan' THEN 1
                             WHEN 'auto' THEN 2 WHEN 'api' THEN 3
                             ELSE 9 END, label",
    ) {
        if let Ok(rows) = s.query_map([app], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<f64>>(3)?,
                r.get::<_, Option<f64>>(4)?,
            ))
        }) {
            // 归并：antigravity 同模型的推理档位 "(High|Low|Medium|Thinking)"
            // 共享配额，按基名合并——取最紧剩余% + 最早重置时间
            let mut grouped: Vec<(String, f64, Option<i64>, Option<f64>, Option<f64>)> =
                Vec::new();
            for (label, pct, resets, used, lim) in rows.flatten() {
                let base = if label.ends_with(')') {
                    label
                        .rfind('(')
                        .map(|i| label[..i].trim_end().to_string())
                        .unwrap_or(label)
                } else {
                    label
                };
                if let Some(g) = grouped.iter_mut().find(|g| g.0 == base) {
                    g.1 = g.1.min(pct); // 最紧的剩余%为准
                    g.2 = match (g.2, resets) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                    g.3 = g.3.or(used);
                    g.4 = g.4.or(lim);
                } else {
                    grouped.push((base, pct, resets, used, lim));
                }
            }
            // plan 类在前，模型行按剩余%升序（最紧张的最显眼）
            grouped.sort_by(|x, y| {
                let rank = |l: &str| match l {
                    "plan" | "_plan" => 0,
                    "auto" | "api" => 1,
                    _ => 9,
                };
                rank(&x.0).cmp(&rank(&y.0)).then(x.1.total_cmp(&y.1))
            });
            a.quota_rows = grouped;
        }
    }
    // 附加信息：cursor 套餐 + tab/composer 采纳行数
    if let Ok(plan) = conn.query_row(
        "SELECT value FROM kv WHERE key=?1",
        [format!("{app}.plan")],
        |r| r.get::<_, String>(0),
    ) {
        a.plan = plan;
    }
    if app == "cursor" {
        let day7: String = conn
            .query_row("SELECT date(?1,'unixepoch','localtime')", [t7], |r| {
                r.get(0)
            })
            .unwrap_or_default();
        let mut s = String::new();
        for (metric, label) in
            [("tabAcceptedLines", "tab"), ("composerAcceptedLines", "composer")]
        {
            let v: i64 = conn
                .query_row(
                    "SELECT sum(value) FROM daily_activity
                     WHERE app='cursor' AND metric=?1 AND day>=?2",
                    rusqlite::params![metric, day7],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if v > 0 {
                s += &format!("{label} 采纳 {v} 行/7d  ");
            }
        }
        a.extra = s;
    }
    a
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
    next_collect: Instant,
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
            next_collect: Instant::now() + FIRST_COLLECT_DELAY,
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
        let mut c = Command::new(py);
        c.arg(&script).arg("collect").current_dir(&dir);
        #[cfg(target_os = "windows")]
        {
            // CREATE_NO_WINDOW：采集不弹控制台窗口
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x08000000);
        }
        c.spawn().is_ok()
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
        // 托盘自驱动定时采集：进程在就有 15min 一轮，睡醒后自然续上
        if Instant::now() >= self.next_collect {
            if spawn_collect() {
                // 采集完成后延迟刷新一次界面
                self.refresh_after_collect = Some(Instant::now() + COLLECT_REFRESH_DELAY);
            }
            self.next_collect = Instant::now() + COLLECT_INTERVAL;
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
