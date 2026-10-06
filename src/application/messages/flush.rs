//! 同期 `emit` を非同期送信へ橋渡しする FIFO バッファ。
//!
//! Worker の Web コンソールは 1 行ごとに PushHub へ POST するが、
//! [`MessageSink::emit`](super::MessageSink::emit) は同期 API なので送信は
//! 起動済みタスクが担う。そのとき守るべき性質をここへ閉じ込める:
//!
//! - **順序**: 送信タスクは常に 1 本。`take` は積まれた順にまとめて返す。
//! - **取りこぼしなし**: 送信が終わって idle に戻す瞬間に積まれた行は、
//!   自分でもう一周するか、その `push` に新しいタスクを起動させる
//!   ([`FlushQueue::finish`] の戻り値が「続けるべきか」を表す)。
//! - **間引き**: 連続 emit で 1 行 1 送信にならないよう、送信直後だけ
//!   最小間隔を空ける ([`FlushQueue::wait_ms`])。最初の 1 通は待たないので
//!   進捗の初動は遅れない。
//!
//! Worker 側 (`worker_entry::push_hub`) は spawn と実際の送信だけを書く。

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// 送信の最小間隔 (ms)。ダウンロードの進捗行は 1 話ごとに届くので、この
/// 程度で十分まとまり、かつ 1 通あたりのサブリクエストを抑えられる。
pub const DEFAULT_MIN_INTERVAL_MS: u64 = 150;

/// 同期送出と非同期送信のあいだに置くバッファ。
#[derive(Debug)]
pub struct FlushQueue<T> {
    items: Mutex<Vec<T>>,
    /// 送信タスクが動いているか (true の間は新しいタスクを起動しない)。
    sending: AtomicBool,
    /// 直近で送信を始めた時刻 (ms)。0 = まだ送っていない。
    last_send_ms: AtomicU64,
    min_interval_ms: u64,
}

impl<T> FlushQueue<T> {
    pub fn new(min_interval_ms: u64) -> Self {
        Self {
            items: Mutex::new(Vec::new()),
            sending: AtomicBool::new(false),
            last_send_ms: AtomicU64::new(0),
            min_interval_ms,
        }
    }

    /// 1 件積む。戻り値が true のとき、呼び出し側は送信タスクを起動する
    /// (false は「既にタスクが動いている」か「バッファが壊れている」)。
    pub fn push(&self, item: T) -> bool {
        match self.items.lock() {
            Ok(mut items) => items.push(item),
            Err(_) => return false,
        }
        self.sending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// 積まれている分を FIFO で取り出す。空なら `None`。
    pub fn take(&self) -> Option<Vec<T>> {
        let mut items = self.items.lock().ok()?;
        if items.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut *items))
    }

    /// 送信が終わったことを記録する。まだ積まれていて自分が続けるべきなら
    /// true を返す (呼び出し側は送信ループをもう一周する)。
    pub fn finish(&self) -> bool {
        self.sending.store(false, Ordering::Release);
        if !self.is_pending() {
            return false;
        }
        self.sending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// まだ送っていない行が残っているか。
    pub fn is_pending(&self) -> bool {
        self.items
            .lock()
            .map(|items| !items.is_empty())
            .unwrap_or(false)
    }

    /// 送信タスクが動いているか (テストと診断用)。
    pub fn is_sending(&self) -> bool {
        self.sending.load(Ordering::Acquire)
    }

    /// 送信を始めた時刻を記録する (間引きの基準)。
    pub fn mark_sent(&self, now_ms: u64) {
        self.last_send_ms.store(now_ms, Ordering::Relaxed);
    }

    /// 次の送信まで待つべき時間 (ms)。0 ならすぐ送ってよい。
    pub fn wait_ms(&self, now_ms: u64) -> u64 {
        let last = self.last_send_ms.load(Ordering::Relaxed);
        if last == 0 {
            return 0;
        }
        self.min_interval_ms
            .saturating_sub(now_ms.saturating_sub(last))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_push_starts_one_sender_and_a_burst_is_coalesced() {
        let queue = FlushQueue::new(DEFAULT_MIN_INTERVAL_MS);
        assert!(queue.push(1), "idle からの push は送信タスクを起動する");
        assert!(!queue.push(2), "送信中の push は新しいタスクを起動しない");
        assert!(!queue.push(3));

        assert_eq!(queue.take(), Some(vec![1, 2, 3]), "積んだ順にまとめて送る");
        assert_eq!(queue.take(), None, "空になったら送るものは無い");
    }

    #[test]
    fn pushes_during_a_send_are_picked_up_by_the_same_task() {
        let queue = FlushQueue::new(0);
        assert!(queue.push(1));
        assert_eq!(queue.take(), Some(vec![1])); // 送信開始

        assert!(!queue.push(2), "送信中の push はタスクを増やさない");
        // 送信完了 → 残りがあるので同じタスクがもう一周する。
        assert!(queue.finish());
        assert_eq!(queue.take(), Some(vec![2]));
        assert!(!queue.finish(), "空になったらタスクは終了する");
        assert!(!queue.is_sending());
    }

    #[test]
    fn a_push_after_the_final_take_starts_a_new_sender() {
        let queue = FlushQueue::new(0);
        assert!(queue.push(1));
        assert_eq!(queue.take(), Some(vec![1]));
        assert!(!queue.finish());
        assert!(!queue.is_sending());

        // idle に戻ったあとの行は新しいタスクが送る (取りこぼさない)。
        assert!(queue.push(2));
        assert_eq!(queue.take(), Some(vec![2]));
        assert!(!queue.finish());
    }

    #[test]
    fn order_is_kept_across_batches() {
        let queue = FlushQueue::new(0);
        assert!(queue.push("a"));
        assert_eq!(queue.take(), Some(vec!["a"]));
        assert!(!queue.push("b"));
        assert!(queue.finish());
        assert_eq!(queue.take(), Some(vec!["b"]));
        assert!(!queue.finish());
    }

    #[test]
    fn the_throttle_only_applies_after_a_send() {
        let queue: FlushQueue<u8> = FlushQueue::new(DEFAULT_MIN_INTERVAL_MS);
        assert_eq!(queue.wait_ms(1_000), 0, "未送信なら待たない");

        queue.mark_sent(1_000);
        assert_eq!(queue.wait_ms(1_000), DEFAULT_MIN_INTERVAL_MS);
        assert_eq!(queue.wait_ms(1_100), DEFAULT_MIN_INTERVAL_MS - 100);
        assert_eq!(queue.wait_ms(1_150), 0);
        assert_eq!(queue.wait_ms(2_000), 0, "間隔を過ぎたら待たない");
    }

    #[test]
    fn interval_zero_never_waits() {
        let queue: FlushQueue<u8> = FlushQueue::new(0);
        queue.mark_sent(500);
        assert_eq!(queue.wait_ms(500), 0);
    }
}
