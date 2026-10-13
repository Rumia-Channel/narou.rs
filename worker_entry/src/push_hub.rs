//! `PushHub` Durable Object — Web UI へのライブイベント配信層。
//!
//! native (`src/web/push.rs`) の `PushServer` に相当する Worker 側の
//! ブロードキャスト基盤。単一インスタンス (`INSTANCE_NAME`) が全 WebSocket
//! 接続を持ち、ジョブ実行側から POST されたイベントを全接続へ転送する。
//!
//! 設計:
//! - コンソール履歴を [`HISTORY_CAPACITY`] 件・[`HISTORY_MAX_BYTES`] バイトまで
//!   DO storage に保持する。チャンク (1 broadcast POST 分) 単位で書き、
//!   起動後の初回アクセスで遅延ロードする。
//! - 実行中の進捗は履歴とは別に最新状態を永続化する。長いログで進捗が
//!   押し出されても、再接続・hibernation 後に各操作の状態を復元できる。
//! - WebSocket は hibernation (`state.accept_web_socket`) で管理する。
//!   接続一覧は DO が保持し、送信時に `state.get_websockets()` で取るので
//!   こちらでソケット簿記を自前実装しない。
//! - 新規接続には受け入れ直後に履歴を順に送ってから live イベントへ
//!   切り替える。履歴のロードは accept より前 (storage 操作中は DO の
//!   入力ゲートが他のイベントを止める) に済ませ、ロード後は await せずに
//!   accept + replay するので、履歴と live の間に broadcast が割り込まない。
//!
//! 内部ルート (stub 経由でのみ到達、外側のルーターには出さない):
//! - `POST /broadcast` — ジョブ実行側からのイベント配信 + 履歴への追記。
//! - `GET /history?stream=stdout|stdout2` — 外側 `/api/history` 用の
//!   履歴テキスト。native `PushServer::get_history_for` と同じ濾過・連結。
//! - `POST /clear` — 外側 `/api/clear_history` 用。永続チャンクとメモリを
//!   同時に空にし、再接続でも復活させない。
//!
//! ワイヤー形式は native の `PushServer::publish_json` と同じ
//! `{ "type": ..., ... }` の JSON 1 通。`main.js` の `handleWsMessage`
//! が解釈する `type` だけを流す。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::Arc;

use narou_rs::application::messages;
use narou_rs::application::push_events;
use narou_rs::application::webui::mutations::{MutationNotice, mutation_events};
use narou_rs::platform::ProgressReporter;
use serde_json::{json, Value};
use wasm_bindgen::JsValue;
use worker::*;

/// wrangler の Durable Object binding 名。
pub const BINDING: &str = "PUSH_HUB";
/// 全接続を束ねる単一インスタンス名。`/ws` とジョブ実行側が同じ名前を引く。
const INSTANCE_NAME: &str = "global";
/// ジョブ実行側 → DO の内部エンドポイント (stub 経由でのみ到達する)。
const BROADCAST_URL: &str = "https://push-hub.local/broadcast";
/// 外側 `/api/history` → DO の内部エンドポイント (stub 専用)。
const HISTORY_URL: &str = "https://push-hub.local/history";
/// 外側 `/api/clear_history` → DO の内部エンドポイント (stub 専用)。
const CLEAR_URL: &str = "https://push-hub.local/clear";
/// 新規接続へリプレイし `/api/history` が返す履歴の最大件数。native の
/// history より小さい — コンソール表示の文脈が復元できれば足りる。UI の
/// 能力表示 (`/api/webui/config`) と DO が同じ定数を共有する。
pub const HISTORY_CAPACITY: usize = 200;
/// 履歴のバイト上限。イベント数と両方で古い方から落とす (長い行ばかりでも
/// DO のメモリ / storage を圧迫しない)。
pub const HISTORY_MAX_BYTES: usize = 256 * 1024;
/// 履歴チャンクの storage キー接頭辞。キーは `h:{seq:016}`。
const CHUNK_KEY_PREFIX: &str = "h:";
const PROGRESS_KEY: &str = "progress";
/// 1 POST で受け付けるイベント数の上限 (高々数件を想定した安全弁)。
const MAX_EVENTS_PER_POST: usize = 64;
/// Drain waits for the sole sender; it never steals a pending batch and races
/// a terminal event ahead of an in-flight progress update.
const DRAIN_POLL_MS: u64 = 10;

/// 履歴チャンクの storage キー。
fn chunk_key(seq: u64) -> String {
    format!("{CHUNK_KEY_PREFIX}{seq:016}")
}

/// storage キーからチャンクの通し番号を読む。
fn chunk_seq(key: &str) -> Option<u64> {
    key.strip_prefix(CHUNK_KEY_PREFIX)?.parse().ok()
}

/// 永続化済み履歴の 1 チャンク = 1 回の broadcast POST に含まれた
/// replayable イベントの payload (生 JSON 文字列)。チャンク単位で書くので、
/// 行を足すたびに全履歴を組み直さない。
struct HistoryChunk {
    seq: u64,
    payloads: Vec<String>,
    bytes: usize,
}

impl HistoryChunk {
    fn entries(&self) -> usize {
        self.payloads.len()
    }
}

/// 再生バッファ。DO の再起動 / hibernation で消えないよう storage の
/// チャンク列が真実源で、これはその遅延ロード済みビュー。
struct HistoryState {
    /// 古い → 新しい順。
    chunks: VecDeque<HistoryChunk>,
    next_seq: u64,
    entries: usize,
    bytes: usize,
    progress: push_events::ProgressReplay,
}

impl HistoryState {
    fn new() -> Self {
        Self {
            chunks: VecDeque::new(),
            next_seq: 1,
            entries: 0,
            bytes: 0,
            progress: push_events::ProgressReplay::default(),
        }
    }

    fn push_chunk(&mut self, chunk: HistoryChunk) {
        self.entries += chunk.entries();
        self.bytes += chunk.bytes;
        self.next_seq = chunk.seq + 1;
        self.chunks.push_back(chunk);
    }

    /// 上限を超えた古いチャンクを捨て、捨てた storage キーを返す。
    /// 最新の 1 チャンクは上限を超えても残す (「今の行」を消さない)。
    fn prune(&mut self) -> Vec<String> {
        let mut dropped = Vec::new();
        while (self.entries > HISTORY_CAPACITY || self.bytes > HISTORY_MAX_BYTES)
            && self.chunks.len() > 1
        {
            let chunk = self.chunks.pop_front().expect("len > 1");
            self.entries -= chunk.entries();
            self.bytes -= chunk.bytes;
            dropped.push(chunk_key(chunk.seq));
        }
        dropped
    }
}

/// Console body/stream semantics have the same owner as native history.
fn history_line(payload: &str, stream: &str) -> Option<String> {
    let value: Value = serde_json::from_str(payload).ok()?;
    if push_events::history_target_console(&value)? != stream { return None; }
    push_events::history_body(&value).map(str::to_owned)
}

/// `broadcast_event` 相当の制御イベント (`{"type": name, "data": data}`)。
pub(crate) fn event(name: &str, data: Value) -> Value {
    push_events::event(name, data)
}

/// `broadcast_echo` 相当のコンソール行イベント。
pub(crate) fn echo(body: &str, target_console: &str) -> Value {
    push_events::echo(body, target_console)
}

/// native `clear_progress_for_job` 相当のスコープ単位クリア。
/// (`data.scope` が job id、`target_console` が表示先)
pub(crate) fn progressbar_scope_clear(scope: &str, target_console: &str) -> Value {
    push_events::progressbar_scope_clear(scope, target_console)
}

/// `notification.queue` — キュー表示の再読込トリガ。
pub(crate) fn notification_queue() -> Value {
    push_events::notification_queue()
}

/// `PUSH_HUB` binding からシングルトン stub を引く。
pub(crate) fn hub_stub(env: &Env) -> Result<Stub> {
    let namespace = env.durable_object(BINDING)?;
    namespace.get_by_name(INSTANCE_NAME)
}

/// DO の内部 `GET /history` を引き、`stream` の履歴テキストを返す。
///
/// 失敗を空履歴に潰さない — 呼び出し側 (`webui/read_views.rs`) が機械可読な
/// エラー応答へ変換する。
pub(crate) async fn history_text(env: &Env, stream: &str) -> Result<String> {
    let mut url =
        Url::parse(HISTORY_URL).map_err(|error| Error::RustError(format!("history url: {error}")))?;
    url.query_pairs_mut().append_pair("stream", stream);
    let mut init = RequestInit::new();
    init.with_method(Method::Get);
    let request = Request::new_with_init(url.as_str(), &init)?;
    let stub = hub_stub(env)?;
    let mut response = stub.fetch_with_request(request).await?;
    if response.status_code() >= 400 {
        return Err(Error::RustError(format!(
            "PushHub history returned status {}",
            response.status_code()
        )));
    }
    let body: Value = response.json().await?;
    body.get("history")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Error::RustError("PushHub history response is missing \"history\"".to_string()))
}

/// DO の内部 `POST /clear` を呼び、永続化済みの履歴を消す。
/// 失敗は呼び出し側がエラー応答へ変換する。
pub(crate) async fn clear_remote_history(env: &Env) -> Result<()> {
    let mut init = RequestInit::new();
    init.with_method(Method::Post);
    init.with_body(Some(JsValue::from_str("{}")));
    let request = Request::new_with_init(CLEAR_URL, &init)?;
    let stub = hub_stub(env)?;
    let response = stub.fetch_with_request(request).await?;
    if response.status_code() >= 400 {
        return Err(Error::RustError(format!(
            "PushHub clear returned status {}",
            response.status_code()
        )));
    }
    Ok(())
}

/// ジョブ終端イベントを native のジョブループ (`src/web/worker.rs`) と
/// 同じ順序で送る。`events` に outcome 固有のイベント (queue_complete /
/// queue_failed 等) を並べ、native が毎回送るトレーラ
/// (スコープ単位の progressbar.clear → table.reload → tag.updateCanvas →
/// notification.queue) をこちらで継ぎ足す。
///
/// `table_reload` は `webui.table.reload-timing` のゲート結果
/// ([`consumer::table_reload_after_job`](crate::consumer::table_reload_after_job)
/// 参照)。false でも progressbar.clear (進捗バーの後始末) と
/// notification.queue (キュー表示) は必ず送る — native が table.reload /
/// tag.updateCanvas だけを条件付きで送り、他は無条件なのと同じ。
pub(crate) async fn broadcast_terminal_events(
    push: &PushHubClient,
    job_id: &narou_rs::application::JobId,
    events: &[Value],
    table_reload: bool,
) {
    let job_id = job_id.as_str();
    let mut batch: Vec<Value> = Vec::with_capacity(events.len() + 5);
    batch.extend_from_slice(events);
    // clear_progress_for_job 相当 — 両コンソールの job スコープのバーを消す。
    batch.push(progressbar_scope_clear(job_id, "stdout"));
    batch.push(progressbar_scope_clear(job_id, "stdout2"));
    if table_reload {
        batch.push(push_events::table_reload());
        batch.push(push_events::tag_update_canvas());
    }
    batch.push(notification_queue());
    push.broadcast_best_effort(&batch).await;
}

/// 変更操作のイベント選択 (`application::webui::mutations`) を best-effort で
/// 配信する。native の各ハンドラが `PushServer` へ送るのと同じイベント列。
/// 送信専用に budget を新規確保する (リクエスト/ジョブの budget を消費しない)。
pub(crate) async fn broadcast_mutations(env: &Env, notice: MutationNotice<'_>) {
    let push = PushHubClient::new(env, crate::budget::SubrequestBudget::new());
    push.broadcast_best_effort(&mutation_events(notice)).await;
}


/// ジョブ実行側から [`PushHub`] へイベントを送るクライアント。
///
/// `Env` だけを保持し、送信ごとに binding と stub を引き直す (binding が
/// 無い構成でも生成だけは通る)。送信失敗は [`Self::broadcast_best_effort`]
/// で trace に落とすだけ — 表示用なのでジョブの成否には関与しない。
#[derive(Debug, Clone)]
pub struct PushHubClient {
    env: Env,
    subrequests: crate::budget::SubrequestBudget,
}

impl PushHubClient {
    pub fn new(env: &Env, subrequests: crate::budget::SubrequestBudget) -> Self {
        Self {
            env: env.clone(),
            subrequests,
        }
    }

    /// Send one event batch to the DO. The caller limits each batch to
    /// [`MAX_EVENTS_PER_POST`]; errors are returned for best-effort handling.
    async fn broadcast(&self, events: &[Value]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let body = serde_json::to_string(&json!({ "events": events }))
            .map_err(|error| Error::RustError(format!("push event serialize: {error}")))?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post);
        init.with_body(Some(JsValue::from_str(&body)));
        let request = Request::new_with_init(BROADCAST_URL, &init)?;
        let stub = hub_stub(&self.env)?;
        // DO fetch もサブリクエストに数えられるので、platform の 1,000 件
        // 上限へ届く前に WorkerBudget がジョブを boundary で yield できるよう
        // 共有カウンタへ記録する。
        self.subrequests.record();
        let response = stub.fetch_with_request(request).await?;
        if response.status_code() >= 400 {
            return Err(Error::RustError(format!(
                "PushHub returned status {}",
                response.status_code()
            )));
        }
        Ok(())
    }

    /// Best-effort delivery. Each request stays within the DO's event limit;
    /// a failed batch is logged but does not prevent later batches from sending.
    pub(crate) async fn broadcast_best_effort(&self, events: &[Value]) {
        for batch in event_batches(events) {
            if let Err(error) = self.broadcast(batch).await {
                console_log!("push hub broadcast failed: {error}");
            }
        }
    }
}

fn event_batches<'a>(events: &'a [Value]) -> impl Iterator<Item = &'a [Value]> + 'a {
    events.chunks(MAX_EVENTS_PER_POST)
}

/// ユーザーに見える行を PushHub の `echo` イベントへ流す
/// [`narou_rs::application::messages::MessageSink`] 実装。
///
/// `emit` は同期 API なので、行はバッファへ積んでから送信タスク
/// (`wasm_bindgen_futures::spawn_local`) を起動する。タスクは常に 1 本で、
/// 積まれた行をまとめて 1 POST にしつつ、送信のたびに最小間隔だけ間引く
/// ([`messages::flush::FlushQueue`] が順序・取りこぼし・間隔を管理する)。
///
/// 以前はジョブの区切りでだけ `drain()` していたため、ダウンロードの
/// 1 話ごとの進捗行が完了時にまとめて出ていた。ジョブ境界の `drain()` は
/// 残りの送信を保証する最後の砦として残す。
///
/// Each invocation owns its queue; concurrent events never share a current sink.
/// `target_console` は [`Stream::target_console`](narou_rs::application::messages::Stream::target_console)
/// (native の stdout=`"stdout"` / stderr=`"stdout2"` 対応) に委譲する。
#[derive(Debug)]
pub struct PushHubSink {
    client: PushHubClient,
    queue: Arc<messages::flush::FlushQueue<Value>>,
    /// `stdout` 行の宛先コンソールの上書き。native
    /// `web::worker::console_target_for_job` と同じく、外部通信を持たない
    /// ジョブ (`concurrency` 有効時の convert など) の行を `stdout2` へ流す。
    console: Option<&'static str>,
}

impl PushHubSink {
    fn new(client: PushHubClient, console: Option<&'static str>) -> Self {
        Self {
            client,
            queue: Arc::new(messages::flush::FlushQueue::new(
                messages::flush::DEFAULT_MIN_INTERVAL_MS,
            )),
            console,
        }
    }

    /// Build an invocation-owned sink without mutating isolate-global routing.
    pub fn shared(client: PushHubClient, console: Option<&'static str>) -> Arc<Self> {
        Arc::new(Self::new(client, console))
    }

    pub(crate) fn publish(&self, mut event: Value) {
        if event["type"].as_str().is_some_and(|kind| kind.starts_with("progressbar.")) {
            event["target_console"] = Value::from(self.console.unwrap_or("stdout"));
        }
        if self.queue.push(event) { self.spawn_sender(); }
    }

    /// Console lines and progress share one ordered, batched sender.
    fn spawn_sender(&self) {
        let client = self.client.clone();
        let queue = Arc::clone(&self.queue);
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                let wait = queue.wait_ms(now_ms());
                if wait > 0 {
                    worker::Delay::from(std::time::Duration::from_millis(wait)).await;
                }
                if let Some(events) = queue.take() {
                    client.broadcast_best_effort(&events).await;
                    queue.mark_sent(now_ms());
                }
                if !queue.finish() { break; }
            }
        });
    }

    pub async fn drain(&self) {
        while self.queue.is_sending() {
            worker::Delay::from(std::time::Duration::from_millis(DRAIN_POLL_MS)).await;
        }
    }
}

impl narou_rs::application::messages::MessageSink for PushHubSink {
    fn emit(&self, stream: narou_rs::application::messages::Stream, text: &str) {
        self.publish(echo(text, stream.target_console_with(self.console)));
    }
}

/// `js_sys::Date::now()` のミリ秒 (間引きの基準にだけ使う単調ではない時計)。
fn now_ms() -> u64 {
    js_sys::Date::now() as u64
}

#[cfg(test)]
mod tests {
    use super::{event_batches, MAX_EVENTS_PER_POST};
    use serde_json::json;

    #[test]
    fn event_batches_preserve_order_across_the_post_limit() {
        let events: Vec<_> = (0..=MAX_EVENTS_PER_POST)
            .map(|index| json!({ "index": index }))
            .collect();
        let batches: Vec<_> = event_batches(&events).collect();

        assert_eq!(
            batches.iter().map(|batch| batch.len()).collect::<Vec<_>>(),
            vec![MAX_EVENTS_PER_POST, 1]
        );
        assert_eq!(
            batches.into_iter().flatten().cloned().collect::<Vec<_>>(),
            events
        );
    }
}

/// Worker transport for the same progress state used by native WebProgress.
/// Updates share the console sender: no 25% threshold and no POST per step.
pub(crate) struct HubProgress {
    sink: Arc<PushHubSink>,
    state: narou_rs::platform::progress::ProgressState,
}

impl HubProgress {
    pub(crate) fn new(sink: Arc<PushHubSink>, topic: &str, scope: &str, execution_token: &str) -> Self {
        let progress = Self {
            sink,
            state: narou_rs::platform::progress::ProgressState::new(topic, scope, execution_token),
        };
        progress.state.init(|phase, identity, snapshot| {
            progress.sink.publish(push_events::progress_update(phase, identity, snapshot));
        });
        progress
    }
}

impl ProgressReporter for HubProgress {
    fn set_length(&self, len: u64) {
        self.state.set_length(len, |phase, identity, snapshot| {
            self.sink.publish(push_events::progress_update(phase, identity, snapshot));
        });
    }

    fn set_position(&self, pos: u64) {
        self.state.set_position(pos, |phase, identity, snapshot| {
            self.sink.publish(push_events::progress_update(phase, identity, snapshot));
        });
    }

    fn inc(&self, delta: u64) {
        self.state.inc(delta, |phase, identity, snapshot| {
            self.sink.publish(push_events::progress_update(phase, identity, snapshot));
        });
    }

    fn set_message(&self, msg: &str) {
        self.state.set_message(msg, |phase, identity, snapshot| {
            self.sink.publish(push_events::progress_update(phase, identity, snapshot));
        });
    }

    fn finish_with_message(&self, _msg: &str) {
        self.state.finish(|phase, identity, snapshot| {
            self.sink.publish(push_events::progress_update(phase, identity, snapshot));
        });
    }

    fn println(&self, msg: &str) {
        use narou_rs::application::messages::{MessageSink, Stream};
        self.sink.emit(Stream::Stdout, msg);
    }
}

impl Drop for HubProgress {
    fn drop(&mut self) { self.finish_with_message(""); }
}

#[durable_object]
pub struct PushHub {
    state: State,
    /// 直近 [`HISTORY_CAPACITY`] 件・[`HISTORY_MAX_BYTES`] バイトの再生バッファ。
    /// `None` は「未ロード」— 初回アクセスで storage から遅延ロードする
    /// (hibernation / DO 再起動後も履歴が戻る)。
    history: RefCell<Option<HistoryState>>,
}

impl PushHub {
    /// 履歴が未ロードなら storage から読む。チャンク列は storage が真実源で、
    /// ここでは新しい順に必要数だけ読んで古い順に組み直す。
    async fn ensure_history_loaded(&self) -> Result<()> {
        if self.history.borrow().is_some() {
            return Ok(());
        }
        let storage = self.state.storage();
        // 1 チャンクは最小 1 イベントなので、上限 +1 を超えるチャンク数は
        // 通常運用では存在しない (append 側が都度 prune する)。
        let listed = storage
            .list_with_options(
                ListOptions::new()
                    .prefix(CHUNK_KEY_PREFIX)
                    .reverse(true)
                    .limit(HISTORY_CAPACITY + 1),
            )
            .await?;
        let mut loaded: Vec<(u64, Vec<String>)> = Vec::new();
        listed.for_each(&mut |value: JsValue, key: JsValue| {
            let Some(key) = key.as_string() else {
                return;
            };
            let Some(seq) = chunk_seq(&key) else {
                console_warn!("push hub: ignoring unexpected history key {key}");
                return;
            };
            let Some(raw) = value.as_string() else {
                console_warn!("push hub: history chunk {key} is not text");
                return;
            };
            match serde_json::from_str::<Vec<String>>(&raw) {
                Ok(payloads) => loaded.push((seq, payloads)),
                // 壊れたチャンクは読み飛ばす (履歴全体を空に落とさない)。
                Err(error) => console_warn!("push hub: history chunk {key} unreadable: {error}"),
            }
        });
        loaded.sort_by_key(|(seq, _)| *seq);
        let mut state = HistoryState::new();
        let persisted_progress = storage.get::<String>(PROGRESS_KEY).await?;
        let has_persisted_progress = persisted_progress.is_some();
        if let Some(raw) = persisted_progress {
            state.progress = serde_json::from_str(&raw)
                .map_err(|error| Error::RustError(format!("progress history decode: {error}")))?;
        }
        for (seq, payloads) in loaded {
            let payloads: Vec<String> = payloads.into_iter().filter(|payload| {
                let Ok(value) = serde_json::from_str::<Value>(payload) else { return false; };
                let replayable = push_events::history_replayable(value["type"].as_str().unwrap_or(""));
                if !has_persisted_progress { state.progress.observe(value); }
                replayable
            }).collect();
            let bytes = payloads.iter().map(String::len).sum();
            state.push_chunk(HistoryChunk {
                seq,
                payloads,
                bytes,
            });
        }
        let dropped = state.prune();
        if !has_persisted_progress {
            storage.put(PROGRESS_KEY, serde_json::to_string(&state.progress)
                .map_err(|error| Error::RustError(format!("progress history encode: {error}")))?).await?;
        }
        *self.history.borrow_mut() = Some(state);
        if !dropped.is_empty() {
            // 上限を超えた古いチャンクは storage からも消す。失敗しても表示は
            // 継続する (次回ロードで再試行される) ので warn のみ。
            if let Err(error) = storage.delete_multiple(dropped).await {
                console_warn!("push hub: history prune failed: {error}");
            }
        }
        Ok(())
    }

    /// replayable payload を 1 チャンクとして永続化し、上限を超えた古い
    /// チャンクを storage からも消す。空なら何もしない。
    async fn persist_history(&self, replayable: &[String]) -> Result<()> {
        if replayable.is_empty() {
            return Ok(());
        }
        self.ensure_history_loaded().await?;
        let seq = {
            let guard = self.history.borrow();
            guard.as_ref().expect("history loaded").next_seq
        };
        let value = serde_json::to_string(replayable)
            .map_err(|error| Error::RustError(format!("history serialize: {error}")))?;
        let storage = self.state.storage();
        storage
            .put_raw(&chunk_key(seq), JsValue::from_str(&value))
            .await?;
        let bytes = replayable.iter().map(String::len).sum();
        let dropped = {
            let mut guard = self.history.borrow_mut();
            let state = guard.as_mut().expect("history loaded");
            state.push_chunk(HistoryChunk {
                seq,
                payloads: replayable.to_vec(),
                bytes,
            });
            state.prune()
        };
        if !dropped.is_empty() {
            storage.delete_multiple(dropped).await?;
        }
        Ok(())
    }

    /// `GET` (upgrade) — WebSocket 接続を受け入れ、履歴をリプレイしてから
    /// hibernation 管理へ渡す。
    async fn accept_client(&self) -> Result<Response> {
        // ロードは accept より前に済ませる。storage 操作の間は DO の入力
        // ゲートが他のイベントを止め、ロード後は accept + replay まで await
        // しないので、live イベントが履歴の途中に割り込まない。
        self.ensure_history_loaded().await?;
        let pair = WebSocketPair::new()?;
        let server = pair.server;

        // accept (= send 可能にする + 接続簿記へ登録) は先。
        self.state.accept_web_socket(&server);
        {
            let guard = self.history.borrow();
            let state = guard.as_ref().expect("history loaded above");
            for chunk in &state.chunks {
                for payload in &chunk.payloads {
                    if let Err(error) = server.send_with_str(payload) {
                        console_warn!("push hub: history replay failed: {error}");
                        let _ = server.close(Some(1011), Some("internal error"));
                        return Response::from_websocket(pair.client);
                    }
                }
            }
            for event in state.progress.snapshots() {
                server.send_with_str(event.to_string())?;
            }
        }
        Response::from_websocket(pair.client)
    }

    /// `POST` — `{"events": [ <event>, ... ]}` を全接続へ転送する。
    /// replayable なものは先に履歴へ積んでから送る (native と同じ順序)。
    ///
    /// 履歴の永続化が失敗しても配信は続ける (表示を止めない) が、成功を
    /// 偽らず 500 で返して送信側の trace に残す。
    async fn broadcast(&self, req: &mut Request) -> Result<Response> {
        let body: Value = req.json().await?;
        let events = match body {
            Value::Object(mut body) => match body.remove("events") {
                Some(Value::Array(events)) => events,
                _ => return Response::error("expected {\"events\": [...]}", 400),
            },
            _ => return Response::error("expected {\"events\": [...]}", 400),
        };
        if events.len() > MAX_EVENTS_PER_POST {
            return Response::error("too many events", 400);
        }

        // Validate the whole batch before mutating replay state.
        let mut payloads = Vec::with_capacity(events.len());
        for event in &events {
            let Some(message_type) = event.get("type").and_then(Value::as_str) else {
                return Response::error("event is missing a string \"type\"", 400);
            };
            let payload = serde_json::to_string(event)
                .map_err(|error| Error::RustError(format!("push event serialize: {error}")))?;
            payloads.push((push_events::history_replayable(message_type), payload));
        }
        self.ensure_history_loaded().await?;
        let progress_changed = {
            let mut guard = self.history.borrow_mut();
            let state = guard.as_mut().expect("history loaded");
            let mut changed = false;
            for event in events { changed |= state.progress.observe(event); }
            changed
        };
        let replayable: Vec<String> = payloads
            .iter()
            .filter(|(replayable, _)| *replayable)
            .map(|(_, payload)| payload.clone())
            .collect();
        let mut persist_error = self.persist_history(&replayable).await.err();
        if progress_changed {
            let progress_json = {
                let guard = self.history.borrow();
                serde_json::to_string(&guard.as_ref().expect("history loaded").progress)
                    .map_err(|error| Error::RustError(format!("progress history encode: {error}")))?
            };
            if let Err(error) = self.state.storage().put(PROGRESS_KEY, progress_json).await {
                if persist_error.is_none() { persist_error = Some(error); }
            }
        }

        let sockets = self.state.get_websockets();
        let mut delivered = 0usize;
        for (_, payload) in &payloads {
            for socket in &sockets {
                if let Err(_error) = socket.send_with_str(payload) {
                    // 切断済みソケットは hibernation が掃除するまで残る
                    // ことがあるので、念のためこちらでも閉じる。
                    let _ = socket.close::<String>(None, None);
                } else {
                    delivered += 1;
                }
            }
        }
        match persist_error {
            Some(error) => Response::error(format!("history persist failed: {error}"), 500),
            None => Response::from_json(&json!({ "ok": true, "delivered": delivered })),
        }
    }

    /// 内部 `GET /history?stream=stdout|stdout2` — 外側 `/api/history` 専用。
    /// native `PushServer::get_history_for` と同じ濾過 (`body` を持ち、
    /// `target_console` が stream と一致する行を "\n" で連結)。
    async fn history_view(&self, req: &Request) -> Result<Response> {
        let stream = req
            .url()?
            .query_pairs()
            .find(|(key, _)| key == "stream")
            .map(|(_, value)| value.into_owned())
            .unwrap_or_else(|| "stdout".to_string());
        self.ensure_history_loaded().await?;
        let mut lines: Vec<String> = Vec::new();
        {
            let guard = self.history.borrow();
            let state = guard.as_ref().expect("history loaded above");
            for chunk in &state.chunks {
                for payload in &chunk.payloads {
                    if let Some(body) = history_line(payload, &stream) {
                        lines.push(body);
                    }
                }
            }
        }
        Response::from_json(&json!({ "history": lines.join("\n") }))
    }

    /// 内部 `POST /clear` — 外側 `/api/clear_history` 専用。storage 上の
    /// 全チャンクとメモリ上の再生バッファを同時に空にする。消去に失敗したら
    /// 成功を偽らずエラーを返し、メモリも触らない。
    async fn clear_history(&self) -> Result<Response> {
        self.ensure_history_loaded().await?;
        let storage = self.state.storage();
        let listed = storage
            .list_with_options(ListOptions::new().prefix(CHUNK_KEY_PREFIX))
            .await?;
        let mut keys: Vec<String> = Vec::new();
        listed.for_each(&mut |_value: JsValue, key: JsValue| {
            if let Some(key) = key.as_string() {
                keys.push(key);
            }
        });
        if !keys.is_empty() {
            storage.delete_multiple(keys).await?;
        }
        let progress = std::mem::take(&mut self.history.borrow_mut().as_mut().expect("history loaded").progress);
        let mut state = HistoryState::new();
        state.progress = progress;
        *self.history.borrow_mut() = Some(state);
        Response::from_json(&json!({ "ok": true }))
    }
}

impl DurableObject for PushHub {
    fn new(state: State, _env: Env) -> Self {
        Self {
            state,
            history: RefCell::new(None),
        }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        match (req.method(), req.path().as_str()) {
            // 内部ルート (stub 経由のみ)。GET の一般形より先に判定する。
            (Method::Get, "/history") => self.history_view(&req).await,
            (Method::Post, "/clear") => self.clear_history().await,
            (Method::Get, _) => {
                let upgrade = req.headers().get("upgrade")?;
                if upgrade
                    .as_deref()
                    .is_none_or(|value| !value.eq_ignore_ascii_case("websocket"))
                {
                    return Response::error("expected a WebSocket upgrade request", 400);
                }
                self.accept_client().await
            }
            (Method::Post, "/broadcast") => self.broadcast(&mut req).await,
            _ => Response::error("Not Found", 404),
        }
    }

    /// クライアントからの入力は使わない (UI は受信のみ)。
    async fn websocket_message(
        &self,
        _ws: WebSocket,
        _message: WebSocketIncomingMessage,
    ) -> Result<()> {
        Ok(())
    }

    /// hibernation が接続簿記を外したあとに呼ばれる。状態を持たないので
    /// やることは無いが、既定実装 (panic) を潰しておく。
    async fn websocket_close(
        &self,
        _ws: WebSocket,
        _code: usize,
        _reason: String,
        _was_clean: bool,
    ) -> Result<()> {
        Ok(())
    }

    async fn websocket_error(&self, _ws: WebSocket, error: Error) -> Result<()> {
        console_warn!("push hub websocket error: {error}");
        Ok(())
    }
}
