use futures_util::Stream;
use pin_project::pin_project;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Stream wrapper that enforces a maximum byte limit during streaming.
/// This prevents unbounded data transfer even if Content-Length is missing or incorrect.
#[pin_project]
pub(super) struct MaxBytesStream<S> {
    #[pin]
    stream: S,
    max_bytes: u64,
    bytes_read: u64,
}

impl<S, B> MaxBytesStream<S>
where
    S: Stream<Item = Result<B, std::io::Error>>,
    B: AsRef<[u8]>,
{
    pub(super) fn new(stream: S, max_bytes: u64) -> Self {
        Self {
            stream,
            max_bytes,
            bytes_read: 0,
        }
    }
}

impl<S, B> Stream for MaxBytesStream<S>
where
    S: Stream<Item = Result<B, std::io::Error>>,
    B: AsRef<[u8]>,
{
    type Item = Result<B, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();

        match this.stream.poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                let chunk_len = chunk.as_ref().len() as u64;
                *this.bytes_read += chunk_len;

                if *this.bytes_read > *this.max_bytes {
                    Poll::Ready(Some(Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("Stream exceeded maximum size of {} bytes", this.max_bytes),
                    ))))
                } else {
                    Poll::Ready(Some(Ok(chunk)))
                }
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}
