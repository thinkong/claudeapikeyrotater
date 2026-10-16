//! Pass-through response streams that observe (and optionally rewrite) individual frames:
//! SSE events for streaming Messages, JSONL lines for batch results.
//! Unrelated frames are forwarded byte-for-byte as soon as they are complete.

use bytes::{Bytes, BytesMut};
use futures_util::Stream;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

pub trait FrameHandler: Send + 'static {
    /// Inspect a complete frame (including its terminator). Return a replacement to rewrite it.
    fn frame(&mut self, frame: &[u8]) -> Option<Bytes>;
    /// Called exactly once: when the upstream ends (`complete = true`), errors, or the
    /// client goes away and the stream is dropped (`complete = false`).
    fn finish(&mut self, complete: bool);
}

#[derive(Clone, Copy)]
pub enum Framing {
    /// Frames end with a blank line (`\n\n` or `\r\n\r\n`).
    Sse,
    /// Frames end with `\n`.
    Lines,
}

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

pub struct Tee {
    inner: ByteStream,
    framing: Framing,
    buf: BytesMut,
    scan_from: usize,
    out: VecDeque<Bytes>,
    handler: Box<dyn FrameHandler>,
    finished: bool,
    upstream_done: bool,
}

impl Tee {
    pub fn new(inner: ByteStream, framing: Framing, handler: Box<dyn FrameHandler>) -> Self {
        Tee { inner, framing, buf: BytesMut::new(), scan_from: 0, out: VecDeque::new(), handler, finished: false, upstream_done: false }
    }

    /// Length of the next complete frame in `buf`, if any.
    fn next_frame_len(&mut self) -> Option<usize> {
        let b = &self.buf[..];
        let start = self.scan_from.min(b.len());
        let found = match self.framing {
            Framing::Lines => b[start..].iter().position(|&c| c == b'\n').map(|p| start + p + 1),
            Framing::Sse => {
                // Back up so a terminator split across chunks is still found.
                let s = start.saturating_sub(3);
                let mut i = s;
                let mut hit = None;
                while i < b.len() {
                    if b[i] == b'\n' {
                        if b.get(i + 1) == Some(&b'\n') {
                            hit = Some(i + 2);
                            break;
                        }
                        if b.get(i + 1) == Some(&b'\r') && b.get(i + 2) == Some(&b'\n') {
                            hit = Some(i + 3);
                            break;
                        }
                    }
                    i += 1;
                }
                hit
            }
        };
        match found {
            Some(n) => {
                self.scan_from = 0;
                Some(n)
            }
            None => {
                self.scan_from = b.len();
                None
            }
        }
    }

    fn drain_frames(&mut self) {
        while let Some(n) = self.next_frame_len() {
            let frame = self.buf.split_to(n).freeze();
            match self.handler.frame(&frame) {
                Some(replacement) => self.out.push_back(replacement),
                None => self.out.push_back(frame),
            }
        }
    }

    fn finish(&mut self, complete: bool) {
        if !self.finished {
            self.finished = true;
            self.handler.finish(complete);
        }
    }
}

impl Stream for Tee {
    type Item = Result<Bytes, reqwest::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(b) = self.out.pop_front() {
                return Poll::Ready(Some(Ok(b)));
            }
            if self.upstream_done {
                return Poll::Ready(None);
            }
            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    self.buf.extend_from_slice(&chunk);
                    self.drain_frames();
                }
                Poll::Ready(Some(Err(e))) => {
                    self.upstream_done = true;
                    self.finish(false);
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(None) => {
                    self.upstream_done = true;
                    if !self.buf.is_empty() {
                        // Trailing frame without terminator: still let the handler see it.
                        let rest = self.buf.split().freeze();
                        let b = self.handler.frame(&rest).unwrap_or(rest);
                        self.out.push_back(b);
                    }
                    self.finish(true);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl Drop for Tee {
    fn drop(&mut self) {
        self.finish(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::sync::{Arc, Mutex};

    struct Collect(Arc<Mutex<Vec<String>>>, Arc<Mutex<Option<bool>>>);
    impl FrameHandler for Collect {
        fn frame(&mut self, f: &[u8]) -> Option<Bytes> {
            self.0.lock().unwrap().push(String::from_utf8_lossy(f).into_owned());
            if f.starts_with(b"event: x") {
                return Some(Bytes::from_static(b"event: y\n\n"));
            }
            None
        }
        fn finish(&mut self, complete: bool) {
            *self.1.lock().unwrap() = Some(complete);
        }
    }

    #[tokio::test]
    async fn sse_frames_split_across_chunks() {
        let chunks: Vec<Result<Bytes, reqwest::Error>> = vec![
            Ok(Bytes::from_static(b"event: a\ndata: 1\n")),
            Ok(Bytes::from_static(b"\nevent: x\nda")),
            Ok(Bytes::from_static(b"ta: 2\n\nevent: c\r\ndata: 3\r\n\r")),
            Ok(Bytes::from_static(b"\n")),
        ];
        let frames = Arc::new(Mutex::new(vec![]));
        let fin = Arc::new(Mutex::new(None));
        let tee = Tee::new(Box::pin(futures_util::stream::iter(chunks)), Framing::Sse, Box::new(Collect(frames.clone(), fin.clone())));
        let out: Vec<Bytes> = tee.map(|r| r.unwrap()).collect().await;
        let joined: Vec<u8> = out.concat();
        assert_eq!(String::from_utf8(joined).unwrap(), "event: a\ndata: 1\n\nevent: y\n\nevent: c\r\ndata: 3\r\n\r\n");
        assert_eq!(frames.lock().unwrap().len(), 3);
        assert_eq!(*fin.lock().unwrap(), Some(true));
    }

    #[tokio::test]
    async fn drop_reports_incomplete() {
        let chunks: Vec<Result<Bytes, reqwest::Error>> = vec![Ok(Bytes::from_static(b"a\nb"))];
        let fin = Arc::new(Mutex::new(None));
        let mut tee = Tee::new(Box::pin(futures_util::stream::iter(chunks).chain(futures_util::stream::pending())), Framing::Lines, Box::new(Collect(Default::default(), fin.clone())));
        assert_eq!(tee.next().await.unwrap().unwrap(), Bytes::from_static(b"a\n"));
        drop(tee);
        assert_eq!(*fin.lock().unwrap(), Some(false));
    }
}
