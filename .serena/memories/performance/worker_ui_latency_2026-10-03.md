# Worker UI latency first remediation

Base: develop af60d395da1dd66705abce31b11e711dce326d7e.
Branch: perf-worker-ui-latency. Do not merge/deploy until wasm and contract checks pass.

D1-only metadata composition now serves list, tags, queue reads and global
settings without initializing S3/login/downloader/queue/rate-limit capabilities.
Authentication is unchanged. Settings save still uses SettingsService and the
existing D1SettingsStore invalidation. Readiness retains full validation.

Tag aggregation no longer reads full NovelRecords. Queue status uses one D1
batch, at most 14 aggregate rows + one running job; queue size returns at most
two lane counts. Detailed pending tasks retain their payload and ordering.
The row bounds refer to data returned to Wasm, not SQL scan counts.

Server-Timing and a GET-only latency client support before/after comparison.
SQL regression tests (8) and diagnostic-client tests (8) passed locally.
No Rust toolchain, production D1 credentials, or deployed timing data was
available in the editing environment; local SQL tests are not a wasm build.
A dedicated no-deploy PR workflow performs wasm checking and workerd contracts
regardless of the SORAHOST deployment switch. See docs/worker-ui-latency.md.
