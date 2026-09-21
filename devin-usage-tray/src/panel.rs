//! panel.rs — egui 轻量面板窗口（`--panel` 模式）
//!
//! 托盘的第二形态：macOS 菜单栏拥挤被系统隐藏、或想要常驻桌面小组件时用。
//! 从托盘菜单"打开面板"唤起，或打成 .app 从 Dock/Spotlight 启动。
//! 卡片式布局：配额卡片 + 趋势图 + 每日堆叠柱状图 + token 构成条 + 全模型族表。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use egui_plot::{Bar, BarChart, Legend, Line, Plot};

use crate::{
    build_report, load_stats, now, open_db, paint_icon, spawn_collect, tok, Agg, ModelRow,
    Stats,
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
    let st0 = load_stats();
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
    quota: Vec<(String, f64)>, // (MM-DD HH:MM, 剩余%)
    daily: Vec<DayRow>,
}

/// days=0 → 全部历史；app="devin" 走 local_sessions，其他应用走 usage_events
fn load_charts(days: i64, app: &str) -> Charts {
    let mut c = Charts::default();
    let Some(conn) = open_db() else { return c };
    let t0 = if days > 0 { now() - days * 86400 } else { 0 };
    if app == "devin" {
        if let Ok(mut s) = conn.prepare(
            "SELECT strftime('%m-%d %H:%M', ts, 'unixepoch', 'localtime'),
                    weekly_quota_remaining_pct
             FROM quota_snapshots WHERE ts >= ?1 ORDER BY ts",
        ) {
            if let Ok(rows) =
                s.query_map([t0], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))
            {
                c.quota = rows.flatten().collect();
            }
        }
    }
    // 各列分开 sum：整行相加遇 NULL 会整行变 NULL（老会话无 metrics）
    let sql = match app {
        "devin" =>
            "SELECT strftime('%Y-%m-%d', created_at, 'unixepoch', 'localtime') day,
                    strftime('%m-%d', created_at, 'unixepoch', 'localtime') md,
                    sum(tok_in), sum(tok_out), sum(tok_cache_read), sum(tok_cache_write)
             FROM local_sessions
             WHERE created_at >= ?1
               AND source NOT IN ('cursor','antigravity','zcode','grok')
             GROUP BY 1 ORDER BY 1",
        "all" =>
            "SELECT day, substr(day,6) md,
                    sum(tok_in), sum(tok_out), sum(tok_cache_read), sum(tok_cache_write)
             FROM (
               SELECT strftime('%Y-%m-%d', created_at, 'unixepoch', 'localtime') day,
                      tok_in, tok_out, tok_cache_read, tok_cache_write
                 FROM local_sessions
                 WHERE created_at >= ?1
                   AND source NOT IN ('cursor','antigravity','zcode','grok')
               UNION ALL
               SELECT day, tok_in, tok_out, tok_cache_read, tok_cache_write
                 FROM usage_events WHERE ts >= ?1
             ) GROUP BY 1 ORDER BY 1",
        _ =>
            "SELECT day, substr(day,6) md,
                    sum(tok_in), sum(tok_out), sum(tok_cache_read), sum(tok_cache_write)
             FROM usage_events WHERE ts >= ?1 AND app = ?2
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
            "devin" | "all" => s.query_map(rusqlite::params![t0], parse).ok(),
            _ => s.query_map(rusqlite::params![t0, app], parse).ok(),
        };
        if let Some(rows) = rows {
            c.daily = rows.flatten().collect();
        }
    }
    c
}

/// 某一天的明细：该日记录数 + 分模型行（token 降序）。
/// devin 系按会话创建日；cursor/antigravity 按事件日（更贴近真实使用日）。
fn load_day_detail(
    ymd: &str,
    app: &str,
) -> (i64, Vec<(String, i64, i64, i64, i64, i64)>) {
    let mut n_sess = 0i64;
    let mut rows = Vec::new();
    let Some(conn) = open_db() else {
        return (0, rows);
    };
    let (sql, p2): (&str, Option<&str>) = match app {
        "devin" =>
            ("SELECT model, count(*) cnt, sum(n_user),
                    sum(tok_in), sum(tok_out), sum(tok_cache_read)
             FROM local_sessions
             WHERE strftime('%Y-%m-%d', created_at, 'unixepoch', 'localtime') = ?1
               AND source NOT IN ('cursor','antigravity','zcode','grok')
             GROUP BY model
             ORDER BY ifnull(sum(tok_in),0)+ifnull(sum(tok_out),0)
                      +ifnull(sum(tok_cache_read),0)+ifnull(sum(tok_cache_write),0) DESC",
             None),
        "all" =>
            ("SELECT model, count(*), sum(nm),
                    sum(tok_in), sum(tok_out), sum(tok_cache_read) FROM (
                SELECT 'devin·'||model model, n_user nm,
                       tok_in, tok_out, tok_cache_read FROM local_sessions
                 WHERE strftime('%Y-%m-%d', created_at, 'unixepoch', 'localtime') = ?1
                   AND source NOT IN ('cursor','antigravity','zcode','grok')
                UNION ALL
                SELECT app||'·'||model, 0, tok_in, tok_out, tok_cache_read
                  FROM usage_events WHERE day=?1
             ) GROUP BY model
             ORDER BY sum(ifnull(tok_in,0)+ifnull(tok_out,0)
                        +ifnull(tok_cache_read,0)) DESC",
             None),
        _ =>
            ("SELECT model, count(*), 0,
                    sum(tok_in), sum(tok_out), sum(tok_cache_read)
             FROM usage_events WHERE day=?1 AND app=?2 GROUP BY model
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
            s.query_map(rusqlite::params![ymd, a], parse).ok()
        } else {
            s.query_map([ymd], parse).ok()
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

struct Panel {
    st: Stats,
    charts: Charts,
    days: i64, // 图表回看范围：7/14/30/90，0=全部
    tab: &'static str, // "devin" | "cursor" | "antigravity" —— 各应用套餐独立
    report: String,
    logo: Option<egui::TextureHandle>,
    sel_day: Option<String>, // 图表中选中的日期（点柱子/下拉）
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
    ui.horizontal(|ui| {
        ui.add_sized(
            [150.0, 16.0],
            egui::Label::new(egui::RichText::new(name).small()).truncate(),
        );
        let frac = (pct / 100.0).clamp(0.0, 1.0) as f32;
        ui.add(
            egui::ProgressBar::new(frac)
                .desired_width(110.0)
                .desired_height(10.0)
                .fill(quota_color(pct)),
        );
        let pct_txt = if pct <= 0.0 {
            egui::RichText::new("已用尽").color(RED).small()
        } else {
            egui::RichText::new(format!("剩 {pct:.0}%")).small()
        };
        ui.add(egui::Label::new(pct_txt));
        let mut tail = String::new();
        if let (Some(u), Some(l)) = (used, lim) {
            tail += &format!(" {}/{}", u as i64, l as i64);
        }
        if let Some(r) = resets {
            let left = r - now();
            tail += &format!(
                " · 重置 {}",
                if left <= 0 {
                    "待刷新".to_string()
                } else if left >= 86400 {
                    format!("{}d{}h后", left / 86400, left % 86400 / 3600)
                } else {
                    format!("{}h后", left / 3600)
                }
            );
        }
        ui.label(egui::RichText::new(tail).weak().small());
    });
}

impl Panel {
    fn new(st: Stats) -> Self {
        let report = build_report(&st);
        Self {
            st,
            charts: load_charts(14, "devin"),
            days: 14,
            tab: "devin",
            report,
            logo: None,
            sel_day: None,
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

    /// x 轴标签抽稀
    fn x_fmt(
        labels: &[String],
        max: usize,
    ) -> impl Fn(egui_plot::GridMark, &std::ops::RangeInclusive<f64>) -> String {
        let owned = labels.to_vec();
        let step = (owned.len() / max).max(1);
        move |mark, _| {
            let i = mark.value.round() as usize;
            if i % step == 0 {
                owned.get(i).cloned().unwrap_or_default()
            } else {
                String::new()
            }
        }
    }

    /// 配额趋势线（y 下界随数据自适应、上界恒为 100；点图叠加让拐点可见）
    fn quota_plot(&self, ui: &mut egui::Ui) {
        if self.charts.quota.len() < 2 {
            return;
        }
        let labels: Vec<String> = self.charts.quota.iter().map(|(l, _)| l.clone()).collect();
        let pts: Vec<[f64; 2]> = self
            .charts
            .quota
            .iter()
            .enumerate()
            .map(|(i, (_, p))| [i as f64, *p])
            .collect();
        // y 下界：数据最小值向下取整到 20 的倍数再留 5pt 余量，<=100 顶格
        let min_v = pts.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min);
        let ymin = ((min_v / 20.0).floor() * 20.0 - 5.0).clamp(0.0, 80.0);
        Plot::new("quota")
            .height(95.0)
            .include_y(ymin)
            .include_y(100.0)
            .x_axis_formatter(Self::x_fmt(&labels, 4))
            .y_axis_formatter(|m, _| format!("{:.0}%", m.value))
            .label_formatter(|name, p| format!("{name} {:.0}%", p.y))
            .legend(Legend::default().position(egui_plot::Corner::LeftTop))
            .show(ui, |pui| {
                pui.line(Line::new("周配额剩余", pts.clone()).color(GREEN).width(2.0_f32));
                pui.points(
                    egui_plot::Points::new("周配额剩余", pts).color(GREEN).radius(2.0_f32),
                );
            });
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

    /// 选中某天的明细块（分模型）
    fn day_detail(&self, ui: &mut egui::Ui) {
        let Some(ymd) = &self.sel_day else { return };
        let (n_sess, rows) = load_day_detail(ymd, self.tab);
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
        list.sort_by(|a, b| b.1.usd.partial_cmp(&a.1.usd).unwrap_or(std::cmp::Ordering::Equal));
        if list.is_empty() {
            return;
        }
        egui::Grid::new("fams")
            .num_columns(7)
            .spacing([10.0, 4.0])
            .striped(true)
            .show(ui, |ui| {
                for h in ["模型族", "会话", "输入", "输出", "缓存读", "时长", "≈$"] {
                    ui.label(egui::RichText::new(h).weak().small());
                }
                ui.end_row();
                for (name, f) in &list {
                    ui.label(name);
                    ui.monospace(format!("{}", f.sessions));
                    ui.monospace(tok_zh(f.tin));
                    ui.monospace(tok_zh(f.tout));
                    ui.monospace(tok_zh(f.tcr));
                    ui.monospace(format!("{:.1}h", f.hours));
                    ui.monospace(usd(f.usd));
                    ui.end_row();
                }
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
        let due = self.reloaded.elapsed() >= RELOAD
            || self
                .collecting
                .is_some_and(|t| t.elapsed() >= COLLECT_DELAY);
        if due {
            self.st = load_stats();
            self.charts = load_charts(self.days, self.tab);
            self.report = build_report(&self.st);
            self.reloaded = Instant::now();
            self.collecting = None;
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
                        let mut s = format!(
                            "{} 估算成本: 近7天 ${:.2} · 累计 ${:.2}",
                            if key == "cursor" { "Cursor" } else { "Antigravity" },
                            ap.usd_7d,
                            ap.usd_all
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
                ] {
                    if ui.selectable_label(self.tab == key, label).clicked()
                        && self.tab != key
                    {
                        self.tab = key;
                        self.charts = load_charts(self.days, self.tab);
                        self.sel_day = None;
                    }
                }
            });
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
                                        self.charts = load_charts(d, self.tab);
                                    }
                                }
                            },
                        );
                    });
                    if self.tab == "devin" {
                        ui.label(egui::RichText::new("配额剩余 %").small().weak());
                        self.quota_plot(ui);
                        if self.charts.quota.len() < 2 {
                            ui.label(
                                egui::RichText::new("快照积累中（每 15min 一条）")
                                    .weak()
                                    .small(),
                            );
                        }
                        ui.add_space(4.0);
                    }
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
                                    "会话 {}（7d {}）· 请求 {}",
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

                // ---- Token 构成卡片（按当前应用；"all" 为双源合并）
                card(ui, |ui| {
                    let mut combined = self.st.total_all.clone();
                    for ap in self.st.apps.values() {
                        let a = &ap.total_all;
                        combined.sessions += a.sessions;
                        combined.tin += a.tin;
                        combined.tout += a.tout;
                        combined.tcr += a.tcr;
                        combined.tcw += a.tcw;
                    }
                    let t = match self.tab {
                        "devin" => &self.st.total_all,
                        key => self
                            .st
                            .apps
                            .get(key)
                            .map(|a| &a.total_all)
                            .unwrap_or(&combined),
                    };
                    let sum = t.tin + t.tout + t.tcr + t.tcw;
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Token 构成").strong());
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    egui::RichText::new(format!("累计 {}", tok_zh(sum)))
                                        .strong(),
                                );
                            },
                        );
                    });
                    self.mix_strip(ui, t);
                });
                ui.add_space(6.0);

                // ---- 模型族卡片（按当前应用）
                card(ui, |ui| {
                    let (models, note): (Vec<ModelRow>, &str) = match self.tab {
                        "devin" => (
                            self.st.all_models.clone(),
                            "gpt-6-astra / gpt-5-6-* 等为 Devin 内部模型，无公开价，按 gpt 档折算",
                        ),
                        "all" => (
                            self.st
                                .all_models
                                .iter()
                                .cloned()
                                .chain(self.st.apps.values().flat_map(|a| {
                                    a.models.iter().cloned()
                                }))
                                .collect(),
                            "全部应用合并视图；各应用计费/订阅独立，成本仅作量级参考",
                        ),
                        key => (
                            self.st
                                .apps
                                .get(key)
                                .map(|a| a.models.clone())
                                .unwrap_or_default(),
                            if key == "cursor" {
                                "Cost=Included 为订阅内用量；估算按 API 刊例价折算"
                            } else {
                                "本地记录的 token 统计；按模型 API 价折算"
                            },
                        ),
                    };
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("模型族（全部模型）").strong());
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
                        self.model_table(ui, &models, note);
                    }
                });
                ui.add_space(6.0);
            });
        });
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}
