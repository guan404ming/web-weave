use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use crate::config::LATENCY_BUFFER_SIZE;

#[derive(Debug, Clone, serde::Serialize)]
pub struct MetricsSnapshot {
    pub urls_discovered: u64,
    pub urls_fetched: u64,
    pub active_domains: u64,
    pub current_qps: f64,
    pub success_rate: f64,
    pub p50_latency_ms: u64,
    pub p95_latency_ms: u64,
    pub p99_latency_ms: u64,
    pub errors_last_minute: u64,
    pub elapsed_secs: u64,
}

impl std::fmt::Display for MetricsSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "discovered={} fetched={} domains={} qps={:.1} success={:.1}% \
             latency p50={}ms p95={}ms p99={}ms errors={} elapsed={}s",
            self.urls_discovered,
            self.urls_fetched,
            self.active_domains,
            self.current_qps,
            self.success_rate * 100.0,
            self.p50_latency_ms,
            self.p95_latency_ms,
            self.p99_latency_ms,
            self.errors_last_minute,
            self.elapsed_secs,
        )
    }
}

struct LatencyTracker {
    buffer: Vec<u64>,
    index: usize,
    full: bool,
}

impl LatencyTracker {
    fn new(capacity: usize) -> Self {
        Self {
            buffer: vec![0; capacity],
            index: 0,
            full: false,
        }
    }

    fn push(&mut self, latency_ms: u64) {
        self.buffer[self.index] = latency_ms;
        self.index += 1;
        if self.index >= self.buffer.len() {
            self.index = 0;
            self.full = true;
        }
    }

    fn percentile(&self, p: f64) -> u64 {
        let len = if self.full {
            self.buffer.len()
        } else {
            self.index
        };
        if len == 0 {
            return 0;
        }
        let mut sorted: Vec<u64> = self.buffer[..len].to_vec();
        sorted.sort_unstable();
        let idx = ((p / 100.0) * len as f64).ceil() as usize;
        let idx = idx.saturating_sub(1).min(len - 1);
        sorted[idx]
    }
}

pub struct Metrics {
    pub urls_discovered: AtomicU64,
    pub urls_fetched: AtomicU64,
    pub fetch_successes: AtomicU64,
    pub fetch_errors: AtomicU64,
    pub robots_blocked: AtomicU64,

    latencies: Mutex<LatencyTracker>,
    started_at: Instant,
    last_fetched: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            urls_discovered: AtomicU64::new(0),
            urls_fetched: AtomicU64::new(0),
            fetch_successes: AtomicU64::new(0),
            fetch_errors: AtomicU64::new(0),
            robots_blocked: AtomicU64::new(0),
            latencies: Mutex::new(LatencyTracker::new(LATENCY_BUFFER_SIZE)),
            started_at: Instant::now(),
            last_fetched: AtomicU64::new(0),
        }
    }

    pub fn record_fetch(&self, latency_ms: u64, success: bool) {
        self.urls_fetched.fetch_add(1, Ordering::Relaxed);
        if success {
            self.fetch_successes.fetch_add(1, Ordering::Relaxed);
        } else {
            self.fetch_errors.fetch_add(1, Ordering::Relaxed);
        }
        self.latencies.lock().unwrap().push(latency_ms);
    }

    pub fn record_discovered(&self, count: u64) {
        self.urls_discovered.fetch_add(count, Ordering::Relaxed);
    }

    pub fn record_robots_blocked(&self) {
        self.robots_blocked.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self, active_domains: u64) -> MetricsSnapshot {
        let fetched = self.urls_fetched.load(Ordering::Relaxed);
        let successes = self.fetch_successes.load(Ordering::Relaxed);
        let errors = self.fetch_errors.load(Ordering::Relaxed);
        let elapsed = self.started_at.elapsed().as_secs().max(1);

        let prev_fetched = self.last_fetched.swap(fetched, Ordering::Relaxed);
        let delta = fetched.saturating_sub(prev_fetched);
        let qps = delta as f64 / 60.0;

        let total = successes + errors;
        let success_rate = if total > 0 {
            successes as f64 / total as f64
        } else {
            1.0
        };

        let tracker = self.latencies.lock().unwrap();
        let p50 = tracker.percentile(50.0);
        let p95 = tracker.percentile(95.0);
        let p99 = tracker.percentile(99.0);

        MetricsSnapshot {
            urls_discovered: self.urls_discovered.load(Ordering::Relaxed),
            urls_fetched: fetched,
            active_domains,
            current_qps: qps,
            success_rate,
            p50_latency_ms: p50,
            p95_latency_ms: p95,
            p99_latency_ms: p99,
            errors_last_minute: delta.saturating_sub((delta as f64 * success_rate) as u64),
            elapsed_secs: elapsed,
        }
    }

    pub fn check_alerts(&self, snapshot: &MetricsSnapshot) {
        if snapshot.urls_fetched > 100 && snapshot.current_qps < 10.0 {
            let msg = format!("Low throughput: QPS={:.1}", snapshot.current_qps);
            tracing::warn!("ALERT: {}", msg);
            notify(&msg);
        }
        if snapshot.urls_fetched > 100 && snapshot.success_rate < 0.5 {
            let msg = format!(
                "High error rate: success={:.1}%",
                snapshot.success_rate * 100.0
            );
            tracing::warn!("ALERT: {}", msg);
            notify(&msg);
        }
        if snapshot.p99_latency_ms > 30_000 {
            let msg = format!("High tail latency: p99={}ms", snapshot.p99_latency_ms);
            tracing::warn!("ALERT: {}", msg);
            notify(&msg);
        }
    }
}

/// Send a macOS alert dialog via osascript.
/// Uses "display alert" which stays on screen until dismissed.
fn notify(message: &str) {
    let script = format!(
        "display alert \"web-weave Alert\" message \"{}\" as critical",
        message.replace('\"', "\\\""),
    );
    std::thread::spawn(move || {
        let _ = std::process::Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .output();
    });
}
