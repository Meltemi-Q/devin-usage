//! panel.rs — egui 轻量面板窗口（`--panel` 模式）
//!
//! 托盘的第二形态：macOS 菜单栏拥挤被系统隐藏、或想要常驻桌面小组件时用。
//! 从托盘菜单"打开面板"唤起，或打成 .app 从 Dock/Spotlight 启动。

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::{build_report, load_stats, now, paint_icon, spawn_collect, tok, Stats};

const RELOAD: Duration = Duration::from_secs(60);
const COLLECT_DELAY: Duration = Duration::from_secs(8);

pub fn run() -> eframe::Result<()> {
    let icon = egui::IconData {
        rgba: paint_icon(None),
        width: 32,
        height: 32,
    };
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Devin 用量")
            .with_inner_size([430.0, 620.0])
            .with_min_inner_size([340.0, 460.0])
            .with_icon(Arc::new(icon)),
        ..Default::default()
    };
    eframe::run_native(
        "Devin 用量",
        opts,
        Box::new(|cc| {
            load_cjk_font(&cc.egui_ctx);
            Ok(Box::new(Panel::new()))
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
        egui::Color32::from_rgb(52, 199, 89)
    } else if pct >= 20.0 {
        egui::Color32::from_rgb(255, 190, 90)
    } else {
        egui::Color32::from_rgb(255, 90, 90)
    }
}

struct Panel {
    st: Stats,
    report: String,
    reloaded: Instant,
    collecting: Option<Instant>,
    on_top: bool,
}

impl Panel {
    fn new() -> Self {
        let st = load_stats();
        let report = build_report(&st);
        Self {
            st,
            report,
            reloaded: Instant::now(),
            collecting: None,
            on_top: false,
        }
    }

    fn agg_line(a: &crate::Agg) -> String {
        format!(
            "{}会话 · {}msg · {}tool | 入{} 出{} 缓读{} 缓写{}",
            a.sessions,
            a.msgs,
            a.tools,
            tok(a.tin),
            tok(a.tout),
            tok(a.tcr),
            tok(a.tcw)
        )
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
            self.report = build_report(&self.st);
            self.reloaded = Instant::now();
            self.collecting = None;
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Devin 用量");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!("采集于 {}", self.st.last_collect_ago))
                            .weak()
                            .small(),
                    );
                });
            });

            if !self.st.has_db {
                ui.add_space(16.0);
                ui.label("无数据 — 先跑 python devin_usage.py collect");
                return;
            }

            if let Some(q) = &self.st.quota {
                ui.add_space(6.0);
                let c = quota_color(q.weekly_pct);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("{} 周配额", q.plan)).strong());
                    ui.label(
                        egui::RichText::new(format!("剩 {:.0}%", q.weekly_pct))
                            .size(22.0)
                            .color(c)
                            .strong(),
                    );
                });
                ui.add(
                    egui::ProgressBar::new((q.weekly_pct / 100.0).clamp(0.0, 1.0) as f32)
                        .fill(c)
                        .desired_height(8.0),
                );
                ui.label(format!(
                    "超额余额 ${:.2} · 日重置 {}h 后 · 周重置 {:.1} 天后",
                    q.overage_usd,
                    (q.daily_reset - now()).max(0) / 3600,
                    (q.weekly_reset - now()).max(0) as f64 / 86400.0
                ));
            }

            ui.add_space(6.0);
            ui.separator();
            ui.add_space(4.0);
            ui.label(egui::RichText::new("SWE-2").strong());
            egui::Grid::new("swe2")
                .num_columns(2)
                .spacing([14.0, 4.0])
                .show(ui, |ui| {
                    ui.label("近7天");
                    ui.monospace(Self::agg_line(&self.st.swe2_7d));
                    ui.end_row();
                    ui.label("累计");
                    ui.monospace(format!(
                        "{} | {:.1}h",
                        Self::agg_line(&self.st.swe2_all),
                        self.st.swe2_all.hours
                    ));
                    ui.end_row();
                });

            if !self.st.per_model.is_empty() {
                ui.add_space(8.0);
                ui.label(egui::RichText::new("分模型").strong());
                egui::Grid::new("models")
                    .num_columns(5)
                    .spacing([14.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for h in ["模型", "7d会话/出", "7d≈$", "总会话", "总≈$"] {
                            ui.label(egui::RichText::new(h).weak().small());
                        }
                        ui.end_row();
                        for m in &self.st.per_model {
                            ui.monospace(&m.model);
                            ui.monospace(format!("{} / {}", m.d7.sessions, tok(m.d7.tout)));
                            ui.monospace(format!("${:.2}", m.usd_7d));
                            ui.monospace(format!("{}", m.all.sessions));
                            ui.monospace(format!("${:.2}", m.usd_all));
                            ui.end_row();
                        }
                    });
            }

            ui.add_space(6.0);
            ui.separator();
            ui.label(format!(
                "等效成本（公开 API 价折算）: 近7天 ${:.2} · SWE-2 ${:.2} · 累计 ${:.2}",
                self.st.usd_7d, self.st.swe2_usd_7d, self.st.usd_all
            ));

            ui.add_space(10.0);
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
        });
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}
