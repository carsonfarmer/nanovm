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
use tokio::net::UnixStream;

use crate::object::BoxError;

/// Idle connections kept per dialer; more than this are closed when done.
const MAX_IDLE: usize = 256;
/// How long a dial waits for a busy peer to take connections off its
/// control socket.
const DIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

struct Conn<B: Body + 'static> {
    tx: http1::SendRequest<B>,
    conn: Option<http1::Connection<TokioIo<UnixStream>, B>>,
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
        mine.set_nonblocking(true)?;
        let (tx, conn) = http1::handshake(TokioIo::new(UnixStream::from_std(mine)?)).await?;
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
/// `ctl`, at most `max` at once (one past that is closed unserved), until
/// the dialer hangs up.
pub async fn serve<F, Fut, R>(ctl: UnixStream, max: usize, f: F) -> anyhow::Result<()>
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<R>> + Send + 'static,
    R: Body<Data = Bytes> + Send + 'static,
    R::Error: Into<BoxError>,
{
    let open = Arc::new(tokio::sync::Semaphore::new(max.min(tokio::sync::Semaphore::MAX_PERMITS)));
    while let Some(fd) = recv_fd(&ctl).await? {
        let Ok(permit) = open.clone().try_acquire_owned() else { continue };
        let sock = std::os::unix::net::UnixStream::from(fd);
        sock.set_nonblocking(true)?;
        let conn = crate::server::serve_conn(UnixStream::from_std(sock)?, f.clone());
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
        tokio::spawn(serve(UnixStream::from_std(peer).unwrap(), usize::MAX, |_req| async {
            Response::new(Full::new(Bytes::from_static(b"ok")))
        }));
        for s in sends {
            assert_eq!(s.await.unwrap().unwrap(), "ok");
        }
    }

    /// A dialer gets at most `max` connections served at once: one more is
    /// closed unserved, and those it has keep working.
    #[tokio::test(flavor = "multi_thread")]
    async fn connections_are_capped() {
        let (ctl, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let gate = Arc::new(tokio::sync::Notify::new());
        let g = gate.clone();
        tokio::spawn(serve(UnixStream::from_std(peer).unwrap(), 2, move |_req| {
            let g = g.clone();
            async move {
                g.notified().await;
                Response::new(Full::new(Bytes::from_static(b"ok")))
            }
        }));
        let dialer = Dialer::<Empty<Bytes>>::new(ctl, 1);
        let send = || {
            let d = dialer.clone();
            tokio::spawn(async move {
                let res = d.send(Request::get("/").body(Empty::new()).unwrap()).await?;
                anyhow::Ok(res.into_body().collect().await?.to_bytes())
            })
        };
        let held = [send(), send()];
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let third = tokio::time::timeout(std::time::Duration::from_secs(1), send()).await;
        assert!(third.is_ok_and(|r| r.unwrap().is_err()), "a third connection was served");
        gate.notify_waiters();
        for h in held {
            assert_eq!(h.await.unwrap().unwrap(), "ok");
        }
        let again = send();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        gate.notify_waiters();
        assert_eq!(again.await.unwrap().unwrap(), "ok", "a pooled connection still works");
    }
}
