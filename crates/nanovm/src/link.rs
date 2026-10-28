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

use std::future::Future;
use std::cell::Cell;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
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
    /// Non-blocking, and written without a runtime: any thread may dial.
    ctl: std::os::unix::net::UnixStream,
    pools: Box<[Mutex<Vec<Conn<B>>>]>,
}

impl<B> Dialer<B>
where
    B: Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<BoxError>,
{
    /// A dialer with a pool for each of `threads` threads.
    pub fn new(ctl: std::os::unix::net::UnixStream, threads: usize) -> anyhow::Result<Arc<Self>> {
        ctl.set_nonblocking(true)?;
        Ok(Arc::new(Dialer {
            ctl,
            pools: (0..threads.max(1)).map(|_| Mutex::default()).collect(),
        }))
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
        while let Err(e) = send_fd(&self.ctl, theirs.as_raw_fd()) {
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
        let conn = &mut c.conn;
        let res = std::future::poll_fn(|cx| {
            if let Some(cn) = conn
                && Pin::new(cn).poll(cx).is_ready()
            {
                *conn = None;
            }
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
/// `ctl`, until the dialer hangs up.
pub async fn serve<F, Fut, R>(ctl: UnixStream, f: F) -> anyhow::Result<()>
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<R>> + Send + 'static,
    R: Body<Data = Bytes> + Send + 'static,
    R::Error: Into<BoxError>,
{
    while let Some(fd) = recv_fd(&ctl).await? {
        let sock = std::os::unix::net::UnixStream::from(fd);
        sock.set_nonblocking(true)?;
        let io = TokioIo::new(UnixStream::from_std(sock)?);
        let f = f.clone();
        let svc = hyper::service::service_fn(move |req| {
            let res = f(req);
            async move { Ok::<_, std::convert::Infallible>(res.await) }
        });
        tokio::spawn(hyper::server::conn::http1::Builder::new().serve_connection(io, svc));
    }
    Ok(())
}

/// Sends `fd` without blocking the thread; see `checkout` for the retry.
fn send_fd(ctl: &std::os::unix::net::UnixStream, fd: RawFd) -> std::io::Result<()> {
    let mut byte = [0u8];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut cmsg = [0u8; 64];
    // SAFETY: `msg` points at `iov` and `cmsg`, both live for the call; the
    // control buffer is large enough for one fd.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(4) as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(4) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<RawFd>(), fd);
        if libc::sendmsg(ctl.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The next fd sent on `ctl`, or `None` once the sender has closed it.
async fn recv_fd(ctl: &UnixStream) -> std::io::Result<Option<OwnedFd>> {
    loop {
        ctl.readable().await?;
        match ctl.try_io(Interest::READABLE, || {
            let mut byte = [0u8];
            let mut iov = libc::iovec {
                iov_base: byte.as_mut_ptr().cast(),
                iov_len: 1,
            };
            let mut cmsg = [0u8; 64];
            // SAFETY: as in `send_fd`; the kernel fills in at most
            // `msg_controllen` bytes of `cmsg`.
            unsafe {
                let mut msg: libc::msghdr = std::mem::zeroed();
                msg.msg_iov = &mut iov;
                msg.msg_iovlen = 1;
                msg.msg_control = cmsg.as_mut_ptr().cast();
                msg.msg_controllen = cmsg.len() as _;
                let n = libc::recvmsg(ctl.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC);
                if n < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if n == 0 {
                    return Ok(None);
                }
                let c = libc::CMSG_FIRSTHDR(&msg);
                if c.is_null() || (*c).cmsg_type != libc::SCM_RIGHTS {
                    return Err(std::io::Error::other("control message without an fd"));
                }
                let fd = std::ptr::read_unaligned(libc::CMSG_DATA(c).cast::<RawFd>());
                Ok(Some(OwnedFd::from_raw_fd(fd)))
            }
        }) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            r => return r,
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
        let small: libc::c_int = 2048;
        // SAFETY: setting an int socket option from a valid pointer.
        unsafe {
            libc::setsockopt(
                ctl.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&small as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as _,
            );
        }
        let dialer = Dialer::<Empty<Bytes>>::new(ctl, 1).unwrap();
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
        tokio::spawn(serve(UnixStream::from_std(peer).unwrap(), |_req| async {
            Response::new(Full::new(Bytes::from_static(b"ok")))
        }));
        for s in sends {
            assert_eq!(s.await.unwrap().unwrap(), "ok");
        }
    }
}
