use crate::{
    decode,
    immutable::{Band, Text},
    PacketLine, StreamingPeekableIter, U16_HEX_BYTES,
};
use futures_io::{AsyncBufRead, AsyncRead};
use futures_lite::ready;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

type ReadLineResult<'a> = Option<std::io::Result<Result<PacketLine<'a>, decode::Error>>>;
/// An implementor of [`AsyncBufRead`] yielding packet lines on each call to [`read_line()`][AsyncBufRead::read_line()].
/// It's also possible to hide the underlying packet lines using the [`Read`][AsyncRead] implementation which is useful
/// if they represent binary data, like the one of a pack file.
pub struct WithSidebands<'a, T, F>
where
    T: AsyncRead,
{
    state: State<'a, T>,
    handle_progress: Option<F>,
    pos: usize,
    cap: usize,
}

impl<'a, T, F> Drop for WithSidebands<'a, T, F>
where
    T: AsyncRead,
{
    fn drop(&mut self) {
        if let State::Idle { ref mut parent } = self.state {
            parent.as_mut().unwrap().reset();
        }
    }
}

impl<'a, T> WithSidebands<'a, T, fn(bool, &[u8])>
where
    T: AsyncRead,
{
    /// Create a new instance with the given provider as `parent`.
    pub fn new(parent: &'a mut StreamingPeekableIter<T>) -> Self {
        WithSidebands {
            state: State::Idle { parent: Some(parent) },
            handle_progress: None,
            pos: 0,
            cap: 0,
        }
    }
}

enum State<'a, T> {
    Idle {
        parent: Option<&'a mut StreamingPeekableIter<T>>,
    },
    ReadLine {
        read_line: Pin<Box<dyn Future<Output = ReadLineResult<'a>> + 'a>>,
        parent_inactive: Option<*mut StreamingPeekableIter<T>>,
    },
}

impl<'a, T, F> WithSidebands<'a, T, F>
where
    T: AsyncRead + Unpin,
    F: FnMut(bool, &[u8]) + Unpin,
{
    /// Create a new instance with the given `parent` provider and the `handle_progress` function.
    ///
    /// Progress or error information will be passed to the given `handle_progress(is_error, text)` function, with `is_error: bool`
    /// being true in case the `text` is to be interpreted as error.
    pub fn with_progress_handler(parent: &'a mut StreamingPeekableIter<T>, handle_progress: F) -> Self {
        WithSidebands {
            state: State::Idle { parent: Some(parent) },
            handle_progress: Some(handle_progress),
            pos: 0,
            cap: 0,
        }
    }

    /// Create a new instance without a progress handler.
    pub fn without_progress_handler(parent: &'a mut StreamingPeekableIter<T>) -> Self {
        WithSidebands {
            state: State::Idle { parent: Some(parent) },
            handle_progress: None,
            pos: 0,
            cap: 0,
        }
    }

    /// Forwards to the parent [StreamingPeekableIter::reset_with()]
    pub fn reset_with(&mut self, delimiters: &'static [PacketLine<'static>]) {
        if let State::Idle { ref mut parent } = self.state {
            parent.as_mut().unwrap().reset_with(delimiters)
        }
    }

    /// Forwards to the parent [StreamingPeekableIter::stopped_at()]
    pub fn stopped_at(&self) -> Option<PacketLine<'static>> {
        match self.state {
            State::Idle { ref parent } => parent.as_ref().unwrap().stopped_at,
            _ => None,
        }
    }

    /// Set or unset the progress handler.
    pub fn set_progress_handler(&mut self, handle_progress: Option<F>) {
        self.handle_progress = handle_progress;
    }

    /// Effectively forwards to the parent [StreamingPeekableIter::peek_line()], allowing to see what would be returned
    /// next on a call to [`read_line()`][io::BufRead::read_line()].
    pub async fn peek_data_line(&mut self) -> Option<std::io::Result<Result<&[u8], crate::decode::Error>>> {
        match self.state {
            State::Idle { ref mut parent } => match parent.as_mut().unwrap().peek_line().await {
                Some(Ok(Ok(crate::PacketLine::Data(line)))) => Some(Ok(Ok(line))),
                Some(Ok(Err(err))) => Some(Ok(Err(err))),
                Some(Err(err)) => Some(Err(err)),
                _ => None,
            },
            _ => None,
        }
    }

    /// Read a packet line as line.
    pub fn read_line<'b>(&'b mut self, buf: &'b mut String) -> ReadLineFuture<'a, 'b, T, F> {
        ReadLineFuture { parent: self, buf }
    }
}

pub struct ReadLineFuture<'a, 'b, T: AsyncRead, F> {
    parent: &'b mut WithSidebands<'a, T, F>,
    buf: &'b mut String,
}

impl<'a, 'b, T, F> Future for ReadLineFuture<'a, 'b, T, F>
where
    T: AsyncRead + Unpin + Send,
    F: FnMut(bool, &[u8]) + Unpin,
{
    type Output = std::io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        assert_eq!(
            self.parent.cap, 0,
            "we don't support partial buffers right now - read-line must be used consistently"
        );
        let Self { buf, parent } = &mut *self;
        let line = std::str::from_utf8(ready!(Pin::new(parent).poll_fill_buf(cx))?)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))
            .unwrap();
        buf.clear();
        buf.push_str(line);
        let bytes = line.len();
        self.parent.cap = 0;
        Poll::Ready(Ok(bytes))
    }
}

impl<'a, T, F> AsyncBufRead for WithSidebands<'a, T, F>
where
    T: AsyncRead + Unpin + Send,
    F: FnMut(bool, &[u8]) + Unpin,
{
    fn poll_fill_buf(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<&[u8]>> {
        use futures_lite::FutureExt;
        use std::io;
        {
            let this = self.as_mut().get_mut();
            if this.pos >= this.cap {
                let (ofs, cap) = loop {
                    match this.state {
                        State::Idle { ref mut parent } => {
                            let parent = parent.take().expect("parent to be present here");
                            let inactive = parent as *mut _;
                            this.state = State::ReadLine {
                                read_line: parent.read_line().boxed(),
                                parent_inactive: Some(inactive),
                            }
                        }
                        State::ReadLine {
                            ref mut read_line,
                            ref mut parent_inactive,
                        } => {
                            let line = ready!(read_line.poll(cx));

                            this.state = {
                                let parent = parent_inactive.take().expect("parent pointer always set");
                                // SAFETY: It's safe to recover the original mutable reference (from which
                                // the `read_line` future was created as the latter isn't accessible anymore
                                // once the state is set to Idle. In other words, either one or the other are
                                // accessible, never both at the same time.
                                // Also: We keep a pointer around which is protected by borrowcheck since it's created
                                // from a legal mutable reference which is moved into the read_line future - we it was manually
                                // implemented we would be able to re-obtain it from there.
                                #[allow(unsafe_code)]
                                let parent = unsafe { &mut *parent };
                                State::Idle { parent: Some(parent) }
                            };

                            let line = match line {
                                Some(line) => line?.map_err(|err| io::Error::new(io::ErrorKind::Other, err))?,
                                None => break (0, 0),
                            };

                            match this.handle_progress.as_mut() {
                                Some(handle_progress) => {
                                    let band = line
                                        .decode_band()
                                        .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
                                    const ENCODED_BAND: usize = 1;
                                    match band {
                                        Band::Data(d) => break (U16_HEX_BYTES + ENCODED_BAND, d.len()),
                                        Band::Progress(d) => {
                                            let text = Text::from(d).0;
                                            handle_progress(false, text);
                                        }
                                        Band::Error(d) => {
                                            let text = Text::from(d).0;
                                            handle_progress(true, text);
                                        }
                                    };
                                }
                                None => {
                                    break match line.as_slice() {
                                        Some(d) => (U16_HEX_BYTES, d.len()),
                                        None => {
                                            return Poll::Ready(Err(io::Error::new(
                                                io::ErrorKind::UnexpectedEof,
                                                "encountered non-data line in a data-line only context",
                                            )))
                                        }
                                    }
                                }
                            }
                        }
                    }
                };
                this.cap = cap + ofs;
                this.pos = ofs;
            }
        }
        let range = self.pos..self.cap;
        match &self.get_mut().state {
            State::Idle { parent } => Poll::Ready(Ok(&parent.as_ref().expect("parent always available").buf[range])),
            State::ReadLine { .. } => unreachable!("at least in theory"),
        }
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        let this = self.get_mut();
        this.pos = std::cmp::min(this.pos + amt, this.cap);
    }
}

impl<'a, T, F> AsyncRead for WithSidebands<'a, T, F>
where
    T: AsyncRead + Unpin + Send,
    F: FnMut(bool, &[u8]) + Unpin,
{
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<std::io::Result<usize>> {
        let nread = {
            use std::io::Read;
            let mut rem = ready!(self.as_mut().poll_fill_buf(cx))?;
            rem.read(buf)?
        };
        self.consume(nread);
        Poll::Ready(Ok(nread))
    }
}