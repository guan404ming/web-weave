#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

mod config;
mod dedup;
mod fetcher;
mod frontier;
mod monitor;
mod parser;
mod robots;
mod store;

use std::io::BufRead;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use config::*;
use dedup::Dedup;
use fetcher::Fetcher;
use frontier::{compute_score, DomainBackoff, Frontier, ScoredUrl, UrlSource};
use monitor::Metrics;
use robots::RobotsManager;
use store::Store;

/// Commands sent to the DB writer task.
enum DbCommand {
    InsertUrls(Vec<(String, String, u32, String)>),
    MarkFetched(Vec<(String, u16)>),
}

/// Minimum workers always running.
const MIN_WORKERS: usize = 10;
/// How often to re-evaluate worker count.
const WORKER_ADJUST_INTERVAL: Duration = Duration::from_secs(30);
/// Workers per active (non-backed-off) domain.
const WORKERS_PER_DOMAIN: f64 = 0.5;
/// Dynamic fetch concurrency bounds.
const MIN_CONCURRENT_FETCHES: usize = 100;
const MAX_CONCURRENT_FETCHES_CAP: usize = 1000;

#[tokio::main]
async fn main() -> Result<()> {
    // Logging: console + rolling file in logs/
    let log_dir = std::env::var("LOG_DIR").unwrap_or_else(|_| DEFAULT_LOG_DIR.to_string());
    std::fs::create_dir_all(&log_dir).ok();
    let file_appender = tracing_appender::rolling::daily(&log_dir, "web-weave.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "web_weave=info".into());

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(non_blocking),
        )
        .init();

    let config = Config::from_env();
    tracing::info!(
        "Starting web-weave: max_workers={}, crawl_duration={}h, resume={}",
        config.num_workers,
        config.crawl_duration.as_secs() / 3600,
        config.resume,
    );

    let store = Arc::new(Store::open(&config.db_path)?);
    let shutdown = Arc::new(AtomicBool::new(false));

    // Initialize or restore
    let (frontier, dedup, backoff) = if config.resume {
        restore_from_checkpoint(&store)?
    } else {
        let frontier = Arc::new(Frontier::new());
        let dedup = Arc::new(Dedup::new(BLOOM_EXPECTED_ITEMS, BLOOM_FP_RATE)?);
        load_seeds(&frontier, &dedup, &config.seed_file)?;
        (frontier, dedup, Arc::new(DomainBackoff::new()))
    };

    tracing::info!(
        "Frontier initialized: {} URLs across {} domains",
        frontier.len(),
        frontier.active_domain_count(),
    );

    let fetcher = Arc::new(Fetcher::new()?);
    let robots = Arc::new(RobotsManager::new(
        fetcher.client.clone(),
        fetcher.limiter.clone(),
    ));
    let metrics = Arc::new(if config.resume {
        match store.load_latest_metrics() {
            Ok(Some((discovered, fetched))) => {
                tracing::info!("Restoring metrics: discovered={}, fetched={}", discovered, fetched);
                Metrics::with_initial(discovered, fetched)
            }
            _ => Metrics::new(),
        }
    } else {
        Metrics::new()
    });
    let active_workers = Arc::new(AtomicUsize::new(0));
    let fetch_limit = Arc::new(AtomicUsize::new(MAX_CONCURRENT_FETCHES_CAP));
    let active_fetches = Arc::new(AtomicUsize::new(0));

    // DB writer channel
    let (db_tx, db_rx) = mpsc::channel::<DbCommand>(10_000);

    // Spawn DB writer
    let store_w = store.clone();
    let shutdown_w = shutdown.clone();
    let db_writer = tokio::spawn(async move {
        db_writer_task(db_rx, store_w, shutdown_w).await;
    });

    // Spawn checkpoint loop
    let checkpoint = {
        let frontier = frontier.clone();
        let dedup = dedup.clone();
        let backoff = backoff.clone();
        let store = store.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            checkpoint_loop(frontier, dedup, backoff, store, shutdown).await;
        })
    };

    // Spawn monitor loop
    let monitor = {
        let metrics = metrics.clone();
        let frontier = frontier.clone();
        let store = store.clone();
        let shutdown = shutdown.clone();
        let backoff = backoff.clone();
        let active_workers = active_workers.clone();
        let robots = robots.clone();
        let fetcher = fetcher.clone();
        tokio::spawn(async move {
            monitor_loop(
                metrics,
                frontier,
                store,
                shutdown,
                backoff,
                active_workers,
                robots,
                fetcher,
            )
            .await;
        })
    };

    // Dynamic worker pool: start with a small batch, scale up as domains grow
    let max_workers = config.num_workers;
    let target_workers = Arc::new(AtomicUsize::new(MIN_WORKERS));

    // Worker scaler task
    let scaler = {
        let frontier = frontier.clone();
        let backoff = backoff.clone();
        let metrics = metrics.clone();
        let target_workers = target_workers.clone();
        let fetch_limit = fetch_limit.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(WORKER_ADJUST_INTERVAL);
            interval.tick().await;
            loop {
                interval.tick().await;
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                let domains = frontier.active_domain_count();
                let backed_off = backoff.backed_off_count();
                let effective_domains = domains.saturating_sub(backed_off);

                // Read latest metrics from monitor loop
                let snapshot = metrics.latest_snapshot.lock().unwrap().clone();

                // Worker scaling: domain-based with success rate adjustment
                let domain_based = (effective_domains as f64 * WORKERS_PER_DOMAIN) as usize;
                let desired_workers = if let Some(ref s) = snapshot {
                    if s.success_rate < 0.5 {
                        domain_based / 2
                    } else {
                        domain_based
                    }
                } else {
                    domain_based
                };
                let desired_workers = desired_workers.clamp(MIN_WORKERS, max_workers);
                target_workers.store(desired_workers, Ordering::Relaxed);

                // Fetch concurrency scaling based on health signals
                let current_limit = fetch_limit.load(Ordering::Relaxed);
                let new_limit = if let Some(ref s) = snapshot {
                    if s.success_rate > 0.8 && s.p50_latency_ms < 3000 {
                        current_limit + 100
                    } else if s.success_rate < 0.5 || s.p50_latency_ms > 5000 {
                        current_limit.saturating_sub(100)
                    } else {
                        current_limit
                    }
                } else {
                    current_limit
                };
                let new_limit = new_limit.clamp(MIN_CONCURRENT_FETCHES, MAX_CONCURRENT_FETCHES_CAP);
                fetch_limit.store(new_limit, Ordering::Relaxed);

                tracing::info!(
                    "Scaler: workers={}/{} fetch_limit={} effective_domains={}",
                    desired_workers, max_workers, new_limit, effective_domains,
                );
            }
        })
    };

    // Spawn worker tasks that self-regulate
    let mut worker_handles = Vec::with_capacity(max_workers);
    for id in 0..max_workers {
        let frontier = frontier.clone();
        let dedup = dedup.clone();
        let fetcher = fetcher.clone();
        let robots = robots.clone();
        let metrics = metrics.clone();
        let backoff = backoff.clone();
        let db_tx = db_tx.clone();
        let shutdown = shutdown.clone();
        let target_workers = target_workers.clone();
        let active_workers = active_workers.clone();
        let fetch_limit = fetch_limit.clone();
        let active_fetches = active_fetches.clone();

        worker_handles.push(tokio::spawn(async move {
            worker_loop(
                id,
                frontier,
                dedup,
                fetcher,
                robots,
                metrics,
                backoff,
                db_tx,
                shutdown,
                target_workers,
                active_workers,
                fetch_limit,
                active_fetches,
            )
            .await;
        }));
    }
    drop(db_tx);

    // Wait for shutdown
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("CTRL-C received, shutting down...");
        }
        _ = tokio::time::sleep(config.crawl_duration) => {
            tracing::info!("Crawl duration reached, shutting down...");
        }
    }

    shutdown.store(true, Ordering::SeqCst);

    let _ = tokio::time::timeout(Duration::from_secs(30), async {
        for h in worker_handles {
            let _ = h.await;
        }
    })
    .await;

    let _ = tokio::time::timeout(Duration::from_secs(10), db_writer).await;

    tracing::info!("Saving final checkpoint...");
    if let Err(e) = save_checkpoint(&frontier, &dedup, &backoff, &store) {
        tracing::error!("Failed to save final checkpoint: {}", e);
    }

    let snapshot = metrics.snapshot(frontier.active_domain_count() as u64);
    tracing::info!("Final metrics: {}", snapshot);

    checkpoint.abort();
    monitor.abort();
    scaler.abort();

    tracing::info!("Shutdown complete.");
    Ok(())
}

fn load_seeds(frontier: &Frontier, dedup: &Dedup, seed_file: &str) -> Result<()> {
    let file = std::fs::File::open(seed_file).context("open seed file")?;
    let reader = std::io::BufReader::new(file);
    let mut count = 0;

    for line in reader.lines() {
        let line = line?;
        let url = line.trim().to_string();
        if url.is_empty() || url.starts_with('#') {
            continue;
        }

        if dedup.check_and_insert(&url) {
            continue;
        }

        let domain = parser::extract_domain(&url).unwrap_or_default();
        let score = compute_score(0, UrlSource::Seed, true);
        frontier.push(ScoredUrl {
            url,
            domain,
            depth: 0,
            score,
            source: UrlSource::Seed,
        });
        count += 1;
    }

    tracing::info!("Loaded {} seed URLs", count);
    Ok(())
}

fn restore_from_checkpoint(
    store: &Store,
) -> Result<(Arc<Frontier>, Arc<Dedup>, Arc<DomainBackoff>)> {
    match store.load_latest_checkpoint()? {
        Some((frontier_data, bloom_data)) => {
            let frontier = Frontier::deserialize(&frontier_data)?;
            let dedup = Dedup::from_bytes(&bloom_data)?;
            let backoff = match store.load_backoff() {
                Ok(Some(data)) => DomainBackoff::deserialize(&data)?,
                _ => {
                    tracing::info!("No backoff checkpoint, starting fresh");
                    DomainBackoff::new()
                }
            };
            tracing::info!("Restored from checkpoint");
            Ok((Arc::new(frontier), Arc::new(dedup), Arc::new(backoff)))
        }
        None => {
            tracing::warn!("No checkpoint found, starting fresh");
            let frontier = Arc::new(Frontier::new());
            let dedup = Arc::new(Dedup::new(BLOOM_EXPECTED_ITEMS, BLOOM_FP_RATE)?);
            Ok((frontier, dedup, Arc::new(DomainBackoff::new())))
        }
    }
}

fn save_checkpoint(
    frontier: &Frontier,
    dedup: &Dedup,
    backoff: &DomainBackoff,
    store: &Store,
) -> Result<()> {
    let frontier_data = frontier.serialize()?;
    let bloom_data = dedup.to_bytes()?;
    let backoff_data = backoff.serialize()?;
    store.save_checkpoint(&frontier_data, &bloom_data)?;
    store.save_backoff(&backoff_data)?;
    Ok(())
}

async fn worker_loop(
    id: usize,
    frontier: Arc<Frontier>,
    dedup: Arc<Dedup>,
    fetcher: Arc<Fetcher>,
    robots: Arc<RobotsManager>,
    metrics: Arc<Metrics>,
    backoff: Arc<DomainBackoff>,
    db_tx: mpsc::Sender<DbCommand>,
    shutdown: Arc<AtomicBool>,
    target_workers: Arc<AtomicUsize>,
    active_workers: Arc<AtomicUsize>,
    fetch_limit: Arc<AtomicUsize>,
    active_fetches: Arc<AtomicUsize>,
) {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }

        // Dynamic scaling: sleep if this worker exceeds the target count
        let target = target_workers.load(Ordering::Relaxed);
        if id >= target {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        active_workers.fetch_add(1, Ordering::Relaxed);

        // Pop next URL from frontier
        let scored_url = match frontier.pop() {
            Some(u) => u,
            None => {
                active_workers.fetch_sub(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        // Skip domains in backoff
        if backoff.is_backed_off(&scored_url.domain) {
            // Re-enqueue the URL so it's not lost
            frontier.push(scored_url);
            active_workers.fetch_sub(1, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }

        // Check robots.txt
        match robots.is_allowed(&scored_url.url, &scored_url.domain).await {
            Ok(true) => {}
            Ok(false) => {
                metrics.record_robots_blocked();
                active_workers.fetch_sub(1, Ordering::Relaxed);
                continue;
            }
            Err(e) => {
                tracing::debug!("Robots error for {}: {}", scored_url.domain, e);
            }
        }

        // Process sitemaps from robots.txt
        let sitemaps = robots.get_sitemaps(&scored_url.domain);
        if !sitemaps.is_empty() {
            let mut sitemap_urls = Vec::new();
            for sitemap_url in sitemaps {
                if !dedup.check_and_insert(&sitemap_url) {
                    let domain = parser::extract_domain(&sitemap_url).unwrap_or_default();
                    let score = compute_score(0, UrlSource::Sitemap, false);
                    sitemap_urls.push(ScoredUrl {
                        url: sitemap_url,
                        domain,
                        depth: 0,
                        score,
                        source: UrlSource::Sitemap,
                    });
                }
            }
            if !sitemap_urls.is_empty() {
                let db_batch: Vec<_> = sitemap_urls
                    .iter()
                    .map(|u| (u.url.clone(), u.domain.clone(), u.depth, "sitemap".to_string()))
                    .collect();
                let _ = db_tx.send(DbCommand::InsertUrls(db_batch)).await;
                frontier.push_batch(sitemap_urls);
            }
        }

        // Respect crawl-delay
        if let Some(delay) = robots.get_crawl_delay(&scored_url.domain) {
            let default_period = PER_DOMAIN_PERIOD.as_secs_f64();
            if delay > default_period {
                tokio::time::sleep(Duration::from_secs_f64(delay - default_period)).await;
            }
        }

        // Wait on rate limiter BEFORE acquiring fetch slot
        fetcher.wait_rate_limit(&scored_url.domain).await;

        // Acquire dynamic fetch slot
        loop {
            if shutdown.load(Ordering::Relaxed) {
                active_workers.fetch_sub(1, Ordering::Relaxed);
                return;
            }
            let active = active_fetches.load(Ordering::Acquire);
            if active < fetch_limit.load(Ordering::Relaxed) {
                if active_fetches
                    .compare_exchange_weak(active, active + 1, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    break;
                }
            } else {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        // Fetch
        let fetch_result = match fetcher.fetch_direct(&scored_url.url).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("Fetch error {}: {}", scored_url.url, e);
                metrics.record_fetch(0, false);
                backoff.record_failure(&scored_url.domain);
                active_fetches.fetch_sub(1, Ordering::Release);
                active_workers.fetch_sub(1, Ordering::Relaxed);
                continue;
            }
        };

        // Release fetch slot immediately after HTTP response is read
        active_fetches.fetch_sub(1, Ordering::Release);

        let success = fetch_result.status.is_success();
        metrics.record_fetch(fetch_result.latency_ms, success);

        if success {
            backoff.record_success(&scored_url.domain);
        } else {
            backoff.record_failure(&scored_url.domain);
        }

        // Record fetch in DB
        let _ = db_tx
            .send(DbCommand::MarkFetched(vec![(
                scored_url.url.clone(),
                fetch_result.status.as_u16(),
            )]))
            .await;

        // Parse links
        if let Some(body) = &fetch_result.body {
            let is_xml = fetch_result
                .content_type
                .as_ref()
                .map(|ct| ct.contains("xml"))
                .unwrap_or(false);

            let links = if is_xml && scored_url.source == UrlSource::Sitemap {
                parser::parse_sitemap_xml(body, MAX_URLS_PER_PAGE)
            } else {
                parser::extract_links(body, &fetch_result.final_url, MAX_URLS_PER_PAGE)
            };

            let new_depth = scored_url.depth + 1;
            if new_depth <= MAX_DEPTH {
                // Pre-compute domains outside the bloom lock
                let link_domains: Vec<(String, String)> = links
                    .into_iter()
                    .map(|link| {
                        let domain = parser::extract_domain(&link).unwrap_or_default();
                        (link, domain)
                    })
                    .collect();

                // Single bloom lock for all URL + domain checks
                let dedup_results = dedup.check_urls_with_domains(&link_domains);

                let mut new_urls = Vec::new();
                for ((link, domain), result) in
                    link_domains.into_iter().zip(dedup_results)
                {
                    if let Some(domain_is_new) = result {
                        let score = compute_score(new_depth, UrlSource::Link, domain_is_new);
                        new_urls.push(ScoredUrl {
                            url: link,
                            domain,
                            depth: new_depth,
                            score,
                            source: UrlSource::Link,
                        });
                    }
                }

                if !new_urls.is_empty() {
                    metrics.record_discovered(new_urls.len() as u64);
                    let db_batch: Vec<_> = new_urls
                        .iter()
                        .map(|u| {
                            (
                                u.url.clone(),
                                u.domain.clone(),
                                u.depth,
                                "link".to_string(),
                            )
                        })
                        .collect();
                    let _ = db_tx.send(DbCommand::InsertUrls(db_batch)).await;
                    frontier.push_batch(new_urls);
                }
            }
        }

        active_workers.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn db_writer_task(
    mut rx: mpsc::Receiver<DbCommand>,
    store: Arc<Store>,
    shutdown: Arc<AtomicBool>,
) {
    let mut url_batch: Vec<(String, String, u32, String)> = Vec::new();
    let mut fetch_batch: Vec<(String, u16)> = Vec::new();

    loop {
        match tokio::time::timeout(BATCH_TIMEOUT, rx.recv()).await {
            Ok(Some(cmd)) => match cmd {
                DbCommand::InsertUrls(urls) => url_batch.extend(urls),
                DbCommand::MarkFetched(items) => fetch_batch.extend(items),
            },
            Ok(None) => break,
            Err(_) => {}
        }

        if url_batch.len() >= BATCH_SIZE
            || (!url_batch.is_empty() && shutdown.load(Ordering::Relaxed))
        {
            if let Err(e) = store.insert_urls_batch(&url_batch) {
                tracing::error!("DB insert error: {}", e);
            }
            url_batch.clear();
        }
        if fetch_batch.len() >= BATCH_SIZE
            || (!fetch_batch.is_empty() && shutdown.load(Ordering::Relaxed))
        {
            if let Err(e) = store.mark_fetched_batch(&fetch_batch) {
                tracing::error!("DB update error: {}", e);
            }
            fetch_batch.clear();
        }
    }

    if !url_batch.is_empty() {
        let _ = store.insert_urls_batch(&url_batch);
    }
    if !fetch_batch.is_empty() {
        let _ = store.mark_fetched_batch(&fetch_batch);
    }
}

async fn checkpoint_loop(
    frontier: Arc<Frontier>,
    dedup: Arc<Dedup>,
    backoff: Arc<DomainBackoff>,
    store: Arc<Store>,
    shutdown: Arc<AtomicBool>,
) {
    let mut interval = tokio::time::interval(CHECKPOINT_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;

    loop {
        interval.tick().await;
        if shutdown.load(Ordering::Relaxed) {
            return;
        }

        tracing::info!("Saving checkpoint...");
        match save_checkpoint(&frontier, &dedup, &backoff, &store) {
            Ok(()) => tracing::info!(
                "Checkpoint saved (frontier={} URLs, {} domains)",
                frontier.len(),
                frontier.active_domain_count(),
            ),
            Err(e) => tracing::error!("Checkpoint failed: {}", e),
        }
    }
}

async fn monitor_loop(
    metrics: Arc<Metrics>,
    frontier: Arc<Frontier>,
    store: Arc<Store>,
    shutdown: Arc<AtomicBool>,
    backoff: Arc<DomainBackoff>,
    active_workers: Arc<AtomicUsize>,
    robots: Arc<RobotsManager>,
    fetcher: Arc<Fetcher>,
) {
    let mut interval = tokio::time::interval(SNAPSHOT_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;
    let mut tick_count: u64 = 0;

    loop {
        interval.tick().await;
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        tick_count += 1;

        let active_domains = frontier.active_domain_count() as u64;
        let snapshot = metrics.snapshot(active_domains);
        *metrics.latest_snapshot.lock().unwrap() = Some(snapshot.clone());
        let workers = active_workers.load(Ordering::Relaxed);
        let backed_off = backoff.backed_off_count();
        let robots_cached = robots.cache_size();
        let limiter_size = fetcher.limiter.len();
        tracing::info!(
            "METRICS: {} | workers={} backed_off={} frontier_size={} robots_cache={} limiter_keys={}",
            snapshot,
            workers,
            backed_off,
            frontier.len(),
            robots_cached,
            limiter_size,
        );
        metrics.check_alerts(&snapshot);

        if let Err(e) = store.save_metrics(&snapshot) {
            tracing::error!("Failed to save metrics: {}", e);
        }

        // Periodic cleanup every 5 minutes
        if tick_count % 5 == 0 {
            let backoff_cleaned = backoff.cleanup();
            let limiter_before = fetcher.limiter.len();
            fetcher.limiter.retain_recent();
            let limiter_after = fetcher.limiter.len();
            tracing::info!(
                "Cleanup: backoff={} removed, limiter={} -> {}",
                backoff_cleaned,
                limiter_before,
                limiter_after,
            );
        }
    }
}
