# Web UI console: shared renderer + history fallback without WebSocket (issue #46, 2026-10-27)

- Problem 1: the live console and the "History" button used different renderers.
  The live path only allowed `<span>` (color / font-weight) and `<ruby>` base text
  through `appendSanitizedConsoleHtml`; the history path pasted the raw line as
  text, so `<span style="color:#bbb">更新を開始します…</span>` showed up as
  literal markup.
- Problem 2: the console only receives live lines over the WebSocket. When the
  handshake failed (proxy that does not forward the upgrade, host missing from
  `server-ws-add-accepted-domains`) *or never completed* (pending handshake, no
  `onopen` / `onclose` at all) the console stayed empty and never updated.
- Fix:
  - `src/web/assets/js/core/console_html.js` holds the single renderer
    (`renderConsoleLine`, `appendSanitizedConsoleHtml`). The live path
    (`src/web/assets/js/main.js`) and the history path
    (`src/web/assets/js/ui/actions.js::replaceConsoleHistory`) both use it.
    The sanitizer rules are unchanged from the previous live-path implementation.
  - `src/web/assets/js/core/console_history.js` polls the history API
    (`GET /api/history?format=json&after=<id>`) every 3 s while the WebSocket is
    unusable. The cursor is kept **per stream** (`stdout` / `stdout2`) and only
    advanced after that stream's response arrived, so a failed request cannot
    skip unseen lines. One request at a time (no overlapping ticks) and a
    generation token invalidated by `stop()` keep a late response from being
    rendered after the WebSocket took over.
  - `src/web/assets/js/core/websocket.js` gives the connection a bounded
    handshake timer (5 s). A stalled handshake switches to the history fallback;
    a successful open stops the fallback (also when the socket was replaced
    after a close, so WS and polling never append at the same time).
  - `src/web/push.rs`: history entries carry an id; `history_since(stream, after)`
    returns the newer lines and the cursor under one lock. `src/web/misc.rs`
    builds the initial JSON snapshot with the same call (`history_since(stream, 0)`)
    so a line published between two reads cannot be skipped.
- Verification: real browser (Firefox) against the built binary - the live
  console and the "History" panel render the same `<span>` as an element, and a
  proxy that stalls the upgrade still fills and updates the console through the
  fallback. Regression tests: `tests/console_history.test.mjs`,
  `tests/console_history_poll.test.mjs`, `tests/websocket_fallback.test.mjs`
  (node) and `src/web/push.rs` (Rust).
