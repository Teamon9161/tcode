//! Byte-level idle watchdog. It guards the raw HTTP byte stream *before* SSE
//! parsing, so "idle" means what it says: no bytes arrived on the socket for
//! `dur`. Any byte resets the timer — including SSE keepalives, pings, and
//! event types a provider never decodes — so a stream that is still growing is
//! never mistaken for a stall. (Guarding the parsed `StreamEvent` stream, as an
//! earlier version did, tripped while the model was busy emitting frames we
//! happened to drop.)

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use eventsource_stream::EventStreamError;
use futures::Stream;
use tokio::time::{Instant, Sleep};

use tcode_core::ProviderError;

/// Error surfaced by [`IdleGuard`], distinguishing a genuine stall from an
/// underlying transport failure so the caller can classify it.
#[derive(Debug)]
pub enum StreamByteError {
    Transport(String),
    Idle(Duration),
}

impl fmt::Display for StreamByteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamByteError::Transport(m) => write!(f, "{m}"),
            StreamByteError::Idle(d) => write!(f, "no data for {d:?}"),
        }
    }
}

/// Wrap a byte stream so a genuine stall — no bytes for `dur` — ends it with
/// `Idle`. Each received chunk resets the deadline.
pub fn idle_guard<S, B, E>(inner: S, dur: Duration) -> IdleGuard<B, E>
where
    S: Stream<Item = Result<B, E>> + Send + 'static,
{
    IdleGuard {
        inner: Box::pin(inner),
        dur,
        sleep: Box::pin(tokio::time::sleep(dur)),
        done: false,
    }
}

pub struct IdleGuard<B, E> {
    inner: Pin<Box<dyn Stream<Item = Result<B, E>> + Send>>,
    dur: Duration,
    sleep: Pin<Box<Sleep>>,
    done: bool,
}

impl<B, E: fmt::Display> Stream for IdleGuard<B, E> {
    type Item = Result<B, StreamByteError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(b))) => {
                this.sleep.as_mut().reset(Instant::now() + this.dur);
                Poll::Ready(Some(Ok(b)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.done = true;
                Poll::Ready(Some(Err(StreamByteError::Transport(e.to_string()))))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => match this.sleep.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    this.done = true;
                    Poll::Ready(Some(Err(StreamByteError::Idle(this.dur))))
                }
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

/// Map an SSE-layer error (whose transport is an [`IdleGuard`]) onto a
/// `ProviderError`. A stall becomes the retryable `IdleTimeout`; everything
/// else stays a retryable `Network` error, as before the guard moved down a
/// layer.
pub fn classify(e: EventStreamError<StreamByteError>) -> ProviderError {
    match e {
        EventStreamError::Transport(StreamByteError::Idle(d)) => ProviderError::IdleTimeout(d),
        EventStreamError::Transport(StreamByteError::Transport(m)) => ProviderError::Network(m),
        other => ProviderError::Network(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[tokio::test]
    async fn idle_fires_when_no_bytes() {
        let inner = futures::stream::pending::<Result<Vec<u8>, std::io::Error>>();
        let mut s = idle_guard(inner, Duration::from_millis(20));
        assert!(matches!(s.next().await, Some(Err(StreamByteError::Idle(_)))));
        assert!(s.next().await.is_none());
    }

    #[tokio::test]
    async fn bytes_pass_through() {
        let inner = futures::stream::iter(vec![
            Ok::<_, std::io::Error>(b"a".to_vec()),
            Ok(b"b".to_vec()),
        ]);
        let items: Vec<_> = idle_guard(inner, Duration::from_millis(20)).collect().await;
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.is_ok()));
    }
}
