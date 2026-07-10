use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;
use tokio::time::{Instant, Sleep};

use crate::provider::{EventStream, ProviderError, StreamEvent};

/// Watchdog: if the underlying stream produces nothing for `dur`, emit
/// one `IdleTimeout` error and end. No silent stalls, ever.
pub fn with_idle_timeout(inner: EventStream, dur: Duration) -> EventStream {
    Box::pin(IdleTimeout {
        inner,
        dur,
        sleep: Box::pin(tokio::time::sleep(dur)),
        done: false,
    })
}

struct IdleTimeout {
    inner: EventStream,
    dur: Duration,
    sleep: Pin<Box<Sleep>>,
    done: bool,
}

impl Stream for IdleTimeout {
    type Item = Result<StreamEvent, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(item)) => {
                let deadline = Instant::now() + this.dur;
                this.sleep.as_mut().reset(deadline);
                Poll::Ready(Some(item))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => match this.sleep.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    this.done = true;
                    Poll::Ready(Some(Err(ProviderError::IdleTimeout(this.dur))))
                }
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_fires() {
        let inner: EventStream = Box::pin(futures::stream::pending());
        let mut s = with_idle_timeout(inner, Duration::from_secs(5));
        let item = s.next().await;
        assert!(matches!(item, Some(Err(ProviderError::IdleTimeout(_)))));
        assert!(s.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn passes_items_through() {
        let inner: EventStream = Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::TextDelta("a".into())),
            Ok(StreamEvent::TextDelta("b".into())),
        ]));
        let s = with_idle_timeout(inner, Duration::from_secs(5));
        let items: Vec<_> = s.collect().await;
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.is_ok()));
    }
}
