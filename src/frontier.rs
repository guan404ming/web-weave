use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::*;

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

#[derive(Serialize, Deserialize)]
struct FrontierInner {
    domain_queues: HashMap<String, BinaryHeap<ScoredUrl>>,
    active_domains: VecDeque<String>,
    active_set: HashSet<String>,
    total_enqueued: u64,
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
            }),
        }
    }

    pub fn push(&self, item: ScoredUrl) {
        let mut inner = self.inner.lock().unwrap();
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
    }

    pub fn push_batch(&self, items: Vec<ScoredUrl>) {
        let mut inner = self.inner.lock().unwrap();
        for item in items {
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
        }
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
                if let Some(queue) = inner.domain_queues.get_mut(&domain) {
                    if let Some(item) = queue.pop() {
                        if queue.is_empty() {
                            inner.domain_queues.remove(&domain);
                            inner.active_set.remove(&domain);
                        } else {
                            inner.active_domains.push_back(domain);
                        }
                        return Some(item);
                    }
                }
                // Queue was empty or missing, remove from active set
                inner.active_set.remove(&domain);
            }
        }
        None
    }

    pub fn len(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        inner
            .domain_queues
            .values()
            .map(|q| q.len())
            .sum()
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
