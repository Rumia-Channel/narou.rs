//! `PushHub` Durable Object — Web UI へのライブイベント配信層。
//!
//! native (`src/web/push.rs`) の `PushServer` に相当する Worker 側の
//! ブロードキャスト基盤。単一インスタンス (`INSTANCE_NAME`) が全 WebSocket
//! 接続を持ち、ジョブ実行側から POST されたイベントを全接続へ転送する。
//!
//! 設計:
//! - 表示用の一時状態なので **永続化しない**。直近 [`HISTORY_CAPACITY`]
//!   件をメモリ上のリングバッファに持つだけ (`ctx.storage()` は使わない)。
//!   DO が再起動して履歴が消えても、UI は再接続して追従する。
//! - WebSocket は hibernation (`state.accept_web_socket`) で管理する。
//!   接続一覧は DO が保持し、送信時に `state.get_websockets()` で取るので
//!   こちらでソケット簿記を自前実装しない。
//! - 新規接続には受け入れ直後に履歴を順に送ってから live イベントへ
//!   切り替える。DO は単一スレッドで fetch を直列化するため、accept 中に
//!   別の broadcast が割り込んで履歴と live の間に割り込むことはない。
//!
//! ワイヤー形式は native の `PushServer::publish_json` と同じ
//! `{ "type": ..., ... }` の JSON 1 通。`main.js` の `handleWsMessage`
//! が解釈する `type` だけを流す。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use narou_rs::application::messages;
use narou_rs::application::push_events;
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
/// 新規接続へリプレイする履歴の最大件数。native の history より小さい —
/// コンソール表示の文脈が復元できれば足りる。
const HISTORY_CAPACITY: usize = 200;
/// 1 POST で受け付けるイベント数の上限 (高々数件を想定した安全弁)。
const MAX_EVENTS_PER_POST: usize = 64;
/// `HubProgress` が `progressbar.step` を送る percent 刻み。1 ジョブあたり
/// 高々 4 通の step POST に抑え、DO subrequest を食い潰さない。
const STEP_PERCENT_INCREMENT: u64 = 25;
/// ジョブ終端 (`PushHubSink::drain`) で送信タスクの完了を待つときの
/// ポーリング間隔 (ms) と上限回数。送信タスクは空になったら即座に終了する
/// ので通常は 1〜2 回で抜ける (間引き中に積まれた分を送る場合だけ延びる)。
const DRAIN_POLL_MS: u64 = 10;
const DRAIN_WAIT_STEPS: usize = 50;

/// 履歴リプレイ対象のイベント種別。native の `history_type_replayable`
/// (`src/web/push.rs`) と同じ集合 — `table.reload` 等の制御イベントは
/// 接続直後に再生されないよう履歴に入れない。
fn history_replayable(message_type: &str) -> bool {
    matches!(
        message_type,
        "echo"
            | "error"
            | "log"
            | "progress"
            | "progressbar.init"
            | "progressbar.step"
            | "progressbar.clear"
    )
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

/// ジョブ終端イベントを native のジョブループ (`src/web/worker.rs`) と
/// 同じ順序で送る。`events` に outcome 固有のイベント (queue_complete /
/// queue_failed 等) を並べ、native が毎回送るトレーラ
/// (スコープ単位の progressbar.clear → table.reload → tag.updateCanvas →
/// notification.queue) をこちらで継ぎ足す。
pub(crate) async fn broadcast_terminal_events(
    push: &PushHubClient,
    job_id: &narou_rs::application::JobId,
    events: &[Value],
) {
    let job_id = job_id.as_str();
    let mut batch: Vec<Value> = Vec::with_capacity(events.len() + 5);
    batch.extend_from_slice(events);
    // clear_progress_for_job 相当 — 両コンソールの job スコープのバーを消す。
    batch.push(progressbar_scope_clear(job_id, "stdout"));
    batch.push(progressbar_scope_clear(job_id, "stdout2"));
    batch.push(push_events::table_reload());
    batch.push(push_events::tag_update_canvas());
    batch.push(notification_queue());
    push.broadcast_best_effort(&batch).await;
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
/// Worker は単一スレッドで jobs を直列実行するため `Mutex` が競合しない
/// (`MessageSink: Send + Sync` への適合のために `RefCell` ではなく `Mutex`)。
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

    /// sink を作り、深い呼び出し経路向けの既定 sink
    /// (`narou_rs::application::messages::emit_default` が読む共有スロット) にも
    /// 登録して返す。download / convert いずれのジョブ境界でも同じ手順。
    pub fn install(client: PushHubClient) -> std::sync::Arc<Self> {
        Self::install_with_console(client, None)
    }

    /// `stdout` 行の宛先コンソールを上書きして sink を作る。`None` は既定
    /// (`stdout` / `stdout2` を `Stream` で分ける)。`emit` は
    /// `Stream::target_console_with` に委譲する。
    pub fn install_with_console(
        client: PushHubClient,
        console: Option<&'static str>,
    ) -> std::sync::Arc<Self> {
        let sink = std::sync::Arc::new(Self::new(client, console));
        narou_rs::application::messages::set_default_sink(sink.clone());
        sink
    }

    /// 積まれた行を送るタスクを起動する (既に動いていれば何もしない)。
    fn spawn_sender(&self) {
        let client = self.client.clone();
        let queue = Arc::clone(&self.queue);
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                while let Some(events) = queue.take() {
                    client.broadcast_best_effort(&events).await;
                    queue.mark_sent(now_ms());
                    // まだ積まれているときだけ間隔を空ける (サブリクエスト保護)。
                    // 空なら即座にループを抜けるので、完了時の遅延は増えない。
                    if queue.is_pending() {
                        let wait = queue.wait_ms(now_ms());
                        if wait > 0 {
                            worker::Delay::from(std::time::Duration::from_millis(wait)).await;
                        }
                    }
                }
                // idle に戻す直前に積まれた行は、自分でもう一周して送る。
                if !queue.finish() {
                    break;
                }
            }
        });
    }

    /// 積まれた行を PushHub へ送る (best-effort)。ジョブ実行の区切りごとに
    /// 呼び、上限を超える場合は複数 POST に分ける。
    ///
    /// 送信タスクが動いている間はそれが捌くのを待つ: ジョブの終端イベントは
    /// `drain()` の後に送られるため、ここで待たないと最後の行が「完了」より
    /// 後に届きうる。待ち切れなかった分はこの場で送る (POST は発行順に届く)。
    pub async fn drain(&self) {
        for _ in 0..DRAIN_WAIT_STEPS {
            if !self.queue.is_sending() {
                break;
            }
            worker::Delay::from(std::time::Duration::from_millis(DRAIN_POLL_MS)).await;
        }
        if let Some(events) = self.queue.take() {
            self.client.broadcast_best_effort(&events).await;
        }
    }
}

impl narou_rs::application::messages::MessageSink for PushHubSink {
    fn emit(&self, stream: narou_rs::application::messages::Stream, text: &str) {
        if self
            .queue
            .push(echo(text, stream.target_console_with(self.console)))
        {
            self.spawn_sender();
        }
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

/// Worker 内で動く [`ProgressReporter`]。native の `WebProgress`
/// (`src/progress.rs`) と同じイベント列を吐く:
/// `progressbar.init` → `progressbar.step` → `progressbar.clear`。
///
/// trait のメソッドは同期 API なので送信は `spawn_local` の
/// fire-and-forget。順序が逆転しても progressbar は後勝ちで収束する。
/// `step` は [`STEP_PERCENT_INCREMENT`]`%` 境界だけ送る。
pub(crate) struct HubProgress {
    client: PushHubClient,
    topic: String,
    scope: String,
    length: AtomicU64,
    position: AtomicU64,
    last_step_bucket: AtomicU64,
    cleared: AtomicBool,
}

impl HubProgress {
    /// 生成と同時に `progressbar.init` を送る (native `WebProgress::new` 相当)。
    /// `scope` は job id — `progressbar_scope_clear` と対応する。
    pub(crate) fn new(client: PushHubClient, topic: impl Into<String>, scope: impl Into<String>) -> Self {
        let progress = Self {
            client,
            topic: topic.into(),
            scope: scope.into(),
            length: AtomicU64::new(0),
            position: AtomicU64::new(0),
            last_step_bucket: AtomicU64::new(0),
            cleared: AtomicBool::new(false),
        };
        progress.publish([push_events::progressbar_init_scoped(
            &progress.topic,
            &progress.scope,
        )]);
        progress
    }

    /// 同期コンテキストからの fire-and-forget 送信。
    fn publish(&self, events: impl IntoIterator<Item = Value>) {
        let client = self.client.clone();
        let events: Vec<Value> = events.into_iter().collect();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(error) = client.broadcast(&events).await {
                console_log!("push hub broadcast failed: {error}");
            }
        });
    }

    fn emit_step(&self) {
        let len = self.length.load(Ordering::Relaxed);
        if len == 0 {
            return;
        }
        let pos = self.position.load(Ordering::Relaxed).min(len);
        let bucket = pos.saturating_mul(100) / len / STEP_PERCENT_INCREMENT;
        let previous = self.last_step_bucket.load(Ordering::Relaxed);
        if bucket <= previous
            || self
                .last_step_bucket
                .compare_exchange(previous, bucket, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        let percent = (pos as f64 / len as f64) * 100.0;
        self.publish([push_events::progressbar_step_scoped(
            pos,
            len,
            percent,
            &self.topic,
            &self.scope,
        )]);
    }

    fn clear_event(&self) -> Value {
        push_events::progressbar_clear_scoped(&self.topic, &self.scope)
    }
}

impl ProgressReporter for HubProgress {
    fn set_length(&self, len: u64) {
        self.length.store(len, Ordering::Relaxed);
    }

    fn set_position(&self, pos: u64) {
        self.position.store(pos, Ordering::Relaxed);
        self.emit_step();
    }

    fn inc(&self, delta: u64) {
        self.position.fetch_add(delta, Ordering::Relaxed);
        self.emit_step();
    }

    fn set_message(&self, _msg: &str) {
        // native の WebProgress と同じく、バーにメッセージは出さない。
    }

    fn finish_with_message(&self, _msg: &str) {
        // native の WebProgress と同じく完了メッセージ自体はコンソールへ出さず、
        // `progressbar.clear` だけ送る。clear は Drop でも再送されないよう
        // フラグで抑止する (native は毎回送るが等価)。
        if !self.cleared.swap(true, Ordering::AcqRel) {
            self.publish([self.clear_event()]);
        }
    }

    fn println(&self, msg: &str) {
        // Worker では stdout が届かないので trace へ (report_line と同じ扱い)。
        console_log!("{msg}");
    }
}

impl Drop for HubProgress {
    fn drop(&mut self) {
        if !self.cleared.swap(true, Ordering::AcqRel) {
            self.publish([self.clear_event()]);
        }
    }
}

#[durable_object]
pub struct PushHub {
    state: State,
    /// 直近 [`HISTORY_CAPACITY`] 件の生 JSON payload。
    /// インメモリのみ — DO の再起動で消えてよい (接続は維持される)。
    history: RefCell<VecDeque<String>>,
}

impl PushHub {
    /// `GET` (upgrade) — WebSocket 接続を受け入れ、履歴をリプレイしてから
    /// hibernation 管理へ渡す。
    fn accept_client(&self) -> Result<Response> {
        let pair = WebSocketPair::new()?;
        let server = pair.server;

        // accept (= send 可能にする + 接続簿記へ登録) は先。登録から履歴送信
        // 完了まで await が無いので、live イベントが履歴の途中に割り込まない。
        self.state.accept_web_socket(&server);
        {
            let history = self.history.borrow();
            for payload in history.iter() {
                if let Err(error) = server.send_with_str(payload) {
                    console_warn!("push hub: history replay failed: {error}");
                    let _ = server.close(Some(1011), Some("internal error"));
                    return Response::from_websocket(pair.client);
                }
            }
        }
        Response::from_websocket(pair.client)
    }

    /// `POST` — `{"events": [ <event>, ... ]}` を全接続へ転送する。
    /// replayable なものは先に履歴へ積んでから送る (native と同じ順序)。
    async fn broadcast(&self, req: &mut Request) -> Result<Response> {
        let body: Value = req.json().await?;
        let Some(events) = body.get("events").and_then(Value::as_array) else {
            return Response::error("expected {\"events\": [...]}", 400);
        };
        if events.len() > MAX_EVENTS_PER_POST {
            return Response::error("too many events", 400);
        }

        let sockets = self.state.get_websockets();
        let mut delivered = 0usize;
        for event in events {
            let Some(message_type) = event.get("type").and_then(Value::as_str) else {
                return Response::error("event is missing a string \"type\"", 400);
            };
            let payload = serde_json::to_string(event)
                .map_err(|error| Error::RustError(format!("push event serialize: {error}")))?;
            if history_replayable(message_type) {
                let mut history = self.history.borrow_mut();
                if history.len() >= HISTORY_CAPACITY {
                    history.pop_front();
                }
                history.push_back(payload.clone());
            }
            for socket in &sockets {
                if let Err(_error) = socket.send_with_str(&payload) {
                    // 切断済みソケットは hibernation が掃除するまで残る
                    // ことがあるので、念のためこちらでも閉じる。
                    let _ = socket.close::<String>(None, None);
                } else {
                    delivered += 1;
                }
            }
        }
        Response::from_json(&json!({ "ok": true, "delivered": delivered }))
    }
}

impl DurableObject for PushHub {
    fn new(state: State, _env: Env) -> Self {
        Self {
            state,
            history: RefCell::new(VecDeque::new()),
        }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        match (req.method(), req.path().as_str()) {
            (Method::Get, _) => {
                let upgrade = req.headers().get("upgrade")?;
                if upgrade
                    .as_deref()
                    .is_none_or(|value| !value.eq_ignore_ascii_case("websocket"))
                {
                    return Response::error("expected a WebSocket upgrade request", 400);
                }
                self.accept_client()
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
