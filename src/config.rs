use std::time::Duration;

// Concurrency
pub const NUM_WORKERS: usize = 800;
pub const MAX_CONCURRENT_FETCHES: usize = 500;
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
pub const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

// Rate limiting: 1 request per 2 seconds per domain
pub const PER_DOMAIN_PERIOD: Duration = Duration::from_secs(2);
pub const PER_DOMAIN_BURST: u32 = 1;

// Frontier scoring
pub const MAX_DEPTH: u32 = 20;
pub const DEPTH_WEIGHT: f64 = 10.0;
pub const NEW_DOMAIN_BONUS: f64 = 50.0;
pub const SITEMAP_BONUS: f64 = 30.0;
pub const MAX_URLS_PER_PAGE: usize = 500;
pub const FRONTIER_CAPACITY: usize = 5_000_000;
pub const FRONTIER_EVICT_BATCH: usize = 500_000;

// Per-domain error backoff
pub const BACKOFF_THRESHOLD: u32 = 3;
pub const BACKOFF_BASE: Duration = Duration::from_secs(30);
pub const BACKOFF_MAX: Duration = Duration::from_secs(3600);
pub const BACKOFF_RATE_THRESHOLD: f64 = 0.5;
pub const BACKOFF_RATE_MIN_ATTEMPTS: u32 = 5;

// Alert cooldown
pub const ALERT_COOLDOWN: Duration = Duration::from_secs(600);

// Bloom filter: sized for 48h at ~200M unique URLs, 0.1% FP (~345MB)
pub const BLOOM_EXPECTED_ITEMS: usize = 1_000_000_000;
pub const BLOOM_FP_RATE: f64 = 0.001;

// Persistence
pub const BATCH_SIZE: usize = 10_000;
pub const BATCH_TIMEOUT: Duration = Duration::from_secs(3);
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(300);

// Monitoring
pub const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(60);
pub const _CRAWL_DURATION: Duration = Duration::from_secs(48 * 3600);
pub const LATENCY_BUFFER_SIZE: usize = 10_000;

// Robots
pub const ROBOTS_CACHE_TTL: Duration = Duration::from_secs(86400);
pub const ROBOTS_CACHE_MAX_SIZE: usize = 10_000;

// User agent
pub const USER_AGENT: &str = "WebWeaveBot/0.1";

pub const DEFAULT_SEED_FILE: &str = "urls.txt";
pub const DEFAULT_DB_PATH: &str = "web_weave.db";
pub const DEFAULT_LOG_DIR: &str = "logs";

pub struct Config {
    pub seed_file: String,
    pub db_path: String,
    pub num_workers: usize,
    pub crawl_duration: Duration,
    pub resume: bool,
}

impl Config {
    pub fn from_env() -> Self {
        let resume = std::env::var("RESUME")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        let num_workers = std::env::var("NUM_WORKERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(NUM_WORKERS);
        let crawl_hours: u64 = std::env::var("CRAWL_HOURS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(48);

        Self {
            seed_file: std::env::var("SEED_FILE")
                .unwrap_or_else(|_| DEFAULT_SEED_FILE.to_string()),
            db_path: std::env::var("DB_PATH")
                .unwrap_or_else(|_| DEFAULT_DB_PATH.to_string()),
            num_workers,
            crawl_duration: Duration::from_secs(crawl_hours * 3600),
            resume,
        }
    }
}
