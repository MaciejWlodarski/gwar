//! Withhold pre-authentication frame headers from tungstenite until their declared
//! lengths pass the smaller limit. No payload allocation or body read is needed.
use std::{
    io,
    pin::Pin,
    task::{Context, Poll, ready},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub(super) struct Transport<S> {
    inner: S,
    pub authenticated: bool,
    header: [u8; 14],
    collected: usize,
    emitted: usize,
    header_len: usize,
    payload: u64,
    message_bytes: u64,
}

impl<S> Transport<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            authenticated: false,
            header: [0; 14],
            collected: 0,
            emitted: 0,
            header_len: 2,
            payload: 0,
            message_bytes: 0,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Transport<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if this.authenticated {
            return Pin::new(&mut this.inner).poll_read(cx, buf);
        }
        if this.emitted < this.collected && this.collected == this.header_len {
            let n = buf.remaining().min(this.collected - this.emitted);
            buf.put_slice(&this.header[this.emitted..this.emitted + n]);
            this.emitted += n;
            return Poll::Ready(Ok(()));
        }
        if this.payload > 0 {
            let n = buf.remaining().min(this.payload as usize);
            let mut part = ReadBuf::new(buf.initialize_unfilled_to(n));
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut part))?;
            let n = part.filled().len();
            this.payload -= n as u64;
            buf.advance(n);
            return Poll::Ready(Ok(()));
        }
        if this.emitted == this.header_len {
            this.collected = 0;
            this.emitted = 0;
            this.header_len = 2;
        }
        loop {
            let mut part = ReadBuf::new(&mut this.header[this.collected..this.header_len]);
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut part))?;
            let n = part.filled().len();
            if n == 0 {
                return Poll::Ready(Ok(()));
            }
            this.collected += n;
            if this.collected < this.header_len {
                continue;
            }
            if this.header_len == 2 {
                this.header_len += match this.header[1] & 127 {
                    126 => 2,
                    127 => 8,
                    _ => 0,
                };
                if this.header[1] & 128 != 0 {
                    this.header_len += 4;
                }
                if this.collected < this.header_len {
                    continue;
                }
            }
            let len = match this.header[1] & 127 {
                126 => u16::from_be_bytes(this.header[2..4].try_into().unwrap()) as u64,
                127 => u64::from_be_bytes(this.header[2..10].try_into().unwrap()),
                n => n as u64,
            };
            let data = this.header[0] & 8 == 0;
            let total = if data { this.message_bytes.saturating_add(len) } else { len };
            if len > super::MAX_FRAME as u64 || total > super::MAX_FRAME as u64 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "pre-authentication WebSocket size limit",
                )));
            }
            if data {
                this.message_bytes = if this.header[0] & 128 != 0 { 0 } else { total };
            }
            this.payload = len;
            let n = buf.remaining().min(this.header_len);
            buf.put_slice(&this.header[..n]);
            this.emitted = n;
            return Poll::Ready(Ok(()));
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Transport<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}
