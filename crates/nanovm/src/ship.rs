//! Ships each object's published versions to a store off the node, which
//! keeps each at `<app>/<object>/<sequence>.tar` once: a put never replaces
//! what is there, so a retry is harmless and a second writer of a sequence
//! (a node that lost its object) is refused. Shipped versions are removed
//! here but the last, which the next names as its parent; what isn't
//! shipped yet stays, within the capture limits, which refuse writes once
//! they are reached (the store down, say), never dropping a version.

use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::config::CaptureConfig;

pub enum Put {
    Stored,
    /// The same bytes were there already.
    Same,
    /// Other bytes were.
    Conflict,
}

/// Immutable keys: a put stores its bytes only where nothing is.
#[async_trait::async_trait]
pub trait Store: Send + Sync {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> io::Result<Put>;
    async fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>>;
}

/// The store `cap` names, if any.
pub fn store(cap: &CaptureConfig) -> anyhow::Result<Option<Arc<dyn Store>>> {
    Ok(match cap.store.as_deref() {
        None => None,
        Some(url) if url.starts_with("http://") || url.starts_with("https://") => Some(Arc::new(S3 {
            client: reqwest::Client::builder().no_proxy().build()?,
            url: url.trim_end_matches('/').into(),
            region: cap.region.clone(),
            key_id: std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default(),
            secret: std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_default(),
        })),
        Some(dir) => Some(Arc::new(Dir(dir.into()))),
    })
}

/// A local (or mounted) directory: a put is a hard link, which never
/// replaces a name.
pub struct Dir(pub PathBuf);

#[async_trait::async_trait]
impl Store for Dir {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> io::Result<Put> {
        let path = self.0.join(key);
        tokio::task::spawn_blocking(move || {
            let dir = path.parent().expect("keys have a directory");
            std::fs::create_dir_all(dir)?;
            let new = path.with_extension(format!("new{}", std::process::id()));
            std::fs::write(&new, &bytes)?;
            std::fs::File::open(&new)?.sync_all()?;
            let linked = std::fs::hard_link(&new, &path);
            std::fs::remove_file(&new)?;
            match linked {
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    Ok(if std::fs::read(&path)? == bytes { Put::Same } else { Put::Conflict })
                }
                linked => linked.and_then(|()| std::fs::File::open(dir)?.sync_all()).map(|()| Put::Stored),
            }
        })
        .await?
    }

    async fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        match tokio::fs::read(self.0.join(key)).await {
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            r => r.map(Some),
        }
    }
}

/// An S3-compatible bucket, by path-style URL, with SigV4-signed requests.
/// A put is conditional (`If-None-Match: *`).
pub struct S3 {
    client: reqwest::Client,
    url: String,
    region: String,
    key_id: String,
    secret: String,
}

impl S3 {
    async fn send(&self, method: reqwest::Method, key: &str, body: Vec<u8>) -> io::Result<reqwest::Response> {
        let encoded: String = key.split('/').map(encode).collect::<Vec<_>>().join("/");
        let url = reqwest::Url::parse(&format!("{}/{encoded}", self.url)).map_err(io::Error::other)?;
        let host = url.host_str().unwrap_or_default().to_owned() + &url.port().map(|p| format!(":{p}")).unwrap_or_default();
        let hash = format!("{:x}", Sha256::digest(&body));
        let date = amz_date(SystemTime::now());
        let headers = [("host", host.as_str()), ("x-amz-content-sha256", &hash), ("x-amz-date", &date)];
        let auth = authorization(method.as_str(), url.path(), &headers, &self.region, &self.key_id, &self.secret);
        let mut req = self.client.request(method.clone(), url).header("authorization", auth).body(body);
        for (name, value) in &headers[1..] {
            req = req.header(*name, *value);
        }
        if method == reqwest::Method::PUT {
            req = req.header("if-none-match", "*");
        }
        req.send().await.map_err(io::Error::other)
    }
}

#[async_trait::async_trait]
impl Store for S3 {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> io::Result<Put> {
        let res = self.send(reqwest::Method::PUT, key, bytes.clone()).await?;
        match res.status().as_u16() {
            200 | 201 => Ok(Put::Stored),
            // Another put got there first, or is still going (409).
            409 | 412 => Ok(if self.get(key).await?.is_some_and(|b| b == bytes) { Put::Same } else { Put::Conflict }),
            s => Err(io::Error::other(format!("store put {key}: HTTP {s}"))),
        }
    }

    async fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        let res = self.send(reqwest::Method::GET, key, vec![]).await?;
        match res.status().as_u16() {
            404 => Ok(None),
            200 => Ok(Some(res.bytes().await.map_err(io::Error::other)?.into())),
            s => Err(io::Error::other(format!("store get {key}: HTTP {s}"))),
        }
    }
}

/// `s`, percent-encoded as SigV4 wants a path segment.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn hmac(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut k = [0; 64];
    k[..key.len()].copy_from_slice(key);
    let pad = |b: u8| k.map(|x| x ^ b);
    let inner = Sha256::new().chain_update(pad(0x36)).chain_update(message).finalize();
    Sha256::new().chain_update(pad(0x5c)).chain_update(inner).finalize().into()
}

/// `t` as SigV4 writes a time: `20130524T000000Z`.
fn amz_date(t: SystemTime) -> String {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = (secs / 86400) as i64 + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    let y = yoe + era * 400 + (m <= 2) as i64;
    let s = secs % 86400;
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", s / 3600, s / 60 % 60, s % 60)
}

/// The SigV4 `Authorization` header of a request with no query, whose
/// sorted, lower-case `headers` include `x-amz-date` and `x-amz-content-sha256`.
fn authorization(method: &str, path: &str, headers: &[(&str, &str)], region: &str, key_id: &str, secret: &str) -> String {
    let value = |name| headers.iter().find(|h| h.0 == name).map_or("", |h| h.1);
    let (date, day) = (value("x-amz-date"), &value("x-amz-date")[..8]);
    let names: Vec<_> = headers.iter().map(|h| h.0).collect();
    let canonical: String = headers.iter().map(|(n, v)| format!("{n}:{}\n", v.trim())).collect();
    let request = format!("{method}\n{path}\n\n{canonical}\n{}\n{}", names.join(";"), value("x-amz-content-sha256"));
    let scope = format!("{day}/{region}/s3/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{date}\n{scope}\n{:x}", Sha256::digest(request));
    let key = [day, region, "s3", "aws4_request"].iter().fold(format!("AWS4{secret}").into_bytes(), |k, part| {
        hmac(&k, part.as_bytes()).to_vec()
    });
    let signature: String = hmac(&key, to_sign.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    format!("AWS4-HMAC-SHA256 Credential={key_id}/{scope}, SignedHeaders={}, Signature={signature}", names.join(";"))
}

fn key(app: &str, object: &str, sequence: u64) -> String {
    format!("{app}/{object}/{sequence:020}.tar")
}

/// Where the shipper records the last version it stored of the object whose
/// versions are in `dir` (`<objects>/~committed/<object>`):
/// `<objects>/~shipped/<object>`.
pub(crate) fn marker(dir: &Path) -> PathBuf {
    let objects = dir.parent().and_then(Path::parent).expect("in ~committed");
    objects.join("~shipped").join(dir.file_name().expect("an object's"))
}

/// Records `sequence` in `marker`, whole.
fn mark(marker: &Path, sequence: u64) -> io::Result<()> {
    let new = marker.with_file_name("~new");
    std::fs::create_dir_all(marker.parent().expect("in ~shipped"))?;
    std::fs::write(&new, sequence.to_string())?;
    std::fs::rename(new, marker)
}

/// Waits, two seconds at most (within an instance's post-return deadline),
/// for the shipper to store the version `sequence` of the object whose
/// versions are in `dir`.
pub(crate) fn stored(dir: &Path, sequence: u64) -> io::Result<()> {
    for _ in 0..1000 {
        if std::fs::read_to_string(marker(dir)).ok().and_then(|s| s.parse::<u64>().ok()) >= Some(sequence) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(io::Error::new(ErrorKind::TimedOut, format!("version {sequence} is not in the store")))
}

/// The sequences of the versions published in `dir`, in order.
fn published(dir: &Path) -> io::Result<Vec<u64>> {
    let mut versions = vec![];
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name();
        if let Some(n) = name.to_str().and_then(|n| n.strip_suffix(".tar")?.parse().ok()) {
            versions.push(n);
        }
    }
    versions.sort();
    Ok(versions)
}

/// Ships what the object whose versions are in `dir` hasn't yet, in order,
/// then removes the shipped ones but the last.
async fn ship_object(store: &dyn Store, app: &str, object: &str, dir: &Path, epoch: u64) -> io::Result<()> {
    let marker = marker(dir);
    let shipped: u64 = std::fs::read_to_string(&marker).ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let versions = published(dir)?;
    if versions.iter().any(|&n| n > shipped) {
        let fence = store.get(&format!("{app}/{object}/fence")).await?;
        if fence.and_then(|f| String::from_utf8(f).ok()?.trim().parse::<u64>().ok()).is_some_and(|f| f > epoch) {
            return Err(io::Error::other(format!("{app}/{object} is fenced past epoch {epoch}")));
        }
    }
    for &n in versions.iter().filter(|&&n| n > shipped) {
        let bytes = tokio::fs::read(dir.join(format!("{n:020}.tar"))).await?;
        match store.put(&key(app, object, n), bytes).await? {
            Put::Stored | Put::Same => {}
            Put::Conflict => return Err(io::Error::other(format!("{app}/{object} version {n} is another writer's"))),
        }
        mark(&marker, n)?;
    }
    for &n in versions.iter().rev().skip(1) {
        match std::fs::remove_file(dir.join(format!("{n:020}.tar"))) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// Ships, until the runtime stops, the versions published under `objects`
/// (`<data_dir>/objects`): every app's, every object's, retrying failures
/// a second later.
pub async fn run(objects: PathBuf, store: Arc<dyn Store>, epoch: u64) {
    let mut seen = std::collections::HashMap::new();
    loop {
        let dirs = std::fs::read_dir(&objects).into_iter().flatten().flatten().flat_map(|app| {
            let committed = app.path().join("~committed");
            let objects = std::fs::read_dir(committed).into_iter().flatten().flatten();
            objects.filter(|o| o.file_type().is_ok_and(|t| t.is_dir())).map(move |o| (app.file_name(), o))
        });
        let mut failed = false;
        for (app, object) in dirs.collect::<Vec<_>>() {
            // Skip an object nothing was published for since its last round:
            // its directory's time is the same, and older than the clock's tick.
            let changed = object.metadata().and_then(|m| m.modified()).ok();
            let old = changed.is_some_and(|t| t.elapsed().is_ok_and(|e| e > Duration::from_millis(100)));
            if old && seen.get(&object.path()) == Some(&changed) {
                continue;
            }
            let (app, id) = (app.to_string_lossy(), object.file_name().to_string_lossy().into_owned());
            match ship_object(&*store, &app, &id, &object.path(), epoch).await {
                Ok(()) => _ = seen.insert(object.path(), changed),
                Err(e) => {
                    eprintln!("nanovm: shipping {app}/{id}: {e}");
                    failed = true;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(if failed { 1000 } else { 20 })).await;
    }
}

/// Rebuilds object `object` of app `app` from what `store` holds, as the
/// volume under `objects` (`<data_dir>/objects/<app>`, where it must not be
/// yet), with its last version kept so history goes on from it. Returns
/// that version's sequence.
pub async fn restore(store: &dyn Store, app: &str, object: &str, objects: &Path) -> io::Result<u64> {
    let staging = objects.join("~restore").join(object);
    std::fs::create_dir_all(&staging)?;
    let mut n = 1;
    while let Some(bytes) = store.get(&key(app, object, n)).await? {
        std::fs::write(staging.join(format!("{n:020}.tar")), bytes)?;
        n += 1;
    }
    let last = crate::committed::restore(&staging, &objects.join(object))?;
    let out = objects.join("~committed").join(object);
    std::fs::create_dir_all(&out)?;
    std::fs::rename(staging.join(format!("{last:020}.tar")), out.join(format!("{last:020}.tar")))?;
    mark(&marker(&out), last)?;
    std::fs::remove_dir_all(staging)?;
    // SAFETY: sync takes no arguments and cannot fail.
    unsafe { libc::sync() };
    Ok(last)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use parking_lot::Mutex;

    /// An S3-compatible bucket in this process: keeps what is put unless
    /// `If-None-Match: *` finds something there, and refuses requests whose
    /// signature doesn't check out, or any at all while `down`.
    pub(crate) struct Fake {
        pub url: String,
        pub objects: Arc<Mutex<std::collections::BTreeMap<String, Vec<u8>>>>,
        pub down: Arc<std::sync::atomic::AtomicBool>,
    }

    pub(crate) async fn fake() -> Fake {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/bucket", listener.local_addr().unwrap());
        let (objects, down) = (Arc::new(Mutex::new(Default::default())), Arc::new(Default::default()));
        let (store, off): (Arc<Mutex<std::collections::BTreeMap<String, Vec<u8>>>>, Arc<std::sync::atomic::AtomicBool>) =
            (Arc::clone(&objects), Arc::clone(&down));
        tokio::spawn(crate::server::serve_http1(listener, move |req| {
            let (store, off) = (store.clone(), off.clone());
            async move {
                let (parts, body) = req.into_parts();
                let body = body.collect().await.unwrap().to_bytes().to_vec();
                let header = |n: &str| parts.headers.get(n).map_or("", |v| v.to_str().unwrap()).to_owned();
                let headers = [("host", header("host")), ("x-amz-content-sha256", header("x-amz-content-sha256")), ("x-amz-date", header("x-amz-date"))];
                let headers: Vec<_> = headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
                let path = parts.uri.path();
                let expected = authorization(parts.method.as_str(), path, &headers, "us-east-1", "id", "secret");
                let key = path.strip_prefix("/bucket/").unwrap().to_owned();
                let status = match () {
                    _ if off.load(std::sync::atomic::Ordering::Relaxed) => 500,
                    _ if header("authorization") != expected || header("x-amz-content-sha256") != format!("{:x}", Sha256::digest(&body)) => 403,
                    _ if parts.method == http::Method::PUT => {
                        let mut store = store.lock();
                        if header("if-none-match") == "*" && store.contains_key(&key) {
                            412
                        } else {
                            store.insert(key, body);
                            200
                        }
                    }
                    _ => match store.lock().get(&key) {
                        Some(bytes) => return http::Response::new(body_of(bytes.clone())),
                        None => 404,
                    },
                };
                let mut res = http::Response::new(body_of(vec![]));
                *res.status_mut() = http::StatusCode::from_u16(status).unwrap();
                res
            }
        }));
        Fake { url, objects, down }
    }

    fn body_of(bytes: Vec<u8>) -> crate::object::RespBody {
        http_body_util::Full::new(bytes::Bytes::from(bytes)).map_err(|e| match e {}).boxed_unsync()
    }

    fn s3(url: &str) -> S3 {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        S3 { client, url: url.into(), region: "us-east-1".into(), key_id: "id".into(), secret: "secret".into() }
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nanovm-ship-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Both stores keep the first bytes put at a key: the same again is
    /// harmless, other bytes are refused.
    #[tokio::test(flavor = "multi_thread")]
    async fn stores_put_once() {
        let fake = fake().await;
        let stores: [Box<dyn Store>; 2] = [Box::new(Dir(tempdir("put"))), Box::new(s3(&fake.url))];
        for store in stores {
            assert!(store.get("a/o/1").await.unwrap().is_none());
            assert!(matches!(store.put("a/o/1", b"one".to_vec()).await.unwrap(), Put::Stored));
            assert!(matches!(store.put("a/o/1", b"one".to_vec()).await.unwrap(), Put::Same));
            assert!(matches!(store.put("a/o/1", b"other".to_vec()).await.unwrap(), Put::Conflict));
            assert_eq!(store.get("a/o/1").await.unwrap().unwrap(), b"one");
        }
        let forged = S3 { secret: "wrong".into(), ..s3(&fake.url) };
        assert!(forged.get("a/o/1").await.is_err(), "the fake checks signatures");
    }

    /// The shipper stores versions in order, then keeps only the last here;
    /// it stops at a version another writer stored, and at a fence above
    /// its epoch.
    #[tokio::test]
    async fn the_shipper_stores_in_order_and_keeps_the_last() {
        let base = tempdir("shipper");
        let (store, dir) = (Dir(base.join("store")), base.join("objects/app/~committed/o"));
        std::fs::create_dir_all(&dir).unwrap();
        let version = |n: u64| std::fs::write(dir.join(format!("{n:020}.tar")), format!("v{n}")).unwrap();
        let local = || published(&dir).unwrap();
        (1..=3).for_each(version);
        ship_object(&store, "app", "o", &dir, 0).await.unwrap();
        assert_eq!(store.get(&key("app", "o", 2)).await.unwrap().unwrap(), b"v2");
        assert_eq!((local(), std::fs::read_to_string(marker(&dir)).unwrap()), (vec![3], "3".into()));
        version(4);
        store.put(&key("app", "o", 5), b"theirs".to_vec()).await.unwrap();
        version(5);
        assert!(ship_object(&store, "app", "o", &dir, 0).await.is_err());
        assert_eq!((local(), std::fs::read_to_string(marker(&dir)).unwrap()), (vec![3, 4, 5], "4".into()));
        let dir = base.join("objects/app/~committed/p");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{:020}.tar", 1)), "p1").unwrap();
        store.put("app/p/fence", b"1".to_vec()).await.unwrap();
        assert!(ship_object(&store, "app", "p", &dir, 0).await.unwrap_err().to_string().contains("fenced"));
        assert!(store.get(&key("app", "p", 1)).await.unwrap().is_none());
        ship_object(&store, "app", "p", &dir, 1).await.unwrap();
        assert_eq!(store.get(&key("app", "p", 1)).await.unwrap().unwrap(), b"p1");
        std::fs::remove_dir_all(base).unwrap();
    }

    /// AWS's documented example request ("GET Object", header-based SigV4).
    #[test]
    fn signatures_match_aws_example() {
        let headers = [
            ("host", "examplebucket.s3.amazonaws.com"),
            ("range", "bytes=0-9"),
            ("x-amz-content-sha256", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
            ("x-amz-date", "20130524T000000Z"),
        ];
        let auth = authorization(
            "GET",
            "/test.txt",
            &headers,
            "us-east-1",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_369_353_600 + 3661);
        assert_eq!(amz_date(t), "20130524T010101Z");
        assert_eq!(amz_date(SystemTime::UNIX_EPOCH + Duration::from_secs(951_782_400)), "20000229T000000Z");
    }
}
