//! Measurements of the TUI in a real pseudo-terminal (docs/adr/0015 Verification: first paint,
//! keystroke response, stream coalescing and memory at fixed transcript sizes). Run in release:
//! `cargo test --release -p aim --test tui_bench -- --ignored --nocapture --test-threads 1`.
//!
//! Times are measured from outside the process through the PTY and the `vt100` parser, so they
//! are upper bounds on what the TUI itself spends.
#![expect(clippy::format_collect, reason = "builds test text")]
#![expect(clippy::unwrap_used, reason = "measurement harness")]

mod tui_support;

use std::time::{Duration, Instant};

use serde_json::json;
use tui_support::{Start, Tui, text};

fn percentile(sorted: &[Duration], p: usize) -> Duration {
    let index = (sorted.len() * p / 100).min(sorted.len().saturating_sub(1));
    sorted.get(index).copied().unwrap_or_default()
}

fn ms(d: Duration) -> String {
    format!("{:.2} ms", d.as_secs_f64() * 1_000.0)
}

#[test]
#[ignore = "measurement; run in release with --nocapture"]
fn tui_first_paint() {
    let mut paint = Vec::new();
    let mut ready = Vec::new();
    for _ in 0..20 {
        let tui = Tui::start(&json!({"responses": []}), &Start::default());
        let first = tui.since_spawn_until(Duration::from_secs(10), |s| s.contains("starting…") || s.contains("idle")).unwrap();
        let idle = tui.since_spawn_until(Duration::from_secs(10), |s| s.contains("idle")).unwrap();
        paint.push(first);
        ready.push(idle);
        tui.quit();
    }
    println!(
        "first paint samples (ms): {:?}",
        paint.iter().map(|d| d.as_secs_f64() * 1_000.0).map(|v| (v * 10.0).round() / 10.0).collect::<Vec<_>>()
    );
    paint.sort();
    ready.sort();
    println!(
        "first paint (spawn → status line): p50 {} p95 {} · session ready (idle): p50 {} p95 {} · n=20",
        ms(percentile(&paint, 50)),
        ms(percentile(&paint, 95)),
        ms(percentile(&ready, 50)),
        ms(percentile(&ready, 95))
    );
}

fn keystrokes(tui: &Tui, n: usize) -> (Duration, Duration, usize) {
    let mut samples = Vec::new();
    let bytes_before = *tui.output_bytes.lock().unwrap();
    for i in 0..n {
        let c = char::from(b'a' + u8::try_from(i % 26).unwrap());
        let expected: String = (0..=i).map(|j| char::from(b'a' + u8::try_from(j % 26).unwrap())).collect();
        let needle = format!("› {expected}");
        let sent = Instant::now();
        tui.send(&c.to_string());
        while !tui.screen().contains(&needle) {
            assert!(sent.elapsed() < Duration::from_secs(5), "key {i} never painted");
            std::thread::sleep(Duration::from_micros(100));
        }
        samples.push(sent.elapsed());
    }
    let bytes = *tui.output_bytes.lock().unwrap() - bytes_before;
    samples.sort();
    (percentile(&samples, 50), percentile(&samples, 95), bytes / n)
}

#[test]
#[ignore = "measurement; run in release with --nocapture"]
fn tui_keystroke_to_paint() {
    let tui = Tui::start(&json!({"responses": []}), &Start::default());
    tui.wait_for("idle");
    let (p50, p95, bytes) = keystrokes(&tui, 60);
    println!("keystroke → painted (empty transcript): p50 {} p95 {} · {bytes} bytes/key · n=60", ms(p50), ms(p95));
    tui.quit();
}

#[test]
#[ignore = "measurement; run in release with --nocapture"]
fn tui_transcript_size() {
    for items in [0_usize, 1_000, 10_000] {
        let start = Instant::now();
        let tui = Tui::start(&json!({"responses": [], "seed_items": items}), &Start::default());
        // With no seed the session is new; its first paint needs no transcript.
        tui.wait("the replay", Duration::from_secs(120), |s| s.contains("idle"));
        let replayed = start.elapsed();
        std::thread::sleep(Duration::from_millis(300));
        let inline_rss = tui.rss_kib();
        let bytes = *tui.output_bytes.lock().unwrap();
        let (p50, p95, _) = keystrokes(&tui, 30);
        tui.send("\x15");
        tui.wait("an empty composer", Duration::from_secs(5), |s| s.contains("ask anything"));
        let toggle = Instant::now();
        tui.send("/fullscreen\r");
        while !(tui.alternate() && tui.screen().contains("idle")) {
            assert!(toggle.elapsed() < Duration::from_secs(60));
            std::thread::sleep(Duration::from_micros(200));
        }
        let open = toggle.elapsed();
        std::thread::sleep(Duration::from_millis(300));
        let full_rss = tui.rss_kib();
        let page = Instant::now();
        tui.send("\x1b[5~");
        let scrolled = if items == 0 {
            Duration::ZERO
        } else {
            page.elapsed() + tui.time_until(Duration::from_secs(10), |s| s.contains("rows below")).unwrap()
        };
        println!(
            "{items} items: attach+replay to idle {} ({} KiB written) · RSS inline {} MiB, fullscreen {} MiB · keystroke p50 {} p95 {} · fullscreen open {} · page up {}",
            ms(replayed),
            bytes / 1024,
            inline_rss / 1024,
            full_rss / 1024,
            ms(p50),
            ms(p95),
            ms(open),
            ms(scrolled)
        );
        tui.send("/fullscreen\r");
        tui.quit();
    }
}

#[test]
#[ignore = "measurement; run in release with --nocapture"]
fn tui_stream_coalescing() {
    let long: String = (0..2_000).map(|i| format!("w{i} ")).collect::<String>();
    let tui = Tui::start(&json!({"responses": [[text(&long, 2_000, 1)]]}), &Start::default());
    tui.wait_for("idle");
    let frames_before = *tui.frames.lock().unwrap();
    let start = Instant::now();
    tui.send("go\r");
    tui.wait("the stream to end", Duration::from_secs(60), |s| s.contains("w1999") && s.contains("idle"));
    let elapsed = start.elapsed();
    let frames = *tui.frames.lock().unwrap() - frames_before;
    println!(
        "stream of 2000 deltas over {}: {frames} frames ({:.1} fps)",
        ms(elapsed),
        f64::from(u32::try_from(frames).unwrap()) / elapsed.as_secs_f64()
    );
    tui.quit();
}
