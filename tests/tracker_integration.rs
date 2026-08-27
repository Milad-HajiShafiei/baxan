//! Integration test: builds a small Rust binary, injects the heap tracker
//! via LD_PRELOAD / DYLD_INSERT_LIBRARIES, and validates the emitted JSONL.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Build baxan (which compiles the tracker shared library via build.rs),
/// then locate and return the path to the compiled tracker.
fn build_and_find_tracker_lib() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));

    // Build baxan so build.rs compiles the tracker
    let status = Command::new("cargo")
        .arg("build")
        .current_dir(manifest_dir)
        .status()
        .expect("failed to run cargo build on baxan");
    assert!(status.success(), "baxan build failed");

    let lib_name = if cfg!(target_os = "linux") {
        "libbaxan_tracker.so"
    } else {
        "libbaxan_tracker.dylib"
    };

    // Walk target/debug/build to find the tracker library
    let build_dir = manifest_dir.join("target").join("debug").join("build");
    if let Ok(entries) = fs::read_dir(&build_dir) {
        for entry in entries.flatten() {
            let candidate = entry.path().join("out").join(lib_name);
            if candidate.exists() {
                return candidate;
            }
        }
    }

    panic!(
        "Could not find tracker library {} after build.",
        lib_name
    );
}

/// Build the test fixture binary and return its path.
fn build_fixture() -> PathBuf {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("heap_app");

    let status = Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(&fixture_dir)
        .status()
        .expect("failed to run cargo build on fixture");
    assert!(status.success(), "fixture build failed");

    fixture_dir
        .join("target")
        .join("release")
        .join("heap_app")
}

#[test]
fn tracker_emits_valid_jsonl_with_heap_events() {
    let tracker_path = build_and_find_tracker_lib();
    let binary_path = build_fixture();

    assert!(binary_path.exists(), "fixture binary not found: {binary_path:?}");

    let events_path = std::env::temp_dir().join(format!(
        "baxan_test_{}.jsonl",
        std::process::id()
    ));

    // Remove any leftover events file
    let _ = fs::remove_file(&events_path);

    // Run the fixture binary with the tracker injected
    let mut cmd = Command::new(&binary_path);
    cmd.env("BAXAN_TRACKER_OUTPUT", &events_path);
    // Set both LD_PRELOAD (Linux) and DYLD_INSERT_LIBRARIES (macOS)
    cmd.env("LD_PRELOAD", &tracker_path);
    cmd.env("DYLD_INSERT_LIBRARIES", &tracker_path);

    let output = cmd.output().expect("failed to spawn fixture binary");
    assert!(
        output.status.success(),
        "fixture exited with error: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Read and parse the events file
    let content = fs::read_to_string(&events_path)
        .unwrap_or_else(|e| panic!("failed to read events file {events_path:?}: {e}"));
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();

    assert!(
        !lines.is_empty(),
        "tracker produced no events — LD_PRELOAD may not have taken effect"
    );

    // Validate each line is valid JSON with required fields
    let mut had_declare = false;
    let mut had_drop = false;
    for (i, line) in lines.iter().enumerate() {
        let val: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line {i} is not valid JSON: {e}\n  content: {line}"));

        // Required fields
        assert!(
            val.get("seq").is_some(),
            "line {i} missing 'seq': {line}"
        );
        assert!(
            val.get("time_ms").is_some(),
            "line {i} missing 'time_ms': {line}"
        );
        assert!(
            val.get("kind").is_some(),
            "line {i} missing 'kind': {line}"
        );
        assert!(
            val.get("id").is_some(),
            "line {i} missing 'id': {line}"
        );
        assert!(
            val.get("name").is_some(),
            "line {i} missing 'name': {line}"
        );
        assert!(
            val.get("type_name").is_some(),
            "line {i} missing 'type_name': {line}"
        );
        assert!(
            val.get("value").is_some(),
            "line {i} missing 'value': {line}"
        );
        assert!(
            val.get("storage").is_some(),
            "line {i} missing 'storage': {line}"
        );
        assert!(
            val.get("zone").is_some(),
            "line {i} missing 'zone': {line}"
        );
        assert!(
            val.get("bytes").is_some(),
            "line {i} missing 'bytes': {line}"
        );
        assert!(
            val.get("thread").and_then(|value| value.as_str()).is_some(),
            "line {i} missing string 'thread': {line}"
        );
        assert!(
            val.get("points_to").and_then(|value| value.as_array()).is_some(),
            "line {i} missing array 'points_to': {line}"
        );

        // Kind must be declare, update, or drop
        let kind = val["kind"].as_str().expect("kind is not a string");
        assert!(
            matches!(kind, "declare" | "update" | "drop"),
            "line {i} has invalid kind '{kind}': {line}"
        );

        // seq should be monotonically increasing
        if i > 0 {
            let prev: u64 = serde_json::from_value(lines[i - 1]
                .parse::<serde_json::Value>()
                .unwrap()["seq"]
                .clone())
            .unwrap();
            let cur: u64 = val["seq"].as_u64().expect("seq is not a u64");
            assert!(
                cur > prev,
                "seq not monotonic at line {i}: {prev} -> {cur}"
            );
        }

        // bytes should be a non-negative integer
        let bytes = val["bytes"].as_u64().expect("bytes is not a u64");
        if kind == "drop" {
            assert_eq!(bytes, 0, "drop events should have bytes=0 at line {i}");
        }

        // storage should be one of the known values
        let storage = val["storage"].as_str().unwrap_or("");
        assert!(
            storage == "heap" || storage == "stack" || storage == "static" || storage == "box"
                || storage == "vec" || storage == "string" || storage == "arc" || storage == "rc"
                || storage == "mutex" || storage == "borrow",
            "line {i} has unexpected storage '{storage}': {line}"
        );

        if kind == "declare" {
            had_declare = true;
        }
        if kind == "drop" {
            had_drop = true;
        }
    }

    // We should see at least some heap allocations (declare) and deallocations (drop)
    assert!(
        had_declare,
        "expected at least one 'declare' event, got {} lines",
        lines.len()
    );
    assert!(
        had_drop,
        "expected at least one 'drop' event, got {} lines",
        lines.len()
    );

    // The fixture allocates several distinct objects, so we should see multiple events
    assert!(
        lines.len() >= 6,
        "expected at least 6 events from the fixture, got {}",
        lines.len()
    );

    // Cleanup
    let _ = fs::remove_file(&events_path);
}

#[test]
fn tracker_handles_empty_binary_gracefully() {
    // A binary that does zero heap allocations should produce zero events
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let empty_dir = fixture_dir.join("empty_app");
    let src_dir = empty_dir.join("src");

    // Create a minimal no-alloc binary
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        empty_dir.join("Cargo.toml"),
        r#"[package]
name = "empty_app"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "empty_app"
path = "src/main.rs"
"#,
    )
    .unwrap();
    fs::write(
        src_dir.join("main.rs"),
        "fn main() { /* no heap allocations */ }\n",
    )
    .unwrap();

    let status = Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(&empty_dir)
        .status()
        .expect("failed to build empty fixture");
    assert!(status.success(), "empty fixture build failed");

    let binary = empty_dir.join("target").join("release").join("empty_app");
    assert!(binary.exists());

    let tracker_path = build_and_find_tracker_lib();
    let events_path = std::env::temp_dir().join(format!(
        "baxan_test_empty_{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&events_path);

    let output = Command::new(&binary)
        .env("BAXAN_TRACKER_OUTPUT", &events_path)
        .env("LD_PRELOAD", &tracker_path)
        .env("DYLD_INSERT_LIBRARIES", &tracker_path)
        .output()
        .expect("failed to run empty fixture");

    assert!(
        output.status.success(),
        "empty fixture exited with error: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The file may not exist (no allocations → no writes) or be empty
    if let Ok(content) = fs::read_to_string(&events_path) {
        let non_empty: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        // Even if there are events (e.g. from the Rust runtime), they should be valid JSON
        for line in &non_empty {
            let _: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("invalid JSON from empty binary: {e}\n  {line}"));
        }
    }

    let _ = fs::remove_file(&events_path);
    let _ = fs::remove_dir_all(&empty_dir);
}
