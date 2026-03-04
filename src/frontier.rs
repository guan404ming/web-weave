use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{
    BACKOFF_BASE, BACKOFF_MAX, BACKOFF_MAX_ENTRIES, BACKOFF_RATE_MIN_ATTEMPTS,
    BACKOFF_RATE_THRESHOLD, BACKOFF_THRESHOLD, DEPTH_WEIGHT, FRONTIER_CAPACITY,
    MAX_DEPTH, MAX_PER_DOMAIN_URLS, NEW_DOMAIN_BONUS, SITEMAP_BONUS,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UrlSource {
    Seed,
    Link,
    Sitemap,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoredUrl {
    pub url: String,
    pub domain: String,
    pub depth: u32,
    pub score: f64,
    pub source: UrlSource,
}

impl PartialEq for ScoredUrl {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score
    }
}

impl Eq for ScoredUrl {}

impl PartialOrd for ScoredUrl {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredUrl {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .partial_cmp(&other.score)
            .unwrap_or(Ordering::Equal)
    }
}

/// Compute priority score for a URL.
/// Higher score = higher priority in the max-heap.
pub fn compute_score(depth: u32, source: UrlSource, is_new_domain: bool) -> f64 {
    let depth_score = (MAX_DEPTH.saturating_sub(depth)) as f64 * DEPTH_WEIGHT;
    let source_bonus = match source {
        UrlSource::Sitemap => SITEMAP_BONUS,
        _ => 0.0,
    };
    let domain_bonus = if is_new_domain {
        NEW_DOMAIN_BONUS
    } else {
        0.0
    };
    depth_score + source_bonus + domain_bonus
}

/// Per-domain error backoff state.
/// Tracks both consecutive failures and overall success/failure ratio.
pub struct DomainBackoff {
    state: Mutex<HashMap<String, BackoffEntry>>,
}

struct BackoffEntry {
    consecutive_failures: u32,
    total_successes: u32,
    total_failures: u32,
    times_backed_off: u32,
    backoff_until: Instant,
}

impl DomainBackoff {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Record a successful fetch for a domain.
    pub fn record_success(&self, domain: &str) {
        let mut state = self.state.lock().unwrap();
        let entry = state.entry(domain.to_string()).or_insert(BackoffEntry {
            consecutive_failures: 0,
            total_successes: 0,
            total_failures: 0,
            times_backed_off: 0,
            backoff_until: Instant::now(),
        });
        entry.consecutive_failures = 0;
        entry.total_successes += 1;
    }

    /// Record a failed fetch. Returns true if the domain should be backed off.
    pub fn record_failure(&self, domain: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        let entry = state.entry(domain.to_string()).or_insert(BackoffEntry {
            consecutive_failures: 0,
            total_successes: 0,
            total_failures: 0,
            times_backed_off: 0,
            backoff_until: Instant::now(),
        });
        entry.consecutive_failures += 1;
        entry.total_failures += 1;

        // Trigger 1: consecutive failures
        let consecutive_trigger = entry.consecutive_failures >= BACKOFF_THRESHOLD;

        // Trigger 2: high failure rate over enough attempts
        let total = entry.total_successes + entry.total_failures;
        let failure_rate = entry.total_failures as f64 / total as f64;
        let rate_trigger =
            total >= BACKOFF_RATE_MIN_ATTEMPTS && failure_rate > BACKOFF_RATE_THRESHOLD;

        if consecutive_trigger || rate_trigger {
            // Progressive backoff: each time a domain is backed off, increase duration
            let severity = entry.times_backed_off
                + if consecutive_trigger {
                    entry.consecutive_failures - BACKOFF_THRESHOLD
                } else {
                    0
                };
            let backoff_secs =
                BACKOFF_BASE.as_secs_f64() * 2f64.powi(severity as i32);
            let backoff = std::time::Duration::from_secs_f64(
                backoff_secs.min(BACKOFF_MAX.as_secs_f64()),
            );
            entry.backoff_until = Instant::now() + backoff;
            entry.times_backed_off += 1;
            tracing::debug!(
                "Domain {} backed off for {:.0}s (consec={}, rate={:.0}%, times={})",
                domain,
                backoff.as_secs_f64(),
                entry.consecutive_failures,
                failure_rate * 100.0,
                entry.times_backed_off,
            );
            true
        } else {
            false
        }
    }

    /// Check if a domain is currently in backoff.
    pub fn is_backed_off(&self, domain: &str) -> bool {
        let state = self.state.lock().unwrap();
        if let Some(entry) = state.get(domain) {
            Instant::now() < entry.backoff_until
        } else {
            false
        }
    }

    /// Number of domains currently in backoff.
    pub fn backed_off_count(&self) -> usize {
        let now = Instant::now();
        let state = self.state.lock().unwrap();
        state
            .values()
            .filter(|e| now < e.backoff_until)
            .count()
    }

    /// Remove expired backoff entries and enforce max size to bound memory.
    pub fn cleanup(&self) -> usize {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        let before = state.len();
        // Remove expired entries
        state.retain(|_, e| now < e.backoff_until);
        // If still over cap, drop entries closest to expiry
        if state.len() > BACKOFF_MAX_ENTRIES {
            let mut entries: Vec<(String, Instant)> = state
                .iter()
                .map(|(k, e)| (k.clone(), e.backoff_until))
                .collect();
            entries.sort_by_key(|(_, t)| *t);
            let to_remove = state.len() - BACKOFF_MAX_ENTRIES;
            for (key, _) in entries.into_iter().take(to_remove) {
                state.remove(&key);
            }
        }
        before - state.len()
    }
}

#[derive(Serialize, Deserialize)]
struct FrontierInner {
    domain_queues: HashMap<String, BinaryHeap<ScoredUrl>>,
    active_domains: VecDeque<String>,
    active_set: HashSet<String>,
    total_enqueued: u64,
    total_evicted: u64,
    current_size: usize,
}

pub struct Frontier {
    inner: Mutex<FrontierInner>,
}

impl Frontier {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(FrontierInner {
                domain_queues: HashMap::new(),
                active_domains: VecDeque::new(),
                active_set: HashSet::new(),
                total_enqueued: 0,
                total_evicted: 0,
                current_size: 0,
            }),
        }
    }

    pub fn push(&self, item: ScoredUrl) {
        let mut inner = self.inner.lock().unwrap();
        Self::push_inner(&mut inner, item);
    }

    pub fn push_batch(&self, items: Vec<ScoredUrl>) {
        let mut inner = self.inner.lock().unwrap();
        for item in items {
            Self::push_inner(&mut inner, item);
        }
    }

    fn push_inner(inner: &mut FrontierInner, item: ScoredUrl) {
        // Admission control: reject if at capacity
        if inner.current_size >= FRONTIER_CAPACITY {
            inner.total_evicted += 1;
            return;
        }

        // Per-domain cap: reject if this domain already has enough queued URLs
        if let Some(queue) = inner.domain_queues.get(&item.domain) {
            if queue.len() >= MAX_PER_DOMAIN_URLS {
                inner.total_evicted += 1;
                return;
            }
        }

        let domain = item.domain.clone();
        inner
            .domain_queues
            .entry(domain.clone())
            .or_default()
            .push(item);
        if inner.active_set.insert(domain.clone()) {
            inner.active_domains.push_back(domain);
        }
        inner.total_enqueued += 1;
        inner.current_size += 1;
    }

    /// Pop the highest-priority URL using round-robin across domains.
    pub fn pop(&self) -> Option<ScoredUrl> {
        let mut inner = self.inner.lock().unwrap();
        let n = inner.active_domains.len();
        if n == 0 {
            return None;
        }

        for _ in 0..n {
            if let Some(domain) = inner.active_domains.pop_front() {
                let popped = inner.domain_queues.get_mut(&domain).and_then(|q| q.pop());
                if let Some(item) = popped {
                    inner.current_size -= 1;
                    let empty = inner.domain_queues.get(&domain).map_or(true, |q| q.is_empty());
                    if empty {
                        inner.domain_queues.remove(&domain);
                        inner.active_set.remove(&domain);
                    } else {
                        inner.active_domains.push_back(domain);
                    }
                    return Some(item);
                }
                inner.active_set.remove(&domain);
            }
        }
        None
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().current_size
    }

    pub fn active_domain_count(&self) -> usize {
        self.inner.lock().unwrap().active_domains.len()
    }

    #[allow(dead_code)]
    pub fn total_enqueued(&self) -> u64 {
        self.inner.lock().unwrap().total_enqueued
    }

    pub fn serialize(&self) -> Result<Vec<u8>> {
        let inner = self.inner.lock().unwrap();
        bincode::serialize(&*inner).context("serialize frontier")
    }

    pub fn deserialize(data: &[u8]) -> Result<Self> {
        let inner: FrontierInner =
            bincode::deserialize(data).context("deserialize frontier")?;
        Ok(Self {
            inner: Mutex::new(inner),
        })
    }
}
