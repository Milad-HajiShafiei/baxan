use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    symbols::Marker,
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Gauge, Paragraph, Tabs, Wrap,
        canvas::{Canvas, Context, Line as CanvasLine, Rectangle},
    },
};
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug)]
#[command(
    name = "baxan",
    version,
    about = "Live Rust memory and lifetime visualizer"
)]
struct Args {
    /// Rust project directory to observe.
    #[arg(short, long, default_value = ".")]
    project: PathBuf,
    /// JSONL event file emitted by an instrumented Rust process.
    #[arg(short, long)]
    events: Option<PathBuf>,
    /// Start with the bundled deterministic demo stream.
    #[arg(long)]
    demo: bool,
    /// Build and run the project with automatic heap-allocation tracking,
    /// then open the terminal visualization. No code changes required.
    #[arg(long)]
    run: bool,
    /// Export a JSON snapshot or text report and exit instead of opening the TUI.
    #[arg(long, value_name = "FILE")]
    export: Option<PathBuf>,
    /// Export format: json or text.
    #[arg(long, default_value = "json", requires = "export")]
    export_format: String,
    /// Extra arguments forwarded to the project binary (used with --run).
    args: Vec<String>,
}

/// The kind of lifecycle event observed for a variable.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A new variable comes into scope.
    Declare,
    /// A variable's value changes (mutation, resize, refcount change, etc.).
    Update,
    /// A variable goes out of scope and is dropped.
    Drop,
}

/// A single observation event describing a variable's state at a point in time.
///
/// This is the core data type of the JSONL protocol. Each line in a Baxan
/// event stream deserializes into one `VariableEvent`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VariableEvent {
    /// Monotonically increasing sequence number.
    pub seq: u64,
    /// Timestamp in milliseconds (used for timeline ordering).
    pub time_ms: u64,
    /// Lifecycle event kind (declare, update, or drop).
    pub kind: EventKind,
    /// Unique variable identifier (used for edges like `points_to` and `borrows`).
    pub id: String,
    /// Display name shown in the graph.
    pub name: String,
    /// Rust type name (e.g. `"Vec<u8>"`, `"Arc<State>"`).
    pub type_name: String,
    /// Human-readable value snapshot (e.g. `"len=4"`, `"ready: true"`).
    pub value: String,
    #[serde(default = "default_location")]
    pub location: String,
    #[serde(default = "default_storage")]
    pub storage: String,
    #[serde(default)]
    pub zone: String,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub points_to: Vec<String>,
    #[serde(default)]
    pub borrows: Vec<String>,
    #[serde(default)]
    pub bytes: usize,
    #[serde(default)]
    pub thread: String,
}

fn default_location() -> String {
    "unknown".into()
}

fn default_storage() -> String {
    "stack".into()
}

/// Internal state tracking a variable across its lifetime.
///
/// Tracks the most recent event, when the variable was declared,
/// when (if ever) it was dropped, and how many updates have been observed.
#[derive(Clone, Debug)]
pub struct VariableState {
    current: VariableEvent,
    /// The `time_ms` at which this variable was first declared.
    pub declared_at: u64,
    dropped_at: Option<u64>,
    updates: u32,
}

impl VariableState {
    /// Returns `true` if this variable is alive (declared and not yet dropped) at the given time.
    pub fn is_alive_at(&self, time_ms: u64) -> bool {
        self.declared_at <= time_ms && self.dropped_at.is_none_or(|drop| time_ms < drop)
    }
}

struct App {
    project: PathBuf,
    events_path: Option<PathBuf>,
    source_events: Vec<VariableEvent>,
    visible_events: Vec<VariableEvent>,
    states: HashMap<String, VariableState>,
    selected: usize,
    active_tab: usize,
    playing: bool,
    follow_live: bool,
    cursor_ms: u64,
    max_ms: u64,
    last_tick: Instant,
    animation_frame: u16,
    file_offset: u64,
    status: String,
    graph_focus: usize,
    graph_page: usize,
    filter: String,
    filter_active: bool,
    auto_scroll: bool,
}

impl App {
    fn with_events(project: PathBuf, source_events: Vec<VariableEvent>, events_path: Option<PathBuf>) -> Self {
        let mut app = Self {
            project,
            events_path,
            source_events,
            visible_events: Vec::new(),
            states: HashMap::new(),
            selected: 0,
            active_tab: 0,
            playing: false,
            follow_live: true,
            cursor_ms: 0,
            max_ms: 1,
            last_tick: Instant::now(),
            animation_frame: 0,
            file_offset: 0,
            status: "Ready · memory map follows runtime events".into(),
            graph_focus: 0,
            graph_page: 0,
            filter: String::new(),
            filter_active: false,
            auto_scroll: true,
        };
        app.rebuild();
        app
    }

    fn new(project: PathBuf, events_path: Option<PathBuf>, demo: bool) -> Self {
        let source_events = if demo || events_path.is_none() {
            demo_events()
        } else {
            Vec::new()
        };
        let mut app = Self {
            project,
            events_path,
            source_events,
            visible_events: Vec::new(),
            states: HashMap::new(),
            selected: 0,
            active_tab: 0,
            playing: false,
            follow_live: true,
            cursor_ms: 0,
            max_ms: 1,
            last_tick: Instant::now(),
            animation_frame: 0,
            file_offset: 0,
            status: "Ready · memory map follows runtime events".into(),
            graph_focus: 0,
            graph_page: 0,
            filter: String::new(),
            filter_active: false,
            auto_scroll: true,
        };
        app.rebuild();
        app
    }

    fn tick(&mut self) {
        self.animation_frame = self.animation_frame.wrapping_add(1);
        if (self.playing || self.follow_live) && self.auto_scroll && self.animation_frame % 30 == 0 {
            self.graph_page = self.graph_page.wrapping_add(1);
        }
        if self.events_path.is_some() && self.follow_live {
            self.read_new_events();
        }
        if self.playing {
            let elapsed_ms = self.last_tick.elapsed().as_millis() as u64;
            if elapsed_ms == 0 {
                return;
            }
            self.last_tick = Instant::now();
            self.cursor_ms = self.cursor_ms.saturating_add(elapsed_ms).min(self.max_ms);
            if self.cursor_ms >= self.max_ms {
                self.playing = false;
            }
            self.rebuild();
        }
    }

    fn step_to_next_event(&mut self) {
        let next_time = self
            .source_events
            .iter()
            .map(|event| event.time_ms)
            .filter(|&time| time > self.cursor_ms)
            .min();
        self.follow_live = false;
        self.playing = false;
        self.cursor_ms = next_time.unwrap_or(self.max_ms);
        self.rebuild();
        self.status = format!("Step: {}ms / {}ms", self.cursor_ms, self.max_ms);
    }

    fn step_to_previous_event(&mut self) {
        let previous_time = self
            .source_events
            .iter()
            .map(|event| event.time_ms)
            .filter(|&time| time < self.cursor_ms)
            .max();
        self.follow_live = false;
        self.playing = false;
        self.cursor_ms = previous_time.unwrap_or(0);
        self.rebuild();
        self.status = format!("Step: {}ms / {}ms", self.cursor_ms, self.max_ms);
    }

    fn read_new_events(&mut self) {
        let Some(path) = self.events_path.clone() else {
            return;
        };
        let Ok(metadata) = fs::metadata(&path) else {
            return;
        };
        if metadata.len() < self.file_offset {
            self.file_offset = 0;
            self.source_events.clear();
        }
        let Ok(file) = File::open(&path) else {
            return;
        };
        let mut reader = BufReader::new(file);
        if self.file_offset > 0 && reader.seek_relative(self.file_offset as i64).is_err() {
            return;
        }
        let mut line = String::new();
        let mut added = 0;
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            if let Ok(event) = serde_json::from_str::<VariableEvent>(line.trim()) {
                if self.source_events.len() < MAX_RECORD_EVENTS {
                    self.source_events.push(event);
                    added += 1;
                }
            }
            line.clear();
        }
        self.file_offset = metadata.len();
        if added > 0 {
            self.status = format!(
                "Live · received {added} event{} · map refreshed",
                if added == 1 { "" } else { "s" }
            );
            self.rebuild();
        }
    }

    fn rebuild(&mut self) {
        self.max_ms = self
            .source_events
            .iter()
            .map(|event| event.time_ms)
            .max()
            .unwrap_or(1);
        if self.follow_live {
            self.cursor_ms = self.max_ms;
        }
        self.states.clear();
        for event in &self.source_events {
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
        self.rebuild_visible();
    }

    fn rebuild_visible(&mut self) {
        let filter = self.filter.to_lowercase();
        self.visible_events = self
            .states
            .values()
            .filter(|state| state.is_alive_at(self.cursor_ms) || state.dropped_at.is_some())
            .filter(|state| filter.is_empty() || matches_filter(state, &filter))
            .map(|state| state.current.clone())
            .collect();
        self.visible_events
            .sort_by_key(|event| (event.time_ms, event.seq));
        self.visible_events.truncate(MAX_VISIBLE_NODES);
        self.visible_events.sort_by_key(|event| (event.time_ms, event.seq));
        if self.selected >= self.visible_events.len() {
            self.selected = self.visible_events.len().saturating_sub(1);
        }
        if self.graph_focus >= self.visible_events.len() {
            self.graph_focus = self.visible_events.len().saturating_sub(1);
        }
        self.graph_page = self.graph_focus / GRAPH_PAGE_SIZE;
    }

    fn selected_state(&self) -> Option<&VariableState> {
        let event = self.visible_events.get(self.selected)?;
        self.states.get(&event.id)
    }

    fn toggle_play(&mut self) {
        if self.source_events.is_empty() {
            self.status = "No events available for playback".into();
            return;
        }
        if self.cursor_ms >= self.max_ms {
            self.cursor_ms = 0;
            self.rebuild();
        }
        self.last_tick = Instant::now();
        self.playing = !self.playing;
        self.follow_live = false;
        self.status = if self.playing {
            "Playback running · use ← → to scrub"
        } else {
            "Playback paused"
        }
        .into();
    }

    fn reset_live(&mut self) {
        self.playing = false;
        self.last_tick = Instant::now();
        self.follow_live = true;
        self.rebuild();
        self.status = "Live · following newest event".into();
    }

    fn toggle_filter(&mut self) {
        self.filter_active = !self.filter_active;
        self.status = if self.filter_active {
            "Filter enabled · type text, Enter applies, Esc clears".into()
        } else {
            "Filter disabled".into()
        };
    }

    fn clear_filter(&mut self) {
        self.filter.clear();
        self.filter_active = false;
        self.rebuild_visible();
        self.status = "Filter cleared".into();
    }

    fn export_record(&mut self) {
        let path = self.project.join(".baxan-recording.jsonl");
        let result = File::create(&path).and_then(|mut file| {
            for event in &self.source_events {
                writeln!(file, "{}", serde_json::to_string(event).unwrap_or_default())?;
            }
            Ok(())
        });
        self.status = result
            .map(|_| format!("Saved recording to {}", path.display()))
            .unwrap_or_else(|error| format!("Save failed: {error}"));
    }
}

const ZONES: [&str; 4] = ["stack", "heap", "data", "sync"];
const GRAPH_PAGE_SIZE: usize = 8;
const MAX_VISIBLE_NODES: usize = 2_000;
const MAX_RECORD_EVENTS: usize = 100_000;

fn ui(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(17),
            Constraint::Length(5),
            Constraint::Length(2),
        ])
        .split(area);
    draw_header(frame, app, vertical[0]);

    if app.active_tab == 0 {
        draw_visualize_panel(frame, app, vertical[1]);
        draw_graph_hints(frame, vertical[1]);
        draw_controls(frame, app, vertical[2], vertical[3]);
        return;
    }
    if app.active_tab == 2 {
        draw_record_view(frame, app, vertical[1]);
        draw_controls(frame, app, vertical[2], vertical[3]);
        return;
    }
    if app.active_tab == 3 {
        draw_relationships_view(frame, app, vertical[1]);
        draw_controls(frame, app, vertical[2], vertical[3]);
        return;
    }

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(vertical[1]);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(11), Constraint::Length(9)])
        .split(body[0]);
    let zones = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(25); 4])
        .split(left[0]);
    for (index, zone) in ZONES.iter().enumerate() {
        frame.render_widget(zone_panel(app, zone), zones[index]);
    }
    frame.render_widget(lifetime_panel(app), left[1]);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(12), Constraint::Min(8)])
        .split(body[1]);
    frame.render_widget(inspector_panel(app), right[0]);
    frame.render_widget(relationship_panel(app), right[1]);
    draw_controls(frame, app, vertical[2], vertical[3]);
}

fn draw_header(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let header = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(12),
            Constraint::Min(20),
            Constraint::Length(48),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "BAXAN",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ◈", Style::default().fg(Color::Yellow)),
        ]))
        .block(header_block()),
        header[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("PROJECT ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                app.project
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Rust project"),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                app.project.display().to_string(),
                Style::default().fg(Color::Gray),
            ),
        ]))
        .block(header_block()),
        header[1],
    );
    let tabs = ["VISUALIZE", "MEMORY MAP", "RECORD", "RELATIONSHIPS"]
        .into_iter()
        .map(Line::from)
        .collect::<Vec<_>>();
    frame.render_widget(
        Tabs::new(tabs)
            .select(app.active_tab)
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .divider(" ")
            .block(header_block()),
        header[2],
    );
}

fn draw_record_view(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let lines = app
        .source_events
        .iter()
        .filter(|event| event.time_ms <= app.cursor_ms || app.follow_live)
        .rev()
        .take(MAX_RECORD_EVENTS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|event| {
            let kind = match event.kind {
                EventKind::Declare => "DECLARE",
                EventKind::Update => "UPDATE ",
                EventKind::Drop => "DROP   ",
            };
            Line::from(vec![
                Span::styled(format!("{:>6}ms ", event.time_ms), Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{} ", kind), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled(event.name.clone(), Style::default().fg(Color::Cyan)),
                Span::raw(format!(" : {} = {} ({} bytes)", event.type_name, event.value, event.bytes)),
            ])
        })
        .collect::<Vec<_>>();
    let scroll = if app.auto_scroll {
        lines.len().saturating_sub(area.height.saturating_sub(2) as usize) as u16
    } else {
        0
    };
    let content = if lines.is_empty() {
        Text::from("No recorded events yet. Use --events or --run to load a recording.")
    } else {
        Text::from(lines)
    };
    frame.render_widget(
        Paragraph::new(content)
            .wrap(Wrap { trim: true })
            .scroll((scroll, 0))
            .block(panel_block(" recording events ")),
        area,
    );
}

fn draw_relationships_view(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let mut lines = Vec::new();
    for event in &app.visible_events {
        for target in &event.points_to {
            lines.push(Line::from(vec![
                Span::styled(event.name.clone(), Style::default().fg(Color::Yellow)),
                Span::styled("  ── owns/update ──▶  ", Style::default().fg(Color::Magenta)),
                Span::styled(target.clone(), Style::default().fg(Color::White)),
            ]));
        }
        for target in &event.borrows {
            lines.push(Line::from(vec![
                Span::styled(event.name.clone(), Style::default().fg(Color::Yellow)),
                Span::styled("  ··· borrows/reference ···▶  ", Style::default().fg(Color::LightBlue)),
                Span::styled(target.clone(), Style::default().fg(Color::White)),
            ]));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "No ownership or borrow relationships reported.",
            Style::default().fg(Color::DarkGray),
        )));
    }
    let scroll = if app.auto_scroll {
        lines.len().saturating_sub(area.height.saturating_sub(2) as usize) as u16
    } else {
        0
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: true })
            .scroll((scroll, 0))
            .block(panel_block(" ownership & borrow relationships ")),
        area,
    );
}

fn draw_visualize_panel(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let canvas = Canvas::default()
        .block(
            Block::default()
                .title(" live memory graph ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .marker(Marker::Braille)
        .x_bounds([0.0, 100.0])
        .y_bounds([0.0, 100.0])
        .paint(|ctx: &mut Context| paint_memory_graph(ctx, app));
    frame.render_widget(canvas, area);
}

fn paint_memory_graph(ctx: &mut Context, app: &App) {
    let focused_id = app.visible_events.get(app.graph_focus).map(|event| event.id.as_str());
    let zone_positions = [
        ("stack", 2.0, 52.0),
        ("heap", 52.0, 52.0),
        ("data", 2.0, 25.0),
        ("sync", 52.0, 25.0),
    ];
    let mut positions = HashMap::new();
    for (zone, x, y) in zone_positions {
        let events = app
            .visible_events
            .iter()
            .filter(|event| event_zone(event) == zone)
            .collect::<Vec<_>>();
        if events.is_empty() {
            continue;
        }
        let color = zone_color(zone);
        ctx.draw(&Rectangle {
            x,
            y,
            width: 46.0,
            height: 42.0,
            color,
        });
        ctx.print(
            x + 1.5,
            y + 39.0,
            Line::from(Span::styled(
                zone_label(zone),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )),
        );
        let page_count = events.len().div_ceil(GRAPH_PAGE_SIZE).max(1);
        let page_start = if app.playing && page_count > 1 {
            ((app.animation_frame as usize / 30) % page_count) * GRAPH_PAGE_SIZE
        } else {
            (app.graph_page % page_count) * GRAPH_PAGE_SIZE
        };
        for (index, event) in events.iter().skip(page_start).take(GRAPH_PAGE_SIZE).enumerate() {
            let column = index % 2;
            let row = index / 2;
            let node_x = x + 2.0 + column as f64 * 22.0;
            let node_y = y + 28.0 - row as f64 * 13.0;
            positions.insert(event.id.as_str(), (node_x + 9.0, node_y + 4.0));
            let selected = app
                .visible_events
                .get(app.selected)
                .is_some_and(|selected| selected.id == event.id);
            let focused = focused_id == Some(event.id.as_str());
            let dropped = app
                .states
                .get(&event.id)
                .is_some_and(|state| state.dropped_at.is_some());
            let node_color = if selected {
                Color::White
            } else if focused {
                Color::Cyan
            } else if dropped {
                Color::DarkGray
            } else {
                color
            };
            ctx.draw(&Rectangle {
                x: node_x,
                y: node_y,
                width: 18.0,
                height: 9.0,
                color: node_color,
            });
            if focused {
                ctx.draw(&Rectangle {
                    x: node_x - 1.0,
                    y: node_y - 1.0,
                    width: 20.0,
                    height: 11.0,
                    color: Color::Cyan,
                });
                ctx.draw(&Rectangle {
                    x: node_x,
                    y: node_y,
                    width: 18.0,
                    height: 9.0,
                    color: node_color,
                });
            }
            let state = &app.states[&event.id];
            let update_mark = if state.updates > 0 { " ⇄" } else { "" };
            ctx.print(
                node_x + 1.0,
                node_y + 7.0,
                Line::from(Span::styled(
                    truncate(&event.name, 15),
                    Style::default().fg(node_color).add_modifier(Modifier::BOLD),
                )),
            );
            ctx.print(
                node_x + 1.0,
                node_y + 5.0,
                Line::from(Span::styled(
                    truncate(&event.type_name, 15),
                    Style::default().fg(Color::Yellow),
                )),
            );
            ctx.print(
                node_x + 1.0,
                node_y + 3.0,
                Line::from(Span::styled(
                    truncate(&format!("{}B {}", event.bytes, update_mark), 15),
                    Style::default().fg(Color::Gray),
                )),
            );
            ctx.print(
                node_x + 1.0,
                node_y + 1.0,
                Line::from(Span::styled(
                    truncate(&event.value, 15),
                    Style::default().fg(Color::White),
                )),
            );
        }
        if events.len() > GRAPH_PAGE_SIZE {
            ctx.print(
                x + 2.0,
                y + 1.5,
                Line::from(Span::styled(
                    format!("page {}/{} · ←/→", (app.graph_page % page_count) + 1, page_count),
                    Style::default().fg(Color::DarkGray),
                )),
            );
        }
    }

    for event in &app.visible_events {
        let Some(&(from_x, from_y)) = positions.get(event.id.as_str()) else {
            continue;
        };
        for target in &event.points_to {
            if let Some(&(to_x, to_y)) = positions.get(target.as_str()) {
                draw_arrow(
                    ctx,
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                    Color::Magenta,
                    false,
                    app.animation_frame,
                );
            }
        }
        for target in &event.borrows {
            if let Some(&(to_x, to_y)) = positions.get(target.as_str()) {
                draw_arrow(
                    ctx,
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                    Color::LightBlue,
                    true,
                    app.animation_frame,
                );
            }
        }
    }
    ctx.print(
        3.0,
        3.0,
        Line::from(Span::styled(
            format!(
                "{} live nodes  ·  t={}ms  ·  {}",
                app.visible_events.len(),
                app.cursor_ms,
                if app.playing {
                    "▶ animating"
                } else {
                    "⏸ paused"
                }
            ),
            Style::default().fg(Color::Cyan),
        )),
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_arrow(
    ctx: &mut Context,
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    color: Color,
    dotted: bool,
    frame: u16,
) {
    let dx = to_x - from_x;
    let dy = to_y - from_y;
    let distance = (dx * dx + dy * dy).sqrt().max(1.0);
    let ux = dx / distance;
    let uy = dy / distance;
    let start = 9.0;
    let end = (distance - 9.0).max(start + 1.0);
    if dotted {
        let offset = f64::from(frame % 6);
        let mut step = offset;
        while step < end - start {
            let x1 = from_x + ux * (start + step);
            let y1 = from_y + uy * (start + step);
            let x2 = from_x + ux * (start + (step + 1.5).min(end - start));
            let y2 = from_y + uy * (start + (step + 1.5).min(end - start));
            ctx.draw(&CanvasLine {
                x1,
                y1,
                x2,
                y2,
                color,
            });
            step += 4.0;
        }
    } else {
        ctx.draw(&CanvasLine {
            x1: from_x + ux * start,
            y1: from_y + uy * start,
            x2: from_x + ux * end,
            y2: from_y + uy * end,
            color,
        });
    }
    let tip_x = from_x + ux * end;
    let tip_y = from_y + uy * end;
    let px = -uy;
    let py = ux;
    ctx.draw(&CanvasLine {
        x1: tip_x,
        y1: tip_y,
        x2: tip_x - ux * 2.5 + px * 1.5,
        y2: tip_y - uy * 2.5 + py * 1.5,
        color,
    });
    ctx.draw(&CanvasLine {
        x1: tip_x,
        y1: tip_y,
        x2: tip_x - ux * 2.5 - px * 1.5,
        y2: tip_y - uy * 2.5 - py * 1.5,
        color,
    });
}

fn matches_filter(state: &VariableState, filter: &str) -> bool {
    let event = &state.current;
    let event_kind = match event.kind {
        EventKind::Declare => "declare",
        EventKind::Update => "update",
        EventKind::Drop => "drop",
    };
    let status = if state.dropped_at.is_some() { "dropped" } else { "alive" };
    [
        event.name.as_str(),
        event.type_name.as_str(),
        event.value.as_str(),
        event.storage.as_str(),
        event.zone.as_str(),
        event.thread.as_str(),
        event.location.as_str(),
        event_kind,
        status,
    ]
    .into_iter()
    .any(|field| field.to_lowercase().contains(filter))
}

fn zone_panel(app: &App, zone: &str) -> Paragraph<'static> {
    let events = app
        .visible_events
        .iter()
        .filter(|event| event_zone(event) == zone)
        .collect::<Vec<_>>();
    let color = zone_color(zone);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            zone.to_uppercase(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  {} node{}",
                events.len(),
                if events.len() == 1 { "" } else { "s" }
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ])];
    if events.is_empty() {
        lines.push(Line::from(Span::styled(
            "  empty",
            Style::default().fg(Color::DarkGray),
        )));
    }
    for (index, event) in events.iter().enumerate() {
        if index > 5 {
            lines.push(Line::from(Span::styled(
                "  + more nodes…",
                Style::default().fg(Color::DarkGray),
            )));
            break;
        }
        let selected = app
            .visible_events
            .get(app.selected)
            .is_some_and(|selected| selected.id == event.id);
        let dropped = app
            .states
            .get(&event.id)
            .is_some_and(|state| state.dropped_at.is_some());
        let node_style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(color)
                .add_modifier(Modifier::BOLD)
        } else if dropped {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White)
        };
        let pointer = if event.points_to.is_empty() {
            ""
        } else {
            " ↗"
        };
        let borrow = if event.borrows.is_empty() { "" } else { " &" };
        lines.push(Line::from(Span::styled(
            format!(
                "┌{}┐",
                truncate(&format!(" {}{}{} ", event.name, pointer, borrow), 17)
            ),
            node_style,
        )));
        lines.push(Line::from(Span::styled(
            format!("│{}│", truncate(&format!(" {} ", event.value), 17)),
            node_style,
        )));
        lines.push(Line::from(Span::styled(
            format!(
                "│{}│",
                truncate(
                    &format!(
                        " @{} ",
                        if event.address.is_empty() {
                            "?"
                        } else {
                            &event.address
                        }
                    ),
                    17
                )
            ),
            node_style,
        )));
        lines.push(Line::from(Span::styled(
            format!("└{}┘", truncate(&format!(" {} ", event.type_name), 17)),
            node_style,
        )));
    }
    Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(format!(" {} ", zone_label(zone)))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(color)),
        )
}

fn lifetime_panel(app: &App) -> Paragraph<'static> {
    let width = 47usize;
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "LIFETIME LANES  ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "● declared   ━ alive   × dropped",
            Style::default().fg(Color::DarkGray),
        ),
    ])];
    for event in app.visible_events.iter().take(5) {
        let state = &app.states[&event.id];
        let start = scale_time(state.declared_at, app.max_ms, width.saturating_sub(1));
        let end_time = state.dropped_at.unwrap_or(app.cursor_ms).min(app.max_ms);
        let end = scale_time(end_time, app.max_ms, width)
            .max(start + 1)
            .min(width);
        let mut bar = vec![' '; width];
        for slot in &mut bar[start..end] {
            *slot = '━';
        }
        bar[start] = '●';
        if state.dropped_at.is_some() && end > 0 {
            bar[end - 1] = '×';
        }
        let color = if event_zone(event) == "stack" {
            Color::Green
        } else {
            zone_color(event_zone(event))
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<11}", truncate(&event.name, 10)),
                Style::default().fg(Color::Gray),
            ),
            Span::styled(
                bar.into_iter().collect::<String>(),
                Style::default().fg(color),
            ),
        ]));
    }
    if app.visible_events.is_empty() {
        lines.push(Line::from(Span::styled(
            "waiting for runtime events…",
            Style::default().fg(Color::DarkGray),
        )));
    }
    Paragraph::new(Text::from(lines)).block(panel_block(" lifetime history "))
}

fn inspector_panel(app: &App) -> Paragraph<'static> {
    let Some(state) = app.selected_state() else {
        return Paragraph::new("No variable selected").block(panel_block(" selected node "));
    };
    let event = &state.current;
    let lifetime = state
        .dropped_at
        .map(|drop| format!("{}ms", drop.saturating_sub(state.declared_at)))
        .unwrap_or_else(|| format!("{}ms+", app.cursor_ms.saturating_sub(state.declared_at)));
    let target_text = if event.points_to.is_empty() {
        "none".into()
    } else {
        event.points_to.join(", ")
    };
    let borrow_text = if event.borrows.is_empty() {
        "none".into()
    } else {
        event.borrows.join(", ")
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(
                event.name.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(event.type_name.clone(), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(format!("value     {}", event.value)),
        Line::from(format!(
            "zone      {}  ·  {}  ·  {} bytes",
            zone_label(event_zone(event)),
            event.storage,
            event.bytes
        )),
        Line::from(format!(
            "address   {}",
            if event.address.is_empty() {
                "not reported"
            } else {
                &event.address
            }
        )),
        Line::from(format!(
            "points to {}  ──▶  {}",
            if event.points_to.is_empty() {
                "none"
            } else {
                "pointer"
            },
            target_text
        )),
        Line::from(format!(
            "borrows   {}  ──▶  {}",
            if event.borrows.is_empty() {
                "none"
            } else {
                "reference"
            },
            borrow_text
        )),
        Line::from(format!(
            "lifetime  {} → {} ({})",
            state.declared_at,
            state
                .dropped_at
                .map_or_else(|| "live".into(), |time| time.to_string()),
            lifetime
        )),
        Line::from(format!(
            "source    {}  ·  thread {}  ·  {} update{}",
            event.location,
            if event.thread.is_empty() {
                "main"
            } else {
                &event.thread
            },
            state.updates,
            if state.updates == 1 { "" } else { "s" }
        )),
    ];
    Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: true })
        .block(panel_block(" selected node "))
}

fn relationship_panel(app: &App) -> Paragraph<'static> {
    let mut lines = vec![Line::from(Span::styled(
        "POINTERS & BORROWS",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))];
    let mut count = 0;
    for event in &app.visible_events {
        for target in &event.points_to {
            lines.push(Line::from(vec![
                Span::styled(
                    truncate(&event.name, 12),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled("  ── owns ──▶  ", Style::default().fg(Color::Magenta)),
                Span::styled(truncate(target, 15), Style::default().fg(Color::White)),
            ]));
            count += 1;
        }
        for target in &event.borrows {
            lines.push(Line::from(vec![
                Span::styled(
                    truncate(&event.name, 12),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled("  ── borrows ─▶  ", Style::default().fg(Color::LightBlue)),
                Span::styled(truncate(target, 15), Style::default().fg(Color::White)),
            ]));
            count += 1;
        }
    }
    if count == 0 {
        lines.push(Line::from(Span::styled(
            "no edges reported",
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(Span::styled(
            "add points_to / borrows to JSONL",
            Style::default().fg(Color::DarkGray),
        )));
    }
    Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: true })
        .block(panel_block(" relationship graph "))
}

fn draw_controls(
    frame: &mut Frame,
    app: &App,
    controls_area: ratatui::layout::Rect,
    status_area: ratatui::layout::Rect,
) {
    // Gauge::percent expects an integer percentage. Integer division before
    // multiplying made every partial position render as 0%.
    let progress = if app.max_ms == 0 {
        0
    } else {
        ((app.cursor_ms.min(app.max_ms) as f64 / app.max_ms as f64) * 100.0).round() as u16
    };
    let controls = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(29),
            Constraint::Min(20),
            Constraint::Length(29),
        ])
        .split(controls_area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " SPACE ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(if app.playing { " pause" } else { " play" }),
            Span::styled(
                "  r",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" live  s save"),
        ]))
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        controls[0],
    );
    let focused_description = app
        .visible_events
        .get(app.graph_focus)
        .map(|event| format!("{} · {} · {} bytes", event.name, event.type_name, event.bytes))
        .unwrap_or_else(|| "No node focused".into());
    let timeline = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Length(3)])
        .split(controls[1]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("  Focused: {focused_description}"),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )))
        .alignment(ratatui::layout::Alignment::Center),
        timeline[0],
    );
    frame.render_widget(
        Gauge::default()
            .block(
                Block::default()
                    .title(" timeline ")
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .gauge_style(Style::default().fg(Color::Cyan))
            .label(format!("{}ms / {}ms", app.cursor_ms, app.max_ms))
            .percent(progress.min(100)),
        timeline[1],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ↑↓", Style::default().fg(Color::Yellow)),
            Span::raw(" node   "),
            Span::styled("←→", Style::default().fg(Color::Yellow)),
            Span::raw(" scrub [ ]   "),
            Span::styled("q", Style::default().fg(Color::Yellow)),
            Span::raw(" quit"),
        ]))
        .alignment(ratatui::layout::Alignment::Right)
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        controls[2],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                if app.filter_active {                "FILTER" } else { "filter" },
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" [{}]  ·  auto-scroll: {}  ·  {}", app.filter, if app.auto_scroll { "on" } else { "off" }, app.status)),
        ]))
        .style(Style::default().fg(Color::Gray)),
        status_area,
    );
}

fn hint_line<'a>(marker: &'a str, label: &'a str, color: Color) -> Line<'a> {
    Line::from(vec![
        Span::styled("●", Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(label, Style::default().fg(Color::White)),
        Span::raw("  "),
        Span::styled(marker, Style::default().fg(color).add_modifier(Modifier::BOLD)),
    ])
}

fn draw_graph_hints(frame: &mut Frame, area: ratatui::layout::Rect) {
    let hints_width = 34.min(area.width.saturating_sub(2));
    let hints_height = 8.min(area.height);
    if hints_width < 20 || hints_height < 3 {
        return;
    }
    let hints_area = ratatui::layout::Rect {
        x: area.x + area.width.saturating_sub(hints_width),
        y: area.y + area.height.saturating_sub(hints_height),
        width: hints_width,
        height: hints_height,
    };
    let hints = Paragraph::new(Text::from(vec![
        hint_line("green", "Stack / frames", Color::Green),
        hint_line("magenta", "Heap / owned", Color::Magenta),
        hint_line("yellow", "Data / static", Color::Yellow),
        hint_line("blue", "Sync / shared", Color::LightBlue),
        hint_line("──", "Owns / update", Color::Magenta),
        hint_line("···", "Borrows / reference", Color::LightBlue),
    ]))
    .block(
        Block::default()
            .title(" legend ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(hints, hints_area);
}


fn event_zone(event: &VariableEvent) -> &str {
    if !event.zone.is_empty() {
        return event.zone.as_str();
    }
    match event.storage.as_str() {
        "heap" | "box" | "vec" | "string" => "heap",
        "data" | "static" | "const" | "rodata" => "data",
        "arc" | "rc" | "mutex" | "rwlock" | "refcell" | "cell" | "atomic" => "sync",
        _ => "stack",
    }
}

fn zone_label(zone: &str) -> &str {
    match zone {
        "stack" => "STACK / FRAMES",
        "heap" => "HEAP / OWNED",
        "data" => "DATA / STATIC",
        "sync" => "SYNC / SHARED",
        _ => "OTHER",
    }
}

fn zone_color(zone: &str) -> Color {
    match zone {
        "stack" => Color::Green,
        "heap" => Color::Magenta,
        "data" => Color::Yellow,
        "sync" => Color::LightBlue,
        _ => Color::Gray,
    }
}

fn scale_time(time: u64, max: u64, width: usize) -> usize {
    if max == 0 {
        0
    } else {
        (time as usize * width / max as usize).min(width.saturating_sub(1))
    }
}

fn truncate(value: &str, width: usize) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= width {
        return value.to_string();
    }
    if width <= 1 {
        return "…".into();
    }
    format!("{}…", chars[..width - 1].iter().collect::<String>())
}

fn panel_block(title: &'static str) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
}

fn header_block() -> Block<'static> {
    Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(Color::DarkGray))
}

fn move_graph_focus(app: &mut App, delta: isize) {
    if app.visible_events.is_empty() {
        app.graph_focus = 0;
        return;
    }
    let len = app.visible_events.len() as isize;
    app.graph_focus = (app.graph_focus as isize + delta).rem_euclid(len) as usize;
    app.selected = app.graph_focus;
    if let Some(event) = app.visible_events.get(app.graph_focus) {
        app.status = format!("Focused node: {} ({})", event.name, event.type_name);
    }
}

fn handle_mouse(app: &mut App, mouse: MouseEvent, area: ratatui::layout::Rect) {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let x = mouse.column;
            let y = mouse.row;
            if y < 3 {
                let tab_start = area.width.saturating_sub(48);
                if x >= tab_start {
                    app.active_tab = ((x - tab_start) / 12).min(3) as usize;
                }
            } else if app.active_tab == 0 && y >= area.height.saturating_sub(7) {
                app.toggle_play();
            }
        }
        MouseEventKind::ScrollUp => move_graph_focus(app, -1),
        MouseEventKind::ScrollDown => move_graph_focus(app, 1),
        _ => {}
    }
}

fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    if key.kind != KeyEventKind::Press {
        return false;
    }
    if app.filter_active {
        match key.code {
            KeyCode::Esc => {
                app.clear_filter();
                return false;
            }
            KeyCode::Enter => {
                app.filter_active = false;
                app.rebuild_visible();
                app.status = format!("Filter applied: {}", app.filter);
                return false;
            }
            KeyCode::Backspace => {
                app.filter.pop();
                app.rebuild_visible();
                return false;
            }
            KeyCode::Char(ch) => {
                app.filter.push(ch);
                app.rebuild_visible();
                return false;
            }
            _ => return false,
        }
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => true,
        KeyCode::Down | KeyCode::Char('j') => {
            move_graph_focus(app, 1);
            false
        }
        KeyCode::Up | KeyCode::Char('k') => {
            move_graph_focus(app, -1);
            false
        }
        KeyCode::Char('h') => {
            move_graph_focus(app, -1);
            false
        }
        KeyCode::Char('l') => {
            move_graph_focus(app, 1);
            false
        }
        KeyCode::Char(' ') => {
            app.toggle_play();
            false
        }
        KeyCode::Char('r') => {
            app.reset_live();
            false
        }
        KeyCode::Char('s') => {
            app.export_record();
            false
        }
        KeyCode::Char('/') => {
            app.toggle_filter();
            false
        }
        KeyCode::Char('c') => {
            app.clear_filter();
            false
        }
        KeyCode::Char('a') => {
            app.auto_scroll = !app.auto_scroll;
            app.status = format!(
                "Auto-scroll {}",
                if app.auto_scroll { "enabled" } else { "disabled" }
            );
            false
        }
        KeyCode::Tab => {
            app.active_tab = (app.active_tab + 1) % 4;
            false
        }
        KeyCode::Char('[') => {
            app.step_to_previous_event();
            false
        }
        KeyCode::Char(']') => {
            app.step_to_next_event();
            false
        }
        KeyCode::Enter => {
            app.selected = app.graph_focus.min(app.visible_events.len().saturating_sub(1));
            false
        }
        KeyCode::Left => {
            if app.graph_page > 0 && !app.playing {
                app.graph_page -= 1;
                app.graph_focus = (app.graph_page * GRAPH_PAGE_SIZE).min(app.visible_events.len().saturating_sub(1));
                app.selected = app.graph_focus;
            } else {
                app.step_to_previous_event();
            }
            false
        }
        KeyCode::Right => {
            let page_count = app.visible_events.len().div_ceil(GRAPH_PAGE_SIZE).max(1);
            if page_count > 1 {
                app.graph_page = (app.graph_page + 1).min(page_count - 1);
                app.graph_focus = (app.graph_page * GRAPH_PAGE_SIZE).min(app.visible_events.len().saturating_sub(1));
                app.selected = app.graph_focus;
            } else {
                app.step_to_next_event();
            }
            false
        }
        _ => false,
    }
}

fn run(mut terminal: DefaultTerminal, mut app: App) -> io::Result<()> {
    loop {
        app.tick();
        terminal.draw(|frame| ui(frame, &app))?;
        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                Event::Key(key) if handle_key(&mut app, key) => return Ok(()),
                Event::Mouse(mouse) => handle_mouse(&mut app, mouse, terminal.get_frame().area()),
                _ => {}
            }
        }
    }
}

fn load_events(path: &Path) -> Vec<VariableEvent> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line.trim()).ok())
        .collect()
}

#[derive(Serialize)]
struct ExportReport {
    event_count: usize,
    node_count: usize,
    cursor_ms: u64,
    max_ms: u64,
    nodes: Vec<VariableEvent>,
}

fn export_report(events: Vec<VariableEvent>, output: &Path, format: &str) -> io::Result<()> {
    let mut states = HashMap::new();
    for event in &events {
        match event.kind {
            EventKind::Declare => {
                states.insert(event.id.clone(), event.clone());
            }
            EventKind::Update => {
                if states.contains_key(&event.id) {
                    states.insert(event.id.clone(), event.clone());
                }
            }
            EventKind::Drop => {
                if let Some(current) = states.get_mut(&event.id) {
                    current.value = "<dropped>".into();
                }
            }
        }
    }
    let max_ms = events.iter().map(|event| event.time_ms).max().unwrap_or(0);
    let nodes = states.into_values().collect::<Vec<_>>();
    let report = ExportReport {
        event_count: events.len(),
        node_count: nodes.len(),
        cursor_ms: max_ms,
        max_ms,
        nodes: nodes.clone(),
    };
    let content = if format.eq_ignore_ascii_case("text") {
        let mut text = format!(
            "Baxan report\\nEvents: {}\\nNodes: {}\\nTimeline: {}ms\\n\\n",
            report.event_count, report.node_count, report.max_ms
        );
        for node in nodes {
            text.push_str(&format!(
                "{}: {} = {} ({} bytes, {})\\n",
                node.name,
                node.type_name,
                node.value,
                node.bytes,
                event_zone(&node)
            ));
        }
        text
    } else if format.eq_ignore_ascii_case("json") {
        serde_json::to_string_pretty(&report).map_err(io::Error::other)?
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "export format must be 'json' or 'text'",
        ));
    };
    fs::write(output, content)
}

// ------------------------------------------------------------------
// Automatic tracking (--run)
// ------------------------------------------------------------------

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn tracker_filename() -> &'static str {
    if cfg!(target_os = "linux") {
        "libbaxan_tracker.so"
    } else {
        "libbaxan_tracker.dylib"
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn extract_tracker() -> io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    const BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libbaxan_tracker.dylib"));
    #[cfg(target_os = "linux")]
    const BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libbaxan_tracker.so"));
    let path = std::env::temp_dir().join(tracker_filename());
    std::fs::write(&path, BYTES)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(path)
}

fn find_binary_name(project: &Path) -> String {
    // Try cargo metadata to find the first binary target
    if let Ok(output) = Command::new("cargo")
        .args(["metadata", "--format-version=1", "--no-deps"])
        .current_dir(project)
        .output()
    {
        if output.status.success() {
            if let Ok(meta) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                if let Some(pkgs) = meta["packages"].as_array() {
                    for pkg in pkgs {
                        if let Some(targets) = pkg["targets"].as_array() {
                            for t in targets {
                                if t["kind"].as_array().is_some_and(|k| k.iter().any(|v| v == "bin")) {
                                    if let Some(name) = t["name"].as_str() {
                                        return name.to_string();
                                    }
                                }
                            }
                        }
                    }
                    // Fall back to package name
                    if let Some(name) = pkgs[0]["name"].as_str() {
                        return name.to_string();
                    }
                }
            }
        }
    }
    // Fall back to directory name
    project
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("main")
        .to_string()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn run_project(_project: &PathBuf, _extra_args: &[String]) -> io::Result<()> {
    eprintln!("Automatic tracking (--run) is not supported on this platform.");
    eprintln!("Use --events with a manual JSONL emitter instead.");
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_project(project: &PathBuf, extra_args: &[String]) -> io::Result<()> {
    let project = fs::canonicalize(project).unwrap_or_else(|_| project.clone());

    // 1. Build the project (no instrumentation or special flags needed:
    //    the tracker rebinds malloc/free pointers at runtime).
    eprintln!("\u{1f4e6} Building project...");
    let status = match Command::new("cargo")
        .arg("build")
        .arg("--release")
        .current_dir(&project)
        .status()
    {
        Ok(s) => s,
        Err(e) => { eprintln!("\u{274c} Failed to run cargo build: {e}"); std::process::exit(1); }
    };
    if !status.success() {
        eprintln!("\u{274c} Build failed");
        std::process::exit(1);
    }
    eprintln!("\u{2705} Build complete");

    // 2. Find the binary
    let bin_name = find_binary_name(&project);
    let bin_path = project.join("target/release").join(&bin_name);
    if !bin_path.exists() {
        eprintln!("\u{274c} Binary not found: {}", bin_path.display());
        std::process::exit(1);
    }

    // 3. Extract the tracker shared library
    let tracker_path = match extract_tracker() {
        Ok(p) => p,
        Err(e) => { eprintln!("\u{274c} Failed to extract tracker: {e}"); std::process::exit(1); }
    };
    eprintln!("\u{1f50d} Tracker loaded: {}", tracker_path.display());

    // 4. Events file
    let events_path = std::env::temp_dir().join(format!("baxan_{}.jsonl", std::process::id()));

    // 5. Run the project with the tracker injected
    eprintln!("\u{1f680} Running project with memory tracking...");
    let mut cmd = Command::new(&bin_path);
    cmd.current_dir(&project);
    cmd.args(extra_args);
    cmd.env("LD_PRELOAD", &tracker_path);
    cmd.env("DYLD_INSERT_LIBRARIES", &tracker_path);
    cmd.env("BAXAN_TRACKER_OUTPUT", &events_path);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => { eprintln!("\u{274c} Failed to run project: {e}"); std::process::exit(1); }
    };
    let exit_status = match child.wait() {
        Ok(s) => s,
        Err(e) => { eprintln!("\u{274c} Process error: {e}"); std::process::exit(1); }
    };
    eprintln!("\u{2705} Process exited: {exit_status}");

    // 6. Load captured events
    let events = load_events(&events_path);
    eprintln!("\u{1f4ca} Captured {} heap allocation events", events.len());

    // The captured events are passed directly into the TUI below. Remove only
    // the temporary tracker library; the event file is no longer needed after
    // loading it into memory.
    let _ = std::fs::remove_file(&tracker_path);

    if events.is_empty() {
        eprintln!("\u{26a0}\u{fe0f}  No heap allocations captured. The program may use a custom allocator.");
        return Ok(());
    }

    // 8. Open the terminal visualization.
    let project = project.clone();
    enable_raw_mode().ok();
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).ok();
    let terminal = ratatui::init();
    let result = run(terminal, App::with_events(project, events, None));
    let _ = std::fs::remove_file(&events_path);
    ratatui::restore();
    disable_raw_mode().ok();
    execute!(io::stdout(), LeaveAlternateScreen).ok();
    result
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    if args.run {
        return run_project(&args.project, &args.args);
    }

    let events = if args.demo {
        demo_events()
    } else if let Some(path) = args.events.as_deref() {
        load_events(path)
    } else {
        demo_events()
    };
    if let Some(output) = args.export.as_deref() {
        return export_report(events, output, &args.export_format);
    }

    let project = fs::canonicalize(&args.project).unwrap_or(args.project);
    enable_raw_mode().ok();
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).ok();        let terminal = ratatui::init();
    let result = run(terminal, App::new(project, args.events, args.demo));
    ratatui::restore();
    disable_raw_mode().ok();
    execute!(io::stdout(), LeaveAlternateScreen).ok();
    if let Err(error) = &result {
        eprintln!("TUI error: {error}");
    }
    result
}

/// Returns a built-in deterministic event stream for demonstration purposes.
///
/// The demo includes stack variables, heap allocations, `Arc` shared state,
/// borrows, mutations, and drops across multiple threads.
pub fn demo_events() -> Vec<VariableEvent> {
    vec![
        VariableEvent {
            seq: 1,
            time_ms: 0,
            kind: EventKind::Declare,
            id: "config".into(),
            name: "config".into(),
            type_name: "Config".into(),
            value: "port: 8080".into(),
            location: "src/main.rs:12".into(),
            storage: "stack".into(),
            zone: "stack".into(),
            address: "0x7ffd_10a0".into(),
            points_to: vec![],
            borrows: vec![],
            bytes: 24,
            thread: "main".into(),
        },
        VariableEvent {
            seq: 2,
            time_ms: 90,
            kind: EventKind::Declare,
            id: "shared".into(),
            name: "shared".into(),
            type_name: "Arc<State>".into(),
            value: "strong=1".into(),
            location: "src/state.rs:8".into(),
            storage: "arc".into(),
            zone: "sync".into(),
            address: "0x7ffd_10c8".into(),
            points_to: vec!["state".into()],
            borrows: vec![],
            bytes: 8,
            thread: "main".into(),
        },
        VariableEvent {
            seq: 3,
            time_ms: 120,
            kind: EventKind::Declare,
            id: "state".into(),
            name: "state".into(),
            type_name: "State".into(),
            value: "ready: true".into(),
            location: "src/state.rs:4".into(),
            storage: "heap".into(),
            zone: "heap".into(),
            address: "0x104a_2200".into(),
            points_to: vec![],
            borrows: vec![],
            bytes: 64,
            thread: "main".into(),
        },
        VariableEvent {
            seq: 4,
            time_ms: 250,
            kind: EventKind::Declare,
            id: "buffer".into(),
            name: "buffer".into(),
            type_name: "Vec<u8>".into(),
            value: "len=4096 cap=8192".into(),
            location: "src/worker.rs:31".into(),
            storage: "heap".into(),
            zone: "heap".into(),
            address: "0x104a_3000".into(),
            points_to: vec![],
            borrows: vec!["view".into()],
            bytes: 8192,
            thread: "worker-1".into(),
        },
        VariableEvent {
            seq: 5,
            time_ms: 330,
            kind: EventKind::Declare,
            id: "view".into(),
            name: "view".into(),
            type_name: "&[u8]".into(),
            value: "len=64".into(),
            location: "src/worker.rs:32".into(),
            storage: "borrow".into(),
            zone: "stack".into(),
            address: "0x7ffe_9000".into(),
            points_to: vec![],
            borrows: vec!["buffer".into()],
            bytes: 16,
            thread: "worker-1".into(),
        },
        VariableEvent {
            seq: 6,
            time_ms: 430,
            kind: EventKind::Update,
            id: "shared".into(),
            name: "shared".into(),
            type_name: "Arc<State>".into(),
            value: "strong=2".into(),
            location: "src/state.rs:8".into(),
            storage: "arc".into(),
            zone: "sync".into(),
            address: "0x7ffd_10c8".into(),
            points_to: vec!["state".into()],
            borrows: vec![],
            bytes: 8,
            thread: "worker-1".into(),
        },
        VariableEvent {
            seq: 7,
            time_ms: 620,
            kind: EventKind::Drop,
            id: "view".into(),
            name: "view".into(),
            type_name: "&[u8]".into(),
            value: "<dropped>".into(),
            location: "src/worker.rs:32".into(),
            storage: "borrow".into(),
            zone: "stack".into(),
            address: "0x7ffe_9000".into(),
            points_to: vec![],
            borrows: vec!["buffer".into()],
            bytes: 16,
            thread: "worker-1".into(),
        },
        VariableEvent {
            seq: 8,
            time_ms: 760,
            kind: EventKind::Drop,
            id: "buffer".into(),
            name: "buffer".into(),
            type_name: "Vec<u8>".into(),
            value: "<dropped>".into(),
            location: "src/worker.rs:31".into(),
            storage: "heap".into(),
            zone: "heap".into(),
            address: "0x104a_3000".into(),
            points_to: vec![],
            borrows: vec![],
            bytes: 8192,
            thread: "worker-1".into(),
        },
        VariableEvent {
            seq: 9,
            time_ms: 980,
            kind: EventKind::Drop,
            id: "config".into(),
            name: "config".into(),
            type_name: "Config".into(),
            value: "<dropped>".into(),
            location: "src/main.rs:12".into(),
            storage: "stack".into(),
            zone: "stack".into(),
            address: "0x7ffd_10a0".into(),
            points_to: vec![],
            borrows: vec![],
            bytes: 24,
            thread: "main".into(),
        },
    ]
}
