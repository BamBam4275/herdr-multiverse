//! Fuzzes the routing-log, backlog, and task-status readers with random
//! and truncated bytes. The assertion in every case below is simply that
//! the call returns rather than panics — malformed input (a crash
//! mid-write, a killed process, a disk full) must fail closed as an
//! `Err`/`None`/empty result, never abort the reading process.

use std::fs;

use mv_state::{backlog, log, tasks};

/// Deterministic xorshift64 PRNG: reproducible across runs, no external
/// fuzzing dependency needed for a bounded number of small cases.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn next_byte(&mut self) -> u8 {
        (self.next_u64() & 0xff) as u8
    }

    fn next_range(&mut self, max: usize) -> usize {
        (self.next_u64() as usize) % max.max(1)
    }
}

fn random_bytes(rng: &mut Rng, max_len: usize) -> Vec<u8> {
    let len = rng.next_range(max_len);
    (0..len).map(|_| rng.next_byte()).collect()
}

/// A handful of valid, ASCII-only seed lines to truncate at arbitrary byte
/// boundaries — truncation is the most representative real-world damage,
/// more so than pure noise.
fn seed_lines() -> Vec<String> {
    vec![
        log::encode_versioned(
            "2026-11-02T12:00:00+00:00",
            1,
            "message",
            &[("to", "fixer"), ("body", "hello")],
        )
        .unwrap(),
        log::encode_versioned(
            "2026-11-02T12:00:01+00:00",
            2,
            "backlog",
            &[
                ("item", "20261102-b-aaaaaa"),
                ("state", "open"),
                ("title", "x"),
            ],
        )
        .unwrap(),
        "legacy\tevent=dispatch\ttask=one\trole=fixer".to_string(),
        "done".to_string(),
        "blocked".to_string(),
        "failed".to_string(),
    ]
}

/// Every seed line cut at ~12 evenly spaced byte offsets, including 0 and
/// the full length. All seed lines are ASCII, so any byte offset is a
/// valid `str` boundary.
fn truncations(line: &str) -> Vec<String> {
    (0..=line.len())
        .step_by((line.len() / 12).max(1))
        .map(|n| line[..n].to_string())
        .collect()
}

#[test]
fn log_parse_never_panics_on_random_or_truncated_lines() {
    let mut rng = Rng::new(0xC0FFEE);
    for _ in 0..300 {
        let bytes = random_bytes(&mut rng, 256);
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let _ = log::parse(&text);
    }
    for line in seed_lines() {
        for truncated in truncations(&line) {
            let _ = log::parse(&truncated);
        }
    }
}

#[test]
fn task_status_parse_never_panics_on_random_or_truncated_text() {
    let mut rng = Rng::new(0xFACADE);
    for _ in 0..300 {
        let bytes = random_bytes(&mut rng, 64);
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let _ = tasks::TaskStatus::parse(&text);
    }
    for line in seed_lines() {
        for truncated in truncations(&line) {
            let _ = tasks::TaskStatus::parse(&truncated);
        }
    }
}

#[test]
fn backlog_read_items_never_panics_on_a_malformed_routing_log() {
    let mut rng = Rng::new(0xBADF00D);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".multiverse")).unwrap();
    let log_path = root.join(".multiverse/routing.log");

    let mut contents = Vec::new();
    for _ in 0..300 {
        contents.extend(random_bytes(&mut rng, 200));
        contents.push(b'\n');
    }
    for line in seed_lines() {
        for truncated in truncations(&line) {
            contents.extend(truncated.as_bytes());
            contents.push(b'\n');
        }
    }
    fs::write(&log_path, &contents).unwrap();

    // No panic is the assertion; `read_items` is documented advisory (a
    // missing or unreadable log is an empty list, never an error), so
    // this only exercises the result rather than asserting its shape.
    let _ = backlog::read_items(root, false).len();
}

#[test]
fn read_status_verbose_never_panics_on_random_or_truncated_status_files() {
    let mut rng = Rng::new(0x5EED5EED);
    let dir = tempfile::tempdir().unwrap();
    for i in 0..300 {
        let bytes = random_bytes(&mut rng, 64);
        let path = dir.path().join(format!("status-{i}"));
        fs::write(&path, &bytes).unwrap();
        let _ = tasks::read_status_verbose(&path);
    }
    for (i, line) in seed_lines().iter().enumerate() {
        for (j, truncated) in truncations(line).into_iter().enumerate() {
            let path = dir.path().join(format!("status-trunc-{i}-{j}"));
            fs::write(&path, truncated.as_bytes()).unwrap();
            let _ = tasks::read_status_verbose(&path);
        }
    }
}
