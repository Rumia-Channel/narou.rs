//! Isolate-local admission. Never keep a pending future across Worker requests.

use std::cell::Cell;
use std::rc::Rc;

thread_local! {
    static HEAVY: Rc<Cell<bool>> = Rc::new(Cell::new(false));
}

/// Held by the operation (or its response stream) until completion/cancellation.
/// `Rc` deliberately makes this token thread-local, like the isolate it protects.
#[derive(Debug)]
pub(crate) struct MemoryPermit {
    occupied: Rc<Cell<bool>>,
}

pub(crate) fn try_heavy() -> Option<MemoryPermit> {
    HEAVY.with(|occupied| {
        if occupied.replace(true) {
            None
        } else {
            Some(MemoryPermit { occupied: Rc::clone(occupied) })
        }
    })
}

impl Drop for MemoryPermit {
    fn drop(&mut self) {
        self.occupied.set(false);
    }
}

pub(crate) fn busy_response() -> worker::Result<worker::Response> {
    let mut response = worker::Response::from_json(&serde_json::json!({
        "error": "memory_capacity", "retryable": true
    }))?.with_status(503);
    response.headers_mut().set("Retry-After", "5")?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_admission_does_not_release_the_owner() {
        let owner = try_heavy().unwrap();
        assert!(try_heavy().is_none());
        assert!(try_heavy().is_none());
        drop(owner);
        assert!(try_heavy().is_some());
    }

    #[test]
    fn dropping_a_cancelled_operation_releases_capacity() {
        {
            let operation = async {
                let _permit = try_heavy().unwrap();
                std::future::pending::<()>().await;
            };
            let mut operation = std::pin::pin!(operation);
            let waker = futures::task::noop_waker();
            let mut context = std::task::Context::from_waker(&waker);
            assert!(operation.as_mut().poll(&mut context).is_pending());
            assert!(try_heavy().is_none());
        }
        assert!(try_heavy().is_some());
    }
}
