use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;

use futures::channel::oneshot;
use futures::prelude::*;

pub(crate) struct BytesPipe {
    buf: VecDeque<u8>,
    waker: Option<Waker>,
}

impl BytesPipe {
    pub(crate) fn new() -> BytesPipe {
        BytesPipe {
            buf: VecDeque::new(),
            waker: None,
        }
    }
}

impl AsyncRead for BytesPipe {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut i = 0;
        for b in buf.iter_mut() {
            *b = match self.buf.pop_front() {
                Some(b) => b,
                None => break,
            };
            i += 1;
        }
        if i == 0 {
            self.waker.replace(cx.waker().clone());
            Poll::Pending
        } else {
            Poll::Ready(Ok(i))
        }
    }
}

impl AsyncWrite for BytesPipe {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut i = 0;
        for b in buf.iter() {
            self.buf.push_back(*b);
            i += 1;
        }
        Poll::Ready(Ok(i))
    }

    fn poll_flush(mut self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
        Poll::Ready(Ok(()))
    }

    fn poll_close(mut self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
        Poll::Ready(Ok(()))
    }
}

pub(crate) struct BufferBlockingRead<T: io::Read + Send + 'static> {
    reader: Arc<Mutex<T>>,
    buf: VecDeque<u8>,
    buf_fin: bool,
    rx: Option<oneshot::Receiver<io::Result<Vec<u8>>>>,
    thread: Option<JoinHandle<()>>,
}

impl<T: io::Read + Send + 'static> BufferBlockingRead<T> {
    pub(crate) fn new(reader: T) -> BufferBlockingRead<T> {
        BufferBlockingRead {
            reader: Arc::new(Mutex::new(reader)),
            buf: VecDeque::new(),
            buf_fin: false,
            rx: None,
            thread: None,
        }
    }
}

impl<T: io::Read + Send + 'static> AsyncRead for BufferBlockingRead<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if self.buf_fin && self.buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let buf_size = 1024 * 64;

        if !self.buf_fin && self.buf.len() < buf_size {
            // Take the existing task or create it.
            let mut rx = match self.rx.take() {
                Some(rx) => rx,
                None => {
                    let reader = self.reader.clone();
                    let (tx, rx) = oneshot::channel();
                    self.thread.replace(std::thread::spawn(move || {
                        let mut buf = vec![0; buf_size];
                        let mut reader = reader.lock().unwrap();
                        let _ = tx.send(reader.read(&mut buf).map(|size| {
                            buf.resize(size, 0);
                            buf
                        }));
                    }));
                    rx
                }
            };

            // Poll the receiver.
            let poll = Pin::new(&mut rx).poll(cx);
            match poll {
                Poll::Ready(res) => {
                    // Drop the thread handle.
                    drop(self.thread.take());

                    let res = res.unwrap();
                    if let Ok(buf) = res {
                        if buf.is_empty() {
                            self.buf_fin = true;
                        }
                        for b in buf.iter() {
                            self.buf.push_back(*b);
                        }
                    } else {
                        self.buf_fin = true;
                    }
                }
                _ => {
                    // If reading is not finished, store the task.
                    if !self.buf_fin {
                        self.rx.replace(rx);
                    }
                }
            }
        }

        let size = std::cmp::min(self.buf.len(), buf.len());
        if !self.buf_fin && size == 0 {
            return Poll::Pending;
        }

        // Timer
        for b in buf.iter_mut().take(size) {
            *b = self.buf.pop_front().unwrap();
        }
        Poll::Ready(Ok(size))
    }
}

impl<T: io::Read + Send + 'static> Drop for BufferBlockingRead<T> {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
