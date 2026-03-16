use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::Instant;

pub struct MonitoredStream {
    inner: TcpStream,
    pub(crate) last_read: Instant,
}

impl MonitoredStream {
    pub fn new(inner: TcpStream) -> Self {
        Self {
            inner,
            last_read: Instant::now(),
        }
    }
}

impl AsyncRead for MonitoredStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let start_len = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        match poll {
            Poll::Ready(Ok(())) => {
                let end_len = buf.filled().len();
                if end_len > start_len {
                    self.last_read = Instant::now();
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl AsyncWrite for MonitoredStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
