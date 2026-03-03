# web-weave

Single-machine URL crawler in Rust. Starts from 1,000 seed URLs and maximizes URL discovery over a 48-hour crawl window while strictly honoring politeness constraints.

## Results (10-minute sample run)

| Metric | Value |
|--------|-------|
| Total discovered URLs | 3,453,117 |
| Total successfully-crawled URLs | 79,766 |
| Unique domains reached | 145,663 |
| Sustained QPS | ~80 |
| Success rate | 83-89% |
| Avg latency (p50) | ~1,000 ms |
| Memory usage (steady-state) | ~500 MB |

48-hour projection: ~14M fetched, 50-200M discovered.

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
|   Capacity: 500K URLs, evicts lowest-score when full               |
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
     |   4. Acquire fetch semaphore (max 200 concurrent)   |
     |   5. Wait on per-domain rate limiter (0.5 QPS)      |
     |   6. HTTP GET (reqwest, 30s timeout)                |
     |   7. Parse links (scraper) or sitemap XML           |
     |   8. Dedup via bloom filter                         |
     |   9. Score and push new URLs to frontier            |
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
                 |   DB Writer       |   Batches up to 1000 rows
                 |   (SQLite WAL)    |   or flushes every 1s
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
| `frontier.rs` | Per-domain priority queues (BinaryHeap) with round-robin scheduling (VecDeque). Capacity-capped at 500K with eviction of lowest-score URLs. Domain error backoff with progressive exponential backoff. |
| `fetcher.rs` | reqwest HTTP client with per-domain rate limiting via governor (0.5 QPS). Compression support (gzip, brotli, deflate). |
| `parser.rs` | HTML link extraction (scraper CSS selector `a[href]`), URL normalization (remove fragments, tracking params, sort query), sitemap XML parsing. |
| `robots.rs` | robots.txt fetch, parse (texting_robots), and cache (DashMap, 24h TTL, 10K cap with LRU eviction). Rate-limited through the same governor limiter as page fetches. |
| `dedup.rs` | Lock-free AtomicBloomFilter (1B items, 0.1% FP, ~1.7GB). Uses `fastbloom` with atomic operations, zero lock contention across 800 workers. |
| `store.rs` | SQLite WAL persistence. Tables: `urls`, `domains`, `frontier_checkpoint`, `metrics_snapshot`. Batch inserts via dedicated writer task. |
| `monitor.rs` | Lock-free atomic counters for throughput. Ring buffer latency tracker with p50/p95/p99 percentiles. Threshold alerting with macOS notifications and 10-min cooldown. |

## URL Priority Scoring

URLs are scored and placed in per-domain max-heaps. Higher score = fetched sooner.

```
score = (MAX_DEPTH - depth) * 10.0
      + (50.0 if new domain)
      + (30.0 if from sitemap)
```

The frontier pops URLs via round-robin across domains, ensuring no single domain monopolizes throughput. When the frontier exceeds 500K URLs, the largest domain queues are trimmed, keeping high-score URLs and evicting low-score ones.

## Persistence and Resume

Every 5 minutes, the frontier (all domain queues) and bloom filter are serialized via bincode and saved to SQLite as a checkpoint blob. On restart with `RESUME=true`, the latest checkpoint is loaded and crawling continues from where it left off. The last 3 checkpoints are retained.

## Dynamic Worker Scaling

Worker count adjusts every 30 seconds based on active domains:

```
target_workers = (active_domains - backed_off_domains) * 0.5
               , clamped to [10, 500]
```

Workers beyond the target sleep instead of polling the frontier. A concurrency semaphore (200 permits) separately bounds in-flight HTTP requests to control memory from response bodies.

## Error Handling

Domains are backed off when they show persistent failures:

- **Consecutive failures**: 3+ consecutive errors triggers exponential backoff (30s base, doubles each time, max 1h).
- **Failure rate**: If a domain's failure rate exceeds 50% over 5+ attempts, backoff triggers.
- **Progressive**: Each time a domain is backed off, the duration increases. Domains that repeatedly fail after recovery get progressively longer cooldowns.

URLs from backed-off domains are re-enqueued (not lost) and retried after the backoff expires.

## Memory Management

| Component | Bound |
|-----------|-------|
| Frontier | 500K URLs, eviction when exceeded |
| Bloom filter | ~72MB fixed |
| Robots cache | 10K entries, LRU eviction of oldest 25% |
| Connection pool | No idle connections (`pool_max_idle_per_host=0`) |
| Response bodies | 2MB max per page, 200 concurrent fetches |
| Rate limiter | Stale keys cleaned every 5 min (`retain_recent`) |
| Backoff state | Expired entries cleaned every 5 min |

## Monitoring

Every 60 seconds, the monitor logs:

```
METRICS: [60s] discovered=+200K fetched=+5K | [total] discovered=2M fetched=50K
         domains=100K qps=80.0 success=85.0%
         latency p50=1000ms p95=6000ms p99=9000ms errors=+700 elapsed=600s
         | workers=500 backed_off=10 frontier_size=500000 robots_cache=10000
```

Alerts fire (with macOS notification) when:
- QPS drops below 10 (after 100+ fetches)
- Success rate drops below 50%
- p99 latency exceeds 30 seconds

## Build and Run

```bash
# Build
cargo build --release

# Run (default: 48h crawl, 500 workers)
cargo run --release

# Custom configuration via environment
CRAWL_HOURS=1 NUM_WORKERS=200 RESUME=true cargo run --release
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SEED_FILE` | `urls.txt` | Path to seed URL file |
| `DB_PATH` | `web_weave.db` | SQLite database path |
| `NUM_WORKERS` | `500` | Maximum worker count |
| `CRAWL_HOURS` | `48` | Crawl duration in hours |
| `RESUME` | `false` | Resume from last checkpoint |
| `LOG_DIR` | `logs` | Directory for log files |

## Dependencies

| Crate | Purpose |
|-------|---------|
| tokio | Async runtime |
| reqwest (rustls-tls) | HTTP client with compression |
| scraper | HTML link extraction |
| texting_robots | robots.txt parsing |
| bloomfilter | URL deduplication |
| rusqlite (bundled) | SQLite persistence |
| governor | Per-domain rate limiting |
| dashmap | Concurrent robots cache |
| tracing + tracing-appender | Structured logging (console + file) |
| serde + bincode | Checkpoint serialization |
