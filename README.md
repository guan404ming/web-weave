# web-weave

Single-machine URL crawler in Rust. Starts from 1,000 seed URLs and maximizes URL discovery over a 48-hour crawl window while strictly honoring politeness constraints.

## Results (30-minute sample run)

| Metric | Value |
|--------|-------|
| Total discovered URLs | 7,720,000 |
| Total successfully-crawled URLs | 222,000 |
| Unique domains reached | 345,600 |
| Frontier size | 759,000 |
| Sustained QPS | ~128 |
| Success rate | 77-78% |
| Avg latency (p50) | ~700 ms |
| Memory usage (steady-state) | ~3 GB |

48-hour projection: ~17M successfully crawled, ~800M discovered, ~3.5M unique domains.

## Hardware Requirements

| Resource | Minimum | Recommended |
|----------|---------|-------------|
| RAM | 4 GB | 8 GB+ |
| CPU | 2 cores | 4+ cores |
| Disk | 10 GB free | 50 GB free |
| Network | Stable broadband | Low-latency connection |

The bloom filter alone uses ~1.7 GB (sized for 1B URLs at 0.1% false positive rate). The frontier can grow up to ~500 MB (5M URLs). SQLite database grows with discovered URLs. Checkpoints (frontier + bloom + backoff serialization) require temporary disk space (~2.5 GB).

## Build and Run

```bash
# Build (requires Rust toolchain)
cargo build --release

# Run with defaults (48h crawl, 800 workers)
cargo run --release

# Custom configuration via environment
CRAWL_HOURS=1 NUM_WORKERS=200 RESUME=true cargo run --release

# Short test run
CRAWL_HOURS=0 CRAWL_MINS=30 cargo run --release

# Resume from last checkpoint after interruption
RESUME=true cargo run --release
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SEED_FILE` | `urls.txt` | Path to seed URL file (one URL per line) |
| `DB_PATH` | `web_weave.db` | SQLite database path |
| `NUM_WORKERS` | `800` | Maximum worker count |
| `CRAWL_HOURS` | `48` | Crawl duration in hours |
| `CRAWL_MINS` | `0` | Additional crawl duration in minutes |
| `RESUME` | `false` | Resume from last checkpoint |
| `LOG_DIR` | `logs` | Directory for log files |

## Output Files

| File | Description |
|------|-------------|
| `web_weave.db` | SQLite database with all discovered/fetched URLs and metrics snapshots |
| `web_weave.bloom` | Bloom filter checkpoint (~1.7 GB) for resume support |
| `web_weave.frontier` | Frontier state checkpoint for resume support |
| `web_weave.backoff` | Domain backoff state checkpoint for resume support |
| `logs/web-weave.log.*` | Daily rolling log files |

## Politeness Guarantees

**0.5 QPS per domain.** Every request to a domain, including robots.txt fetches, goes through a shared governor keyed rate limiter configured at 1 request per 2 seconds (0.5 QPS). The limiter is keyed by domain string and enforced before any HTTP request leaves the process. Burst is set to 1, so no token accumulation is possible.

**robots.txt honored.** Before fetching any URL, the worker checks a cached robots.txt for that domain. On first access, robots.txt is fetched (also rate-limited), parsed with `texting_robots`, and cached for 24 hours. If robots.txt disallows the URL, the fetch is skipped entirely. If robots.txt specifies a `crawl-delay` greater than 2 seconds, the worker sleeps for the additional time. Sitemap URLs from robots.txt are extracted and fed into the frontier with a scoring bonus.

## Service Architecture

```
                          +------------------+
                          |   Seed URLs      |
                          |   (urls.txt)     |
                          +--------+---------+
                                   |
                                   v
+--------------------------------------------------------------------+
|                          Frontier                                  |
|   Per-domain MinMaxHeap (double-ended priority queue)              |
|   Round-robin VecDeque across domains                              |
|   Capacity: 5M URLs, smart admission replaces lowest-score URLs   |
+----+----------+----------+----------+----------+----------+---+----+
     |          |          |          |          |          |
     v          v          v          v          v          v
 +--------+ +--------+ +--------+ +--------+ +--------+ +--------+
 |Worker 1| |Worker 2| |Worker 3| |  ...   | |  ...   | |Worker N|
 +--------+ +--------+ +--------+ +--------+ +--------+ +--------+
     |          |          |          |          |          |
     |   Each worker:                                      |
     |   1. Pop URL from frontier (round-robin)            |
     |   2. Check domain backoff                           |
     |   3. Check robots.txt (rate-limited, cached 24h)    |
     |   4. Wait on per-domain rate limiter (0.5 QPS)      |
     |   5. Acquire dynamic fetch slot (adaptive limit)    |
     |   6. HTTP GET (reqwest, 15s timeout)                |
     |   7. Release fetch slot after response              |
     |   8. Parse links (scraper) or sitemap XML           |
     |   9. Batch dedup via lock-free bloom filter         |
     |  10. Score and push new URLs to frontier            |
     |                                                     |
     +------------------+--+--+----------------------------+
                        |  |  |
                        v  v  v
                 +------+--+--+------+
                 |  mpsc channel     |
                 +--------+----------+
                          |
                          v
                 +--------+----------+
                 |   DB Writer       |   Batches up to 10K rows
                 |   (SQLite WAL)    |   or flushes every 3s
                 +-------------------+

  Background tasks:
  - Checkpoint loop: serialize frontier + bloom + backoff every 5 min
  - Monitor loop: log metrics every 60s, cleanup every min
  - Worker scaler: adjust workers and fetch concurrency every 30s
```

## Module Overview

| Module | Responsibility |
|--------|---------------|
| `main.rs` | Orchestrator. Spawns workers, DB writer, checkpoint/monitor/scaler loops. Dynamic worker scaling and adaptive fetch concurrency based on metrics. Graceful shutdown via ctrl-c or crawl duration. Backoff state persisted across restarts. |
| `config.rs` | All tunable constants and environment-based configuration. Supports `CRAWL_HOURS` and `CRAWL_MINS`. |
| `frontier.rs` | Per-domain MinMaxHeap (double-ended priority queue) with round-robin scheduling. Smart admission: when a domain queue is full (5 URLs), replaces the lowest-score URL if the new one scores higher. Domain error backoff with progressive exponential backoff, serializable for checkpoint persistence. Backoff entries capped at 500K. |
| `fetcher.rs` | reqwest HTTP client with per-domain rate limiting via governor (0.5 QPS). Compression support (gzip, brotli, deflate). Split rate-limit wait and fetch for optimal concurrency. |
| `parser.rs` | HTML link extraction (scraper CSS selector `a[href]` with OnceLock-cached selectors), URL normalization (remove fragments, tracking params, sort query), sitemap XML parsing. |
| `robots.rs` | robots.txt fetch, parse (texting_robots), and cache (DashMap, 24h TTL, 10K cap with LRU eviction). Rate-limited through the same governor limiter as page fetches. |
| `dedup.rs` | Lock-free AtomicBloomFilter (1B items, 0.1% FP, ~1.7 GB). Uses `fastbloom` with atomic operations, zero lock contention across 800 workers. Batch URL+domain checking in single pass. |
| `store.rs` | SQLite WAL persistence. Tables: `urls`, `fetched`, `metrics_snapshot`. Batch inserts via dedicated writer task. Checkpoints saved to files (frontier, bloom, backoff) to avoid SQLite blob limits. |
| `monitor.rs` | Lock-free atomic counters for throughput. Ring buffer latency tracker with p50/p95/p99 percentiles. Publishes latest snapshot for scaler consumption. Threshold alerting with 10-min cooldown. |

## URL Priority Scoring

URLs are scored and placed in per-domain MinMaxHeaps. Higher score = fetched sooner.

```
score = (MAX_DEPTH - depth) * 10.0
      + (50.0 if new domain)
      + (30.0 if from sitemap)
```

The frontier pops the highest-score URL via round-robin across domains, ensuring no single domain monopolizes throughput. When a domain queue reaches its cap (5 URLs), new URLs replace the lowest-score entry if they score higher, maintaining frontier quality without batch eviction pauses.

## Persistence and Resume

Every 5 minutes, the frontier, bloom filter, and domain backoff state are serialized and saved to files (`web_weave.frontier`, `web_weave.bloom`, `web_weave.backoff`). Metrics counters (discovered, fetched) are restored from the latest `metrics_snapshot` row. On restart with `RESUME=true`, the latest checkpoint is loaded and crawling continues from where it left off.

## Dynamic Worker and Concurrency Scaling

The scaler adjusts every 30 seconds based on live metrics:

**Worker count:**
```
target_workers = (active_domains - backed_off_domains) * 0.5
               , clamped to [10, 800]
               , halved if success rate < 50%
```

**Fetch concurrency limit** (replaces fixed semaphore):
- Starts at 1000 concurrent fetches
- Increases by 100 if success rate > 80% and p50 latency < 3s
- Decreases by 100 if success rate < 50% or p50 latency > 5s
- Clamped to [100, 1000]

Workers beyond the target sleep instead of polling the frontier.

## Error Handling

Domains are backed off when they show persistent failures:

- **Consecutive failures**: 3+ consecutive errors triggers exponential backoff (30s base, doubles each time, max 1h).
- **Failure rate**: If a domain's failure rate exceeds 50% over 5+ attempts, backoff triggers.
- **Progressive**: Each time a domain is backed off, the duration increases. Domains that repeatedly fail after recovery get progressively longer cooldowns.
- **Persistent**: Backoff state is saved to checkpoint files and restored on resume, preventing re-fetching of known-bad domains.

URLs from backed-off domains are re-enqueued (not lost) and retried after the backoff expires. Backoff entries are capped at 500K and expired entries are cleaned every minute.

## Memory Management

| Component | Bound |
|-----------|-------|
| Bloom filter | ~1.7 GB fixed (1B items, lock-free) |
| Frontier | 5M URLs (~500 MB), smart admission control |
| Robots cache | 10K entries, LRU eviction of oldest 25% |
| Connection pool | 1 idle connection per host, 15s timeout |
| Response bodies | 2 MB max per page, up to 1000 concurrent fetches |
| Rate limiter | Stale keys cleaned every minute (`retain_recent`) |
| Backoff state | 500K max entries, expired entries cleaned every minute |

## Monitoring

Every 60 seconds, the monitor logs:

```
METRICS: [60s] discovered=+176K fetched=+7.6K | [total] discovered=11M fetched=345K
         domains=507K qps=128.0 success=75.4%
         latency p50=748ms p95=4386ms p99=6704ms errors=+1885 elapsed=2760s
         | workers=800 backed_off=0 frontier_size=1105632 robots_cache=8242
```

Alerts fire when:
- QPS drops below 10 (after 100+ fetches)
- Success rate drops below 50%
- p99 latency exceeds 30 seconds

## Dependencies

| Crate | Purpose |
|-------|---------|
| tokio | Async runtime |
| reqwest (rustls-tls) | HTTP client with compression |
| scraper | HTML link extraction |
| texting_robots | robots.txt parsing |
| fastbloom | Lock-free atomic bloom filter for URL deduplication |
| rusqlite (bundled) | SQLite persistence |
| governor | Per-domain rate limiting |
| dashmap | Concurrent robots cache |
| min-max-heap | Double-ended priority queue for smart frontier admission |
| tracing + tracing-appender | Structured logging (console + rolling file) |
| serde + bincode | Checkpoint serialization |
