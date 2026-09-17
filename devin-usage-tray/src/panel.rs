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

use crate::{build_report, load_stats, now, open_db, paint_icon, spawn_collect, tok, Stats};

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
            .with_title("Devin 用量")
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
        "Devin 用量",
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

#[derive(Default)]
struct Charts {
    quota: Vec<(String, f64)>,      // (MM-DD HH:MM, 剩余%)
    daily: Vec<(String, [f64; 4])>, // (MM-DD, [in, out, cr, cw])
}

/// days=0 → 全部历史
fn load_charts(days: i64) -> Charts {
    let mut c = Charts::default();
    let Some(conn) = open_db() else { return c };
    let t0 = if days > 0 { now() - days * 86400 } else { 0 };
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
    // 各列分开 sum：整行相加遇 NULL 会整行变 NULL（老会话无 metrics）
    if let Ok(mut s) = conn.prepare(
        "SELECT strftime('%m-%d', created_at, 'unixepoch', 'localtime'),
                sum(tok_in), sum(tok_out), sum(tok_cache_read), sum(tok_cache_write)
         FROM local_sessions WHERE created_at >= ?1 GROUP BY 1 ORDER BY 1",
    ) {
        if let Ok(rows) = s.query_map([t0], |r| {
            Ok((
                r.get::<_, String>(0)?,
                [
                    r.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
                    r.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
                    r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                    r.get::<_, Option<f64>>(4)?.unwrap_or(0.0),
                ],
            ))
        }) {
            c.daily = rows.flatten().collect();
        }
    }
    c
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
    report: String,
    logo: Option<egui::TextureHandle>,
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

impl Panel {
    fn new(st: Stats) -> Self {
        let report = build_report(&st);
        Self {
            st,
            charts: load_charts(14),
            days: 14,
            report,
            logo: None,
            reloaded: Instant::now(),
            collecting: None,
            on_top: false,
        }
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

    /// 配额趋势面积图（自动 y 范围——只收集了几小时时不再被压成平线）
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
        Plot::new("quota")
            .height(100.0)
            .x_axis_formatter(Self::x_fmt(&labels, 4))
            .y_axis_formatter(|m, _| format!("{:.0}%", m.value))
            .label_formatter(|name, p| format!("{name} {:.0}%", p.y))
            .legend(Legend::default())
            .show(ui, |pui| {
                pui.line(
                    Line::new("周配额剩余", pts)
                        .color(GREEN)
                        .width(2.0_f32)
                        .fill(0.0_f32),
                );
            });
    }

    /// 每日 token 堆叠柱状图（按 in/out/缓存读/缓存写 分色，单位 M）
    fn daily_plot(&self, ui: &mut egui::Ui) {
        if self.charts.daily.is_empty() {
            return;
        }
        let labels: Vec<String> = self.charts.daily.iter().map(|(l, _)| l.clone()).collect();
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
                        .map(|(i, (_, v))| {
                            let off: f64 = (0..*idx).map(|j| v[j] / 1e6).sum();
                            let mut b = Bar::new(i as f64, v[*idx] / 1e6)
                                .width(0.6)
                                .fill(*color);
                            b.base_offset = Some(off);
                            b
                        })
                        .collect::<Vec<Bar>>(),
                )
            })
            .collect();
        let labels_h = labels.clone();
        Plot::new("daily")
            .height(110.0)
            .x_axis_formatter(Self::x_fmt(&labels, 7))
            .y_axis_formatter(|m, _| format!("{:.0}M", m.value))
            .label_formatter(move |name, p| {
                // 悬停显示当日各构成（堆叠后 p.y 是累计顶，用天标签索引原值）
                let i = p.x.round() as usize;
                let day = labels_h.get(i).cloned().unwrap_or_default();
                format!("{day} · {name}")
            })
            .legend(Legend::default())
            .show(ui, |pui| {
                for c in charts {
                    pui.bar_chart(c);
                }
            });
    }

    /// token 构成比例条（累计 in/out/缓存读/缓存写）
    fn mix_strip(&self, ui: &mut egui::Ui) {
        let t = &self.st.total_all;
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

    /// 模型族汇总表（全模型，不限 swe-2；按等效$降序，最多 10 族 + 其他）
    fn model_table(&self, ui: &mut egui::Ui) {
        struct Fam {
            sessions: i64,
            msgs: i64,
            tout: i64,
            usd: f64,
            hours: f64,
            sources: Vec<String>,
        }
        let mut fams: BTreeMap<String, Fam> = BTreeMap::new();
        for m in &self.st.all_models {
            let f = fams.entry(family_of(&m.model)).or_insert(Fam {
                sessions: 0,
                msgs: 0,
                tout: 0,
                usd: 0.0,
                hours: 0.0,
                sources: vec![],
            });
            f.sessions += m.all.sessions;
            f.msgs += m.all.msgs;
            f.tout += m.all.tout;
            f.usd += m.usd_all;
            f.hours += m.all.hours;
            if !m.source.is_empty() && !f.sources.contains(&m.source) {
                f.sources.push(m.source.clone());
            }
        }
        let mut list: Vec<(String, Fam)> = fams.into_iter().collect();
        list.sort_by(|a, b| b.1.usd.partial_cmp(&a.1.usd).unwrap_or(std::cmp::Ordering::Equal));
        if list.is_empty() {
            return;
        }
        let total_fams = list.len();
        egui::Grid::new("fams")
            .num_columns(5)
            .spacing([12.0, 4.0])
            .striped(true)
            .show(ui, |ui| {
                for h in ["模型族", "会话", "msg", "输出", "≈$"] {
                    ui.label(egui::RichText::new(h).weak().small());
                }
                ui.end_row();
                for (name, f) in list.iter().take(10) {
                    let src = if f.sources.len() > 1 { "·" } else { "" };
                    ui.label(format!("{}{}", name, src));
                    ui.monospace(format!("{}", f.sessions));
                    ui.monospace(format!("{}", f.msgs));
                    ui.monospace(tok_zh(f.tout));
                    ui.monospace(format!("${:.2}", f.usd));
                    ui.end_row();
                }
                if total_fams > 10 {
                    ui.label(format!("…等 {} 族", total_fams));
                    ui.end_row();
                }
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
            self.charts = load_charts(self.days);
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
            ui.label(format!(
                "等效成本（公开 API 价折算）: 近7天 ${:.2} · SWE-2 ${:.2} · 累计 ${:.2}",
                self.st.usd_7d, self.st.swe2_usd_7d, self.st.usd_all
            ));
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
                    ctx.copy_text(self.report.clone());
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
                ui.heading("Devin 用量");
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

            egui::ScrollArea::vertical().show(ui, |ui| {
                // ---- 配额卡片
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
                                        self.charts = load_charts(d);
                                    }
                                }
                            },
                        );
                    });
                    ui.label(egui::RichText::new("配额剩余 %").small().weak());
                    self.quota_plot(ui);
                    if self.charts.quota.len() < 2 {
                        ui.label(
                            egui::RichText::new("快照积累中（每 15min 一条）").weak().small(),
                        );
                    }
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("每日 token（M）· 悬停看明细").small().weak(),
                    );
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
                    }
                });
                ui.add_space(6.0);

                // ---- SWE-2 卡片（紧凑数字列，不溢出）
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

                // ---- Token 构成卡片
                card(ui, |ui| {
                    let t = &self.st.total_all;
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
                    self.mix_strip(ui);
                });
                ui.add_space(6.0);

                // ---- 模型族卡片
                card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("模型族（全部模型）").strong());
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "共 {} 个模型",
                                        self.st.all_models.len()
                                    ))
                                    .weak()
                                    .small(),
                                );
                            },
                        );
                    });
                    self.model_table(ui);
                });
                ui.add_space(6.0);
            });
        });
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}
