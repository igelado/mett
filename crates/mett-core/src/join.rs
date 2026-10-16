//! Running a few futures at a time.

use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

/// Runs `futures`, at most `limit` at a time, and gives their outputs in
/// order: a slow one doesn't hold up the others, nor do many at once flood
/// a backend. A `limit` of 0 runs them one at a time.
pub async fn join_bounded<F: Future>(futures: Vec<F>, limit: usize) -> Vec<F::Output> {
    join_bounded_until(futures, limit, |_| false)
        .await
        .into_iter()
        .map(|output| output.expect("every future finished"))
        .collect()
}

/// Like [`join_bounded`], but stops as soon as `decided` says the outputs
/// so far (in order, `None` for those not finished) are enough, say to
/// tell the error to report: those not finished are dropped, and give
/// `None`.
pub async fn join_bounded_until<F: Future>(
    futures: Vec<F>,
    limit: usize,
    mut decided: impl FnMut(&[Option<F::Output>]) -> bool,
) -> Vec<Option<F::Output>> {
    let limit = limit.max(1);
    let mut futures: Vec<Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut outputs: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        // Futures do nothing until polled: polling the first `limit`
        // unfinished ones runs only those.
        let mut running = 0;
        for i in 0..futures.len() {
            if outputs[i].is_some() {
                continue;
            }
            if running == limit {
                break;
            }
            match futures[i].as_mut().poll(cx) {
                Poll::Ready(done) => {
                    outputs[i] = Some(done);
                    if decided(&outputs) {
                        return Poll::Ready(());
                    }
                }
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

    #[tokio::test]
    async fn a_limit_of_zero_runs_one_at_a_time() {
        let futures = (0..3).map(|i| async move { i }).collect();
        assert_eq!(join_bounded(futures, 0).await, [0, 1, 2]);
    }

    #[tokio::test]
    async fn stops_once_the_outputs_so_far_decide() {
        let futures = (0..6)
            .map(|i| async move {
                if i != 1 {
                    // Never finishes: only stopping ends the wait.
                    std::future::pending::<()>().await;
                }
                i
            })
            .collect();
        let done = join_bounded_until(futures, 3, |outputs| outputs[1].is_some()).await;
        assert_eq!(done, [None, Some(1), None, None, None, None]);
    }
}
