//! Runtime-free future combinator for the contract suites (mage-core has no async runtime).

use std::pin::Pin;
use std::task::{Context, Poll};

/// Polls every future concurrently within the calling task (like `futures::future::join_all`);
/// results in input order. With a real adapter the futures overlap on the database, so races such
/// as "8 concurrent enqueues of one dedupe key" are exercised.
pub async fn join_all<F: Future>(futures: impl IntoIterator<Item = F>) -> Vec<F::Output> {
    JoinAll { pending: futures.into_iter().map(|f| Some(Box::pin(f))).collect(), done: Vec::new() }.await
}

struct JoinAll<F: Future> {
    pending: Vec<Option<Pin<Box<F>>>>,
    done: Vec<Option<F::Output>>,
}

// Sound: `poll` never pins an output or projects a pin into a field; the futures are boxed.
impl<F: Future> Unpin for JoinAll<F> {}

impl<F: Future> Future for JoinAll<F> {
    type Output = Vec<F::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // `JoinAll` is `Unpin` (see above), so the fields are reachable mutably.
        let this = self.get_mut();
        if this.done.len() != this.pending.len() {
            this.done.resize_with(this.pending.len(), || None);
        }
        let mut all_ready = true;
        for (slot, out) in this.pending.iter_mut().zip(this.done.iter_mut()) {
            if let Some(future) = slot {
                match future.as_mut().poll(cx) {
                    Poll::Ready(value) => {
                        *out = Some(value);
                        *slot = None;
                    }
                    Poll::Pending => all_ready = false,
                }
            }
        }
        if all_ready { Poll::Ready(this.done.drain(..).flatten().collect()) } else { Poll::Pending }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// Ready on its second poll, so completion order differs from input order.
    struct YieldOnce<'a> {
        polled: bool,
        value: u32,
        log: &'a Cell<u32>,
    }

    impl Future for YieldOnce<'_> {
        type Output = u32;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
            if self.polled || self.value & 1 == 0 {
                self.log.set(self.log.get() * 10 + self.value);
                Poll::Ready(self.value)
            } else {
                self.polled = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    #[tokio::test]
    async fn results_keep_input_order_while_futures_interleave() {
        let log = Cell::new(0);
        let futures = (1..=4).map(|value| YieldOnce { polled: false, value, log: &log });
        assert_eq!(join_all(futures).await, vec![1, 2, 3, 4]);
        assert_eq!(log.get(), 2413, "even values finish in the first round, odd ones in the second");
    }
}
