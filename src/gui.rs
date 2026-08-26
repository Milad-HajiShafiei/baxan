use std::collections::HashMap;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, FontId, Pos2, Rect, Stroke, StrokeKind, Vec2};

use super::{EventKind, VariableEvent, VariableState};

// ---------------------------------------------------------------------------
// Colors
// ---------------------------------------------------------------------------
const STACK_COLOR: Color32 = Color32::from_rgb(40, 180, 80);
const HEAP_COLOR: Color32 = Color32::from_rgb(180, 60, 180);
const DATA_COLOR: Color32 = Color32::from_rgb(220, 200, 40);
const SYNC_COLOR: Color32 = Color32::from_rgb(80, 180, 240);
const OWN_COLOR: Color32 = Color32::from_rgb(220, 80, 220);
const BORROW_COLOR: Color32 = Color32::from_rgb(100, 180, 240);
const TEXT_DIM: Color32 = Color32::from_rgb(120, 120, 120);
const TEXT_BRIGHT: Color32 = Color32::from_rgb(220, 220, 220);

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
}

impl GuiApp {
    pub fn new(cc: &eframe::CreationContext<'_>, events: Vec<VariableEvent>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        cc.egui_ctx.style_mut_of(egui::Theme::Dark, |s| {
            s.spacing.item_spacing = Vec2::new(8.0, 4.0);
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
            status: "Ready \u{00b7} press V to open visualization".into(),
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
            "Playing"
        } else {
            "Paused"
        }
        .into();
    }

    fn reset_live(&mut self) {
        self.playing = false;
        self.follow_live = true;
        self.rebuild();
        self.status = "Following live events".into();
    }

    fn scrub_left(&mut self) {
        self.follow_live = false;
        self.playing = false;
        self.cursor_ms = self.cursor_ms.saturating_sub(200);
        self.rebuild();
    }

    fn scrub_right(&mut self) {
        self.follow_live = false;
        self.playing = false;
        self.cursor_ms = (self.cursor_ms + 200).min(self.max_ms);
        self.rebuild();
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
    }
}

// ---------------------------------------------------------------------------
// Welcome screen
// ---------------------------------------------------------------------------
impl GuiApp {
    fn draw_welcome(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(60.0);
            ui.heading("BAXAN");
            ui.label(
                egui::RichText::new("Live Rust Memory & Lifetime Visualizer").strong(),
            );
            ui.add_space(24.0);

            ui.horizontal(|ui| {
                ui.label("Press");
                ui.colored_label(Color32::from_rgb(80, 200, 240), "V");
                ui.label("to open the visualization");
            });
            ui.add_space(20.0);

            ui.label(format!("Loaded {} events", self.events.len()));
            ui.label(format!("Max timeline: {} ms", self.max_ms));
            ui.add_space(20.0);

            ui.label(
                egui::RichText::new("Keyboard shortcuts").strong(),
            );
            ui.label("  V        \u{2014} toggle visualization");
            ui.label("  Space    \u{2014} play / pause");
            ui.label("  R        \u{2014} follow live events");
            ui.label("  \u{2191}\u{2193}        \u{2014} select node");
            ui.label("  \u{2190}\u{2192}        \u{2014} scrub timeline");
            ui.add_space(24.0);

            if ui
                .button(
                    egui::RichText::new("Open Visualization  [V]")
                        .strong(),
                )
                .clicked()
            {
                self.show_viz = true;
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Visualization
// ---------------------------------------------------------------------------
impl GuiApp {
    fn draw_visualization(&mut self, ui: &mut egui::Ui) {
        use egui::containers::panel::{CentralPanel, Panel};

        // ---- top bar ----
        Panel::top("header").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("BAXAN")
                        .strong()
                        .color(Color32::from_rgb(80, 200, 240)),
                );
                ui.separator();
                ui.label(format!("{} live nodes", self.visible_events.len()));
                ui.separator();
                ui.label(format!("t = {} ms", self.cursor_ms));
                ui.separator();
                if self.playing {
                    ui.colored_label(
                        Color32::from_rgb(40, 200, 80),
                        "\u{25b6} Playing",
                    );
                } else {
                    ui.colored_label(
                        Color32::from_rgb(200, 200, 80),
                        "\u{23f8} Paused",
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new("V = toggle viz")
                            .color(TEXT_DIM),
                    );
                });
            });
        });

        // ---- bottom bar ----
        Panel::bottom("timeline").show(ui, |ui| {
            ui.horizontal(|ui| {
                let mut cursor = self.cursor_ms as f32;
                let max = self.max_ms as f32;
                if ui
                    .add(egui::Slider::new(&mut cursor, 0.0..=max).text("ms"))
                    .changed()
                {
                    self.cursor_ms = cursor as u64;
                    self.follow_live = false;
                    self.playing = false;
                    self.rebuild();
                }
                ui.separator();
                if ui.button("\u{25b6} Play").clicked() {
                    self.toggle_play();
                }
                if ui.button("\u{23f8} Pause").clicked() {
                    self.playing = false;
                }
                if ui.button("\u{23e9} Live").clicked() {
                    self.reset_live();
                }
            });
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(&self.status).color(TEXT_DIM),
                );
            });
        });

        // ---- right inspector ----
        Panel::right("inspector")
            .min_size(220.0)
            .default_size(240.0)
            .show(ui, |ui| {
                self.draw_inspector(ui);
            });

        // ---- central graph ----
        CentralPanel::default().show(ui, |ui| {
            self.draw_memory_graph(ui);
        });
    }

    // ---- inspector panel --------------------------------------------------
    fn draw_inspector(&mut self, ui: &mut egui::Ui) {
        ui.heading("Inspector");
        ui.separator();

        let Some(event) = self.visible_events.get(self.selected) else {
            ui.label("No variable selected");
            return;
        };
        let state = &self.states[&event.id];

        ui.horizontal(|ui| {
            ui.colored_label(
                Color32::from_rgb(80, 200, 240),
                &event.name,
            );
            ui.label(
                egui::RichText::new(&event.type_name)
                    .color(Color32::from_rgb(220, 200, 40)),
            );
        });
        ui.add_space(8.0);

        egui::Grid::new("inspector_grid").show(ui, |ui| {
            ui.label("value");
            ui.label(&event.value);
            ui.end_row();

            ui.label("zone");
            ui.label(zone_label(event_zone(event)));
            ui.end_row();

            ui.label("storage");
            ui.label(&event.storage);
            ui.end_row();

            ui.label("address");
            ui.label(if event.address.is_empty() {
                "?"
            } else {
                &event.address
            });
            ui.end_row();

            ui.label("bytes");
            ui.label(event.bytes.to_string());
            ui.end_row();

            ui.label("points to");
            ui.label(if event.points_to.is_empty() {
                "none".into()
            } else {
                event.points_to.join(", ")
            });
            ui.end_row();

            ui.label("borrows");
            ui.label(if event.borrows.is_empty() {
                "none".into()
            } else {
                event.borrows.join(", ")
            });
            ui.end_row();

            let lifetime = state
                .dropped_at
                .map(|d| format!("{} ms", d.saturating_sub(state.declared_at)))
                .unwrap_or_else(|| {
                    format!("{} ms+", self.cursor_ms.saturating_sub(state.declared_at))
                });
            ui.label("lifetime");
            ui.label(format!(
                "{} \u{2192} {} ({})",
                state.declared_at,
                state
                    .dropped_at
                    .map_or("live".into(), |t| t.to_string()),
                lifetime
            ));
            ui.end_row();

            ui.label("source");
            ui.label(&event.location);
            ui.end_row();

            ui.label("thread");
            ui.label(if event.thread.is_empty() {
                "main"
            } else {
                &event.thread
            });
            ui.end_row();

            ui.label("updates");
            ui.label(state.updates.to_string());
            ui.end_row();
        });
    }

    // ---- memory graph (painted) -------------------------------------------
    fn draw_memory_graph(&mut self, ui: &mut egui::Ui) {
        let available = ui.available_size();
        let (response, painter) =
            ui.allocate_painter(Vec2::new(available.x, available.y), egui::Sense::click());
        let rect = response.rect;

        let margin = 12.0_f32;
        let gap = 12.0_f32;
        let zone_w = (rect.width() - margin * 2.0 - gap) / 2.0;
        let zone_h = (rect.height() - margin * 2.0 - gap) / 2.0;

        let zone_defs: [(&str, &str, Color32, Pos2); 4] = [
            (
                "STACK / FRAMES",
                "stack",
                STACK_COLOR,
                rect.left_top() + Vec2::new(margin, margin),
            ),
            (
                "HEAP / OWNED",
                "heap",
                HEAP_COLOR,
                rect.left_top() + Vec2::new(margin + zone_w + gap, margin),
            ),
            (
                "DATA / STATIC",
                "data",
                DATA_COLOR,
                rect.left_top() + Vec2::new(margin, margin + zone_h + gap),
            ),
            (
                "SYNC / SHARED",
                "sync",
                SYNC_COLOR,
                rect.left_top() + Vec2::new(margin + zone_w + gap, margin + zone_h + gap),
            ),
        ];

        let mut node_rects: HashMap<String, Rect> = HashMap::new();

        for (label, zone, color, pos) in &zone_defs {
            let zone_rect = Rect::from_min_size(*pos, Vec2::new(zone_w, zone_h));

            // zone background + border
            painter.rect_filled(zone_rect, 6.0, color.linear_multiply(0.06));
            painter.rect_stroke(
                zone_rect,
                6.0,
                Stroke::new(1.0, color.linear_multiply(0.4)),
                StrokeKind::Inside,
            );

            // zone label
            painter.text(
                zone_rect.left_top() + Vec2::new(8.0, 6.0),
                egui::Align2::LEFT_TOP,
                *label,
                FontId::proportional(11.0),
                *color,
            );

            // nodes
            let zone_events: Vec<&VariableEvent> = self
                .visible_events
                .iter()
                .filter(|e| event_zone(e) == *zone)
                .collect();

            let area_x = zone_rect.left() + 8.0;
            let area_y = zone_rect.top() + 24.0;
            let area_w = zone_w - 16.0;
            let node_w = (area_w - 8.0) / 2.0;
            let node_h = 64.0_f32;
            let node_gap = 8.0_f32;

            for (j, event) in zone_events.iter().take(8).enumerate() {
                let col = j % 2;
                let row = j / 2;
                let nx = area_x + col as f32 * (node_w + node_gap);
                let ny = area_y + row as f32 * (node_h + node_gap);
                let nr = Rect::from_min_size(Pos2::new(nx, ny), Vec2::new(node_w, node_h));

                let is_selected = self
                    .visible_events
                    .get(self.selected)
                    .is_some_and(|s| s.id == event.id);
                let is_dropped = self
                    .states
                    .get(&event.id)
                    .is_some_and(|s| s.dropped_at.is_some());

                let node_color = if is_selected {
                    Color32::WHITE
                } else if is_dropped {
                    TEXT_DIM
                } else {
                    *color
                };

                painter.rect_filled(nr, 4.0, node_color.linear_multiply(0.12));
                painter.rect_stroke(
                    nr,
                    4.0,
                    Stroke::new(
                        if is_selected { 2.0 } else { 1.0 },
                        node_color.linear_multiply(0.6),
                    ),
                    StrokeKind::Inside,
                );

                let state = &self.states[&event.id];
                let upd = if state.updates > 0 {
                    " \u{21c4}"
                } else {
                    ""
                };

                painter.text(
                    nr.left_top() + Vec2::new(6.0, 4.0),
                    egui::Align2::LEFT_TOP,
                    truncate(&event.name, 18),
                    FontId::proportional(11.0),
                    node_color,
                );
                painter.text(
                    nr.left_top() + Vec2::new(6.0, 18.0),
                    egui::Align2::LEFT_TOP,
                    truncate(&event.type_name, 18),
                    FontId::proportional(10.0),
                    Color32::from_rgb(220, 200, 40),
                );
                painter.text(
                    nr.left_top() + Vec2::new(6.0, 32.0),
                    egui::Align2::LEFT_TOP,
                    truncate(&format!("{}B{}", event.bytes, upd), 18),
                    FontId::proportional(10.0),
                    TEXT_DIM,
                );
                painter.text(
                    nr.left_top() + Vec2::new(6.0, 46.0),
                    egui::Align2::LEFT_TOP,
                    truncate(&event.value, 18),
                    FontId::proportional(10.0),
                    TEXT_BRIGHT,
                );

                node_rects.insert(event.id.clone(), nr);
            }

            if zone_events.len() > 8 {
                painter.text(
                    zone_rect.left_bottom() + Vec2::new(8.0, -14.0),
                    egui::Align2::LEFT_BOTTOM,
                    format!("+{} more", zone_events.len() - 8),
                    FontId::proportional(10.0),
                    TEXT_DIM,
                );
            }
        }

        // arrows
        for event in &self.visible_events {
            let Some(from_r) = node_rects.get(&event.id) else {
                continue;
            };
            let from = from_r.center();
            for target in &event.points_to {
                if let Some(to_r) = node_rects.get(target.as_str()) {
                    draw_arrow(
                        &painter,
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
                        &painter,
                        from,
                        to_r.center(),
                        BORROW_COLOR,
                        true,
                        self.animation_frame,
                    );
                }
            }
        }

        // click to select
        if response.clicked()
            && let Some(pos) = response.interact_pointer_pos()
        {
            for (i, event) in self.visible_events.iter().enumerate() {
                if let Some(r) = node_rects.get(&event.id)
                    && r.contains(pos)
                {
                    self.selected = i;
                    break;
                }
            }
        }
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

fn zone_label(zone: &str) -> &'static str {
    match zone {
        "stack" => "STACK / FRAMES",
        "heap" => "HEAP / OWNED",
        "data" => "DATA / STATIC",
        "sync" => "SYNC / SHARED",
        _ => "OTHER",
    }
}

fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else if max <= 1 {
        "\u{2026}".into()
    } else {
        format!(
            "{}\u{2026}",
            chars[..max - 1].iter().collect::<String>()
        )
    }
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
    let start = from + dir * 28.0;
    let end = to - dir * 28.0;
    let length = (end - start).length();

    let stroke = Stroke::new(1.5, color);

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
    let back = end - dir * 10.0;
    painter.line_segment([end, back + perp * 5.0], stroke);
    painter.line_segment([end, back - perp * 5.0], stroke);
}
