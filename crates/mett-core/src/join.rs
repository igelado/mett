//! Running a few futures at a time.

use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

/// Runs `futures`, at most `limit` at a time, and gives their outputs in
/// order: a slow one doesn't hold up the others, nor do many at once flood
/// a backend.
pub async fn join_bounded<F: Future>(futures: Vec<F>, limit: usize) -> Vec<F::Output> {
    let mut futures: Vec<Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut outputs: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        // Futures do nothing until polled: polling the first `limit`
        // unfinished ones runs only those.
        let mut running = 0;
        for (future, output) in futures.iter_mut().zip(outputs.iter_mut()) {
            if output.is_some() {
                continue;
            }
            if running == limit {
                break;
            }
            match future.as_mut().poll(cx) {
                Poll::Ready(done) => *output = Some(done),
                Poll::Pending => running += 1,
            }
        }
        if running == 0 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    outputs
        .into_iter()
        .map(|output| output.expect("every future finished"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_a_few_at_a_time_in_order() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (running, most) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let futures = (0..10)
            .map(|i| {
                let (running, most) = (&running, &most);
                async move {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    running.fetch_sub(1, Ordering::SeqCst);
                    i
                }
            })
            .collect();
        let done = join_bounded(futures, 3).await;
        assert_eq!(done, (0..10).collect::<Vec<_>>());
        assert_eq!(most.load(Ordering::SeqCst), 3);
    }
}
