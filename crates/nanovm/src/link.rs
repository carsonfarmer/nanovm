//! HTTP/1.1 between a supervisor and a worker, one request at a time per
//! connection, over socketpairs handed across a control socket.
//!
//! The dialing side keeps a pool of idle connections and makes a new
//! socketpair whenever none is free, sending one end to the other side with
//! `SCM_RIGHTS`. The accepting side serves each connection it receives. Both
//! ends run a request inline in the task that asked for it, with no
//! connection task in between: on this path every handoff between tasks is a
//! thread wakeup, which costs more than the HTTP framing. For the same reason
//! a dialer keeps one pool per thread (see [`set_thread`]), so a connection
//! is only used on the thread whose runtime drives it.

use std::cell::Cell;
use std::future::Future;
use std::io::{IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use http::{Request, Response};
use http_body::{Body, Frame};
use hyper::body::Incoming;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::net::UnixStream;

use crate::object::BoxError;

/// Idle connections kept per dialer; more than this are closed when done.
const MAX_IDLE: usize = 256;
/// How long a dial waits for a busy peer to take connections off its
/// control socket.
const DIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A link's socket, registered with the reactor for reads only. A Unix
/// stream socket signals writability whenever its peer reads, so with
/// write interest every request woke the other process for nothing. A
/// write that finds the socket full waits on a second descriptor for it,
/// registered for writes until the write goes through.
struct Io {
    fd: AsyncFd<OwnedFd>,
    full: Option<AsyncFd<OwnedFd>>,
}

impl Io {
    fn new(sock: std::os::unix::net::UnixStream) -> std::io::Result<Io> {
        sock.set_nonblocking(true)?;
        Ok(Io { fd: register(sock.into(), Interest::READABLE)?, full: None })
    }

    fn poll_write_with(&mut self, cx: &mut Context<'_>, write: impl Fn(BorrowedFd<'_>) -> rustix::io::Result<usize>) -> Poll<std::io::Result<usize>> {
        loop {
            match write(self.fd.as_fd()) {
                Err(rustix::io::Errno::AGAIN) => {}
                done => {
                    self.full = None;
                    return Poll::Ready(done.map_err(Into::into));
                }
            }
            let full = match &self.full {
                Some(full) => full,
                None => self.full.insert(register(self.fd.get_ref().try_clone()?, Interest::WRITABLE)?),
            };
            ready!(full.poll_write_ready(cx))?.clear_ready();
        }
    }
}

fn register(fd: OwnedFd, interest: Interest) -> std::io::Result<AsyncFd<OwnedFd>> {
    // SAFETY: an `OwnedFd` stays open, with the same number, until dropped.
    Ok(unsafe { AsyncFd::register_with_interest(fd, interest) }?)
}

impl tokio::io::AsyncRead for Io {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        loop {
            let mut ready = ready!(self.fd.poll_read_ready(cx))?;
            // SAFETY: `read` only writes to the buffer, and `n` bytes of it
            // are initialized once it returns `n`.
            let (len, dst) = (buf.remaining(), unsafe { buf.unfilled_mut() });
            match ready.try_io(|fd| Ok(rustix::io::read(fd, dst)?.0.len())) {
                Ok(Ok(n)) => {
                    // A short read has emptied the socket: wait for the next
                    // edge rather than read again to find out.
                    if 0 < n && n < len {
                        ready.clear_ready();
                    }
                    unsafe { buf.assume_init(n) };
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => {}
            }
        }
    }
}

impl tokio::io::AsyncWrite for Io {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<std::io::Result<usize>> {
        self.get_mut().poll_write_with(cx, |fd| rustix::io::write(fd, data))
    }

    fn poll_write_vectored(self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[IoSlice<'_>]) -> Poll<std::io::Result<usize>> {
        self.get_mut().poll_write_with(cx, |fd| rustix::io::writev(fd, data))
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(rustix::net::shutdown(self.fd.get_ref(), rustix::net::Shutdown::Write).map_err(Into::into))
    }
}

struct Conn<B: Body + 'static> {
    tx: http1::SendRequest<B>,
    conn: Option<http1::Connection<TokioIo<Io>, B>>,
}

impl<B> Conn<B>
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<BoxError>,
{
    /// Drives the connection; `false` once it has closed.
    fn poll_conn(&mut self, cx: &mut Context<'_>) -> bool {
        if let Some(c) = &mut self.conn
            && Pin::new(c).poll(cx).is_ready()
        {
            self.conn = None;
        }
        self.conn.is_some()
    }
}

thread_local! {
    static THREAD: Cell<usize> = const { Cell::new(0) };
}

/// Names the calling thread's pool in every dialer: thread `i` of a
/// thread-per-core server uses pool `i`.
pub fn set_thread(i: usize) {
    THREAD.with(|t| t.set(i));
}

/// The dialing end of a link.
pub struct Dialer<B: Body + 'static> {
    /// Written without a runtime, so any thread may dial.
    ctl: std::os::unix::net::UnixStream,
    pools: Box<[Mutex<Vec<Conn<B>>>]>,
}

impl<B> Dialer<B>
where
    B: Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<BoxError>,
{
    /// A dialer with a pool for each of `threads` threads.
    pub fn new(ctl: std::os::unix::net::UnixStream, threads: usize) -> Arc<Self> {
        Arc::new(Dialer {
            ctl,
            pools: (0..threads.max(1)).map(|_| Mutex::default()).collect(),
        })
    }

    fn pool(&self) -> &Mutex<Vec<Conn<B>>> {
        &self.pools[THREAD.with(Cell::get) % self.pools.len()]
    }

    async fn checkout(&self) -> anyhow::Result<Conn<B>> {
        while let Some(c) = self.pool().lock().pop() {
            if c.conn.is_some() && c.tx.is_ready() {
                return Ok(c);
            }
        }
        let (mine, theirs) = std::os::unix::net::UnixStream::pair()?;
        // A busy peer (all threads instantiating, say) can fall behind on
        // its control socket; wait for it rather than fail the request.
        let start = std::time::Instant::now();
        while let Err(e) = send_fd(&self.ctl, theirs.as_fd()) {
            if e.kind() != std::io::ErrorKind::WouldBlock || start.elapsed() > DIAL_TIMEOUT {
                return Err(e.into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        drop(theirs);
        let io = TokioIo::new(Io::new(mine)?);
        let (tx, conn) = http1::Builder::new().max_header_size(crate::server::MAX_HEAD).handshake(io).await?;
        Ok(Conn { tx, conn: Some(conn) })
    }

    /// Sends `req` on an idle connection (or a new one). The connection goes
    /// back to the pool once the response body has been read to the end.
    pub async fn send(self: &Arc<Self>, req: Request<B>) -> anyhow::Result<Response<LinkBody<B>>> {
        let mut c = self.checkout().await?;
        let mut sent = std::pin::pin!(c.tx.send_request(req));
        let res = std::future::poll_fn(|cx| {
            c.poll_conn(cx);
            sent.as_mut().poll(cx)
        })
        .await?;
        Ok(res.map(|inner| LinkBody {
            inner,
            conn: Some(c),
            dialer: self.clone(),
        }))
    }
}

/// A response body that keeps its connection running while it is read.
pub struct LinkBody<B: Body + 'static> {
    inner: Incoming,
    conn: Option<Conn<B>>,
    dialer: Arc<Dialer<B>>,
}

impl<B> Body for LinkBody<B>
where
    B: Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<BoxError>,
{
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let this = &mut *self;
        if let Some(c) = &mut this.conn {
            c.poll_conn(cx);
        }
        let frame = ready!(Pin::new(&mut this.inner).poll_frame(cx));
        // A reader that knows the length stops at the last byte instead of
        // polling for the end, so the end is checked after every frame.
        if (frame.is_none() || this.inner.is_end_stream())
            && let Some(mut c) = this.conn.take()
        {
            // Let the connection see the end of the response before reuse.
            if c.poll_conn(cx) {
                let mut idle = this.dialer.pool().lock();
                if idle.len() < MAX_IDLE {
                    idle.push(c);
                }
            }
        }
        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// The accepting end: serves HTTP/1.1 on every connection received on
/// `ctl` that gets a permit from each of `caps` (one that doesn't is closed
/// unserved), until the dialer hangs up.
pub async fn serve<F, Fut, R>(ctl: UnixStream, caps: Vec<Arc<tokio::sync::Semaphore>>, f: F) -> anyhow::Result<()>
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<R>> + Send + 'static,
    R: Body<Data = Bytes> + Send + 'static,
    R::Error: Into<BoxError>,
{
    while let Some(fd) = recv_fd(&ctl).await? {
        let Ok(permit) = caps.iter().map(|c| c.clone().try_acquire_owned()).collect::<Result<Vec<_>, _>>() else {
            continue;
        };
        let conn = crate::server::serve_conn(Io::new(fd.into())?, None, f.clone());
        tokio::spawn(async move {
            let _ = conn.await;
            drop(permit);
        });
    }
    Ok(())
}

/// Sends `fd` without blocking the thread; see `checkout` for the retry.
fn send_fd(ctl: &std::os::unix::net::UnixStream, fd: BorrowedFd<'_>) -> std::io::Result<()> {
    let fds = [fd];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut cmsg = SendAncillaryBuffer::new(&mut space);
    cmsg.push(SendAncillaryMessage::ScmRights(&fds));
    rustix::net::sendmsg(ctl, &[IoSlice::new(&[0])], &mut cmsg, SendFlags::DONTWAIT | SendFlags::NOSIGNAL)?;
    Ok(())
}

/// The next fd sent on `ctl`, or `None` once the sender has closed it.
async fn recv_fd(ctl: &UnixStream) -> std::io::Result<Option<OwnedFd>> {
    loop {
        ctl.readable().await?;
        let received = ctl.try_io(Interest::READABLE, || {
            let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
            let mut cmsg = RecvAncillaryBuffer::new(&mut space);
            let n = rustix::net::recvmsg(ctl, &mut [IoSliceMut::new(&mut [0])], &mut cmsg, RecvFlags::CMSG_CLOEXEC)?.bytes;
            let fd = cmsg.drain().find_map(|m| match m {
                RecvAncillaryMessage::ScmRights(mut fds) => fds.next(),
                _ => None,
            });
            Ok((n, fd))
        });
        match received {
            Ok((0, _)) => return Ok(None),
            Ok((_, Some(fd))) => return Ok(Some(fd)),
            Ok((_, None)) => return Err(std::io::Error::other("control message without an fd")),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Empty, Full};

    /// Dials while the peer's control socket is full (here: a tiny buffer and
    /// an acceptor that starts late) wait for it instead of failing.
    #[tokio::test(flavor = "multi_thread")]
    async fn dials_wait_for_a_slow_acceptor() {
        let (ctl, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let small = 2048;
        rustix::net::sockopt::set_socket_send_buffer_size(&ctl, small).unwrap();
        let dialer = Dialer::<Empty<Bytes>>::new(ctl, 1);
        let sends: Vec<_> = (0..64)
            .map(|_| {
                let d = dialer.clone();
                tokio::spawn(async move {
                    let res = d.send(Request::get("/").body(Empty::new()).unwrap()).await?;
                    anyhow::Ok(res.into_body().collect().await?.to_bytes())
                })
            })
            .collect();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        peer.set_nonblocking(true).unwrap();
        tokio::spawn(serve(UnixStream::from_std(peer).unwrap(), vec![], |_req| async {
            Response::new(Full::new(Bytes::from_static(b"ok")))
        }));
        for s in sends {
            assert_eq!(s.await.unwrap().unwrap(), "ok");
        }
    }

    /// Bodies larger than a socket's buffer go through both ways: a write
    /// that finds the socket full waits until the peer has read.
    #[tokio::test(flavor = "multi_thread")]
    async fn bodies_larger_than_the_socket_buffer_go_through() {
        let (ctl, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        tokio::spawn(serve(UnixStream::from_std(peer).unwrap(), vec![], |req: Request<Incoming>| async move {
            let body = req.into_body().collect().await.unwrap().to_bytes();
            Response::new(Full::new(Bytes::from(body.repeat(2))))
        }));
        let dialer = Dialer::<Full<Bytes>>::new(ctl, 1);
        let sent = async {
            let res = dialer.send(Request::post("/").body(Full::new(Bytes::from(vec![7; 4 << 20]))).unwrap()).await?;
            anyhow::Ok(res.into_body().collect().await?.to_bytes())
        };
        let echoed = tokio::time::timeout(std::time::Duration::from_secs(10), sent).await.expect("a full socket stalled").unwrap();
        assert_eq!(echoed.len(), 8 << 20);
    }

    /// A dialer gets at most its own cap of connections served at once, and
    /// all dialers together at most the shared one: one more is closed
    /// unserved, and those served keep working.
    #[tokio::test(flavor = "multi_thread")]
    async fn connections_are_capped() {
        use tokio::sync::Semaphore;
        let gate = Arc::new(tokio::sync::Notify::new());
        let shared = Arc::new(Semaphore::new(3));
        let dialer = || {
            let (ctl, peer) = std::os::unix::net::UnixStream::pair().unwrap();
            peer.set_nonblocking(true).unwrap();
            let g = gate.clone();
            let caps = vec![Arc::new(Semaphore::new(2)), shared.clone()];
            tokio::spawn(serve(UnixStream::from_std(peer).unwrap(), caps, move |_req| {
                let g = g.clone();
                async move {
                    g.notified().await;
                    Response::new(Full::new(Bytes::from_static(b"ok")))
                }
            }));
            Dialer::<Empty<Bytes>>::new(ctl, 1)
        };
        let send = |d: &Arc<Dialer<Empty<Bytes>>>| {
            let d = d.clone();
            tokio::spawn(async move {
                let res = d.send(Request::get("/").body(Empty::new()).unwrap()).await?;
                anyhow::Ok(res.into_body().collect().await?.to_bytes())
            })
        };
        let refused = async |d: &Arc<Dialer<Empty<Bytes>>>| {
            let r = tokio::time::timeout(std::time::Duration::from_secs(1), send(d)).await;
            r.is_ok_and(|r| r.unwrap().is_err())
        };
        let (a, b) = (dialer(), dialer());
        let held = [send(&a), send(&a)];
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(refused(&a).await, "a third connection from one dialer was served");
        let held_b = send(&b);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(refused(&b).await, "a fourth connection in all was served");
        gate.notify_waiters();
        for h in held.into_iter().chain([held_b]) {
            assert_eq!(h.await.unwrap().unwrap(), "ok");
        }
        let again = send(&a);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        gate.notify_waiters();
        assert_eq!(again.await.unwrap().unwrap(), "ok", "a pooled connection still works");
    }
}
