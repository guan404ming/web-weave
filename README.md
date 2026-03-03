# web-weave

Single-machine URL crawler in Rust. Starts from 1,000 seed URLs and maximizes URL discovery over a 48-hour crawl window while strictly honoring politeness constraints.

## Results (30-minute sample run)

| Metric | Value |
|--------|-------|
| Total discovered URLs | 8,135,425 |
| Total successfully-crawled URLs | 222,754 |
| Unique domains reached | 345,827 |
| Sustained QPS | ~128 |
| Success rate | 77-78% |
| Avg latency (p50) | ~550 ms |
| Memory usage (steady-state) | ~2.5 GB |

48-hour projection: ~17M successfully crawled, ~800M discovered, ~3.5M unique domains.

## Hardware Requirements

| Resource | Minimum | Recommended |
|----------|---------|-------------|
| RAM | 4 GB | 8 GB+ |
| CPU | 2 cores | 4+ cores |
| Disk | 10 GB free | 50 GB free |
| Network | Stable broadband | Low-latency connection |

The bloom filter alone uses ~1.7 GB (sized for 1B URLs at 0.1% false positive rate). The frontier can grow up to ~500 MB (5M URLs). SQLite database grows with discovered URLs. Checkpoints (frontier + bloom serialization) require temporary disk space (~2 GB).

## Build and Run

```bash
# Build (requires Rust toolchain)
cargo build --release

# Run with defaults (48h crawl, 800 workers)
cargo run --release

# Custom configuration via environment
CRAWL_HOURS=1 NUM_WORKERS=200 RESUME=true cargo run --release

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
| `RESUME` | `false` | Resume from last checkpoint |
| `LOG_DIR` | `logs` | Directory for log files |

## Output Files

| File | Description |
|------|-------------|
| `web_weave.db` | SQLite database with all discovered/fetched URLs, domain stats, metrics snapshots |
| `web_weave.bloom` | Bloom filter checkpoint (~1.7 GB) for resume support |
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
|   Per-domain BinaryHeap (max-heap by score)                        |
|   Round-robin VecDeque across domains                              |
|   Capacity: 5M URLs, evicts lowest-score when full                 |
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
     |   5. Acquire fetch semaphore (max 500 concurrent)   |
     |   6. HTTP GET (reqwest, 15s timeout)                |
     |   7. Release semaphore immediately after response   |
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
  - Checkpoint loop: serialize frontier + bloom filter every 5 min
  - Monitor loop: log metrics every 60s, cleanup every 5 min
  - Worker scaler: adjust active worker count every 30s
```

## Module Overview

| Module | Responsibility |
|--------|---------------|
| `main.rs` | Orchestrator. Spawns workers, DB writer, checkpoint/monitor/scaler loops. Handles graceful shutdown via ctrl-c or crawl duration. |
| `config.rs` | All tunable constants and environment-based configuration. |
| `frontier.rs` | Per-domain priority queues (BinaryHeap) with round-robin scheduling (VecDeque). Capacity-capped at 5M with batch eviction. Domain error backoff with progressive exponential backoff. |
| `fetcher.rs` | reqwest HTTP client with per-domain rate limiting via governor (0.5 QPS). Compression support (gzip, brotli, deflate). Split rate-limit wait and fetch for optimal semaphore utilization. |
| `parser.rs` | HTML link extraction (scraper CSS selector `a[href]` with OnceLock-cached selectors), URL normalization (remove fragments, tracking params, sort query), sitemap XML parsing. |
| `robots.rs` | robots.txt fetch, parse (texting_robots), and cache (DashMap, 24h TTL, 10K cap with LRU eviction). Rate-limited through the same governor limiter as page fetches. |
| `dedup.rs` | Lock-free AtomicBloomFilter (1B items, 0.1% FP, ~1.7 GB). Uses `fastbloom` with atomic operations, zero lock contention across 800 workers. Batch URL+domain checking in single pass. |
| `store.rs` | SQLite WAL persistence. Tables: `urls`, `domains`, `frontier_checkpoint`, `metrics_snapshot`. Batch inserts via dedicated writer task. Bloom filter saved to separate file (too large for SQLite blob). |
| `monitor.rs` | Lock-free atomic counters for throughput. Ring buffer latency tracker with p50/p95/p99 percentiles. Threshold alerting with 10-min cooldown. |

## URL Priority Scoring

URLs are scored and placed in per-domain max-heaps. Higher score = fetched sooner.

```
score = (MAX_DEPTH - depth) * 10.0
      + (50.0 if new domain)
      + (30.0 if from sitemap)
```

The frontier pops URLs via round-robin across domains, ensuring no single domain monopolizes throughput. When the frontier exceeds 5.5M URLs, the largest domain queues are trimmed, keeping high-score URLs and evicting low-score ones.

## Persistence and Resume

Every 5 minutes, the frontier is serialized via bincode and saved to SQLite, while the bloom filter is saved to a separate file (`web_weave.bloom`). On restart with `RESUME=true`, the latest checkpoint is loaded and crawling continues from where it left off. The last 3 checkpoints are retained.

## Dynamic Worker Scaling

Worker count adjusts every 30 seconds based on active domains:

```
target_workers = (active_domains - backed_off_domains) * 0.5
               , clamped to [10, 800]
```

Workers beyond the target sleep instead of polling the frontier. A concurrency semaphore (500 permits) separately bounds in-flight HTTP requests to control memory from response bodies.

## Error Handling

Domains are backed off when they show persistent failures:

- **Consecutive failures**: 3+ consecutive errors triggers exponential backoff (30s base, doubles each time, max 1h).
- **Failure rate**: If a domain's failure rate exceeds 50% over 5+ attempts, backoff triggers.
- **Progressive**: Each time a domain is backed off, the duration increases. Domains that repeatedly fail after recovery get progressively longer cooldowns.

URLs from backed-off domains are re-enqueued (not lost) and retried after the backoff expires.

## Memory Management

| Component | Bound |
|-----------|-------|
| Bloom filter | ~1.7 GB fixed (1B items, lock-free) |
| Frontier | 5M URLs (~500 MB), batch eviction when exceeded |
| Robots cache | 10K entries, LRU eviction of oldest 25% |
| Connection pool | 1 idle connection per host, 15s timeout |
| Response bodies | 2 MB max per page, 500 concurrent fetches |
| Rate limiter | Stale keys cleaned every 5 min (`retain_recent`) |
| Backoff state | Expired entries cleaned every 5 min |

## Monitoring

Every 60 seconds, the monitor logs:

```
METRICS: [60s] discovered=+280K fetched=+7.5K | [total] discovered=8M fetched=222K
         domains=346K qps=128.0 success=78.0%
         latency p50=550ms p95=4700ms p99=7200ms errors=+1700 elapsed=1740s
         | workers=800 backed_off=1 frontier_size=5000000 robots_cache=9400
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
| tracing + tracing-appender | Structured logging (console + rolling file) |
| serde + bincode | Checkpoint serialization |
