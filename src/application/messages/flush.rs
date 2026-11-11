//! Invocation-owned, byte-bounded FIFO. Async producers await capacity; only
//! explicitly replaceable progress snapshots may coalesce. Console/terminal
//! events retain order. Bytes remain charged while a batch is in flight.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

pub const DEFAULT_MIN_INTERVAL_MS: u64 = 150;
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024;

#[derive(Debug)]
struct State<T> {
    items: VecDeque<(T, usize)>,
    bytes: usize,
    in_flight_bytes: usize,
    last_send_ms: u64,
    producer: Option<Waker>,
    consumer: Option<Waker>,
}

/// One invocation has one producer, and at most one sender.
#[derive(Debug)]
pub struct FlushQueue<T> {
    state: Mutex<State<T>>,
    min_interval_ms: u64,
    max_bytes: usize,
    measure: fn(&T) -> usize,
    replaces: fn(&T, &T) -> bool,
}

impl<T> FlushQueue<T> {
    pub fn new(min_interval_ms: u64, max_bytes: usize, measure: fn(&T) -> usize, replaces: fn(&T, &T) -> bool) -> Self {
        Self {
            state: Mutex::new(State {
                items: VecDeque::new(), bytes: 0, in_flight_bytes: 0,
                last_send_ms: 0, producer: None, consumer: None,
            }),
            min_interval_ms, max_bytes, measure, replaces,
        }
    }

    /// Oversized individual events must be delivered directly under backpressure.
    pub fn fits(&self, item: &T) -> bool {
        (self.measure)(item).saturating_add(std::mem::size_of::<(T, usize)>()) <= self.max_bytes
    }

    /// Rejected ownership stays with the producer, which must await capacity.
    pub fn try_push(&self, item: T) -> Result<(), T> {
        let bytes = (self.measure)(&item).saturating_add(std::mem::size_of::<(T, usize)>());
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let replace = state.items.iter().rposition(|(old, _)| (self.replaces)(old, &item));
        let removed = replace.map_or(0, |index| state.items[index].1);
        if state.bytes.saturating_sub(removed).saturating_add(bytes) > self.max_bytes {
            return Err(item);
        }
        if let Some(index) = replace { state.items.remove(index); }
        state.bytes = state.bytes - removed + bytes;
        state.items.push_back((item, bytes));
        let consumer = state.consumer.take();
        drop(state);
        if let Some(consumer) = consumer { consumer.wake(); }
        Ok(())
    }

    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.bytes <= self.max_bytes / 2 {
            Poll::Ready(())
        } else {
            state.producer = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    /// Wait for an actual release after a failed `try_push`, even if below the
    /// ordinary low-water mark. The producer retains only its single item.
    pub fn poll_empty(&self, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.bytes == 0 { Poll::Ready(()) }
        else {
            state.producer = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    pub fn poll_pending(&self, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.items.is_empty() { Poll::Ready(()) }
        else {
            state.consumer = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    pub fn take(&self) -> Option<Vec<T>> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.items.is_empty() || state.in_flight_bytes != 0 { return None; }
        let mut batch = Vec::with_capacity(state.items.len().min(64));
        while batch.len() < 64 && state.in_flight_bytes < 64 * 1024 {
            let Some((item, bytes)) = state.items.pop_front() else { break };
            state.in_flight_bytes += bytes;
            batch.push(item);
        }
        Some(batch)
    }

    pub fn finish(&self) -> bool {
        let (more, producer) = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.bytes -= state.in_flight_bytes;
            state.in_flight_bytes = 0;
            (!state.items.is_empty(), state.producer.take())
        };
        if let Some(producer) = producer { producer.wake(); }
        more
    }

    pub fn mark_sent(&self, now_ms: u64) {
        self.state.lock().unwrap_or_else(|error| error.into_inner()).last_send_ms = now_ms;
    }

    pub fn wait_ms(&self, now_ms: u64) -> u64 {
        let last = self.state.lock().unwrap_or_else(|error| error.into_inner()).last_send_ms;
        if last == 0 { 0 } else { self.min_interval_ms.saturating_sub(now_ms.saturating_sub(last)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(capacity: usize) -> FlushQueue<String> {
        FlushQueue::new(150, capacity, String::capacity, |_, _| false)
    }

    #[test]
    fn in_flight_bytes_apply_backpressure_without_dropping_console_lines() {
        let queue = queue(256);
        queue.try_push("first".repeat(20)).unwrap();
        let first = queue.take().unwrap();
        let next = "second".repeat(20);
        assert_eq!(queue.try_push(next.clone()), Err(next.clone()));
        assert!(!queue.finish());
        queue.try_push(next.clone()).unwrap();
        assert_eq!(first, vec!["first".repeat(20)]);
        assert_eq!(queue.take(), Some(vec![next]));
        assert!(!queue.finish());
    }

    #[test]
    fn only_matching_replaceable_progress_is_coalesced() {
        let queue = FlushQueue::new(0, 512, |s: &String| s.capacity(), |a, b| a.starts_with("step:") && b.starts_with("step:"));
        queue.try_push("step:1".into()).unwrap();
        queue.try_push("console".into()).unwrap();
        queue.try_push("step:2".into()).unwrap();
        queue.try_push("terminal".into()).unwrap();
        assert_eq!(queue.take(), Some(vec!["console".into(), "step:2".into(), "terminal".into()]));
        assert!(!queue.finish());
    }

    #[test]
    fn batches_are_single_flight_and_throttled() {
        let queue = queue(4096);
        queue.try_push("a".into()).unwrap();
        queue.try_push("b".into()).unwrap();
        assert_eq!(queue.wait_ms(1000), 0);
        queue.mark_sent(1000);
        assert_eq!(queue.wait_ms(1100), 50);
        assert_eq!(queue.take(), Some(vec!["a".into(), "b".into()]));
        assert!(queue.take().is_none(), "a second batch cannot bypass an in-flight batch");
        assert!(!queue.finish());
        queue.try_push("c".into()).unwrap();
    }

    #[test]
    fn queued_output_wakes_the_invocation_owned_sender() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        use std::task::Wake;
        struct WakeCount(AtomicUsize);
        impl Wake for WakeCount {
            fn wake(self: Arc<Self>) { self.0.fetch_add(1, Ordering::Relaxed); }
        }
        let wakes = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        let queue = queue(4096);
        assert!(queue.poll_pending(&mut cx).is_pending());
        queue.try_push("first".into()).unwrap();
        assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
        assert!(queue.poll_pending(&mut cx).is_ready());
        assert_eq!(queue.take(), Some(vec!["first".into()]));
        assert!(!queue.finish());
        assert!(queue.poll_pending(&mut cx).is_pending());
        queue.try_push("second".into()).unwrap();
        assert_eq!(wakes.0.load(Ordering::Relaxed), 2);
        assert_eq!(queue.take(), Some(vec!["second".into()]));
        assert!(!queue.finish());
    }
}
