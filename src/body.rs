use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use hyper::body::{Frame, SizeHint};
use tokio::task::JoinHandle;

const CHUNK: u64 = 64 * 1024;

pub enum Body {
    Empty,
    Full(Option<Bytes>),
    File(FileBody),
}

pub struct FileBody {
    prefix: Option<Bytes>,
    file: Arc<File>,
    offset: u64,
    remaining: u64,
    pending: Option<JoinHandle<io::Result<Vec<u8>>>>,
}

impl Body {
    pub fn full(bytes: impl Into<Bytes>) -> Self {
        Body::Full(Some(bytes.into()))
    }

    /// `offset` counts from the start of `file` and `head` must be its first bytes; the part
    /// of the range inside `head` is sent without reading the file again.
    pub fn from_file(file: File, head: Bytes, offset: u64, len: u64) -> Self {
        let head_len = head.len() as u64;
        let end = offset + len;
        if end <= head_len {
            return Body::full(head.slice(offset as usize..end as usize));
        }
        let prefix = head.slice(offset.min(head_len) as usize..);
        let prefix_len = prefix.len() as u64;
        Body::File(FileBody {
            prefix: (!prefix.is_empty()).then_some(prefix),
            file: Arc::new(file),
            offset: offset + prefix_len,
            remaining: len - prefix_len,
            pending: None,
        })
    }
}

impl hyper::body::Body for Body {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<io::Result<Frame<Bytes>>>> {
        match self.get_mut() {
            Body::Empty => Poll::Ready(None),
            Body::Full(bytes) => Poll::Ready(bytes.take().map(|bytes| Ok(Frame::data(bytes)))),
            Body::File(file) => file
                .poll_chunk(cx)
                .map(|chunk| chunk.map(|chunk| chunk.map(Frame::data))),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            Body::Empty => true,
            Body::Full(bytes) => bytes.is_none(),
            Body::File(file) => file.prefix.is_none() && file.remaining == 0,
        }
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(match self {
            Body::Empty => 0,
            Body::Full(bytes) => bytes.as_ref().map_or(0, |bytes| bytes.len() as u64),
            Body::File(file) => {
                file.remaining + file.prefix.as_ref().map_or(0, |prefix| prefix.len() as u64)
            }
        })
    }
}

impl FileBody {
    fn poll_chunk(&mut self, cx: &mut Context<'_>) -> Poll<Option<io::Result<Bytes>>> {
        if let Some(prefix) = self.prefix.take() {
            return Poll::Ready(Some(Ok(prefix)));
        }
        if self.remaining == 0 {
            return Poll::Ready(None);
        }
        let pending = self.pending.get_or_insert_with(|| {
            let file = self.file.clone();
            let offset = self.offset;
            let len = self.remaining.min(CHUNK) as usize;
            tokio::task::spawn_blocking(move || {
                let mut buf = vec![0; len];
                file.read_exact_at(&mut buf, offset).map(|()| buf)
            })
        });
        let chunk = ready!(Pin::new(pending).poll(cx));
        self.pending = None;
        let chunk = chunk??;
        self.offset += chunk.len() as u64;
        self.remaining -= chunk.len() as u64;
        Poll::Ready(Some(Ok(Bytes::from(chunk))))
    }
}
