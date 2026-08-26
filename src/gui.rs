use std::collections::HashMap;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, Pos2, Rect, RichText,
    Sense, Slider, Stroke, StrokeKind, TextEdit, Vec2,
};

use super::{EventKind, VariableEvent, VariableState};

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------
const PANEL: Color32 = Color32::from_rgb(24, 26, 38);
const PANEL_2: Color32 = Color32::from_rgb(30, 33, 47);
const BORDER: Color32 = Color32::from_rgb(48, 54, 74);
const ACCENT: Color32 = Color32::from_rgb(94, 205, 255);
const STACK_COLOR: Color32 = Color32::from_rgb(52, 211, 153);
const HEAP_COLOR: Color32 = Color32::from_rgb(192, 132, 252);
const DATA_COLOR: Color32 = Color32::from_rgb(251, 191, 36);
const SYNC_COLOR: Color32 = Color32::from_rgb(96, 165, 250);
const OWN_COLOR: Color32 = Color32::from_rgb(232, 121, 249);
const BORROW_COLOR: Color32 = Color32::from_rgb(125, 211, 252);
const TEXT: Color32 = Color32::from_rgb(231, 234, 243);
const TEXT_DIM: Color32 = Color32::from_rgb(140, 147, 165);
const DROPPED: Color32 = Color32::from_rgb(248, 113, 113);
const ALIVE: Color32 = Color32::from_rgb(52, 211, 153);

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------
pub struct GuiApp {
    events: Vec<VariableEvent>,
    states: HashMap<String, VariableState>,
    visible_events: Vec<VariableEvent>,
    selected: usize,
    playing: bool,
    follow_live: bool,
    cursor_ms: u64,
    max_ms: u64,
    last_tick: Instant,
    animation_frame: u32,
    show_viz: bool,
    status: String,
    search: String,
    scale: f32,
    show_help: bool,
}

impl GuiApp {
    pub fn new(cc: &eframe::CreationContext<'_>, events: Vec<VariableEvent>) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = PANEL;
        visuals.window_fill = PANEL;
        visuals.extreme_bg_color = Color32::from_rgb(15, 16, 24);
        visuals.faint_bg_color = PANEL_2;
        visuals.override_text_color = Some(TEXT);
        visuals.selection.bg_fill = ACCENT.gamma_multiply(0.25);
        visuals.selection.stroke = Stroke::new(1.0, ACCENT);
        for wv in [
            &mut visuals.widgets.noninteractive,
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
        ] {
            wv.corner_radius = CornerRadius::same(6);
        }
        visuals.widgets.noninteractive.bg_fill = PANEL_2;
        visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        visuals.widgets.inactive.bg_fill = PANEL_2;
        visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(40, 44, 62);
        visuals.widgets.hovered.fg_stroke = Stroke::new(1.5, TEXT);
        visuals.widgets.active.bg_fill = ACCENT.gamma_multiply(0.22);
        visuals.widgets.active.fg_stroke = Stroke::new(2.0, ACCENT);
        cc.egui_ctx.set_visuals(visuals);

        cc.egui_ctx.style_mut_of(egui::Theme::Dark, |s| {
            s.spacing.item_spacing = Vec2::new(8.0, 6.0);
            s.spacing.button_padding = Vec2::new(12.0, 5.0);
            s.spacing.scroll.bar_width = 6.0;
            s.text_styles.insert(egui::TextStyle::Body, FontId::proportional(14.0));
            s.text_styles.insert(egui::TextStyle::Button, FontId::proportional(14.0));
            s.text_styles.insert(egui::TextStyle::Heading, FontId::proportional(22.0));
            s.text_styles.insert(egui::TextStyle::Small, FontId::proportional(11.0));
        });

        let mut app = Self {
            events,
            states: HashMap::new(),
            visible_events: Vec::new(),
            selected: 0,
            playing: false,
            follow_live: true,
            cursor_ms: 0,
            max_ms: 1,
            last_tick: Instant::now(),
            animation_frame: 0,
            show_viz: false,
            status: "Ready · press V to open visualization".into(),
            search: String::new(),
            scale: 1.0,
            show_help: false,
        };
        app.rebuild();
        app
    }

    // ---- state management -------------------------------------------------

    fn tick(&mut self) {
        self.animation_frame = self.animation_frame.wrapping_add(1);
        if self.playing && self.last_tick.elapsed() >= Duration::from_millis(120) {
            self.last_tick = Instant::now();
            self.cursor_ms = self.cursor_ms.saturating_add(120).min(self.max_ms);
            if self.cursor_ms >= self.max_ms {
                self.playing = false;
                self.status = "Playback complete".into();
            }
            self.rebuild();
        }
    }

    fn rebuild(&mut self) {
        self.max_ms = self.events.iter().map(|e| e.time_ms).max().unwrap_or(1);
        if self.follow_live {
            self.cursor_ms = self.max_ms;
        }
        self.states.clear();
        for event in &self.events {
            if event.time_ms > self.cursor_ms {
                break;
            }
            match event.kind {
                EventKind::Declare => {
                    self.states.insert(
                        event.id.clone(),
                        VariableState {
                            current: event.clone(),
                            declared_at: event.time_ms,
                            dropped_at: None,
                            updates: 0,
                        },
                    );
                }
                EventKind::Update => {
                    if let Some(state) = self.states.get_mut(&event.id) {
                        state.current = event.clone();
                        state.updates += 1;
                    }
                }
                EventKind::Drop => {
                    if let Some(state) = self.states.get_mut(&event.id) {
                        state.dropped_at = Some(event.time_ms);
                        state.current.value = "<dropped>".into();
                    }
                }
            }
        }
        self.visible_events = self
            .states
            .values()
            .filter(|s| s.is_alive_at(self.cursor_ms) || s.dropped_at.is_some())
            .map(|s| s.current.clone())
            .collect();
        self.visible_events.sort_by_key(|e| e.seq);
        if self.selected >= self.visible_events.len() {
            self.selected = self.visible_events.len().saturating_sub(1);
        }
    }

    fn toggle_play(&mut self) {
        if self.cursor_ms >= self.max_ms {
            self.cursor_ms = 0;
            self.rebuild();
        }
        self.playing = !self.playing;
        self.follow_live = false;
        self.status = if self.playing {
            "Playing".into()
        } else {
            "Paused".into()
        };
    }

    fn reset_live(&mut self) {
        self.playing = false;
        self.follow_live = true;
        self.rebuild();
        self.status = "Following live events".into();
    }

    fn scrub_to(&mut self, ms: u64) {
        self.cursor_ms = ms.min(self.max_ms);
        self.follow_live = false;
        self.playing = false;
        self.rebuild();
    }

    fn scrub_left(&mut self) {
        self.scrub_to(self.cursor_ms.saturating_sub(200));
    }

    fn scrub_right(&mut self) {
        self.scrub_to(self.cursor_ms.saturating_add(200));
    }
}

// ---------------------------------------------------------------------------
// eframe::App
// ---------------------------------------------------------------------------
impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // -- keyboard shortcuts --
        if ui.input(|i| i.key_pressed(egui::Key::V)) {
            self.show_viz = !self.show_viz;
        }
        if ui.input(|i| i.key_pressed(egui::Key::Space)) {
            self.toggle_play();
        }
        if ui.input(|i| i.key_pressed(egui::Key::R)) {
            self.reset_live();
        }
        if ui.input(|i| i.key_pressed(egui::Key::H)) {
            self.show_help = !self.show_help;
        }
        if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
            self.selected = self.selected.saturating_sub(1);
        }
        if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
            self.selected =
                (self.selected + 1).min(self.visible_events.len().saturating_sub(1));
        }
        if ui.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
            self.scrub_left();
        }
        if ui.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
            self.scrub_right();
        }

        // -- animation tick --
        self.tick();
        if self.playing {
            ui.ctx().request_repaint();
        }

        // -- draw --
        if self.show_viz {
            self.draw_visualization(ui);
        } else {
            self.draw_welcome(ui);
        }

        if self.show_help {
            self.draw_help(ui);
        }
    }
}

// ---------------------------------------------------------------------------
// Welcome screen
// ---------------------------------------------------------------------------
impl GuiApp {
    fn draw_welcome(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            let full = ui.available_size();
            ui.vertical_centered(|ui| {
                ui.add_space((full.y * 0.06).max(16.0));

                // logo mark
                ui.horizontal(|ui| {
                    let (d, p) = ui.allocate_painter(Vec2::new(18.0, 18.0), Sense::hover());
                    p.circle_filled(d.rect.center(), 8.0, ACCENT);
                    ui.label(RichText::new("BAXAN").size(46.0).strong().color(ACCENT));
                });
                ui.add_space(2.0);
                ui.label(
                    RichText::new("Live Rust memory & lifetime visualizer")
                        .size(16.0)
                        .color(TEXT_DIM),
                );
                ui.add_space(30.0);

                // stat cards
                let stats = [
                    ("Events", self.events.len().to_string()),
                    ("Live nodes", self.visible_events.len().to_string()),
                    ("Timeline", format_time(self.max_ms)),
                    ("Threads", distinct_threads(&self.events)),
                ];
                ui.horizontal(|ui| {
                    for (label, value) in stats {
                        stat_card(ui, label, &value);
                    }
                });
                ui.add_space(30.0);

                let btn = egui::Button::new(
                    RichText::new("Open Visualization   [V]").size(16.0).strong(),
                )
                .corner_radius(8)
                .min_size(Vec2::new(240.0, 46.0));
                if ui.add(btn).clicked() {
                    self.show_viz = true;
                }
                ui.add_space(30.0);

                // shortcuts panel
                egui::Frame::new()
                    .fill(PANEL)
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::symmetric(28, 16))
                    .stroke(Stroke::new(1.0, BORDER))
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("Shortcuts").strong().color(TEXT));
                            ui.add_space(8.0);
                            shortcut_row(ui, "V", "toggle visualization");
                            shortcut_row(ui, "Space", "play / pause");
                            shortcut_row(ui, "R", "follow live events");
                            shortcut_row(ui, "↑ ↓", "select node");
                            shortcut_row(ui, "← →", "scrub timeline");
                            shortcut_row(ui, "H", "help");
                        });
                    });
            });
        });
    }
}

// ---------------------------------------------------------------------------
// Visualization
// ---------------------------------------------------------------------------
impl GuiApp {
    fn draw_visualization(&mut self, ui: &mut egui::Ui) {
        use egui::containers::panel::{CentralPanel, Panel};

        // ---- top toolbar ----
        Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("BAXAN").strong().size(16.0).color(ACCENT));
                ui.separator();
                ui.label(
                    RichText::new(format!("{} nodes", self.visible_events.len()))
                        .color(TEXT_DIM),
                );
                ui.separator();
                ui.label(
                    RichText::new(format!("t = {}", format_time(self.cursor_ms)))
                        .color(TEXT_DIM),
                );
                ui.separator();
                ui.add(
                    TextEdit::singleline(&mut self.search)
                        .hint_text("Search name / type / value…")
                        .desired_width(190.0),
                );
                if !self.search.is_empty() && ui.small_button("clear").clicked() {
                    self.search.clear();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .button(RichText::new("? ").strong())
                        .on_hover_text("Help (H)")
                        .clicked()
                    {
                        self.show_help = !self.show_help;
                    }
                    ui.separator();
                    if ui
                        .button(RichText::new("Live").strong())
                        .on_hover_text("Follow live events (R)")
                        .clicked()
                    {
                        self.reset_live();
                    }
                    let label = if self.playing { "Pause" } else { "Play" };
                    if ui
                        .button(RichText::new(label).strong())
                        .on_hover_text("Play / pause (Space)")
                        .clicked()
                    {
                        self.toggle_play();
                    }
                    ui.separator();
                    ui.label(RichText::new("Zoom").color(TEXT_DIM));
                    ui.add(Slider::new(&mut self.scale, 0.6..=1.8).show_value(false));
                });
            });
            ui.add_space(4.0);
        });

        // ---- right inspector ----
        Panel::right("inspector")
            .min_size(250.0)
            .default_size(270.0)
            .show(ui, |ui| {
                self.draw_inspector(ui);
            });

        // ---- bottom timeline ----
        Panel::bottom("timeline").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui
                    .button(RichText::new("⏮").strong())
                    .on_hover_text("Restart")
                    .clicked()
                {
                    self.scrub_to(0);
                }
                let label = if self.playing { "⏸" } else { "▶" };
                if ui
                    .button(RichText::new(label).strong())
                    .on_hover_text("Play / pause (Space)")
                    .clicked()
                {
                    self.toggle_play();
                }
                if ui
                    .button(RichText::new("⏭").strong())
                    .on_hover_text("Follow live (R)")
                    .clicked()
                {
                    self.reset_live();
                }
                ui.separator();
                let mut cursor = self.cursor_ms as f32;
                let max = self.max_ms as f32;
                let slider = Slider::new(&mut cursor, 0.0..=max)
                    .text(format_time(self.cursor_ms))
                    .show_value(false);
                if ui.add(slider).changed() {
                    self.scrub_to(cursor as u64);
                }
                if self.follow_live {
                    ui.colored_label(ALIVE, "● live");
                }
            });
            ui.add_space(4.0);
            self.draw_event_ticks(ui);
            ui.horizontal(|ui| {
                ui.label(RichText::new(&self.status).color(TEXT_DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new("Space play · R live · H help · V close")
                            .color(TEXT_DIM),
                    );
                });
            });
            ui.add_space(4.0);
        });

        // ---- central graph ----
        CentralPanel::default().show(ui, |ui| {
            self.draw_memory_graph(ui);
        });
    }

    // ------------------------------------------------------------------
    // Inspector
    // ------------------------------------------------------------------
    fn draw_inspector(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Inspector").strong().size(16.0).color(TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!(
                        "{} / {}",
                        self.selected + 1,
                        self.visible_events.len()
                    ))
                    .color(TEXT_DIM),
                );
            });
        });
        ui.add_space(4.0);
        ui.separator();

        let Some(event) = self.visible_events.get(self.selected) else {
            ui.add_space(28.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No variable selected").color(TEXT_DIM));
                ui.label(
                    RichText::new("Click a node in the graph, or use ↑ ↓")
                        .size(12.0)
                        .color(TEXT_DIM),
                );
            });
            return;
        };
        let state = &self.states[&event.id];
        let color = zone_color(event_zone(event));

        // header card
        egui::Frame::new()
            .fill(PANEL_2)
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::same(10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (d, p) = ui.allocate_painter(Vec2::new(10.0, 10.0), Sense::hover());
                    p.circle_filled(d.rect.center(), 5.0, color);
                    ui.label(RichText::new(&event.name).strong().size(16.0).color(TEXT));
                });
                ui.label(RichText::new(&event.type_name).size(13.0).color(color));
                ui.add_space(4.0);
                ui.label(
                    RichText::new(&event.value)
                        .size(13.0)
                        .color(TEXT_DIM)
                        .italics(),
                );
            });
        ui.add_space(8.0);

        // lifetime bar
        ui.label(RichText::new("Lifetime").size(12.0).color(TEXT_DIM));
        draw_lifetime_bar(
            ui,
            state.declared_at,
            state.dropped_at,
            self.cursor_ms,
            self.max_ms,
            color,
        );
        ui.add_space(8.0);

        // size bar
        let max_bytes = self
            .visible_events
            .iter()
            .map(|e| e.bytes)
            .max()
            .unwrap_or(1)
            .max(1);
        ui.label(RichText::new("Size").size(12.0).color(TEXT_DIM));
        let frac = (event.bytes as f32 / max_bytes as f32).clamp(0.0, 1.0);
        let (resp, painter) =
            ui.allocate_painter(Vec2::new(ui.available_width(), 12.0), Sense::hover());
        let r = resp.rect;
        painter.rect_filled(r, CornerRadius::same(6), Color32::from_rgb(15, 16, 24));
        painter.rect_filled(
            Rect::from_min_size(r.min, Vec2::new(r.width() * frac, r.height())),
            CornerRadius::same(6),
            color.gamma_multiply(0.9),
        );
        resp.on_hover_text(format!("{} bytes (of {} max)", event.bytes, max_bytes));
        ui.add_space(8.0);

        // details grid
        egui::Frame::new()
            .fill(PANEL_2)
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::same(10))
            .show(ui, |ui| {
                egui::Grid::new("inspector_grid")
                    .num_columns(2)
                    .spacing(Vec2::new(12.0, 5.0))
                    .show(ui, |ui| {
                        detail_row(ui, "zone", zone_label(event_zone(event)));
                        detail_row(ui, "storage", &event.storage);
                        detail_row(ui, "address", if event.address.is_empty() { "?" } else { &event.address });
                        detail_row(ui, "bytes", &event.bytes.to_string());
                        detail_row(ui, "thread", if event.thread.is_empty() { "main" } else { &event.thread });
                        detail_row(ui, "location", &event.location);
                        detail_row(ui, "updates", &state.updates.to_string());
                        let points_to = if event.points_to.is_empty() {
                            "—".to_string()
                        } else {
                            event.points_to.join(", ")
                        };
                        detail_row(ui, "points to", &points_to);
                        let borrows = if event.borrows.is_empty() {
                            "—".to_string()
                        } else {
                            event.borrows.join(", ")
                        };
                        detail_row(ui, "borrows", &borrows);
                        let lifetime = state
                            .dropped_at
                            .map(|d| {
                                format!(
                                    "{} → {} ({} ms)",
                                    state.declared_at,
                                    d,
                                    d.saturating_sub(state.declared_at)
                                )
                            })
                            .unwrap_or_else(|| {
                                format!(
                                    "{} → live ({} ms+)",
                                    state.declared_at,
                                    self.cursor_ms.saturating_sub(state.declared_at)
                                )
                            });
                        detail_row(ui, "lifetime", &lifetime);
                    });
            });
        ui.add_space(8.0);

        // status chips
        ui.horizontal(|ui| {
            if let Some(d) = state.dropped_at {
                chip(ui, DROPPED, &format!("dropped @ {d} ms"));
            } else {
                chip(ui, ALIVE, "alive");
            }
            if state.updates > 0 {
                chip(ui, ACCENT, &format!("{} updates", state.updates));
            }
        });
    }

    // (lifetime bar moved to a free function below)

    // ------------------------------------------------------------------
    // Memory graph
    // ------------------------------------------------------------------
    fn draw_memory_graph(&mut self, ui: &mut egui::Ui) {
        // legend row
        ui.horizontal(|ui| {
            legend_dot(ui, STACK_COLOR, "stack");
            legend_dot(ui, HEAP_COLOR, "heap");
            legend_dot(ui, DATA_COLOR, "data");
            legend_dot(ui, SYNC_COLOR, "sync");
            ui.separator();
            ui.label(RichText::new("— owns").color(OWN_COLOR));
            ui.label(RichText::new("··· borrows").color(BORROW_COLOR));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!(
                        "{} nodes · zoom {:0.0}%",
                        self.visible_events.len(),
                        self.scale * 100.0
                    ))
                    .color(TEXT_DIM),
                );
            });
        });
        ui.separator();

        let scale = self.scale;
        let search_lc = self.search.to_lowercase();
        let mut node_rects: HashMap<String, Rect> = HashMap::new();
        let mut clicked: Option<usize> = None;
        let mut matches = 0usize;

        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .scroll_source(egui::containers::scroll_area::ScrollSource::ALL)
            .show(ui, |ui| {
                let avail = ui.available_width();
                let gap = 12.0;
                let zone_w = ((avail - gap) / 2.0).max(280.0);

                ui.horizontal(|ui| {
                    self.zone_panel(ui, "stack", STACK_COLOR, zone_w, scale, &search_lc, &mut node_rects, &mut clicked, &mut matches);
                    self.zone_panel(ui, "heap", HEAP_COLOR, zone_w, scale, &search_lc, &mut node_rects, &mut clicked, &mut matches);
                });
                ui.add_space(gap);
                ui.horizontal(|ui| {
                    self.zone_panel(ui, "data", DATA_COLOR, zone_w, scale, &search_lc, &mut node_rects, &mut clicked, &mut matches);
                    self.zone_panel(ui, "sync", SYNC_COLOR, zone_w, scale, &search_lc, &mut node_rects, &mut clicked, &mut matches);
                });

                // arrows (drawn above cards, in scroll content space)
                for event in &self.visible_events {
                    let Some(from_r) = node_rects.get(&event.id) else {
                        continue;
                    };
                    let from = from_r.center();
                    for target in &event.points_to {
                        if let Some(to_r) = node_rects.get(target.as_str()) {
                            draw_arrow(
                                ui.painter(),
                                from,
                                to_r.center(),
                                OWN_COLOR,
                                false,
                                self.animation_frame,
                            );
                        }
                    }
                    for target in &event.borrows {
                        if let Some(to_r) = node_rects.get(target.as_str()) {
                            draw_arrow(
                                ui.painter(),
                                from,
                                to_r.center(),
                                BORROW_COLOR,
                                true,
                                self.animation_frame,
                            );
                        }
                    }
                }
            });

        if let Some(i) = clicked {
            self.selected = i;
        }

        if self.visible_events.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.label(
                    RichText::new("No live variables at this point in the timeline")
                        .color(TEXT_DIM),
                );
            });
        } else if !self.search.is_empty() && matches == 0 {
            ui.vertical_centered(|ui| {
                ui.add_space(16.0);
                ui.label(
                    RichText::new(format!("No nodes match “{}”", self.search)).color(TEXT_DIM),
                );
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn zone_panel(
        &self,
        ui: &mut egui::Ui,
        zone: &str,
        color: Color32,
        width: f32,
        scale: f32,
        search_lc: &str,
        node_rects: &mut HashMap<String, Rect>,
        clicked: &mut Option<usize>,
        matches: &mut usize,
    ) {
        let zone_events: Vec<(usize, &VariableEvent)> = self
            .visible_events
            .iter()
            .enumerate()
            .filter(|(_, e)| event_zone(e) == zone)
            .collect();

        egui::Frame::new()
            .fill(PANEL)
            .stroke(Stroke::new(1.0, color.linear_multiply(0.35)))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(10))
            .show(ui, |ui| {
                ui.set_width(width - 20.0);
                ui.horizontal(|ui| {
                    let (d, p) = ui.allocate_painter(Vec2::new(12.0, 12.0), Sense::hover());
                    p.circle_filled(d.rect.center(), 5.0, color);
                    ui.label(RichText::new(zone_label(zone)).strong().size(13.0).color(color));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("{}", zone_events.len()))
                                .size(12.0)
                                .color(TEXT_DIM),
                        );
                    });
                });
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(6.0);

                if zone_events.is_empty() {
                    ui.add_space(6.0);
                    ui.label(RichText::new("— empty —").size(12.0).color(TEXT_DIM));
                    return;
                }

                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                    for (i, event) in &zone_events {
                        let is_selected = self
                            .visible_events
                            .get(self.selected)
                            .is_some_and(|s| s.id == event.id);
                        let is_match = search_lc.is_empty()
                            || event.name.to_lowercase().contains(search_lc)
                            || event.type_name.to_lowercase().contains(search_lc)
                            || event.value.to_lowercase().contains(search_lc);
                        if is_match {
                            *matches += 1;
                        }
                        let state = &self.states[&event.id];
                        let (rect, clicked_here) =
                            self.node_card(ui, event, state, color, is_selected, is_match, scale);
                        if clicked_here {
                            *clicked = Some(*i);
                        }
                        node_rects.insert(event.id.clone(), rect);
                    }
                });
            });
    }

    #[allow(clippy::too_many_arguments)]
    fn node_card(
        &self,
        ui: &mut egui::Ui,
        event: &VariableEvent,
        state: &VariableState,
        color: Color32,
        is_selected: bool,
        is_match: bool,
        scale: f32,
    ) -> (Rect, bool) {
        let w = 210.0 * scale;
        let h = 80.0 * scale;
        let (response, painter) = ui.allocate_painter(Vec2::new(w, h), Sense::click());
        let rect = response.rect;
        let hovered = response.hovered();
        let dim = if is_match { 1.0 } else { 0.3 };

        // glow behind selected / hovered card
        if is_selected || hovered {
            painter.rect_filled(
                rect.expand(3.0),
                CornerRadius::same(10),
                color.linear_multiply(if is_selected { 0.20 } else { 0.10 } * dim),
            );
        }

        let fill = color.linear_multiply(if is_selected { 0.22 } else { 0.09 } * dim);
        let border = if is_selected {
            ACCENT
        } else if hovered {
            color.gamma_multiply(0.9)
        } else {
            color.linear_multiply(0.5 * dim)
        };
        let border_w = if is_selected { 2.0 } else { 1.0 };

        painter.rect_filled(rect, CornerRadius::same(8), fill);
        painter.rect_stroke(
            rect,
            CornerRadius::same(8),
            Stroke::new(border_w, border),
            StrokeKind::Inside,
        );

        // left accent notch
        painter.rect_filled(
            Rect::from_min_size(rect.min, Vec2::new(3.0, rect.height())),
            CornerRadius::same(3),
            color.gamma_multiply(0.85),
        );

        let name_color = if is_match { TEXT } else { TEXT_DIM };
        painter.text(
            rect.left_top() + Vec2::new(12.0, 6.0),
            Align2::LEFT_TOP,
            truncate(&event.name, 24),
            FontId::proportional(12.5 * scale),
            name_color,
        );
        painter.text(
            rect.left_top() + Vec2::new(12.0, 6.0 + 19.0 * scale),
            Align2::LEFT_TOP,
            truncate(&event.type_name, 26),
            FontId::proportional(11.0 * scale),
            color,
        );
        painter.text(
            rect.left_top() + Vec2::new(12.0, 6.0 + 38.0 * scale),
            Align2::LEFT_TOP,
            truncate(&event.value, 30),
            FontId::proportional(10.5 * scale),
            TEXT_DIM,
        );

        // bottom row
        let mut bottom = format!("{} B", event.bytes);
        if state.updates > 0 {
            bottom.push_str(&format!(" · {} updates", state.updates));
        }
        painter.text(
            rect.left_bottom() + Vec2::new(12.0, -6.0),
            Align2::LEFT_BOTTOM,
            truncate(&bottom, 34),
            FontId::proportional(10.0 * scale),
            TEXT_DIM,
        );

        // dropped badge
        if state.dropped_at.is_some() {
            painter.text(
                rect.right_top() + Vec2::new(-10.0, 6.0),
                Align2::RIGHT_TOP,
                "dropped",
                FontId::proportional(10.0 * scale),
                DROPPED,
            );
        }

        let clicked = response.clicked();

        response.on_hover_ui(|ui| {
            egui::Frame::new()
                .fill(PANEL)
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::same(10))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&event.name).strong().size(15.0).color(TEXT));
                        ui.label(RichText::new(&event.type_name).size(13.0).color(color));
                    });
                    ui.label(RichText::new(&event.value).size(13.0).color(TEXT_DIM));
                    ui.separator();
                    egui::Grid::new("tip_grid")
                        .num_columns(2)
                        .spacing(Vec2::new(10.0, 3.0))
                        .show(ui, |ui| {
                            tip_row(ui, "zone", zone_label(event_zone(event)));
                            tip_row(ui, "storage", &event.storage);
                            tip_row(ui, "address", if event.address.is_empty() { "?" } else { &event.address });
                            tip_row(ui, "bytes", &event.bytes.to_string());
                            tip_row(ui, "location", &event.location);
                            tip_row(ui, "thread", if event.thread.is_empty() { "main" } else { &event.thread });
                            tip_row(
                                ui,
                                "lifetime",
                                &format!(
                                    "{} ms → {}",
                                    state.declared_at,
                                    state
                                        .dropped_at
                                        .map_or("live".into(), |d| format!("{d} ms"))
                                ),
                            );
                        });
                });
        });

        (rect, clicked)
    }

    // ------------------------------------------------------------------
    // Timeline
    // ------------------------------------------------------------------
    fn draw_event_ticks(&mut self, ui: &mut egui::Ui) {
        let height = 22.0;
        let (response, painter) = ui.allocate_painter(
            Vec2::new(ui.available_width(), height),
            Sense::click_and_drag(),
        );
        let rect = response.rect;
        painter.rect_filled(rect, CornerRadius::same(3), Color32::from_rgb(15, 16, 24));

        let max = self.max_ms.max(1) as f32;
        for e in &self.events {
            let x = rect.left() + (e.time_ms as f32 / max) * rect.width();
            let c = zone_color(event_zone(e));
            painter.line_segment(
                [Pos2::new(x, rect.top() + 3.0), Pos2::new(x, rect.bottom() - 3.0)],
                Stroke::new(1.0, c.gamma_multiply(0.85)),
            );
        }
        // cursor marker
        let cx = rect.left() + (self.cursor_ms as f32 / max) * rect.width();
        painter.line_segment(
            [Pos2::new(cx, rect.top()), Pos2::new(cx, rect.bottom())],
            Stroke::new(2.0, ACCENT),
        );

        if response.clicked() || response.dragged() {
            if let Some(pos) = response.interact_pointer_pos() {
                let t = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
                self.scrub_to((t * max) as u64);
            }
        }
        response.on_hover_text("Click or drag to scrub the timeline");
    }

    // ------------------------------------------------------------------
    // Help overlay
    // ------------------------------------------------------------------
    fn draw_help(&mut self, ui: &mut egui::Ui) {
        egui::Window::new("Help")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                ui.label(RichText::new("Shortcuts").strong());
                ui.add_space(4.0);
                shortcut_row(ui, "V", "open / close visualization");
                shortcut_row(ui, "Space", "play / pause");
                shortcut_row(ui, "R", "follow live events");
                shortcut_row(ui, "↑ ↓", "select node");
                shortcut_row(ui, "← →", "scrub timeline");
                shortcut_row(ui, "H", "toggle this help");
                ui.separator();
                ui.label(RichText::new("Zones").strong());
                ui.add_space(4.0);
                legend_row(ui, STACK_COLOR, "STACK / FRAMES", "local variables, borrows");
                legend_row(ui, HEAP_COLOR, "HEAP / OWNED", "Box, Vec, String, allocations");
                legend_row(ui, DATA_COLOR, "DATA / STATIC", "statics, consts");
                legend_row(ui, SYNC_COLOR, "SYNC / SHARED", "Arc, Rc, Mutex, channels");
                ui.separator();
                ui.label(RichText::new("Edges").strong());
                ui.add_space(4.0);
                legend_row(ui, OWN_COLOR, "— solid", "points to / owns");
                legend_row(ui, BORROW_COLOR, "··· dotted", "borrows");
                ui.add_space(8.0);
                if ui.button("Close").clicked() {
                    self.show_help = false;
                }
            });
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
pub fn event_zone(event: &VariableEvent) -> &str {
    if !event.zone.is_empty() {
        return &event.zone;
    }
    match event.storage.as_str() {
        "heap" | "box" | "vec" | "string" => "heap",
        "data" | "static" | "const" | "rodata" => "data",
        "arc" | "rc" | "mutex" | "rwlock" | "refcell" | "cell" | "atomic" => "sync",
        _ => "stack",
    }
}

pub fn zone_label(zone: &str) -> &'static str {
    match zone {
        "stack" => "STACK / FRAMES",
        "heap" => "HEAP / OWNED",
        "data" => "DATA / STATIC",
        "sync" => "SYNC / SHARED",
        _ => "OTHER",
    }
}

pub fn zone_color(zone: &str) -> Color32 {
    match zone {
        "stack" => STACK_COLOR,
        "heap" => HEAP_COLOR,
        "data" => DATA_COLOR,
        "sync" => SYNC_COLOR,
        _ => TEXT_DIM,
    }
}

fn format_time(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.2} s", ms as f64 / 1000.0)
    } else {
        format!("{ms} ms")
    }
}

fn distinct_threads(events: &[VariableEvent]) -> String {
    let set: std::collections::HashSet<&str> =
        events.iter().map(|e| e.thread.as_str()).filter(|t| !t.is_empty()).collect();
    if set.is_empty() {
        "1".into()
    } else {
        set.len().to_string()
    }
}

fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else if max <= 1 {
        "…".into()
    } else {
        format!("{}…", chars[..max - 1].iter().collect::<String>())
    }
}

fn stat_card(ui: &mut egui::Ui, label: &str, value: &str) {
    egui::Frame::new()
        .fill(PANEL)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(22, 12))
        .stroke(Stroke::new(1.0, BORDER))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(value).size(20.0).strong().color(ACCENT));
                ui.label(RichText::new(label).size(12.0).color(TEXT_DIM));
            });
        });
}

fn shortcut_row(ui: &mut egui::Ui, key: &str, desc: &str) {
    ui.horizontal(|ui| {
        egui::Frame::new()
            .fill(PANEL_2)
            .corner_radius(CornerRadius::same(4))
            .inner_margin(Margin::symmetric(8, 2))
            .show(ui, |ui| {
                ui.label(RichText::new(key).monospace().color(ACCENT));
            });
        ui.label(RichText::new(desc).color(TEXT_DIM));
    });
}

fn legend_dot(ui: &mut egui::Ui, color: Color32, text: &str) {
    ui.horizontal(|ui| {
        let (d, p) = ui.allocate_painter(Vec2::new(10.0, 10.0), Sense::hover());
        p.circle_filled(d.rect.center(), 4.0, color);
        ui.label(RichText::new(text).size(12.0).color(TEXT_DIM));
    });
}

fn legend_row(ui: &mut egui::Ui, color: Color32, name: &str, desc: &str) {
    ui.horizontal(|ui| {
        let (d, p) = ui.allocate_painter(Vec2::new(10.0, 10.0), Sense::hover());
        p.circle_filled(d.rect.center(), 4.0, color);
        ui.label(RichText::new(name).strong().size(12.5).color(color));
        ui.label(RichText::new(desc).size(12.0).color(TEXT_DIM));
    });
}

fn detail_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(RichText::new(label).size(12.0).color(TEXT_DIM));
    ui.label(RichText::new(value).size(12.5).color(TEXT));
    ui.end_row();
}

fn tip_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(RichText::new(label).size(12.0).color(TEXT_DIM));
    ui.label(RichText::new(value).size(12.0).color(TEXT));
    ui.end_row();
}

fn draw_lifetime_bar(
    ui: &mut egui::Ui,
    declared_at: u64,
    dropped_at: Option<u64>,
    cursor_ms: u64,
    max_ms: u64,
    color: Color32,
) {
    let (resp, painter) =
        ui.allocate_painter(Vec2::new(ui.available_width(), 14.0), Sense::hover());
    let r = resp.rect;
    painter.rect_filled(r, CornerRadius::same(7), Color32::from_rgb(15, 16, 24));
    let max = max_ms.max(1) as f32;
    let x0 = r.left() + (declared_at as f32 / max) * r.width();
    let x1 = r.left() + (dropped_at.unwrap_or(cursor_ms) as f32 / max) * r.width();
    if x1 > x0 {
        painter.rect_filled(
            Rect::from_min_max(Pos2::new(x0, r.top()), Pos2::new(x1, r.bottom())),
            CornerRadius::same(7),
            color.gamma_multiply(0.85),
        );
    }
    // cursor marker
    let cx = r.left() + (cursor_ms as f32 / max) * r.width();
    painter.line_segment(
        [Pos2::new(cx, r.top() + 1.0), Pos2::new(cx, r.bottom() - 1.0)],
        Stroke::new(1.5, TEXT),
    );
    resp.on_hover_text(format!(
        "declared {declared_at} ms → {}",
        dropped_at.map_or("live".into(), |d| format!("dropped {d} ms"))
    ));
}

fn chip(ui: &mut egui::Ui, color: Color32, text: &str) {
    egui::Frame::new()
        .fill(color.linear_multiply(0.15))
        .corner_radius(CornerRadius::same(20))
        .inner_margin(Margin::symmetric(10, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(12.0).color(color));
        });
}

fn draw_arrow(
    painter: &egui::Painter,
    from: Pos2,
    to: Pos2,
    color: Color32,
    dotted: bool,
    frame: u32,
) {
    let dir = to - from;
    let dist = dir.length();
    if dist < 60.0 {
        return;
    }
    let dir = dir.normalized();
    let start = from + dir * 30.0;
    let end = to - dir * 30.0;
    let length = (end - start).length();
    if length < 8.0 {
        return;
    }

    // soft glow under the line
    painter.line_segment([start, end], Stroke::new(3.5, color.linear_multiply(0.15)));
    let stroke = Stroke::new(1.6, color);

    if dotted {
        let dash = 6.0_f32;
        let gap = 4.0_f32;
        let total = dash + gap;
        let offset = (frame as f32 * 1.5) % total;
        let mut pos = offset;
        while pos < length {
            let p1 = start + dir * pos;
            let p2 = start + dir * (pos + dash).min(length);
            painter.line_segment([p1, p2], stroke);
            pos += total;
        }
    } else {
        painter.line_segment([start, end], stroke);
    }

    // arrowhead
    let perp = Vec2::new(-dir.y, dir.x);
    let back = end - dir * 11.0;
    painter.line_segment([end, back + perp * 6.0], stroke);
    painter.line_segment([end, back - perp * 6.0], stroke);
}
