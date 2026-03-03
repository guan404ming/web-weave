use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use dashmap::DashMap;
use governor::Jitter;
use reqwest::Client;
use texting_robots::Robot;
use url::Url;

use crate::config::{ROBOTS_CACHE_MAX_SIZE, ROBOTS_CACHE_TTL, USER_AGENT};
use crate::fetcher::KeyedLimiter;

struct CachedRobots {
    robot: Robot,
    sitemaps: Vec<String>,
    fetched_at: Instant,
}

pub struct RobotsManager {
    cache: DashMap<String, Option<CachedRobots>>,
    client: Client,
    limiter: Arc<KeyedLimiter>,
}

impl RobotsManager {
    pub fn new(client: Client, limiter: Arc<KeyedLimiter>) -> Self {
        Self {
            cache: DashMap::new(),
            client,
            limiter,
        }
    }

    /// Check if a URL is allowed by robots.txt. Fetches and caches on first access.
    pub async fn is_allowed(&self, url: &str, domain: &str) -> Result<bool> {
        self.ensure_cached(url, domain).await?;

        if let Some(entry) = self.cache.get(domain) {
            if let Some(cached) = entry.value() {
                return Ok(cached.robot.allowed(url));
            }
        }
        // No robots.txt or fetch failed, allow by default
        Ok(true)
    }

    /// Get cached sitemap URLs for a domain.
    pub fn get_sitemaps(&self, domain: &str) -> Vec<String> {
        self.cache
            .get(domain)
            .and_then(|entry| {
                entry
                    .value()
                    .as_ref()
                    .map(|cached| cached.sitemaps.clone())
            })
            .unwrap_or_default()
    }

    /// Get crawl-delay for a domain if specified and greater than our default.
    pub fn get_crawl_delay(&self, domain: &str) -> Option<f64> {
        self.cache.get(domain).and_then(|entry| {
            entry
                .value()
                .as_ref()
                .and_then(|cached| cached.robot.delay.map(|d| d as f64))
        })
    }

    /// Number of cached entries.
    pub fn cache_size(&self) -> usize {
        self.cache.len()
    }

    /// Ensure robots.txt is cached for the given domain.
    async fn ensure_cached(&self, url: &str, domain: &str) -> Result<()> {
        // Check if already cached and not stale
        if let Some(entry) = self.cache.get(domain) {
            if let Some(cached) = entry.value() {
                if cached.fetched_at.elapsed() < ROBOTS_CACHE_TTL {
                    return Ok(());
                }
            } else {
                // Failed fetch is cached
                return Ok(());
            }
        }

        // Evict oldest entries if cache is too large
        if self.cache.len() > ROBOTS_CACHE_MAX_SIZE {
            let mut entries: Vec<(String, Instant)> = self
                .cache
                .iter()
                .filter_map(|entry| {
                    let fetched = entry
                        .value()
                        .as_ref()
                        .map(|c| c.fetched_at)
                        .unwrap_or(Instant::now());
                    Some((entry.key().clone(), fetched))
                })
                .collect();
            entries.sort_by_key(|(_, t)| *t);
            let evict_count = entries.len() / 4;
            for (key, _) in entries.into_iter().take(evict_count) {
                self.cache.remove(&key);
            }
            tracing::debug!(
                "Robots cache evicted {} entries (size: {})",
                evict_count,
                self.cache.len(),
            );
        }

        // Rate limit the robots.txt fetch (same limiter as page fetches)
        let jitter = Jitter::up_to(Duration::from_millis(200));
        self.limiter
            .until_key_ready_with_jitter(&domain.to_string(), jitter)
            .await;

        // Fetch robots.txt
        let robots_url = Self::robots_url(url, domain)?;
        match self.fetch_robots(&robots_url).await {
            Ok((robot, sitemaps)) => {
                self.cache.insert(
                    domain.to_string(),
                    Some(CachedRobots {
                        robot,
                        sitemaps,
                        fetched_at: Instant::now(),
                    }),
                );
            }
            Err(e) => {
                tracing::debug!("Failed to fetch robots.txt for {}: {}", domain, e);
                self.cache.insert(domain.to_string(), None);
            }
        }
        Ok(())
    }

    async fn fetch_robots(&self, robots_url: &str) -> Result<(Robot, Vec<String>)> {
        let resp = self
            .client
            .get(robots_url)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;

        if !resp.status().is_success() {
            anyhow::bail!("HTTP {}", resp.status());
        }

        let body = resp.bytes().await?;
        let robot = Robot::new(USER_AGENT, &body)?;

        let sitemaps: Vec<String> = robot
            .sitemaps
            .iter()
            .map(|s| s.to_string())
            .collect();

        Ok((robot, sitemaps))
    }

    fn robots_url(url: &str, domain: &str) -> Result<String> {
        let parsed = Url::parse(url)?;
        let scheme = parsed.scheme();
        Ok(format!("{}://{}/robots.txt", scheme, domain))
    }
}
