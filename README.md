# Baxan

**Live Rust memory and lifetime visualizer.**

Baxan consumes a newline-delimited JSON stream emitted by an instrumented Rust process and renders a real-time graph of variable lifetimes, values, allocation classes, heap addresses, byte sizes, and drop history. It ships with two front-ends — an **egui** native GUI (default) and a **Ratatui** terminal UI — so you can pick whichever fits your workflow.

## Features

- **Memory graph visualization** — variables are laid out inside *stack*, *heap*, *data/static*, and *sync/shared* zones as typed boxes showing name, type, value, address, and byte size.
- **Ownership and borrow arrows** — solid arrows for ownership/pointer links (`points_to`), dotted animated arrows for borrows/references (`borrows`).
- **Timeline playback** — scrub, play, and pause through recorded events with smooth animation.
- **Live tailing** — point Baxan at a JSONL file that a running process is appending to; new events appear in real time.
- **Inspector panel** — select any node to see its full details: value, zone, storage class, address, lifetime span, source location, thread, and update count.
- **Dual front-ends** — egui GUI for a graphical experience, Ratatui TUI for remote/SSH/headless environments.
- **Demo mode** — built-in deterministic event stream so you can try Baxan without writing any instrumentation.

## Installation

```bash
# Clone and run directly
git clone <repo-url> && cd baxan/baxan
cargo run --release          # egui GUI (default)
cargo run --release -- --tui # Ratatui TUI
```

### From crates.io (once published)

```bash
cargo install baxan
baxan --demo
```

### Requirements

- Rust 1.85+ (edition 2024)
- A graphical environment for the egui GUI (X11, Wayland, macOS, Windows)
- The TUI mode works over SSH or in any terminal with 24-bit color support

## Quick start

```bash
# Launch the GUI with the bundled demo stream
cargo run --release

# Launch the TUI with the demo
cargo run --release -- --tui --demo

# Watch a live event file
cargo run --release -- --events /path/to/events.jsonl

# Observe a specific project directory (for path display)
cargo run --release -- --project /path/to/rust/project --events events.jsonl
```

## Usage

### Command-line options

| Flag | Description |
|------|-------------|
| `-p`, `--project <DIR>` | Rust project directory to observe (default: `.`) |
| `-e`, `--events <FILE>` | JSONL event file to load or tail |
| `--demo` | Start with the bundled deterministic demo stream |
| `--tui` | Use the Ratatui terminal interface instead of the egui GUI |
| `-V`, `--version` | Print version |
| `-h`, `--help` | Print help |

### egui GUI (default)

When you launch Baxan without `--tui`, the native egui window opens with a welcome screen.

| Key | Action |
|-----|--------|
| **V** | Toggle the visualization on/off |
| **Space** | Play / pause the timeline animation |
| **R** | Jump to live mode (follow newest events) |
| **↑** / **↓** | Select previous / next variable node |
| **←** / **→** | Scrub the timeline backward / forward by 200 ms |
| **Click** | Click a node in the graph to select it |

The visualization is divided into four color-coded zones:

| Zone | Color | What it shows |
|------|-------|---------------|
| **STACK / FRAMES** | 🟢 Green | Local variables, function parameters, borrows |
| **HEAP / OWNED** | 🟣 Magenta | `Vec`, `String`, `Box`, and other heap-allocated data |
| **DATA / STATIC** | 🟡 Yellow | `static`, `const`, read-only data |
| **SYNC / SHARED** | 🔵 Blue | `Arc`, `Rc`, `Mutex`, `RwLock`, `RefCell`, `Cell` |

The right-side **Inspector** panel shows full details for the selected variable.

### Ratatui TUI (`--tui`)

| Key | Action |
|-----|--------|
| **Space** | Play / pause the recording |
| **r** | Follow live events |
| **↑** / **↓** or **j** / **k** | Select previous / next node |
| **←** / **→** | Scrub the timeline |
| **Tab** | Switch between views (VISUALIZE, MEMORY MAP, RECORD, RELATIONSHIPS) |
| **s** | Save the current recording to `.baxan-recording.jsonl` |
| **q** / **Esc** | Quit |

The TUI has four tabs:

1. **VISUALIZE** — full-screen memory graph with zone rectangles, variable boxes, and animated arrows.
2. **MEMORY MAP** — four zone panels with ASCII-art variable boxes, a lifetime lane timeline, an inspector, and a relationship list.
3. **RECORD** — (reserved for future recording UI).
4. **RELATIONSHIPS** — flat list of all ownership and borrow edges.

## JSONL protocol

Each line is one JSON event. Baxan expects the following fields:

### Event fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `seq` | `u64` | ✅ | Monotonically increasing sequence number |
| `time_ms` | `u64` | ✅ | Timestamp in milliseconds (for timeline ordering) |
| `kind` | `"declare"` \| `"update"` \| `"drop"` | ✅ | Event type |
| `id` | `string` | ✅ | Unique variable identifier (used for edges) |
| `name` | `string` | ✅ | Display name shown in the graph |
| `type_name` | `string` | ✅ | Rust type name (e.g. `"Vec<u8>"`, `"Arc<State>"`) |
| `value` | `string` | ✅ | Human-readable value snapshot (e.g. `"len=4"`, `"ready: true"`) |
| `location` | `string` | ❌ | Source location (default: `"unknown"`) |
| `storage` | `string` | ❌ | Allocation class (default: `"stack"`) |
| `zone` | `string` | ❌ | Override zone grouping (auto-inferred from `storage` if omitted) |
| `address` | `string` | ❌ | Display address or allocation identity |
| `points_to` | `string[]` | ❌ | IDs this variable owns / points to (solid arrows) |
| `borrows` | `string[]` | ❌ | IDs this variable borrows / references (dotted arrows) |
| `bytes` | `u64` | ❌ | Byte size of the allocation |
| `thread` | `string` | ❌ | Thread name (default: `"main"`) |

### Event kinds

- **`declare`** — a new variable comes into scope. Baxan creates a node in the graph.
- **`update`** — the variable's value changes (mutation, resize, refcount change, etc.). The node updates in place; the update count increments.
- **`drop`** — the variable goes out of scope. The node is shown as dropped (dimmed) in the graph but remains available for replay.

### Zone inference

If `zone` is omitted, Baxan infers it from `storage`:

| Storage value | Inferred zone |
|---------------|---------------|
| `stack`, `borrow`, *(default)* | stack |
| `heap`, `box`, `vec`, `string` | heap |
| `data`, `static`, `const`, `rodata` | data |
| `arc`, `rc`, `mutex`, `rwlock`, `refcell`, `cell`, `atomic` | sync |

### Example event stream

```json
{"seq":1,"time_ms":0,"kind":"declare","id":"config","name":"config","type_name":"Config","value":"port: 8080","location":"src/main.rs:12","storage":"stack","zone":"stack","address":"0x7ffd_10a0","points_to":[],"borrows":[],"bytes":24,"thread":"main"}
{"seq":2,"time_ms":90,"kind":"declare","id":"shared","name":"shared","type_name":"Arc<State>","value":"strong=1","location":"src/state.rs:8","storage":"arc","zone":"sync","address":"0x7ffd_10c8","points_to":["state"],"borrows":[],"bytes":8,"thread":"main"}
{"seq":3,"time_ms":120,"kind":"declare","id":"state","name":"state","type_name":"State","value":"ready: true","location":"src/state.rs:4","storage":"heap","zone":"heap","address":"0x104a_2200","points_to":[],"borrows":[],"bytes":64,"thread":"main"}
{"seq":4,"time_ms":430,"kind":"update","id":"shared","name":"shared","type_name":"Arc<State>","value":"strong=2","location":"src/state.rs:8","storage":"arc","zone":"sync","address":"0x7ffd_10c8","points_to":["state"],"borrows":[],"bytes":8,"thread":"worker-1"}
{"seq":5,"time_ms":620,"kind":"drop","id":"state","name":"state","type_name":"State","value":"<dropped>","location":"src/state.rs:4","storage":"heap","zone":"heap","address":"0x104a_2200","points_to":[],"borrows":[],"bytes":64,"thread":"main"}
```

## Writing an emitter

Any Rust program can emit Baxan-compatible JSONL. Here is a minimal example using `serde_json`:

```rust
use serde::Serialize;
use std::io::Write;

#[derive(Serialize)]
struct Event {
    seq: u64,
    time_ms: u64,
    kind: &'static str,
    id: &'static str,
    name: &'static str,
    type_name: &'static str,
    value: String,
    #[serde(skip_serializing_if = "str::is_empty")]
    location: &'static str,
    #[serde(skip_serializing_if = "str::is_empty")]
    storage: &'static str,
    #[serde(skip_serializing_if = "str::is_empty")]
    zone: &'static str,
    #[serde(skip_serializing_if = "str::is_empty")]
    address: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    points_to: Vec<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    borrows: Vec<&'static str>,
    #[serde(skip_serializing_if = "is_zero")]
    bytes: u64,
    #[serde(skip_serializing_if = "str::is_empty")]
    thread: &'static str,
}

fn is_zero(v: &u64) -> bool { *v == 0 }

fn emit(seq: u64, event: &Event) {
    let mut out = std::io::stdout();
    serde_json::to_writer(&mut out, event).unwrap();
    writeln!(out).unwrap();
    out.flush().unwrap();
}

fn main() {
    // Example: declare a Vec, update it, then drop it
    let start = std::time::Instant::now();
    let ms = || start.elapsed().as_millis() as u64;

    emit(1, &Event {
        seq: 1, time_ms: ms(), kind: "declare",
        id: "buffer", name: "buffer", type_name: "Vec<u8>",
        value: "len=0 cap=0".into(), location: "src/main.rs:10",
        storage: "heap", zone: "heap",
        address: format!("0x{:x}", 0x104a_3000),
        points_to: vec![], borrows: vec![], bytes: 0, thread: "main",
    });

    // ... mutation, more events ...

    emit(2, &Event {
        seq: 2, time_ms: ms(), kind: "drop",
        id: "buffer", name: "buffer", type_name: "Vec<u8>",
        value: "<dropped>".into(), location: "src/main.rs:10",
        storage: "heap", zone: "heap",
        address: format!("0x{:x}", 0x104a_3000),
        points_to: vec![], borrows: vec![], bytes: 8192, thread: "main",
    });
}
```

Run the emitter and pipe to Baxan, or write to a file Baxan is tailing:

```bash
# Option A: pipe
cargo run --release -p my-emitter | cargo run --release -p baxan -- --events /dev/stdin

# Option B: file tailing (Baxan polls the file for new lines)
cargo run --release -p my-emitter > events.jsonl &
cargo run --release -p baxan -- --events events.jsonl
```

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                   Instrumented Rust process         │
│  (proc-macro · tracing layer · debugger adapter)    │
│                        │                            │
│                   JSONL stdout / file               │
└────────────────────────────┬─────────────────────────────────┘
                         │
                     ┌────▼────┐
                     │  Baxan │
                     └────┬────┘
                ┌──────────┼──────────┐
                ▼         ▼        ▼
         ┌─────────┐  ┌───────────┐   ┌─────────────┐
         │  egui  │  │ Ratatui │   |  (future) │
         │  GUI   │  │  TUI    │   |    Web    │
         └─────────┘  └───────────┘   └─────────────┘
```

- **Protocol consumer** — Baxan never instruments Rust code itself. It reads observations from an external emitter, which can be a proc-macro, a `tracing` layer, a `gdb`/`lldb` adapter, or a `cargo` runner.
- **Stateful rebuild** — the in-memory recording is reconstructed at every timeline position. Dropped values remain available for replay while the live map shows the current state.
- **Dual front-end** — the same event types and state management code (`EventKind`, `VariableEvent`, `VariableState`) are shared between the egui and Ratatui front-ends via `mod gui` and inline TUI code.
- **Zone-based layout** — variables are grouped into four memory zones (stack, heap, data, sync) for at-a-glance understanding of allocation patterns.

## Dependencies

| Crate | Purpose |
|-------|---------|
| [eframe](https://crates.io/crates/eframe) 0.36 | Native egui GUI framework |
| [ratatui](https://crates.io/crates/ratatui) 0.30 | Terminal UI framework |
| [crossterm](https://crates.io/crates/crossterm) 0.29 | Terminal backend for Ratatui |
| [clap](https://crates.io/crates/clap) 4.6 | Command-line argument parsing |
| [serde](https://crates.io/crates/serde) 1.0 | JSON (de)serialization |
| [serde_json](https://crates.io/crates/serde_json) 1.0 | JSONL parsing |

## Roadmap

- [ ] Live event tailing in the egui GUI (read new JSONL lines while running)
- [ ] Draggable nodes in the visualization graph
- [ ] Dark / light theme toggle
- [ ] WebAssembly target for in-browser visualization
- [ ] Integration with `cargo-inspect` / `rust-analyzer` for automatic instrumentation
- [ ] Export snapshots as SVG / PNG
- [ ] Multi-thread timeline with per-thread lanes

## License

MIT
