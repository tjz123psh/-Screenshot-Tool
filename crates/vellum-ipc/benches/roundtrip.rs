//! Control socket round-trip benchmark.
//!
//! ARCHITECTURE.md §6 asks for the hotkey path to be measured rather than
//! assumed, so this drives the real [`daemon::run`] accept loop over a real
//! Unix socket with the real client, in a private runtime directory. Nothing
//! here is a simplified stand-in: replacing the loop with a hand-written
//! `accept()` would measure code that never ships.
//!
//! `ping` is the request the hotkey path actually waits on. `status` is
//! included because the CLI and tray call it every two seconds and it reaps a
//! finished child first, so it is the slower of the two read-only commands.

use std::time::{Duration, Instant};

use vellum_ipc::{client, daemon, Request};

const WARMUP: usize = 50;
const SAMPLES: usize = 1000;

fn main() {
    let dir = std::env::temp_dir().join(format!("vellum-bench-{}", std::process::id()));
    // Set before the daemon thread starts: both sides resolve the socket path
    // through this variable, so it must be in place for the whole process.
    unsafe { std::env::set_var("VELLUM_RUNTIME_DIR", &dir) };

    let handle = std::thread::spawn(daemon::run);
    if !wait_for_daemon() {
        eprintln!("daemon did not come up at {}", dir.display());
        std::process::exit(1);
    }

    let ping = measure("ping", &Request::Ping, client::PING_TIMEOUT);
    let status = measure("status", &Request::Status, client::STATUS_TIMEOUT);

    client::send(&Request::Shutdown, client::ACTION_TIMEOUT);
    let _ = handle.join();
    let _ = std::fs::remove_dir_all(&dir);

    println!("socket round-trip, {SAMPLES} samples each\n");
    for report in [ping, status] {
        report.print();
    }
}

fn wait_for_daemon() -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if client::ping() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

struct Report {
    label: &'static str,
    samples: Vec<Duration>,
}

impl Report {
    /// Percentiles, not just a mean: a mean would hide a loop that answers most
    /// requests instantly and occasionally stalls for a whole poll interval,
    /// which is exactly the failure mode worth catching here.
    fn print(&self) {
        let mean = self.samples.iter().sum::<Duration>() / self.samples.len() as u32;
        println!(
            "{:<8} mean {:>7.3} ms  p50 {:>7.3} ms  p95 {:>7.3} ms  p99 {:>7.3} ms  max {:>7.3} ms",
            self.label,
            ms(mean),
            ms(self.percentile(50.0)),
            ms(self.percentile(95.0)),
            ms(self.percentile(99.0)),
            ms(self.percentile(100.0)),
        );
    }

    fn percentile(&self, pct: f64) -> Duration {
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let index = ((pct / 100.0) * (sorted.len() - 1) as f64).round() as usize;
        sorted[index]
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn measure(label: &'static str, request: &Request, timeout: Duration) -> Report {
    for _ in 0..WARMUP {
        assert!(client::send(request, timeout).is_some(), "{label} failed");
    }

    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let start = Instant::now();
        let response = client::send(request, timeout);
        samples.push(start.elapsed());
        assert!(response.is_some(), "{label} failed");
    }
    Report { label, samples }
}
