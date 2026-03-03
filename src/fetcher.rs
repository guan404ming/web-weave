use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use governor::clock::DefaultClock;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Jitter, Quota, RateLimiter};
use reqwest::{Client, StatusCode};

use crate::config::*;

pub type KeyedLimiter = RateLimiter<String, DefaultKeyedStateStore<String>, DefaultClock>;

pub struct FetchResult {
    pub status: StatusCode,
    pub body: Option<String>,
    pub content_type: Option<String>,
    pub latency_ms: u64,
    pub final_url: String,
}

pub struct Fetcher {
    pub client: Client,
    pub limiter: Arc<KeyedLimiter>,
}

impl Fetcher {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(HTTP_TIMEOUT)
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::limited(5))
            .pool_max_idle_per_host(0)
            .gzip(true)
            .brotli(true)
            .deflate(true)
            .build()?;

        let quota = Quota::with_period(PER_DOMAIN_PERIOD)
            .unwrap()
            .allow_burst(NonZeroU32::new(PER_DOMAIN_BURST).unwrap());
        let limiter = Arc::new(RateLimiter::keyed(quota));

        Ok(Self { client, limiter })
    }

    /// Fetch a URL, respecting per-domain rate limits.
    pub async fn fetch(&self, url: &str, domain: &str) -> Result<FetchResult> {
        let jitter = Jitter::up_to(Duration::from_millis(200));
        self.limiter
            .until_key_ready_with_jitter(&domain.to_string(), jitter)
            .await;

        let start = Instant::now();
        let resp = self.client.get(url).send().await?;
        let latency_ms = start.elapsed().as_millis() as u64;
        let status = resp.status();
        let final_url = resp.url().to_string();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        let body = if status.is_success() {
            let is_html = content_type
                .as_ref()
                .map(|ct| {
                    ct.contains("text/html")
                        || ct.contains("text/xml")
                        || ct.contains("application/xml")
                })
                .unwrap_or(false);
            if is_html {
                let text = resp.text().await?;
                if text.len() > MAX_RESPONSE_BYTES {
                    Some(text[..MAX_RESPONSE_BYTES].to_string())
                } else {
                    Some(text)
                }
            } else {
                None
            }
        } else {
            None
        };

        Ok(FetchResult {
            status,
            body,
            content_type,
            latency_ms,
            final_url,
        })
    }
}
