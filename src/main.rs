mod gui;

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
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
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
    /// Use the terminal (TUI) interface instead of the egui GUI.
    #[arg(long)]
    tui: bool,
    /// Build and run the project with automatic heap-allocation tracking,
    /// then open the visualization.  No code changes required.
    #[arg(long)]
    run: bool,
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
}

impl App {
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
        };
        app.rebuild();
        app
    }

    fn tick(&mut self) {
        self.animation_frame = self.animation_frame.wrapping_add(1);
        if self.events_path.is_some() && !self.playing {
            self.read_new_events();
        }
        if self.playing && self.last_tick.elapsed() >= Duration::from_millis(120) {
            self.last_tick = Instant::now();
            self.cursor_ms = self.cursor_ms.saturating_add(120).min(self.max_ms);
            if self.cursor_ms >= self.max_ms {
                self.playing = false;
            }
            self.rebuild_visible();
        }
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
                self.source_events.push(event);
                added += 1;
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
        self.visible_events = self
            .states
            .values()
            .filter(|state| state.is_alive_at(self.cursor_ms) || state.dropped_at.is_some())
            .map(|state| state.current.clone())
            .collect();
        self.visible_events.sort_by_key(|event| event.seq);
        if self.selected >= self.visible_events.len() {
            self.selected = self.visible_events.len().saturating_sub(1);
        }
    }

    fn selected_state(&self) -> Option<&VariableState> {
        let event = self.visible_events.get(self.selected)?;
        self.states.get(&event.id)
    }

    fn toggle_play(&mut self) {
        if self.cursor_ms >= self.max_ms {
            self.cursor_ms = 0;
            self.rebuild();
        }
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
        self.follow_live = true;
        self.rebuild();
        self.status = "Live · following newest event".into();
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

fn ui(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(17),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .split(area);
    draw_header(frame, app, vertical[0]);

    if app.active_tab == 0 {
        draw_visualize_panel(frame, app, vertical[1]);
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

fn draw_visualize_panel(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let canvas = Canvas::default()
        .block(
            Block::default()
                .title(" live memory graph · dotted = borrow/reference · solid = ownership/update ")
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
        for (index, event) in events.iter().take(8).enumerate() {
            let column = index % 2;
            let row = index / 2;
            let node_x = x + 2.0 + column as f64 * 22.0;
            let node_y = y + 28.0 - row as f64 * 13.0;
            positions.insert(event.id.as_str(), (node_x + 9.0, node_y + 4.0));
            let selected = app
                .visible_events
                .get(app.selected)
                .is_some_and(|selected| selected.id == event.id);
            let dropped = app
                .states
                .get(&event.id)
                .is_some_and(|state| state.dropped_at.is_some());
            let node_color = if selected {
                Color::White
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
        if events.len() > 8 {
            ctx.print(
                x + 2.0,
                y + 1.5,
                Line::from(Span::styled(
                    format!("+{} more nodes", events.len() - 8),
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
    let progress = app.cursor_ms.checked_div(app.max_ms)
        .map(|ratio| (ratio * 100) as u16)
        .unwrap_or(0);
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
            .percent(progress),
        controls[1],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ↑↓", Style::default().fg(Color::Yellow)),
            Span::raw(" node   "),
            Span::styled("←→", Style::default().fg(Color::Yellow)),
            Span::raw(" scrub   "),
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
        Paragraph::new(format!("  {}", app.status)).style(Style::default().fg(Color::Gray)),
        status_area,
    );
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

fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    if key.kind != KeyEventKind::Press {
        return false;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => true,
        KeyCode::Down | KeyCode::Char('j') => {
            app.selected = (app.selected + 1).min(app.visible_events.len().saturating_sub(1));
            false
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.selected = app.selected.saturating_sub(1);
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
        KeyCode::Tab => {
            app.active_tab = (app.active_tab + 1) % 4;
            false
        }
        KeyCode::Left => {
            app.follow_live = false;
            app.playing = false;
            app.cursor_ms = app.cursor_ms.saturating_sub(200);
            app.rebuild();
            false
        }
        KeyCode::Right => {
            app.follow_live = false;
            app.playing = false;
            app.cursor_ms = (app.cursor_ms + 200).min(app.max_ms);
            app.rebuild();
            false
        }
        _ => false,
    }
}

fn run(mut terminal: DefaultTerminal, mut app: App) -> io::Result<()> {
    loop {
        app.tick();
        terminal.draw(|frame| ui(frame, &app))?;
        if event::poll(Duration::from_millis(80))?
            && let Event::Key(key) = event::read()?
            && handle_key(&mut app, key)
        {
            return Ok(());
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
fn run_project(_project: &PathBuf, _extra_args: &[String]) -> eframe::Result<()> {
    eprintln!("Automatic tracking (--run) is not supported on this platform.");
    eprintln!("Use --events with a manual JSONL emitter instead.");
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_project(project: &PathBuf, extra_args: &[String]) -> eframe::Result<()> {
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

    // 7. Clean up temp files
    let _ = std::fs::remove_file(&tracker_path);
    let _ = std::fs::remove_file(&events_path);

    if events.is_empty() {
        eprintln!("\u{26a0}\u{fe0f}  No heap allocations captured. The program may use a custom allocator.");
        return Ok(());
    }

    // 8. Open the visualization
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_title("Baxan \u{2014} Memory Visualization"),
        ..Default::default()
    };

    eframe::run_native(
        "Baxan",
        options,
        Box::new(move |cc| Ok(Box::new(gui::GuiApp::new(cc, events)))),
    )
}

fn main() -> eframe::Result<()> {
    let args = Args::parse();

    if args.run {
        return run_project(&args.project, &args.args);
    }

    if args.tui {
        let project = fs::canonicalize(&args.project).unwrap_or(args.project);
        enable_raw_mode().ok();
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).ok();
        let terminal = ratatui::init();
        let result = run(terminal, App::new(project, args.events, args.demo));
        ratatui::restore();
        disable_raw_mode().ok();
        execute!(io::stdout(), LeaveAlternateScreen).ok();
        if let Err(e) = result { eprintln!("TUI error: {e}"); }
        return Ok(());
    }

    // Default: egui GUI
    let events = if args.demo {
        demo_events()
    } else if let Some(path) = args.events {
        load_events(&path)
    } else {
        demo_events()
    };

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_title("Baxan \u{2014} Memory Visualizer"),
        ..Default::default()
    };

    eframe::run_native(
        "Baxan",
        options,
        Box::new(move |cc| Ok(Box::new(gui::GuiApp::new(cc, events)))),
    )
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
