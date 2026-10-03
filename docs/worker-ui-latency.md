# Worker UI latency: first remediation

## Scope

This change targets ordinary interactive requests, not EPUB/image generation:
`/api/list`, `/api/global_setting`, `/api/tag_list`, `/api/queue/status`,
`/api/get_queue_size`, and `/api/get_pending_tasks`.

The baseline is develop commit `af60d395da1dd66705abce31b11e711dce326d7e`.
There is no production latency, memory, or CPU measurement in this change.

## Changes

- D1-only composition replaces `WorkerRuntime::build_ui` on these paths. It
  does not load S3 configuration, login credentials, HTTP/relay settings or
  downloader/rate-limiter/queue services. The original authentication check
  still runs first. Full infrastructure validation remains in readiness and
  in routes that actually use those dependencies.
- Global settings continue to use `SettingsService` and `D1SettingsStore` for
  reads and writes, including the existing validation and cache invalidation.
- Library lists continue to use `LibraryService::list`, preserving search,
  sort, pagination, frozen/new markers, and the JSON payload. The legacy
  `all=true` behavior is deliberately not changed by this performance fix.
- Tags are aggregated from `novel_tags` in SQL rather than deserializing every
  full novel. Frequency order is preserved; ties, previously dependent on a
  HashMap iteration order, now use deterministic binary tag order. Orphan tags
  are excluded by a join to `novels`. Existing HTML escaping, color allocation
  and empty-list fallback behavior remain.
- Queue status uses one D1 batch containing a grouped count and at most one
  running job (needed only for the single-job label). Results are bounded to
  at most 14 count rows plus one job regardless of the pending queue size.
  This bounds data returned to the Worker, NOT SQLite's rows scanned.
- Queue size returns at most two lane counts. Detailed pending-task listings
  still return their actual rows as required by the API, without unrelated
  runtime initialization.
- Successful list/tag/queue-status/queue-size/global-settings GET responses add
  a `Server-Timing` metric. It measures post-authentication handler wall time,
  including I/O, and is neither CPU time nor D1 SQL execution time. No SQL,
  titles, cookies, tokens or other request data are placed in this header.

No new response cache, schema migration, dependency, secret, storage-backend
switch or deployment is introduced. S3/Queue misconfiguration no longer makes
D1-only metadata unavailable; that dependency isolation is intentional.

## Regression checks

```sh
python -m unittest discover -s worker_entry/tests -p 'test_ui_*.py' -v
node --test worker_entry/tests/ui_latency.test.mjs
cargo check --locked -p narou_worker --target wasm32-unknown-unknown
cd worker_entry && node tests/run.mjs --release
```

The Python tests execute SQL extracted from the production Rust source against
an in-memory SQLite schema covering the referenced columns. They test all
status/lane combinations, empty data, tag ordering/case/Unicode, orphan tags,
one/multiple running jobs, and a 10,000-job pending queue. They do not compile
Rust or emulate D1. The Node tests validate the measurement client, not Worker
behavior. The dedicated PR workflow also type-checks the wasm target and runs
the existing local-workerd contract suite, without deployment credentials and
regardless of the SORAHOST deployment switch.

## Before/after measurement

Use the same client location, dataset, Cloudflare configuration and activity
level on both revisions. Obtain production credentials through normal secret
handling, not command-line arguments or checked-in files.

```sh
# NAROU_ADMIN_TOKEN is read from the environment.
# For Access, set both CF_ACCESS_CLIENT_ID and CF_ACCESS_CLIENT_SECRET too.
node worker_entry/tests/ui_latency.mjs --base https://YOUR-WORKER \
  --samples 10 --output before.json
# Repeat after deploying the candidate to an explicitly selected test target.
node worker_entry/tests/ui_latency.mjs --base https://YOUR-TEST-WORKER \
  --samples 10 --output after.json
```

Only sequential GETs are sent. There is no job enqueue, settings POST or
concurrency load test. These are normal endpoint accesses, including any
existing lazy tag-color initialization. The output records timing and response
size, not response bodies or credentials. Existing output files are not
replaced. Redirects and unexpected JSON shapes are errors, preventing an
Access/login HTML page from being mistaken for a fast successful API call.

`headers_ms` measures the client time to receive headers, and `total_ms` includes
reading the response body. First requests are reported separately from repeated
requests; neither is guaranteed to use a cold or warm isolate. P95 from only ten
samples is noisy. `--pause-ms 31000` can help compare requests beyond the existing
30-second cache TTL, but does not force isolate eviction. Avoid mixing idle and
active-job traffic in one comparison.

Correlate results with Workers CPU/wall time and errors. D1 `meta.duration` /
`meta.timings.sql_duration_ms` exclude network time. If latency remains high,
inspect authentication/settings waits, D1 query duration, rows read, attempts,
served region and primary/replica status. Workers' clocks advance at I/O
boundaries; do not use `Date.now()` alone to benchmark synchronous Wasm code.

References:
- https://developers.cloudflare.com/d1/worker-api/d1-database/
- https://developers.cloudflare.com/d1/worker-api/return-object/
- https://developers.cloudflare.com/workers/runtime-apis/performance/

## Not addressed here

Full-text/EPUB streaming, animation memory, D1 object-store keyset pagination,
full-record library projections, duplicate unfiltered counts, frontend request
coalescing, bookmarks between requests, and heavy-job isolation remain separate
work. No reduction in production milliseconds or memory is claimed until
measured. Merge/deployment should follow successful wasm and contract checks.
