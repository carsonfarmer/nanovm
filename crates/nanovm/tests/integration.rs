//! End-to-end tests against the real guests in `guests/`. Build them first
//! with `scripts/build-guests.sh`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use nanovm::config::{AppConfig, NodeConfig};
#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;

#[path = "../support/collector.rs"]
mod collector;

use load::{Reply, request};
use nanovm::table::Node;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn guest(name: &str) -> Vec<u8> {
    let p = repo()
        .join("guests/target/wasm32-wasip2/release")
        .join(format!("{name}.wasm"));
    std::fs::read(&p).unwrap_or_else(|_| panic!("{} is missing: run scripts/build-guests.sh", p.display()))
}

/// The test apps, `counter` and `hostile`, as their registry configs give
/// them.
fn app(name: &str) -> AppConfig {
    let limits = match name {
        // A generous CPU budget: CPU kills are tested on `hostile`.
        "counter" => serde_json::json!({
            "memory_mb": 16, "cpu_ms": 500, "request_timeout_ms": 2000,
            "idle_timeout_ms": 60000, "disk_mb": 1, "max_files": 100,
        }),
        "hostile" => serde_json::json!({
            "memory_mb": 32, "cpu_ms": 100, "request_timeout_ms": 1000,
            "post_return_timeout_ms": 500, "disk_mb": 8, "max_files": 200,
            "max_resources": 2000, "max_outbound_concurrency": 2,
            "egress": ["example.com", "localhost"],
        }),
        _ => panic!("no test app {name}"),
    };
    AppConfig {
        name: name.into(),
        digest: "test".into(),
        env: Default::default(),
        config: Default::default(),
        secrets: Default::default(),
        limits: serde_json::from_value(limits).unwrap(),
        worker: Default::default(),
    }
}

/// A dev-mode node config with its own data directory, whose operator allows
/// the egress the test apps ask for; the compile cache is shared between
/// tests.
fn node_cfg(test: &str, max_live: usize) -> NodeConfig {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let dir = root.join("nodes").join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cache = root.join("cwasm-cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::os::unix::fs::symlink(&cache, dir.join("cache")).unwrap();
    let mut cfg = NodeConfig {
        data_dir: dir,
        max_live_objects: max_live,
        memory_reservation_mb: 64,
        dev: true,
        // The cgroup tests name their own; the rest run where none is delegated.
        cgroup: "off".into(),
        ..NodeConfig::default()
    };
    cfg.limits.egress = ["example.com", "localhost", "127.0.0.1"].map(String::from).into();
    cfg.validate().unwrap();
    cfg
}

/// An in-process node running `app` (named for its guest).
fn node_with(cfg: NodeConfig, app: AppConfig) -> Arc<Node> {
    nanovm::engine::preflight(&cfg).unwrap();
    Node::new(cfg, &app, &guest(&app.name), None).unwrap()
}

/// An in-process node running the test app `name`.
fn node(test: &str, name: &str, max_live: usize) -> Arc<Node> {
    node_with(node_cfg(test, max_live), app(name))
}

async fn get(node: &Arc<Node>, id: &str, path: &str) -> Reply {
    request(node, "GET", id, path, "").await
}

/// Waits until `f` holds, for up to 2 s.
async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(2), "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Waits until every live object is idle, so LRU eviction has a candidate.
async fn settle(node: &Arc<Node>) {
    eventually("objects to go idle", || node.phases().keys().all(|p| *p == "idle")).await;
}

// ---- persistence and isolation ----------------------------------------

/// Object `id`'s directory under `objects` (an app's): its volume, `data`,
/// its `journal` and its `committed` versions.
fn object_dir(objects: &Path, id: &str) -> PathBuf {
    nanovm::app::object_dir(objects, id)
}

fn captured_files(objects: &Path, id: &str, sequence: u64) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(object_dir(objects, id).join(format!("committed/{sequence:020}.tar"))).unwrap();
    tar::Archive::new(file).entries().unwrap().map(|entry| {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().into_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        (path, bytes)
    }).filter(|(path, _)| path != Path::new("nanovm.json")).collect()
}

/// A node ships each committed version to a store and keeps only the last;
/// the object is then rebuilt from the store on a fresh data root, served,
/// and its history goes on in the store from there.
#[tokio::test(flavor = "multi_thread")]
async fn shipped_versions_restore_an_object_on_a_fresh_root() {
    use nanovm::ship::Dir;
    let store = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ship-store");
    let _ = std::fs::remove_dir_all(&store);
    // Shipped: the object's head names the version, or a later one.
    let shipped = |n: u64| {
        let head = std::fs::read(store.join("counter/o/head")).ok();
        head.and_then(|h| serde_json::from_slice::<nanovm::ship::Head>(&h).ok()).is_some_and(|h| h.sequence >= n)
    };
    let shipping = |name| {
        let mut cfg = node_cfg(name, 8);
        cfg.capture = Some(nanovm::config::CaptureConfig {
            store: Some(store.to_str().unwrap().into()),
            snapshot_every: 2,
            ..Default::default()
        });
        cfg
    };
    let cfg = shipping("ship_source");
    let node = node_with(cfg.clone(), app("counter"));
    for n in 1..=3 {
        assert_eq!(request(&node, "POST", "o", "/incr", "").await.json()["count"], n);
    }
    eventually("versions shipped", || shipped(3)).await;
    let local = object_dir(&cfg.app_dirs("counter")[0], "o").join("committed");
    eventually("shipped versions removed", || std::fs::read_dir(&local).unwrap().count() == 1).await;
    node.shutdown().await;
    drop(node);
    std::fs::remove_dir_all(&cfg.data_dir).unwrap();
    for (name, count) in [("ship_restored", 3), ("ship_restored_again", 4)] {
        let fresh = shipping(name);
        let objects = fresh.app_dirs("counter")[0].clone();
        let restored = nanovm::ship::restore(&Dir(store.clone()), "counter", "o", &objects, None, 0).await.unwrap();
        assert_eq!((restored.version, restored.head), (count, count));
        let node = node_with(fresh.clone(), app("counter"));
        assert_eq!(get(&node, "o", "/").await.json()["count"], count);
        assert_eq!(request(&node, "POST", "o", "/incr", "").await.json()["count"], count + 1);
        eventually("the next version shipped", || shipped(count + 1)).await;
        node.shutdown().await;
        drop(node);
        if name == "ship_restored" {
            std::fs::remove_dir_all(&fresh.data_dir).unwrap();
        }
    }
    let (head, _) = nanovm::ship::head(&Dir(store.clone()), "counter", "o").await.unwrap();
    assert_eq!((head.sequence, head.node.as_str(), head.lease), (5, "", 0));
}

/// With remote acknowledgement, a commit whose version can't be stored is
/// not acknowledged, though it stands here, and the object serves nothing
/// until it counts (N-725); and with the store down, writes go on until the
/// versions waiting here reach `max_mb`, then are refused, never versions
/// dropped; the backlog says what waits.
#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_store_withholds_acknowledgement_and_bounds_the_outbox() {
    for remote_ack in [true, false] {
        let mut cfg = node_cfg(if remote_ack { "ship_remote_ack" } else { "ship_outbox" }, 8);
        cfg.capture = Some(nanovm::config::CaptureConfig {
            // A store no write reaches.
            store: Some("/dev/null/store".into()),
            max_mb: 1,
            remote_ack,
            ..Default::default()
        });
        let node = node_with(cfg.clone(), app("counter"));
        let r = request(&node, "POST", "o", "/incr", "").await;
        let local = object_dir(&cfg.app_dirs("counter")[0], "o").join("committed");
        if remote_ack {
            assert_eq!(r.error.as_deref(), Some("commit-failed"), "{}", r.body);
            assert_eq!(std::fs::read_dir(&local).unwrap().count(), 1, "the commit stands here");
            assert_eq!(get(&node, "o", "/").await.error.as_deref(), Some("not-stored"), "served before it counts");
        } else {
            assert_eq!(r.json()["count"], 1);
            // Each version about 300 KiB: three fit in 1 MiB with the first.
            let value = |n: u8| String::from_utf8(vec![b'a' + n; 300 << 10]).unwrap();
            for n in 1..=3 {
                assert_eq!(request(&node, "PUT", "o", "/kv/k", &value(n)).await.status, 204, "write {n}");
            }
            let r = request(&node, "PUT", "o", "/kv/k", &value(4)).await;
            assert_eq!(r.error.as_deref(), Some("commit-failed"), "a full outbox refuses: {}", r.body);
            assert_eq!(get(&node, "o", "/kv/k").await.body, value(3));
            assert_eq!(std::fs::read_dir(&local).unwrap().count(), 4, "every unshipped version kept");
            let backlog = nanovm::ship::backlog(&cfg.data_dir.join("objects"));
            assert_eq!((backlog.versions, backlog.objects), (4, 1));
            assert!(backlog.bytes > 900 << 10, "{backlog:?}");
        }
        node.shutdown().await;
    }
}

/// Acknowledging remotely, a commit waits for the store (`ship::ACK`, 2 s)
/// whatever the app's own deadlines: committing is the node's work, not the
/// guest's (B-507). Here the store acknowledges after about a second, twice
/// the hostile app's post-return deadline: the write is answered 200
/// (before: 504, "deadline exceeded while post-return").
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_acknowledgement_is_not_cut_short_by_the_apps_deadline() {
    let mut cfg = node_cfg("slow_ack", 8);
    let store = cfg.data_dir.join("store");
    cfg.capture = Some(nanovm::config::CaptureConfig { store: Some(store.to_str().unwrap().into()), remote_ack: true, ..Default::default() });
    let node = node_with(cfg, app("hostile"));
    assert_eq!(get(&node, "o", "/commit?v=one").await.status, 200);
    // The store's next move of the head waits a second for its lock.
    let lock = std::fs::File::options().append(true).open(store.join("hostile/o/head.lock")).unwrap();
    lock.lock().unwrap();
    let held = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1000));
        drop(lock);
    });
    let t0 = Instant::now();
    let r = get(&node, "o", "/commit?v=two").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(t0.elapsed() >= Duration::from_millis(900), "answered before the store acknowledged: {:?}", t0.elapsed());
    held.join().unwrap();
    node.shutdown().await;
}

/// With the store down, a node that answers once a write is durable here
/// keeps answering writes, many more than the 16 versions it once held,
/// without a snapshot of the volume for each 16; once the store is back,
/// every version ships, in order, and the backlog empties. Another object
/// that stopped writing ships too.
#[tokio::test(flavor = "multi_thread")]
async fn writes_go_on_through_a_store_outage_and_ship_in_order_after() {
    use nanovm::ship::Dir;
    let base = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ship-outage");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let (store, down) = (base.join("store"), base.join("store.down"));
    let mut cfg = node_cfg("ship_outage", 8);
    cfg.capture = Some(nanovm::config::CaptureConfig { store: Some(store.to_str().unwrap().into()), ..Default::default() });
    let node = node_with(cfg.clone(), app("counter"));
    let head = |o: &str| {
        let bytes = std::fs::read(store.join("counter").join(o).join("head")).ok();
        bytes.map_or(0, |h| serde_json::from_slice::<nanovm::ship::Head>(&h).unwrap().sequence)
    };
    // A big file in the volume, which a snapshot would copy whole.
    assert_eq!(request(&node, "PUT", "o", "/kv/big", &"b".repeat(200 << 10)).await.status, 204);
    assert_eq!(request(&node, "POST", "o", "/incr", "").await.json()["count"], 1);
    // Shipped, and marked so (just after the head moves).
    let objects = cfg.data_dir.join("objects");
    eventually("shipped", || head("o") == 2 && nanovm::ship::backlog(&objects).versions == 0).await;
    // The store goes away: a file where its directory was.
    std::fs::rename(&store, &down).unwrap();
    std::fs::write(&store, "down").unwrap();
    for n in 2..=60 {
        let r = request(&node, "POST", "o", "/incr", "").await;
        assert_eq!(r.status, 200, "write {n} during the outage: {}", r.body);
        assert_eq!(r.json()["count"], n);
        if n == 3 {
            assert_eq!(request(&node, "POST", "p", "/incr", "").await.json()["count"], 1);
        }
    }
    let backlog = nanovm::ship::backlog(&objects);
    assert_eq!((backlog.versions, backlog.objects), (60, 2));
    println!("{} bytes wait to be shipped", backlog.bytes);
    // The store is back.
    std::fs::remove_file(&store).unwrap();
    std::fs::rename(&down, &store).unwrap();
    let t0 = Instant::now();
    while head("o") < 61 || head("p") < 1 || nanovm::ship::backlog(&objects).versions > 0 {
        assert!(t0.elapsed() < Duration::from_secs(20), "shipped {} and {} of 61 and 1", head("o"), head("p"));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // In order: each version in the store follows the one before. Of the
    // 16-version marks, only the first in the outage (17, with 15 waiting)
    // took a snapshot.
    let (head, _) = nanovm::ship::head(&Dir(store.clone()), "counter", "o").await.unwrap();
    let (mut digest, mut snapshots) = (head.digest.clone(), vec![]);
    for n in (1..=61).rev() {
        let bytes = std::fs::read(store.join(format!("counter/o/{n:020}-{digest}.tar"))).unwrap();
        let version = nanovm::committed::identity(&bytes[..]).unwrap();
        assert_eq!(version.sequence, n);
        snapshots.extend(version.snapshot.then_some(n));
        digest = version.parent;
    }
    assert_eq!(digest, "", "version 1 is the first");
    assert_eq!(snapshots, [17, 1]);
    node.shutdown().await;
    drop(node);
    // A fresh root restores all of it.
    let mut fresh = node_cfg("ship_outage_restored", 8);
    fresh.capture = cfg.capture.clone();
    nanovm::ship::restore(&Dir(store.clone()), "counter", "o", &fresh.app_dirs("counter")[0], None, 0).await.unwrap();
    let node = node_with(fresh, app("counter"));
    assert_eq!(get(&node, "o", "/").await.json()["count"], 60);
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn app_storage_has_one_local_owner() {
    let cfg = node_cfg("app_storage_has_one_local_owner", 2);
    let app = app("counter");
    let wasm = guest("counter");
    let first = Node::new(cfg.clone(), &app, &wasm, None).unwrap();
    let held = cfg.app_dirs(&app.name)[1].join("held");
    std::fs::create_dir_all(&held).unwrap();
    std::fs::write(held.join("live"), b"untouched").unwrap();
    let reject = || {
        let result = Node::new(cfg.clone(), &app, &wasm, None);
        assert!(result.is_err(), "two nodes admitted to the same app storage");
        assert!(result.err().unwrap().to_string().contains("app storage is already owned"));
        assert_eq!(std::fs::read(held.join("live")).unwrap(), b"untouched", "competing admission cleaned live scratch");
    };
    reject();
    let mut other = app.clone();
    other.name = "other".into();
    drop(Node::new(cfg.clone(), &other, &wasm, None).unwrap());
    let owner = first.app.clone();
    first.shutdown().await;
    reject();
    drop(first);
    reject();
    drop(owner);
    assert!(Node::new(cfg.clone(), &app, b"invalid component", None).is_err());
    let reopened = Node::new(cfg, &app, &wasm, None).unwrap();
    assert_eq!(get(&reopened, "object", "/").await.status, 200);
    reopened.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn volume_survives_eviction_memory_does_not() {
    let node = node("volume_survives_eviction", "counter", 2);
    for n in 1..=3 {
        let r = request(&node, "POST", "a", "/incr", "").await;
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(r.json()["count"], n);
        assert_eq!(r.json()["instance_hits"], n);
    }
    // Two more objects on a two-slot node push `a` out.
    for id in ["b", "c"] {
        settle(&node).await;
        assert_eq!(get(&node, id, "/").await.status, 200);
    }
    assert!(node.metrics.evicted_lru.load(Relaxed) >= 1);
    settle(&node).await;
    let r = get(&node, "a", "/").await;
    assert_eq!(r.json()["count"], 3, "volume kept");
    assert_eq!(r.json()["instance_hits"], 1, "fresh instance");
}

/// Every directory under `objects` (an app's), relative to it, but the
/// empty ones waiting in its pool (`~new`).
fn dirs_under(objects: &Path) -> Vec<String> {
    let mut found = vec![];
    let mut stack = vec![objects.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let rel = e.path().strip_prefix(objects).unwrap().to_string_lossy().into_owned();
            if e.file_type().unwrap().is_dir() && rel != "~new" {
                found.push(rel);
                stack.push(e.path());
            }
        }
    }
    found.sort();
    found
}

/// A scanner's 404s and reads of objects that don't exist leave nothing on
/// disk, while instances are live and after they are gone: an object's
/// directory appears with the first change it commits. The audit's 300
/// `wp-login.php` probes left 301 volume directories.
#[tokio::test(flavor = "multi_thread")]
async fn objects_that_change_nothing_leave_nothing() {
    let node = node("objects_that_change_nothing_leave_nothing", "counter", 1000);
    let objects = node.cfg.app_dirs("counter")[0].clone();
    for n in 0..300 {
        let r = get(&node, &format!("scan{n}"), "/wp-login.php").await;
        assert_eq!(r.status, 404, "{}", r.body);
    }
    for n in 0..20 {
        let r = get(&node, &format!("read{n}"), "/").await;
        assert_eq!((r.status, r.json()["count"].as_u64()), (200, Some(0)), "{}", r.body);
    }
    assert_eq!(dirs_under(&objects), Vec::<String>::new(), "while live");
    assert_eq!(request(&node, "POST", "w", "/incr", "").await.status, 200);
    let written = dirs_under(&objects);
    assert!(!written.is_empty() && written.iter().all(|d| !d.contains("scan") && !d.contains("read")), "{written:?}");
    node.shutdown().await;
    assert_eq!(dirs_under(&objects), written, "once gone");
    // The pool holds at most one empty directory per instance that was live at once.
    let pool = std::fs::read_dir(objects.join("~new")).map_or(0, |d| d.count());
    assert!(pool <= 321, "{pool}");
}

/// The ids of the objects with a directory under `objects` (an app's).
fn objects_in(objects: &Path) -> Vec<String> {
    let shards = std::fs::read_dir(objects).unwrap().flatten().filter(|s| s.file_name().len() == 2);
    let mut ids: Vec<_> = shards.flat_map(|s| std::fs::read_dir(s.path()).unwrap().flatten())
        .map(|o| o.file_name().to_string_lossy().into_owned()).collect();
    ids.sort();
    ids
}

/// A new object whose first request fails leaves nothing, nor does one
/// that empties its volume, once its instance ends; and only empty volumes
/// go.
#[tokio::test(flavor = "multi_thread")]
async fn failed_and_emptied_objects_leave_nothing() {
    let node = node("failed_and_emptied_objects_leave_nothing", "hostile", 8);
    let objects = node.cfg.app_dirs("hostile")[0].clone();
    // Writes a file and a key, then fails: rolled back when the instance ends.
    for then in ["panic", "fail", "error"] {
        let r = get(&node, &format!("failed-{then}"), &format!("/commit?v=x&then={then}")).await;
        assert!(r.status >= 500, "{then}: {}", r.status);
    }
    // A request that fails at 507 (`fail`) commits: it answered.
    eventually("failed instances to end", || objects_in(&objects) == ["failed-fail"]).await;
    assert_eq!(get(&node, "failed-panic", "/commit").await.json()["file"], "", "rolled back");
    assert_eq!(objects_in(&objects), ["failed-fail"], "only the one that answered kept its change");
    // An object that empties its volume goes when its instance ends.
    assert_eq!(get(&node, "emptied", "/files?mount=data&n=2").await.status, 200);
    assert_eq!(get(&node, "kept", "/files?mount=data&n=2").await.status, 200);
    assert_eq!(get(&node, "emptied", "/clean?mount=data&n=2").await.json()["removed"], 2);
    assert_eq!(get(&node, "kept", "/clean?mount=data&n=1").await.json()["removed"], 1);
    assert!(objects_in(&objects).contains(&"emptied".to_string()), "while its instance lives");
    node.shutdown().await;
    assert_eq!(objects_in(&objects), ["failed-fail", "kept"]);
}

/// An app has at most `max_objects_per_app` objects, however many clients
/// make new ones at once; reads of unknown objects are refused once it has
/// them (507 `object-limit`), existing objects are served, and an object
/// that goes makes room. Counted again when the app starts.
#[tokio::test(flavor = "multi_thread")]
async fn an_app_has_at_most_its_objects() {
    let mut cfg = node_cfg("an_app_has_at_most_its_objects", 64);
    cfg.max_objects_per_app = 5;
    let node = node_with(cfg.clone(), app("hostile"));
    let objects = node.cfg.app_dirs("hostile")[0].clone();
    let made = futures::future::join_all((0..40).map(|n| {
        let node = node.clone();
        async move { get(&node, &format!("o{n}"), "/files?mount=data&n=1").await.status }
    }))
    .await;
    assert_eq!(made.iter().filter(|s| **s == 200).count(), 5, "{made:?}");
    assert!(made.iter().all(|s| [200, 507].contains(s)), "{made:?}");
    let kept = objects_in(&objects);
    assert_eq!(kept.len(), 5, "{kept:?}");
    let r = get(&node, "another", "/").await;
    assert_eq!((r.status, r.error.as_deref()), (507, Some("object-limit")), "{}", r.body);
    assert!(r.body.contains("max_objects_per_app"), "{}", r.body);
    assert_eq!(get(&node, &kept[0], "/").await.status, 200, "existing objects are served");
    assert_eq!(objects_in(&objects), kept, "a refusal makes nothing");
    // One empties itself and its instance ends: there is room again.
    assert_eq!(get(&node, &kept[0], "/clean?mount=data&n=1").await.status, 200);
    assert_eq!(get(&node, &kept[0], "/panic").await.status, 500);
    eventually("the emptied object to go", || objects_in(&objects).len() == 4).await;
    // Its place is given back once its directory is removed, a moment later.
    let t0 = Instant::now();
    while get(&node, "another", "/files?mount=data&n=1").await.status != 200 {
        assert!(t0.elapsed() < Duration::from_secs(2), "no room made");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let r = get(&node, "one-more", "/files?mount=data&n=1").await;
    assert_eq!((r.status, r.error.as_deref()), (507, Some("object-limit")));
    node.shutdown().await;
    drop(node);
    // A node started on the same data counts them again.
    let node = node_with(cfg, app("hostile"));
    let r = get(&node, "one-more", "/").await;
    assert_eq!((r.status, r.error.as_deref()), (507, Some("object-limit")));
    assert_eq!(objects_in(&objects).len(), 5);
}

/// The audit's full disk: on a small filesystem, new objects are refused
/// (507 `disk-full`) once it has less free than a new object's quota, and
/// nothing is made for them; objects that exist are still served. Root
/// only: it mounts a tmpfs.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_disk_makes_no_new_objects() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("skipped: mounting a tmpfs needs root");
        return;
    }
    let mut cfg = node_cfg("a_full_disk_makes_no_new_objects", 64);
    let mnt = cfg.data_dir.join("small");
    std::fs::create_dir(&mnt).unwrap();
    let mounted = std::process::Command::new("mount").args(["-t", "tmpfs", "-o", "size=4m", "tmpfs"]).arg(&mnt).status().unwrap();
    assert!(mounted.success());
    let _unmount = scopeguard::guard(mnt.clone(), |m| _ = std::process::Command::new("umount").arg("-l").arg(m).status());
    std::os::unix::fs::symlink(cfg.data_dir.join("cache"), mnt.join("cache")).unwrap();
    cfg.data_dir = mnt;
    let node = node_with(cfg, app("counter"));
    let objects = node.cfg.app_dirs("counter")[0].clone();
    let mut refused = None;
    for n in 0..2000 {
        let r = request(&node, "POST", &format!("o{n}"), "/incr", "").await;
        if r.status != 200 {
            refused = Some((n, r));
            break;
        }
    }
    let (n, r) = refused.expect("the disk filled");
    eprintln!("a 4 MiB disk held {n} counters before the first refusal: {}", r.body);
    assert_eq!((r.status, r.error.as_deref()), (507, Some("disk-full")), "{}", r.body);
    let made = objects_in(&objects);
    assert_eq!(made.len(), n, "every object before the refusal, none after");
    for m in n..n + 100 {
        let r = request(&node, "POST", &format!("o{m}"), "/incr", "").await;
        assert_eq!(r.error.as_deref(), Some("disk-full"), "{}", r.body);
        assert_eq!(get(&node, &format!("r{m}"), "/").await.error.as_deref(), Some("disk-full"));
    }
    assert_eq!(objects_in(&objects), made, "nothing made while the disk is full");
    let r = get(&node, "o0", "/").await;
    assert_eq!((r.status, r.json()["count"].as_u64()), (200, Some(1)), "existing objects are served");
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn objects_do_not_share_volumes() {
    let node = node("objects_do_not_share_volumes", "counter", 8);
    assert_eq!(
        request(&node, "PUT", "x", "/kv/k", "secret").await.status,
        204
    );
    let r = get(&node, "x", "/kv/k").await;
    assert_eq!((r.status, r.body.as_str()), (200, "secret"));
    let r = get(&node, "y", "/kv/k").await;
    assert_eq!(r.status, 404, "{}", r.body);
    assert_eq!(request(&node, "DELETE", "x", "/kv/k", "").await.status, 204);
    assert_eq!(get(&node, "x", "/kv/k").await.status, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_routes() {
    let node = node("bad_routes", "counter", 8);
    for id in ["", "..", ".hidden", "a/b", "_nanovm", &"x".repeat(129)] {
        let r = get(&node, id, "/").await;
        assert_eq!((r.status, r.error.as_deref()), (400, Some("bad-object-id")), "{id:?}");
    }
}

// ---- concurrency model ------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn one_request_at_a_time_per_object() {
    let node = node("one_request_at_a_time", "hostile", 8);
    let reqs = (0..8).map(|_| {
        let node = node.clone();
        tokio::spawn(async move { get(&node, "serial", "/concurrency?ms=20").await })
    });
    for r in futures::future::join_all(reqs).await {
        let r = r.unwrap();
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(r.json()["max_active"], 1);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn objects_run_in_parallel() {
    let node = node("objects_run_in_parallel", "hostile", 16);
    let t0 = Instant::now();
    let reqs = (0..8).map(|i| {
        let node = node.clone();
        tokio::spawn(async move { get(&node, &format!("p{i}"), "/sleep?ms=300").await })
    });
    for r in futures::future::join_all(reqs).await {
        assert_eq!(r.unwrap().status, 200);
    }
    assert!(t0.elapsed() < Duration::from_millis(900), "took {:?}", t0.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
async fn full_queue_is_rejected() {
    let node = node("full_queue_is_rejected", "hostile", 8);
    let reqs = (0..80).map(|_| {
        let node = node.clone();
        tokio::spawn(async move { get(&node, "q", "/sleep?ms=10").await })
    });
    let mut ok = 0;
    let mut busy = 0;
    for r in futures::future::join_all(reqs).await {
        match r.unwrap() {
            r if r.status == 200 => ok += 1,
            r if r.error.as_deref() == Some("object-busy") => busy += 1,
            r => panic!("{} {:?} {}", r.status, r.error, r.body),
        }
    }
    assert!(ok >= 64, "ok={ok}");
    assert!(busy >= 1, "busy={busy}");
    assert_eq!(node.metrics.rejected_busy.load(Relaxed), busy);
}

// ---- per-object limits ------------------------------------------------

/// Runs one attack, checks the host killed the instance with `reason`, and
/// that the object comes back on a fresh instance.
async fn killed(node: &Arc<Node>, id: &str, path: &str, status: u16, reason: &str) -> Reply {
    let before = get(node, id, "/").await;
    assert_eq!(before.status, 200, "{:?} {}", before.error, before.body);
    let r = get(node, id, path).await;
    assert_eq!(
        (r.status, r.error.as_deref()),
        (status, Some(reason)),
        "{path}: {}",
        r.body
    );
    let after = get(node, id, "/").await;
    assert_eq!(after.status, 200, "{:?} {}", after.error, after.body);
    assert_eq!(after.json()["hits"], 1, "{path}: instance was replaced");
    r
}

/// A guest's response body is cut at `max_response_body_kb`: past it the
/// reader gets an error, never a body that looks whole.
#[tokio::test(flavor = "multi_thread")]
async fn response_bodies_are_capped() {
    let mut hostile = app("hostile");
    hostile.limits.max_response_body_kb = 1024;
    let node = node_with(node_cfg("response_bodies_are_capped", 4), hostile);
    let at_cap = get(&node, "a", "/big?mb=1").await;
    assert_eq!((at_cap.status, at_cap.body.len()), (200, 1 << 20));
    let past = get(&node, "a", "/big?mb=2").await;
    assert!(past.body.starts_with("<body error: length limit exceeded"), "{} bytes", past.body.len());
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_limit() {
    let node = node("memory_limit", "hostile", 8);
    let r = killed(&node, "mem", "/alloc?mb=1024", 500, "memory").await;
    assert!(r.body.contains("32MiB"), "{}", r.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn cpu_limit() {
    let node = node("cpu_limit", "hostile", 8);
    let r = killed(&node, "cpu", "/spin", 500, "cpu").await;
    // 100 ms budget: killed well before the 1 s wall-clock limit.
    assert!(r.elapsed < Duration::from_millis(800), "took {:?}", r.elapsed);
}

/// Regression: CPU charged to the killed request used to carry over into the
/// next instantiation, so an epoch tick during it killed the new instance
/// at birth (503 instantiate-failed). A 1 ms tick makes that tick likely.
#[tokio::test(flavor = "multi_thread")]
async fn cpu_kill_does_not_poison_next_instance() {
    let mut cfg = node_cfg("cpu_kill_does_not_poison_next_instance", 8);
    cfg.epoch_tick_ms = 1;
    let node = node_with(cfg, app("hostile"));
    for i in 0..60 {
        let r = get(&node, "k", "/spin").await;
        assert_eq!(r.error.as_deref(), Some("cpu"), "{}", r.body);
        let r = get(&node, "k", "/").await;
        assert_eq!(r.status, 200, "round {i}: {:?} {}", r.error, r.body);
    }
}

/// Regression: instantiation was charged wall time since its slot was
/// acquired, including host work before the guest ran (scanning `/data`),
/// so an epoch tick during start-up could kill a new instance that had
/// used no CPU. Here the scan alone outlasts the 10 ms budget.
#[tokio::test(flavor = "multi_thread")]
async fn start_up_is_charged_guest_cpu_not_host_work() {
    let mut cfg = node_cfg("start_up_is_charged_guest_cpu", 8);
    cfg.epoch_tick_ms = 1;
    let vol = object_dir(&cfg.data_dir.join("objects/hostile"), "s").join("data");
    std::fs::create_dir_all(&vol).unwrap();
    for i in 0..20_000 {
        std::fs::write(vol.join(format!("f{i}")), b"").unwrap();
    }
    let mut hostile = app("hostile");
    hostile.limits.cpu_ms = 10;
    let node = node_with(cfg, hostile);
    for i in 0..30 {
        let r = get(&node, "s", "/").await;
        assert_eq!(r.status, 200, "round {i}: {:?} {}", r.error, r.body);
        assert_eq!(get(&node, "s", "/panic").await.status, 500);
    }
}

/// CPU time of the calling thread.
#[cfg(target_os = "linux")]
fn thread_cpu() -> Duration {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    assert_eq!(unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut t) }, 0);
    Duration::new(t.tv_sec as u64, t.tv_nsec as u32)
}

/// Regression (N-640): the budget charged wall time inside each poll, so a
/// guest whose thread was waiting for a core was charged for the wait, and
/// an overloaded node killed honest requests for CPU. Here the node shares
/// one core with seven busy threads: a request that needs a third of its
/// budget succeeds however long it takes, and one that spins forever is
/// still killed after about its budget of CPU.
#[cfg(target_os = "linux")]
#[test]
fn cpu_budget_is_cpu_not_time_waiting_for_a_core() {
    std::thread::spawn(|| {
        // This thread, and the threads it starts (the node's runtime and
        // epoch ticker, the busy threads), run on one CPU.
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            assert_eq!(libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set), 0);
            let cpu = (0..libc::CPU_SETSIZE as usize).rev().find(|&c| libc::CPU_ISSET(c, &set)).unwrap();
            libc::CPU_ZERO(&mut set);
            libc::CPU_SET(cpu, &mut set);
            assert_eq!(libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set), 0);
        }
        // One thread runs the whole node, so its CPU time is the node's.
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let budget = Duration::from_millis(100);
            let mut hostile = app("hostile");
            hostile.limits.cpu_ms = budget.as_millis() as u64;
            hostile.limits.request_timeout_ms = 30_000;
            let node = node_with(node_cfg("cpu_budget_is_cpu", 8), hostile);
            let measured = async |path: &str| {
                let (cpu, wall) = (thread_cpu(), Instant::now());
                let r = get(&node, "o", path).await;
                (r, thread_cpu() - cpu, wall.elapsed())
            };
            assert_eq!(get(&node, "o", "/").await.status, 200);
            // Steps that take about a third of the budget, timed alone.
            let probe = 1u64 << 22;
            let (r, cpu, _) = measured(&format!("/spin?n={probe}")).await;
            assert_eq!(r.status, 200, "{:?} {}", r.error, r.body);
            let steps = (probe as f64 * budget.as_secs_f64() / 3.0 / cpu.as_secs_f64()) as u64;

            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let busy: Vec<_> = (0..7)
                .map(|_| {
                    let stop = stop.clone();
                    std::thread::spawn(move || while !stop.load(Relaxed) { std::hint::spin_loop() })
                })
                .collect();
            let (r, cpu, wall) = measured(&format!("/spin?n={steps}")).await;
            let (spun, spun_cpu, spun_wall) = measured("/spin").await;
            stop.store(true, Relaxed);
            busy.into_iter().for_each(|b| b.join().unwrap());

            println!("{steps} steps: {} after {cpu:?} of CPU in {wall:?}; spinning: {} after {spun_cpu:?} in {spun_wall:?}",
                r.status, spun.status);
            assert!(wall > 3 * cpu, "no contention: {cpu:?} of CPU in {wall:?}");
            assert_eq!(r.status, 200, "{:?} {} after {cpu:?} of CPU in {wall:?}", r.error, r.body);
            assert_eq!(spun.error.as_deref(), Some("cpu"), "{}", spun.body);
            assert!(
                spun_cpu > budget * 9 / 10 && spun_cpu < budget * 3 / 2,
                "killed after {spun_cpu:?} of CPU in {spun_wall:?}"
            );
            node.shutdown().await;
        });
    })
    .join()
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn wall_clock_limit() {
    let node = node("wall_clock_limit", "hostile", 8);
    let r = killed(&node, "slow", "/sleep?ms=60000", 504, "timeout").await;
    assert!(
        r.elapsed >= Duration::from_millis(1000) && r.elapsed < Duration::from_millis(2500),
        "took {:?}",
        r.elapsed
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn guest_trap() {
    let node = node("guest_trap", "hostile", 8);
    killed(&node, "boom", "/panic", 500, "trap").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn resource_table_limit() {
    let node = node("resource_table_limit", "hostile", 8);
    killed(&node, "leak", "/leak", 500, "resource-table").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn kills_do_not_disturb_neighbours() {
    let node = node("kills_do_not_disturb_neighbours", "hostile", 16);
    let attacks: Vec<_> = ["/spin", "/alloc?mb=1024", "/panic", "/sleep?ms=60000", "/leak"]
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            let node = node.clone();
            tokio::spawn(async move { get(&node, &format!("bad{i}"), p).await })
        })
        .collect();
    let t0 = Instant::now();
    let mut n = 0;
    while t0.elapsed() < Duration::from_millis(1200) {
        let r = get(&node, "canary", "/").await;
        assert_eq!(r.status, 200, "{}", r.body);
        n += 1;
    }
    for a in attacks {
        assert!(a.await.unwrap().status >= 500);
    }
    assert!(n > 100, "canary only got {n} requests through");
}

#[tokio::test(flavor = "multi_thread")]
async fn cpu_hogs_leave_cores_for_short_requests() {
    // Hogs that take turns on two lanes of a loaded host get their 100 ms
    // of CPU after more than hostile's 1 s deadline: the budget, not the
    // deadline, is under test.
    let mut hostile = app("hostile");
    hostile.limits.request_timeout_ms = 10_000;
    let node = node_with(node_cfg("cpu_hogs_leave_cores_for_short_requests", 128), hostile);
    let cores = std::thread::available_parallelism().unwrap().get();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Four spinners per core: without heavy lanes they hold every worker.
    let hogs: Vec<_> = (0..4 * cores)
        .map(|i| {
            let (node, stop) = (node.clone(), stop.clone());
            tokio::spawn(async move {
                while !stop.load(Relaxed) {
                    let r = get(&node, &format!("hog{i}"), "/spin").await;
                    assert_eq!(r.error.as_deref(), Some("cpu"), "{}", r.body);
                }
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut lat = Vec::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(1000) {
        let r = get(&node, "canary", "/").await;
        assert_eq!(r.status, 200, "{}", r.body);
        lat.push(r.elapsed);
    }
    stop.store(true, Relaxed);
    for h in hogs {
        h.await.unwrap();
    }
    lat.sort();
    let p99 = lat[lat.len() * 99 / 100];
    assert!(lat.len() > 200, "canary only got {} requests through", lat.len());
    assert!(
        p99 < Duration::from_millis(50),
        "canary p99 {p99:?} with {} hogs",
        4 * cores
    );
}

/// A guest that has used a tick lets the other requests on its thread run:
/// on a one-thread runtime, a short request sent while a spinner is past its
/// first 10 ms (when it must take a heavy lane) finishes long before the
/// spinner runs out of CPU.
#[tokio::test(flavor = "current_thread")]
async fn a_spinner_yields_its_thread_at_each_tick() {
    let node = node("a_spinner_yields_its_thread_at_each_tick", "hostile", 8);
    for id in ["canary", "spinner"] {
        assert_eq!(get(&node, id, "/").await.status, 200);
    }
    let t0 = Instant::now();
    // Spawned: the test's own future runs only once the queue is empty.
    let timed = |id: &'static str, path: &'static str| {
        let node = node.clone();
        tokio::spawn(async move { (get(&node, id, path).await, t0.elapsed()) })
    };
    let spinner = timed("spinner", "/spin");
    let n = node.clone();
    let canary = tokio::spawn(async move {
        // Without a timer, which this runtime checks only now and then: the
        // loop runs only while the spinner lets the thread go.
        while t0.elapsed() < Duration::from_millis(30) {
            let mut yielded = false;
            std::future::poll_fn(|cx| {
                if std::mem::replace(&mut yielded, true) {
                    return std::task::Poll::Ready(());
                }
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            })
            .await;
        }
        (get(&n, "canary", "/").await, t0.elapsed())
    });
    let ((canary, canary_at), (spun, spun_at)) = (canary.await.unwrap(), spinner.await.unwrap());
    assert_eq!(spun.error.as_deref(), Some("cpu"), "{}", spun.body);
    assert_eq!(canary.status, 200, "{}", canary.body);
    assert!(
        canary_at < spun_at && canary.elapsed < Duration::from_millis(50),
        "the canary took {:?} and was done at {canary_at:?}, the spinner at {spun_at:?}",
        canary.elapsed
    );
}

// ---- filesystem layout and quotas -------------------------------------

/// `hostile` with room for heavy file work: writes and creates run inline
/// and count against the CPU budget, which on a loaded host (the full suite)
/// can pass 100 ms, and their syncs can take seconds. These tests are about
/// quotas, not limits.
fn filler() -> AppConfig {
    let mut hostile = app("hostile");
    hostile.limits.cpu_ms = 1_000;
    hostile.limits.request_timeout_ms = 10_000;
    hostile
}

#[tokio::test(flavor = "multi_thread")]
async fn file_count_limit() {
    let node = node_with(node_cfg("file_count_limit", 8), filler());
    let r = get(&node, "f", "/files?mount=tmp").await;
    assert_eq!(r.status, 507, "{}", r.body);
    assert_eq!(r.json()["created"], 200);
    assert_eq!(r.json()["error"], "quota");
}

#[tokio::test(flavor = "multi_thread")]
async fn disk_quota_and_tmp_wipe() {
    let node = node_with(node_cfg("disk_quota_and_tmp_wipe", 8), filler());
    let fill = async |name: &str, mount: &str, mb: u32| {
        get(
            &node,
            "t",
            &format!("/fill?mount={mount}&mb={mb}&name={name}"),
        )
        .await
    };
    // /tmp and /data share one 8 MiB budget.
    assert_eq!(fill("a", "tmp", 4).await.status, 200);
    assert_eq!(fill("b", "data", 4).await.status, 200);
    let r = fill("c", "tmp", 1).await;
    assert_eq!(r.status, 507, "{}", r.body);
    assert_eq!(r.json()["error"], "insufficient-space");
    // A new instance gets an empty /tmp, which frees its share.
    assert_eq!(get(&node, "t", "/panic").await.status, 500);
    assert_eq!(fill("a", "tmp", 4).await.status, 200);
    // and the old one's scratch directory, which it left full, is gone from
    // the host: the one there holds only the new instance's file.
    let scratch = node.cfg.data_dir.join("tmp/hostile");
    let dirs = entries(&scratch);
    assert_eq!(dirs.len(), 1, "{dirs:?}");
    assert_eq!(entries(&scratch.join(&dirs[0])), ["a"]);
}

/// The names in directory `dir`.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    names
}

/// An instance that leaves its `/tmp` empty hands the scratch directory to
/// the next instance, of whichever object (making and removing one per
/// instance cost more than the rest of a cold start's file work); one that
/// leaves anything there has its directory removed, so every instance
/// starts with an empty `/tmp`.
#[tokio::test(flavor = "multi_thread")]
async fn only_empty_scratch_is_reused() {
    let node = node_with(node_cfg("only_empty_scratch_is_reused", 8), filler());
    let scratch = node.cfg.data_dir.join("tmp/hostile");
    assert_eq!(get(&node, "t", "/files?mount=tmp&n=2").await.status, 200);
    assert_eq!(get(&node, "t", "/clean?mount=tmp&n=2").await.json()["removed"], 2);
    let first = entries(&scratch);
    assert_eq!(first.len(), 1, "{first:?}");
    // The reply to a trap comes before the instance is gone.
    let gone = || node.metrics.stores_live.load(Relaxed) == 0;
    assert_eq!(get(&node, "t", "/panic").await.status, 500);
    eventually("t's instance gone", gone).await;
    assert_eq!(get(&node, "u", "/files?mount=tmp&n=1").await.status, 200);
    assert_eq!(entries(&scratch), first, "u's instance reuses t's directory");
    assert_eq!(entries(&scratch.join(&first[0])), ["f0"]);
    // u leaves `f0` behind, so its directory is removed, not handed on.
    assert_eq!(get(&node, "u", "/panic").await.status, 500);
    eventually("u's instance gone", gone).await;
    assert_eq!(get(&node, "v", "/").await.status, 200);
    let now = entries(&scratch);
    assert!(now.len() == 1 && now != first, "{now:?}");
    assert!(entries(&scratch.join(&now[0])).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn rename_quota_accounting() {
    let node = node("rename_quota_accounting", "hostile", 8);
    for name in ["a", "b"] {
        let r = get(&node, "r", &format!("/fill?mount=tmp&mb=4&name={name}")).await;
        assert_eq!(r.status, 200, "{}", r.body);
    }
    // Renaming a file onto itself is a no-op and must not credit the quota.
    let r = get(&node, "r", "/rename?mount=tmp&from=a&n=10").await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = get(&node, "r", "/fill?mount=tmp&mb=1&name=c").await;
    assert_eq!(r.status, 507, "{}", r.body);
    assert_eq!(r.json()["error"], "insufficient-space");
    // Renaming `c` (empty, from the refused fill) over `a` frees a's 4 MiB.
    let r = get(&node, "r", "/rename?mount=tmp&from=c&to=a").await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = get(&node, "r", "/fill?mount=tmp&mb=4&name=d").await;
    assert_eq!(r.status, 200, "{}", r.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn data_quota_survives_restart() {
    let node = node_with(node_cfg("data_quota_survives_restart", 8), filler());
    for name in ["a", "b"] {
        let r = get(&node, "d", &format!("/fill?mount=data&mb=4&name={name}")).await;
        assert_eq!(r.status, 200, "{}", r.body);
    }
    assert_eq!(get(&node, "d", "/panic").await.status, 500);
    // The new instance rescans the volume: it is still full.
    let r = get(&node, "d", "/fill?mount=data&mb=1&name=c").await;
    assert_eq!(r.status, 507, "{}", r.body);
    assert_eq!(r.json()["error"], "insufficient-space");
    let vol = object_dir(&node.cfg.data_dir.join("objects/hostile"), "d").join("data");
    assert_eq!(std::fs::metadata(vol.join("a")).unwrap().len(), 4 << 20);
    assert!(node.metrics.fs_denied.load(Relaxed) >= 1);
}

/// `hostile` with room for file work (see `filler`) and a 2 MiB quota: a
/// new object needs that much free on the small disk below.
fn small_filler() -> AppConfig {
    let mut f = filler();
    f.limits.disk_mb = 2;
    f
}

/// A supervisor whose data is on a tmpfs of `mb` MiB, as the audit's 8 MiB
/// root was, with `app_disk_mb` and `disk_reserve_mb`; why not, unless root.
async fn on_a_small_disk(test: &str, mb: u64, budget: u64, reserve: u64) -> Result<(Arc<nanovm::supervisor::Supervisor>, std::net::SocketAddr, SmallDisk), String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("mounting a tmpfs needs root".into());
    }
    let mut cfg = node_cfg(test, 8);
    let cache = std::fs::read_link(cfg.data_dir.join("cache")).unwrap();
    let mounted = std::process::Command::new("mount").args(["-t", "tmpfs", "-o", &format!("size={mb}m"), "tmpfs"]).arg(&cfg.data_dir).status();
    if !mounted.as_ref().is_ok_and(|s| s.success()) {
        return Err(format!("mount -t tmpfs failed: {mounted:?}"));
    }
    let disk = SmallDisk(cfg.data_dir.clone());
    std::os::unix::fs::symlink(cache, cfg.data_dir.join("cache")).unwrap();
    (cfg.app_disk_mb, cfg.disk_reserve_mb) = (budget, reserve);
    let (sup, addr) = supervised_with(cfg).await;
    Ok((sup, addr, disk))
}

/// A tmpfs, unmounted when dropped.
struct SmallDisk(PathBuf);

impl Drop for SmallDisk {
    fn drop(&mut self) {
        let _ = std::process::Command::new("umount").arg("-l").arg(&self.0).status();
    }
}

/// The `fs-denied` events the supervisor at `addr` has had from its workers.
async fn denials(addr: std::net::SocketAddr) -> Vec<String> {
    stats_json(addr).await;
    let logs = captures().lock()[&addr].1.lock().logs.clone();
    logs.iter().filter(|l| l["kind"] == "fs-denied").map(|l| l["detail"].as_str().unwrap_or_default().to_owned()).collect()
}

/// An app that fills its disk budget has its own writes refused, with a
/// status that says so, and no other app's: on the audit's 8 MiB disk,
/// where one app's objects stopped every write on the node.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_app_budget_refuses_only_that_apps_writes() {
    let (_sup, addr, _disk) = match on_a_small_disk("a_full_app_budget_refuses_only_that_apps_writes", 8, 3, 1).await {
        Ok(s) => s,
        Err(why) => return eprintln!("skipped: {why}"),
    };
    let (a, b) = (publish("budget-a", &small_filler()).await, publish("budget-b", &small_filler()).await);
    let fill = async |app: &str, object: &str, name: &str, mb: u32| {
        let (status, body) = app_get(addr, app, &format!("/{object}/fill?mount=data&mb={mb}&name={name}")).await;
        (status, serde_json::from_str::<serde_json::Value>(&body).unwrap_or_default())
    };
    assert_eq!(fill(&a, "o1", "fill.bin", 2).await.0, 200);
    // Another object of the same app gets what is left of its 3 MiB.
    let (status, body) = fill(&a, "o2", "f", 2).await;
    assert_eq!((status, body["error"].as_str(), body["written"].as_u64()), (507, Some("insufficient-space"), Some(1 << 20)), "{body}");
    assert!(denials(addr).await.iter().any(|d| d.contains("the app's 3 MiB disk budget is full")));
    // The other app's writes go on.
    let (status, body) = fill(&b, "o1", "f", 2).await;
    assert_eq!(status, 200, "{body}");
    // Freeing an object's space lets the app write again.
    assert_eq!(app_get(addr, &a, "/o1/clean?mount=data").await.0, 200);
    let (status, body) = fill(&a, "o2", "g", 1).await;
    assert_eq!(status, 200, "{body}");
}

/// Below the node's free-space reserve every app's writes that need more
/// space are refused cleanly (507 from the app, not a failed commit), while
/// writes in place still commit; freeing space ends it. A new object isn't
/// started once the disk has less free than its quota above the reserve
/// (objects' room check, with limits' reserve).
#[tokio::test(flavor = "multi_thread")]
async fn the_disk_reserve_keeps_writes_committing() {
    let (_sup, addr, _disk) = match on_a_small_disk("the_disk_reserve_keeps_writes_committing", 8, 100, 3).await {
        Ok(s) => s,
        Err(why) => return eprintln!("skipped: {why}"),
    };
    let (files, counter) = (publish("reserve-files", &small_filler()).await, publish("reserve-counter", &app("counter")).await);
    let (status, body) = app_get(addr, &files, "/mine/fill?mount=data&mb=1&name=x").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(raw(addr, &format!("POST /c0/incr HTTP/1.1\r\nHost: x\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n", names(&counter)), b"").await.0, 200);
    // Objects that exist, each with room under its 2 MiB quota to grow by 1 MiB.
    for g in 0..4 {
        assert_eq!(app_get(addr, &files, &format!("/g{g}/fill?mount=data&mb=0")).await.0, 200);
    }
    // New objects of 2 MiB fill the disk until one hasn't its quota free
    // above the reserve: it is refused before it runs.
    let mut hogs = 0;
    let body = loop {
        let (status, body) = app_get(addr, &files, &format!("/hog{hogs}/fill?mount=data&mb=2")).await;
        hogs += 1;
        if status != 200 {
            assert_eq!(status, 507, "{body}");
            break body;
        }
    };
    assert!(body.contains("disk-full") && body.contains("above the node's 3.0 MiB reserve"), "{body}");
    // Objects that exist grow down to the reserve, then are refused by it:
    // with less than the reserve and 2 MiB free, at most two grow.
    let refused = 'grow: {
        for g in 0..4 {
            let (status, body) = app_get(addr, &files, &format!("/g{g}/fill?mount=data&mb=1")).await;
            if status != 200 {
                break 'grow (status, body);
            }
        }
        panic!("four objects grew by 1 MiB with less than the reserve and 2 MiB free");
    };
    assert!(refused.0 == 507 && refused.1.contains("insufficient-space"), "{refused:?}");
    assert!(denials(addr).await.iter().any(|d| d.contains("the node's disk is below its 3.0 MiB reserve")));
    // Every app's growth is refused by the app, as on a full disk, and a new
    // object before it runs: never a failed commit, as the audit saw on
    // every write.
    let incr = async |object: &str| {
        raw(addr, &format!("POST /{object}/incr HTTP/1.1\r\nHost: x\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n", names(&counter)), b"").await
    };
    let (status, body) = incr("c0").await;
    assert!(status == 507 && body.contains("insufficient-space"), "{status} {body}");
    for i in 1..20 {
        let (status, body) = incr(&format!("c{i}")).await;
        assert!(status == 507 && body.contains("disk-full"), "{status} {body}");
    }
    // A write in place still commits.
    let (status, body) = app_get(addr, &files, "/mine/fill?mount=data&mb=1&name=x").await;
    assert_eq!(status, 200, "{body}");
    // The hogs made (the last was refused, so never made), and what grew.
    for object in (0..hogs - 1).map(|h| format!("hog{h}")).chain((0..4).map(|g| format!("g{g}"))) {
        assert_eq!(app_get(addr, &files, &format!("/{object}/clean?mount=data")).await.0, 200);
    }
    let (status, body) = incr("c1").await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_escapes_blocked() {
    let node = node("sandbox_escapes_blocked", "hostile", 8);
    let r = get(&node, "e", "/escape").await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(!r.body.contains("LEAKED") && !r.body.contains("OPENED"), "{}", r.body);
}

/// `/data` and `/tmp` are separate filesystems to the guest: a rename
/// between them is refused as cross-device, as POSIX does.
#[tokio::test(flavor = "multi_thread")]
async fn renames_between_mounts_are_cross_device() {
    let node = node("renames_between_mounts_are_cross_device", "hostile", 8);
    let r = get(&node, "r", "/cross-rename").await;
    assert_eq!(r.json()["error"], "cross-device", "{}", r.body);
    assert!(!object_dir(&node.cfg.data_dir.join("objects/hostile"), "r").join("data/x.txt").exists());
}

// ---- egress -----------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn egress_unlisted_host_denied() {
    let node = node("egress_unlisted_host_denied", "hostile", 8);
    let r = get(
        &node,
        "n",
        "/fetch?url=http://169.254.169.254/latest/meta-data/",
    )
    .await;
    assert_eq!(r.status, 502, "{}", r.body);
    assert!(
        r.json()["denied"].as_str().unwrap().contains("HttpRequestDenied"),
        "{}",
        r.body
    );
    assert!(node.metrics.egress_denied.load(Relaxed) >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn egress_allowed_request_round_trips() {
    use http_body_util::BodyExt;
    use prost::Message;
    use opentelemetry_proto::tonic::{collector::trace::v1::ExportTraceServiceRequest, common::v1::any_value::Value};
    let spans = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = node_cfg("egress_allowed_request_round_trips", 8);
    cfg.telemetry_endpoint = format!("http://{}", sink.local_addr().unwrap());
    let captured = spans.clone();
    let leaked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let leak = leaked.clone();
    let collector = tokio::spawn(nanovm::server::serve_http1(sink, move |req| {
        let captured = captured.clone();
        let leak = leak.clone();
        async move {
            let trace = req.uri().path() == "/v1/traces";
            let body = req.into_body().collect().await.unwrap().to_bytes();
            if body.windows(7).any(|s| s == b"PRIVATE") { leak.store(true, Relaxed); }
            if trace {
                captured.lock().extend(ExportTraceServiceRequest::decode(body).unwrap().resource_spans
                    .into_iter().flat_map(|r| r.scope_spans).flat_map(|s| s.spans));
            }
            nanovm::server::ok_json(serde_json::json!({}))
        }
    }));
    // In dev mode an IP-literal allowlist entry may be private; this
    // exercises the whole send path.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let contexts = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let received = contexts.clone();
    let server = tokio::spawn(nanovm::server::serve_http1(listener, move |req| {
        let received = received.clone();
        async move {
            if req.uri().path() == "/stall" { return std::future::pending().await; }
            received.lock().push(req.headers().get("traceparent").and_then(|v| v.to_str().ok()).unwrap_or("").to_owned());
            let status = req.uri().path().trim_start_matches('/').parse::<u16>().unwrap();
            nanovm::object::error_response(status, "PRIVATE", "hello")
        }
    }));
    let mut hostile = app("hostile");
    hostile.limits.egress.push(format!("127.0.0.1:{port}"));
    hostile.limits.request_timeout_ms = 300;
    let node = node_with(cfg, hostile);
    for status in [201, 404, 503] {
        let r = get(&node, "n", &format!("/fetch?url=http://127.0.0.1:{port}/{status}?secret=PRIVATE")).await;
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(r.json()["status"], status);
    }
    let denied = get(&node, "n", "/fetch?url=http://127.0.0.1:1/denied?secret=PRIVATE").await;
    assert!(denied.body.contains("HttpRequestDenied"));
    let timeout = get(&node, "n", &format!("/fetch?url=http://127.0.0.1:{port}/stall")).await;
    assert_eq!(timeout.status, 504);
    node.shutdown().await;
    let telemetry = node.telemetry.clone();
    tokio::task::spawn_blocking(move || telemetry.flush().unwrap()).await.unwrap();
    let spans = spans.lock();
    let attr = |s: &opentelemetry_proto::tonic::trace::v1::Span, key: &str| {
        s.attributes.iter().find(|a| a.key == key).and_then(|a| a.value.as_ref()).and_then(|v| v.value.clone())
    };
    let clients: Vec<_> = spans.iter().filter(|s| s.kind == 3).collect();
    assert_eq!(clients.len(), 5, "every external call, including denial and cancellation, has a CLIENT span");
    for (i, status) in [201, 404, 503].into_iter().enumerate() {
        let client = clients.iter().find(|s| attr(s, "url.path") == Some(Value::StringValue(format!("/{status}")))).unwrap();
        let parent = spans.iter().find(|s| s.span_id == client.parent_span_id).unwrap();
        assert_eq!(parent.trace_id, client.trace_id);
        assert_eq!(parent.status.as_ref().map_or(0, |s| s.code), 0, "handled dependency failures don't fail the caller");
        assert_eq!(attr(client, "http.response.status_code"), Some(Value::IntValue(status)));
        assert_eq!(attr(client, "server.address"), Some(Value::StringValue("127.0.0.1".into())));
        assert_eq!(client.status.as_ref().map_or(0, |s| s.code), if status >= 400 { 2 } else { 0 });
        assert_eq!(attr(client, "error.type"), if status >= 400 { Some(Value::StringValue(status.to_string())) } else { None });
        let trace = opentelemetry::trace::TraceId::from_bytes(client.trace_id.clone().try_into().unwrap());
        let span = opentelemetry::trace::SpanId::from_bytes(client.span_id.clone().try_into().unwrap());
        assert_eq!(contexts.lock()[i], format!("00-{trace}-{span}-01"));
    }
    let denied = clients.iter().find(|s| attr(s, "url.path") == Some(Value::StringValue("/denied".into()))).unwrap();
    assert_eq!(denied.status.as_ref().unwrap().code, 2);
    let cancelled = clients.iter().find(|s| attr(s, "url.path") == Some(Value::StringValue("/stall".into()))).unwrap();
    assert_eq!(attr(cancelled, "error.type"), Some(Value::StringValue("cancelled".into())));
    assert_eq!(cancelled.status.as_ref().unwrap().code, 2);
    let parent = spans.iter().find(|s| s.span_id == cancelled.parent_span_id).unwrap();
    assert_eq!(parent.trace_id, cancelled.trace_id);
    assert_eq!(attr(parent, "error.type"), Some(Value::StringValue("timeout".into())));
    assert!(!leaked.load(Relaxed), "query secrets must not appear in spans or runtime logs");
    server.abort();
    collector.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn egress_listed_name_resolving_to_loopback_denied() {
    let node = node("egress_loopback_denied", "hostile", 8);
    // Something is listening, so only the IP filter can stop the request.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let r = get(&node, "n", &format!("/fetch?url=http://localhost:{port}/")).await;
    assert_eq!(r.status, 502, "{}", r.body);
    assert!(
        r.json()["denied"].as_str().unwrap().contains("DestinationIpProhibited"),
        "{}",
        r.body
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the internet"]
async fn egress_listed_host_allowed() {
    let node = node("egress_listed_host_allowed", "hostile", 8);
    let r = get(&node, "n", "/fetch?url=https://example.com/").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["status"], 200);
}

/// An app reaches only what both its list and the operator's allow, in
/// process and through the supervisor: one asking for every port of a host
/// gets the one port the operator lists.
#[tokio::test(flavor = "multi_thread")]
async fn egress_is_capped_by_the_operator() {
    let mut ports = vec![];
    for _ in 0..2 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        ports.push(listener.local_addr().unwrap().port());
        tokio::spawn(nanovm::server::serve_http1(listener, |_| async { nanovm::server::ok_json(serde_json::json!({})) }));
    }
    let mut hostile = app("hostile");
    hostile.limits.egress = vec!["127.0.0.1".into()];
    let capped = |test: &str| {
        let mut cfg = node_cfg(test, 8);
        cfg.limits.egress = vec![format!("127.0.0.1:{}", ports[0])];
        cfg
    };
    let node = node_with(capped("egress_capped_in_process"), hostile.clone());
    let (_sup, addr) = supervised_with(capped("egress_capped_supervised")).await;
    let published = publish("egress_capped", &hostile).await;
    for (port, allowed) in [(ports[0], true), (ports[1], false)] {
        let path = format!("/fetch?url=http://127.0.0.1:{port}/");
        let r = get(&node, "n", &path).await;
        let (status, body) = app_get(addr, &published, &format!("/n{path}")).await;
        for (status, body) in [(r.status, r.body), (status, body)] {
            match allowed {
                true => assert_eq!(status, 200, "{body}"),
                false => assert!(status == 502 && body.contains("HttpRequestDenied"), "{status} {body}"),
            }
        }
    }
}

// ---- eviction and capacity --------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn lru_eviction_keeps_node_under_capacity() {
    let node = node("lru_eviction", "counter", 4);
    for i in 0..10 {
        settle(&node).await;
        let r = get(&node, &format!("o{i}"), "/").await;
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(node.live_objects() <= 4);
    }
    assert!(node.metrics.evicted_lru.load(Relaxed) >= 6);
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_objects_are_evicted() {
    let mut counter = app("counter");
    counter.limits.idle_timeout_ms = 100;
    let node = node_with(node_cfg("idle_objects_are_evicted", 8), counter);
    assert_eq!(get(&node, "i", "/").await.status, 200);
    eventually("idle eviction", || node.live_objects() == 0).await;
    assert_eq!(node.metrics.evicted_idle.load(Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn at_capacity_with_no_idle_object() {
    let node = node("at_capacity_with_no_idle_object", "hostile", 2);
    let busy: Vec<_> = (0..2)
        .map(|i| {
            let node = node.clone();
            tokio::spawn(async move { get(&node, &format!("b{i}"), "/sleep?ms=500").await })
        })
        .collect();
    eventually("both objects busy", || node.phases().get("handling") == Some(&2)).await;
    let r = get(&node, "late", "/").await;
    assert_eq!((r.status, r.error.as_deref()), (503, Some("node-at-capacity")));
    for b in busy {
        assert_eq!(b.await.unwrap().status, 200);
    }
    settle(&node).await;
    assert_eq!(get(&node, "late", "/").await.status, 200);
}

/// Regression: an evicted instance frees its pool slot only after its actor
/// exits, so churning through new objects used to fail instantiation. Now a
/// new store waits for a slot, so live stores never pass `max_live_objects`.
#[tokio::test(flavor = "multi_thread")]
async fn churn_past_capacity_never_fails() {
    let node = node("churn_past_capacity", "counter", 16);
    let next = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let workers = (0..8).map(|_| {
        let (node, next) = (node.clone(), next.clone());
        tokio::spawn(async move {
            loop {
                let i = next.fetch_add(1, Relaxed);
                if i >= 3000 {
                    return;
                }
                let r = get(&node, &format!("churn{i}"), "/").await;
                assert_eq!(r.status, 200, "object {i}: {:?} {}", r.error, r.body);
            }
        })
    });
    for w in futures::future::join_all(workers).await {
        w.unwrap();
    }
    let peak = node.metrics.stores_peak.load(Relaxed);
    assert!(peak <= 16, "{peak} live stores");
    assert!(node.live_objects() <= 16);
    assert!(node.metrics.evicted_lru.load(Relaxed) >= 3000 - 16);
}

// ---- HTTP front end ---------------------------------------------------

/// Sends one raw HTTP/1.1 request and returns the status code and body.
async fn raw(addr: std::net::SocketAddr, head: &str, body: &[u8]) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(head.as_bytes()).await.unwrap();
    // The server may answer and close before reading an oversized body.
    let _ = s.write_all(body).await;
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out).await;
    let text = String::from_utf8_lossy(&out).to_string();
    let status = text.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

#[tokio::test(flavor = "multi_thread")]
async fn http_front_end() {
    let node = node("http_front_end", "counter", 8);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let n = node.clone();
    tokio::spawn(nanovm::server::serve_http1(listener, move |req| nanovm::server::handle(n.clone(), req)));

    let (status, body) = raw(
        addr,
        "GET /_nanovm/ready HTTP/1.1\r\nHost: worker\r\nConnection: close\r\n\r\n",
        b"",
    ).await;
    assert_eq!(status, 204, "{body}");
    assert!(body.is_empty(), "readiness must not return a stats snapshot: {body}");
    assert_eq!(node.live_objects(), 0);

    let (status, body) = raw(
        addr,
        "GET /web/ HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        b"",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""object":"web""#), "{body}");

    let (status, _) = raw(
        addr,
        "GET //web/ HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        b"",
    )
    .await;
    assert_eq!(status, 400);

    let big = vec![b'x'; 2 << 20];
    let head = format!(
        "POST /web/kv/big HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        big.len()
    );
    let (status, _) = raw(addr, &head, &big).await;
    assert_eq!(status, 413);

    for path in ["/_nanovm/stats", "/_nanovm/events"] {
        assert_eq!(http_get(addr, path).await.0, 404);
    }

    let (status, _) = raw(
        addr,
        "GET /_nanovm/nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        b"",
    )
    .await;
    assert_eq!(status, 404);
}

/// A supervisor (one jailed worker per app, in dev mode) behind an HTTP
/// listener.
async fn supervised(test: &str) -> (Arc<nanovm::supervisor::Supervisor>, std::net::SocketAddr) {
    supervised_with(node_cfg(test, 8)).await
}

type Capture = (std::sync::Weak<nanovm::supervisor::Supervisor>, Arc<parking_lot::Mutex<collector::Collector>>);
fn captures() -> &'static parking_lot::Mutex<std::collections::HashMap<std::net::SocketAddr, Capture>> {
    static CAPTURES: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashMap<std::net::SocketAddr, Capture>>> = std::sync::OnceLock::new();
    CAPTURES.get_or_init(Default::default)
}

async fn supervised_with(cfg: NodeConfig) -> (Arc<nanovm::supervisor::Supervisor>, std::net::SocketAddr) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (supervised_at(cfg, listener, None).await, addr)
}

/// A supervisor serving on `listener`, with `leases` its lease store.
async fn supervised_at(mut cfg: NodeConfig, listener: tokio::net::TcpListener, leases: Option<Arc<dyn object_store::ObjectStore>>) -> Arc<nanovm::supervisor::Supervisor> {
    let captured = Arc::new(parking_lot::Mutex::new(collector::Collector::default()));
    if cfg.telemetry_endpoint == NodeConfig::default().telemetry_endpoint {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        cfg.telemetry_endpoint = format!("http://{}", listener.local_addr().unwrap());
        let captured = captured.clone();
        tokio::spawn(nanovm::server::serve_http1(listener, move |req| {
            let captured = captured.clone();
            async move {
                use http_body_util::BodyExt;
                let path = req.uri().path().to_owned();
                let body = req.into_body().collect().await.unwrap().to_bytes();
                captured.lock().ingest(&path, &body).unwrap();
                http::Response::new(http_body_util::Full::new(bytes::Bytes::new()).map_err(|e| match e {}).boxed_unsync())
            }
        }));
    }
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_nanovm"));
    let sup = nanovm::supervisor::Supervisor::new(cfg, exe, 1, leases).unwrap();
    let addr = listener.local_addr().unwrap();
    captures().lock().retain(|_, (sup, _)| sup.strong_count() != 0);
    captures().lock().insert(addr, (Arc::downgrade(&sup), captured));
    let s = sup.clone();
    tokio::spawn(nanovm::gate::serve(sup.gate.clone(), listener, None, move |req| s.clone().handle(req)));
    sup
}

/// A registry on loopback for the whole test run, as dev mode allows.
fn registry() -> std::net::SocketAddr {
    static REGISTRY: std::sync::OnceLock<container_registry::test_support::RunningRegistry> =
        std::sync::OnceLock::new();
    REGISTRY
        .get_or_init(|| container_registry::ContainerRegistry::builder().build_for_testing().run_in_background())
        .bound_addr()
}

/// Publishes `app` (its guest is the one its name says) to the test registry
/// as `nanovm/<repo>`, and returns it by digest, as `nanovm-app` names it.
async fn publish(repo: &str, app: &AppConfig) -> String {
    publish_wasm(repo, app, guest(&app.name)).await
}

async fn publish_wasm(repo: &str, app: &AppConfig, wasm: Vec<u8>) -> String {
    publish_version(repo, app, wasm, "1.0.0").await
}

async fn publish_version(repo: &str, app: &AppConfig, wasm: Vec<u8>, version: &str) -> String {
    let spec = serde_json::json!({ "limits": app.limits, "env": app.env, "config": app.config });
    publish_spec(repo, spec, wasm, version).await
}

/// Publishes `wasm` with the manifest `spec` (its config blob).
async fn publish_spec(repo: &str, spec: serde_json::Value, wasm: Vec<u8>, version: &str) -> String {
    let r = format!("{}/nanovm/{repo}:latest", registry()).parse().unwrap();
    let oci = nanovm::registry::Oci::new(true).unwrap();
    let version = version.parse().unwrap();
    let digest = oci.push_app(&r, None, serde_json::to_vec(&spec).unwrap(), wasm, &version).await.unwrap();
    assert_eq!(oci.digest(&r).await.unwrap(), digest, "the tag names what was pushed");
    format!("{}/nanovm/{repo}@{digest}", registry())
}

/// The headers naming `app` to a dev-mode node, which checks no signature.
fn names(app: &str) -> String {
    format!("nanovm-app: {app}\r\nnanovm-trust: dev dev\r\n")
}

/// The app's name on the node (its worker's, in stats and cgroups).
fn worker(app: &str) -> String {
    nanovm::registry::app_name("", &app.parse().unwrap())
}

/// `GET path` on `app`.
async fn app_get(addr: std::net::SocketAddr, app: &str, path: &str) -> (u16, String) {
    let head = format!("GET {path} HTTP/1.1\r\nHost: x\r\n{}Connection: close\r\n\r\n", names(app));
    raw(addr, &head, b"").await
}

async fn http_get(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
    raw(addr, &format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"), b"").await
}

async fn stats_json(addr: std::net::SocketAddr) -> serde_json::Value {
    let (sup, captured) = captures().lock()[&addr].clone();
    let sup = sup.upgrade().unwrap();
    let started = Instant::now();
    // A killed worker's link can close before its process watcher runs.
    while let Err(e) = sup.flush_telemetry().await {
        assert!(started.elapsed() < Duration::from_secs(2), "telemetry flush failed: {e:#}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    captured.lock().snapshot()
}

/// `nanovm admin --config … <args>` against the node `cfg` describes: its
/// exit status, and what it printed.
fn admin(cfg: &NodeConfig, args: &[&str]) -> (bool, String) {
    let config = cfg.data_dir.join("admin.toml");
    std::fs::write(&config, format!("data_dir = {:?}\n", cfg.data_dir)).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_nanovm"))
        .args(["admin", "--config"]).arg(&config).args(args).output().unwrap();
    let text = [out.stdout, out.stderr].concat();
    (out.status.success(), String::from_utf8_lossy(&text).into_owned())
}

/// The admin socket answers the supervisor's own user (root here) and no
/// one else: its directory refuses any other user, no HTTP route reaches it,
/// and the jail lets no worker make a socket (`jail_blocks_escapes`).
#[tokio::test(flavor = "multi_thread")]
async fn admin_socket_is_the_supervisors_own() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let cfg = node_cfg("admin_socket_is_the_supervisors_own", 8);
    let (_sup, addr) = supervised_with(cfg.clone()).await;
    let counter = publish("counter", &app("counter")).await;
    assert_eq!(app_get(addr, &counter, "/a/").await.0, 200);

    let (ok, out) = admin(&cfg, &["apps"]);
    assert!(ok, "{out}");
    let apps: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(apps[0]["app"], worker(&counter), "{out}");
    let (ok, out) = admin(&cfg, &["nonsense"]);
    assert!(!ok && out.contains("unknown command"), "{out}");

    let socket = nanovm::admin::socket(&cfg);
    let dir = std::fs::metadata(socket.parent().unwrap()).unwrap();
    assert_eq!((dir.permissions().mode() & 0o777, dir.uid()), (0o700, unsafe { libc::geteuid() }));
    // Another user can't connect.
    if unsafe { libc::geteuid() } == 0 {
        let path = std::ffi::CString::new(socket.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: the child makes only async-signal-safe calls, then exits.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe {
                let mut addr: libc::sockaddr_un = std::mem::zeroed();
                addr.sun_family = libc::AF_UNIX as u16;
                let bytes = path.as_bytes_with_nul();
                std::ptr::copy_nonoverlapping(bytes.as_ptr().cast(), addr.sun_path.as_mut_ptr(), bytes.len());
                if libc::setgroups(0, std::ptr::null()) != 0 || libc::setgid(65534) != 0 || libc::setuid(65534) != 0 {
                    libc::_exit(100);
                }
                let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
                let r = libc::connect(fd, (&raw const addr).cast(), size_of::<libc::sockaddr_un>() as u32);
                libc::_exit(if r == 0 { 0 } else { *libc::__errno_location() });
            }
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert_eq!(libc::WEXITSTATUS(status), libc::EACCES, "another user connected (0) or failed otherwise");
    }
    // No HTTP route leads there, with or without an app.
    for path in ["/_nanovm/admin", "/_nanovm/command", "/_nanovm/delete"] {
        assert_eq!(http_get(addr, path).await.0, 404, "{path}");
        assert_eq!(app_get(addr, &counter, path).await.0, 400, "{path}");
    }
}

/// A node whose data directory is too long for a socket's path (at most 107
/// bytes) still serves its admin socket: it is bound, and reached, through
/// the directory it is in, opened (recovery's B-508).
#[tokio::test(flavor = "multi_thread")]
async fn the_admin_socket_takes_a_long_data_dir() {
    let mut cfg = node_cfg("the_admin_socket_takes_a_long_data_dir", 8);
    cfg.data_dir = cfg.data_dir.join("long".repeat(30));
    let socket = nanovm::admin::socket(&cfg);
    assert!(socket.as_os_str().len() > 160, "{}", socket.display());
    let (_sup, _addr) = supervised_with(cfg.clone()).await;
    let (ok, out) = admin(&cfg, &["apps"]);
    assert!(ok && out.trim() == "[]", "{out}");
}

/// An operator deletes an object, then all of an app's data, on a running
/// node: its worker stops first, the node's copy and what it shipped go,
/// the rest of the app is untouched, and the app goes on from nothing.
#[tokio::test(flavor = "multi_thread")]
async fn an_operator_deletes_an_object_and_an_app() {
    let mut cfg = node_cfg("an_operator_deletes_an_object_and_an_app", 8);
    let store = cfg.data_dir.join("store");
    cfg.capture = Some(nanovm::config::CaptureConfig { store: Some(store.to_str().unwrap().into()), ..Default::default() });
    let (_sup, addr) = supervised_with(cfg.clone()).await;
    let counter = publish("counter", &app("counter")).await;
    let name = worker(&counter);
    let objects = cfg.app_dirs(&name)[0].clone();
    let incr = async |id: &str| raw(addr, &format!("POST /{id}/incr HTTP/1.1\r\nHost: x\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n", names(&counter)), b"").await;
    let count = async |id: &str| serde_json::from_str::<serde_json::Value>(&app_get(addr, &counter, &format!("/{id}/")).await.1).unwrap()["count"].clone();
    let shipped = |id: &str| store.join(&name).join(id).join("head");
    for id in ["a", "a", "b"] {
        assert_eq!(incr(id).await.0, 200);
    }
    eventually_long("both shipped", || shipped("a").exists() && shipped("b").exists()).await;

    let (ok, out) = admin(&cfg, &["delete", &name, "a"]);
    assert!(ok, "{out}");
    assert!(!object_dir(&objects, "a").exists() && !store.join(&name).join("a").exists(), "a is gone, here and in the store");
    assert!(object_dir(&objects, "b").exists() && shipped("b").exists(), "b is untouched");
    assert_eq!((count("a").await, count("b").await), (0.into(), 1.into()));
    // It goes on from nothing, and ships again.
    assert_eq!(incr("a").await.0, 200);
    eventually_long("a shipped again", || shipped("a").exists()).await;

    let (ok, out) = admin(&cfg, &["delete-app", &name]);
    assert!(ok, "{out}");
    for dir in cfg.app_dirs(&name) {
        assert!(!dir.exists(), "{} is left", dir.display());
    }
    assert!(!store.join(&name).exists(), "nothing of the app in the store");
    assert_eq!(std::fs::read_dir(cfg.data_dir.join("deleted")).unwrap().count(), 0, "nothing left over");
    assert_eq!((count("a").await, count("b").await), (0.into(), 0.into()), "the app starts from nothing");

    for bad in [vec!["delete", "../x", "a"], vec!["delete", &name, ".."], vec!["delete", &name, "_nanovm"], vec!["delete-app", "a/b"]] {
        let (ok, out) = admin(&cfg, &bad);
        assert!(!ok && out.contains("not an app and object"), "{bad:?}: {out}");
    }
}

/// Waits until `f` holds, for up to 10 s.
async fn eventually_long(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// `POST path` on `app`, with no body.
async fn app_post(addr: std::net::SocketAddr, app: &str, path: &str) -> (u16, String) {
    let head = format!("POST {path} HTTP/1.1\r\nHost: x\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n", names(app));
    raw(addr, &head, b"").await
}

/// `nanovm admin` with `args`, off the runtime's threads: the command's
/// JSON answer, or its error.
async fn admin_json(cfg: &NodeConfig, args: &[&str]) -> Result<serde_json::Value, String> {
    let (cfg, args) = (cfg.clone(), args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
    let (ok, out) = tokio::task::spawn_blocking(move || admin(&cfg, &args.iter().map(String::as_str).collect::<Vec<_>>())).await.unwrap();
    if ok { Ok(serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"))) } else { Err(out) }
}

/// An operator restores one object, then all of one app's objects, from the
/// store into a running node, through `nanovm admin`: to an older version
/// (which becomes the version after the head: the head only moves on, and
/// the versions between stay in the store), and to the head, over a damaged
/// volume. Another app writes throughout without missing a write; while an
/// object is paused for a restore it answers 503 and the app's other objects
/// serve. Bad names and a damaged version in the store are refused, and
/// nothing changes.
#[tokio::test(flavor = "multi_thread")]
async fn restores_an_object_and_an_app_into_a_running_node() {
    let store = Path::new(env!("CARGO_TARGET_TMPDIR")).join("restore-store");
    let _ = std::fs::remove_dir_all(&store);
    let mut cfg = node_cfg("restore_running", 8);
    cfg.capture = Some(nanovm::config::CaptureConfig { store: Some(store.to_str().unwrap().into()), ..Default::default() });
    let (sup, addr) = supervised_with(cfg.clone()).await;
    let (a, b) = (publish("restore-a", &app("counter")).await, publish("restore-b", &app("counter")).await);
    let (wa, wb) = (worker(&a), worker(&b));
    let head = |app: &str, o: &str| {
        let bytes = std::fs::read(store.join(app).join(o).join("head")).ok();
        bytes.map_or(0, |h| serde_json::from_slice::<nanovm::ship::Head>(&h).unwrap().sequence)
    };
    let count = |(status, body): (u16, String)| {
        assert_eq!(status, 200, "{body}");
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["count"].as_u64().unwrap()
    };
    // B writes throughout.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = tokio::spawn({
        let (b, stop) = (b.clone(), stop.clone());
        async move {
            let mut n = 0;
            while !stop.load(Relaxed) {
                n += 1;
                let (status, body) = app_post(addr, &b, "/w/incr").await;
                assert_eq!((status, serde_json::from_str::<serde_json::Value>(&body).unwrap()["count"].as_u64()), (200, Some(n)), "{body}");
            }
            n
        }
    });
    for n in 1..=5 {
        assert_eq!(count(app_post(addr, &a, "/o/incr").await), n);
    }
    assert_eq!(count(app_post(addr, &a, "/p/incr").await), 1);
    let t0 = Instant::now();
    while head(&wa, "o") < 5 || head(&wa, "p") < 1 {
        assert!(t0.elapsed() < Duration::from_secs(5), "never shipped");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Back to version 3, live: made version 6.
    let done = admin_json(&cfg, &["restore", &wa, "o", "3"]).await.unwrap();
    assert_eq!((&done[0]["object"], &done[0]["version"], &done[0]["head"]), (&"o".into(), &3.into(), &6.into()), "{done}");
    assert!(done[0]["said"].as_str().unwrap().contains("versions 4 to 5 stay in the store"), "{done}");
    assert_eq!(head(&wa, "o"), 6, "the restore shipped its version");
    assert_eq!(count(app_get(addr, &a, "/o/").await), 3);
    assert_eq!(count(app_post(addr, &a, "/o/incr").await), 4);
    eventually("version 7 shipped", || head(&wa, "o") == 7).await;
    let versions = admin_json(&cfg, &["versions", &wa, "o"]).await.unwrap();
    assert_eq!(versions["head"], 7, "{versions}");
    let listed: Vec<_> = versions["versions"].as_array().unwrap().iter().map(|v| v["version"].as_u64().unwrap()).collect();
    assert_eq!(listed, (1..=7).collect::<Vec<_>>());
    assert!(admin_json(&cfg, &["versions"]).await.unwrap().as_array().unwrap().contains(&wb.clone().into()));

    // The volume is damaged under the running node: back to the head.
    let volume = object_dir(&cfg.app_dirs(&wa)[0], "o").join("data");
    std::fs::write(volume.join("count"), "999").unwrap();
    assert_eq!(count(app_get(addr, &a, "/o/").await), 999);
    let done = admin_json(&cfg, &["restore", &wa, "o"]).await.unwrap();
    assert_eq!((&done[0]["version"], &done[0]["head"]), (&7.into(), &7.into()), "{done}");
    assert_eq!(count(app_get(addr, &a, "/o/").await), 4);

    // Paused, an object answers 503 and the app's others serve.
    let objects = cfg.app_dirs(&wa)[0].clone();
    sup.pause(&wa, "o", &objects).await.unwrap();
    assert_eq!(app_get(addr, &a, "/o/").await, (503, r#"{"detail":"the object is being restored","error":"object-restoring"}"#.into()));
    assert_eq!(count(app_get(addr, &a, "/p/").await), 1);
    assert!(admin_json(&cfg, &["restore", &wa, "o"]).await.unwrap_err().contains("being restored already"));
    nanovm::ship::resume(&objects, "o").unwrap();

    // All of the app's objects, to their heads.
    let done = admin_json(&cfg, &["restore", &wa]).await.unwrap();
    let mut restored: Vec<_> = done.as_array().unwrap().iter().map(|r| (r["object"].as_str().unwrap().to_owned(), r["version"].as_u64().unwrap())).collect();
    restored.sort();
    assert_eq!(restored, [("o".to_owned(), 7), ("p".to_owned(), 1)]);
    assert_eq!((count(app_get(addr, &a, "/o/").await), count(app_get(addr, &a, "/p/").await)), (4, 1));

    // A version here the store refuses (it doesn't follow the head) stops a
    // restore, which would drop it, unless the operator says to drop it.
    let committed = object_dir(&objects, "o").join("committed");
    std::fs::copy(committed.join(format!("{:020}.tar", 7)), committed.join(format!("{:020}.tar", 8))).unwrap();
    let e = admin_json(&cfg, &["restore", &wa, "o"]).await.unwrap_err();
    assert!(e.contains("versions the store doesn't"), "{e}");
    assert!(committed.join(format!("{:020}.tar", 8)).exists());
    let done = admin_json(&cfg, &["restore", &wa, "o", "--discard-unshipped"]).await.unwrap();
    assert_eq!(done[0]["version"], 7, "{done}");
    assert!(!committed.join(format!("{:020}.tar", 8)).exists());

    // Refused: names that aren't one, a version the store doesn't have, and a
    // version whose bytes aren't what its key names. Nothing changes.
    for args in [vec!["restore", "..", "o"], vec!["restore", &wa, "../p"], vec!["restore", &wa, "_nanovm"], vec!["restore", &wa, "o", "8"], vec!["versions", "..", "o"]] {
        assert!(admin_json(&cfg, &args).await.is_err(), "{args:?}");
    }
    let key = std::fs::read_dir(store.join(&wa).join("o")).unwrap().map(|e| e.unwrap().path())
        .find(|p| p.file_name().unwrap().to_str().unwrap().starts_with(&format!("{:020}-", 2))).unwrap();
    let mut bytes = std::fs::read(&key).unwrap();
    let last = bytes.len() - 1024 - 1;
    bytes[last / 2] ^= 1;
    std::fs::write(&key, bytes).unwrap();
    let e = admin_json(&cfg, &["restore", &wa, "o", "2"]).await.unwrap_err();
    assert!(e.contains("isn't what its key names"), "{e}");
    assert_eq!(count(app_get(addr, &a, "/o/").await), 4);
    assert_eq!(count(app_post(addr, &a, "/o/incr").await), 5);
    // Neither a client nor a guest reaches the worker's eviction.
    assert_eq!(http_get(addr, "/_nanovm/evict/o").await.0, 404);
    assert_eq!(app_get(addr, &a, "/_nanovm/evict/o").await.0, 400);

    stop.store(true, Relaxed);
    let writes = writer.await.unwrap();
    assert!(writes > 10, "B wrote {writes} times");
    eventually("everything shipped", || head(&wb, "w") == writes && head(&wa, "o") == 8).await;
    // Marked shipped just after the head moves.
    let t0 = Instant::now();
    while admin_json(&cfg, &["backlog"]).await.unwrap() != serde_json::json!({ "versions": 0, "bytes": 0, "objects": 0 }) {
        assert!(t0.elapsed() < Duration::from_secs(2), "versions still wait");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn supervised_workers_contain_attacks() {
    let (_sup, addr) = supervised("supervised_workers_contain_attacks").await;
    assert_eq!(http_get(addr, "/_nanovm/ready").await, (204, String::new()));
    for path in ["/_nanovm/stats", "/_nanovm/events"] {
        assert_eq!(http_get(addr, path).await.0, 404);
    }
    let (counter, hostile) = (publish("counter", &app("counter")).await, publish("hostile", &app("hostile")).await);

    let (status, body) = app_get(addr, &counter, "/s/").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""object":"s""#), "{body}");

    for (path, error) in [
        ("/s/alloc?mb=1024", r#""error":"memory""#),
        ("/s/spin", r#""error":"cpu""#),
        ("/s/panic", r#""error":"trap""#),
        // The supervisor applies the allowlist and the IP filter.
        ("/s/fetch?url=http://169.254.169.254/", "HttpRequestDenied"),
        ("/s/fetch?url=http://localhost:9/", "DestinationIpProhibited"),
        ("/s/escape", "not-permitted"),
    ] {
        let (_, body) = app_get(addr, &hostile, path).await;
        assert!(body.contains(error), "{path}: {body}");
    }
    // The worker survived all of it.
    let (status, _) = app_get(addr, &hostile, "/s/").await;
    assert_eq!(status, 200);
    // A request must name its app, and its object, strictly.
    let (status, body) = http_get(addr, "/s/").await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(app_get(addr, &hostile, "//s/").await.0, 400);
    assert_eq!(app_get(addr, &hostile, "/_nanovm/stats").await.0, 400);

    let v = stats_json(addr).await;
    assert!(v["rss_mb"].as_f64().unwrap() > 0.0, "{v}");
    assert!(v["uptime_ms"].as_f64().unwrap() > 0.0, "{v}");
    assert_eq!(v["apps"][worker(&hostile).as_str()]["egress_denied"], 2, "{v}");
    for app in [worker(&counter), worker(&hostile)] {
        let pid = v["workers"][&app]["pid"].as_u64().unwrap();
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let field = |k: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(k))
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .to_string()
        };
        assert_eq!(field("Seccomp:"), "2", "{app}");
        assert_eq!(field("NoNewPrivs:"), "1", "{app}");
        // Shown as `nanovm worker`, though spawned from /proc/self/exe (N-552).
        assert_eq!(std::fs::read_to_string(format!("/proc/{pid}/cmdline")).unwrap(), "nanovm\0worker\0", "{app}");
        if unsafe { libc::geteuid() } == 0 {
            let uid: u32 = field("Uid:").parse().unwrap();
            assert!(uid >= 200_000, "{app} runs as uid {uid}");
        }
    }
    let captured = captures().lock()[&addr].1.clone();
    assert!(captured.lock().logs.iter().any(|log| log["app"] == worker(&hostile) && log["kind"] == "egress-denied"));
}

#[tokio::test(flavor = "multi_thread")]
async fn killed_worker_is_restarted_and_keeps_its_volumes() {
    let mut cfg = node_cfg("killed_worker_is_restarted", 8);
    cfg.capture = Some(Default::default());
    let (_sup, addr) = supervised_with(cfg.clone()).await;
    let (counter, hostile) = (publish("counter", &app("counter")).await, publish("hostile", &app("hostile")).await);
    assert_eq!(app_get(addr, &hostile, "/k/").await.0, 200);
    let incr = format!("POST /k/incr HTTP/1.1\r\nHost: x\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n", names(&counter));
    let (status, body) = raw(addr, &incr, b"").await;
    assert_eq!(status, 200, "{body}");
    let objects = cfg.app_dirs(&worker(&counter))[0].clone();
    assert_eq!(captured_files(&objects, "k", 1)[Path::new("data/count")], b"1");

    let pid = stats_json(addr).await["workers"][worker(&counter).as_str()]["pid"].as_u64().unwrap();
    let mut competing = app("counter");
    competing.name = worker(&counter);
    let error = Node::new(cfg, &competing, &guest("counter"), None).err().expect("jailed worker must own its storage");
    assert!(error.to_string().contains("app storage is already owned"), "{error:#}");
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    let t0 = Instant::now();
    while stats_json(addr).await["workers"][worker(&counter).as_str()]["up"] == true {
        assert!(t0.elapsed() < Duration::from_secs(2), "worker still up");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Neighbours are untouched.
    assert_eq!(app_get(addr, &hostile, "/k/").await.0, 200);

    // The next request restarts it; `/data` survived.
    let (status, body) = app_get(addr, &counter, "/k/").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""count":1"#), "{body}");
    let v = stats_json(addr).await;
    assert_eq!(v["workers"][worker(&counter).as_str()]["starts"], 2, "{v}");
    assert_ne!(v["workers"][worker(&counter).as_str()]["pid"].as_u64().unwrap(), pid);
    assert_eq!(raw(addr, &incr, b"").await.0, 200);
    assert_eq!(captured_files(&objects, "k", 2)[Path::new("data/count")], b"2");
    assert_eq!(captured_files(&objects, "k", 1)[Path::new("data/count")], b"1");
}

/// A worker whose compile of its component is cached starts from that alone:
/// the registry's copy of the component is neither read nor hashed again
/// (it was checked when fetched). Here that copy is overwritten, and a
/// restart still serves.
#[tokio::test(flavor = "multi_thread")]
async fn a_cached_compile_is_used_without_reading_the_component() {
    use sha2::Digest;
    let test = "a_cached_compile_is_used_without_reading_the_component";
    let (_sup, addr) = supervised(test).await;
    let counter = publish("counter", &app("counter")).await;
    assert_eq!(app_get(addr, &counter, "/k/").await.0, 200);
    let wasm = guest("counter");
    let blob = Path::new(env!("CARGO_TARGET_TMPDIR")).join("nodes").join(test).join("registry")
        .join(format!("sha256-{:x}", sha2::Sha256::digest(&wasm)));
    std::fs::write(&blob, vec![0; wasm.len()]).unwrap();
    let pid = stats_json(addr).await["workers"][worker(&counter).as_str()]["pid"].as_u64().unwrap();
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    let t0 = Instant::now();
    while stats_json(addr).await["workers"][worker(&counter).as_str()]["up"] == true {
        assert!(t0.elapsed() < Duration::from_secs(2), "worker still up");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (status, body) = app_get(addr, &counter, "/k/").await;
    assert_eq!(status, 200, "{body}");
}

/// A start takes a worker spawned ahead of need, which has built its engine
/// and waits for its app (that was 11 of a restart's 21 ms), and the next
/// one is spawned after it.
#[tokio::test(flavor = "multi_thread")]
async fn workers_are_spawned_ahead() {
    let (_sup, addr) = supervised("workers_are_spawned_ahead").await;
    let (counter, hostile) = (publish("counter", &app("counter")).await, publish("hostile", &app("hostile")).await);
    // In clock ticks since boot, as /proc has process start times.
    let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let now = || std::fs::read_to_string("/proc/uptime").unwrap().split(' ').next().unwrap().parse::<f64>().unwrap() * tck;
    let started = |pid: u64| {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        stat.rsplit(") ").next().unwrap().split(' ').nth(19).unwrap().parse::<f64>().unwrap()
    };
    for app in [&counter, &hostile] {
        // Ticks are 10 ms; a spare is spawned within a few.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let asked = now();
        assert_eq!(app_get(addr, app, "/k/").await.0, 200);
        let pid = stats_json(addr).await["workers"][worker(app).as_str()]["pid"].as_u64().unwrap();
        assert!(started(pid) < asked, "{app}'s worker was spawned when it was asked for");
    }
}

/// Runs `jail_probe` in a child process (a jail cannot be left), then checks
/// what the jail let it do.
#[cfg(target_os = "linux")]
#[test]
fn jail_blocks_escapes() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("jail-probe");
    let _ = std::fs::remove_dir_all(&dir);
    for d in ["mine", "other"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    std::fs::write(dir.join("other/secret"), "s").unwrap();
    if unsafe { libc::geteuid() } == 0 {
        std::os::unix::fs::chown(dir.join("mine"), Some(200_099), Some(200_099)).unwrap();
    }
    // A host service's datagram socket, for the probe to try to reach, and
    // a socket as the admin socket is.
    let _admin = std::os::unix::net::UnixListener::bind(dir.join("admin.sock")).unwrap();
    let service = format!("nanovm-jail-probe-{}", std::process::id());
    let service_sock = {
        use std::os::linux::net::SocketAddrExt;
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(&service).unwrap();
        std::os::unix::net::UnixDatagram::bind_addr(&addr).unwrap()
    };
    let mut probe = std::process::Command::new(std::env::current_exe().unwrap());
    // Its own process group, so the signals below reach its forked child.
    std::os::unix::process::CommandExt::process_group(&mut probe, 0);
    let child = probe
        .args(["--exact", "jail_probe", "--nocapture", "--test-threads", "1"])
        .env("NANOVM_JAIL_PROBE", &dir)
        .env("NANOVM_JAIL_PROBE_SERVICE", &service)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Stop and continue the probe during its final sleep, as a debugger or
    // job control would: the kernel resumes the sleep via restart_syscall.
    std::thread::sleep(Duration::from_millis(150));
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGSTOP);
        std::thread::sleep(Duration::from_millis(20));
        libc::kill(-(child.id() as i32), libc::SIGCONT);
    }
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let results: std::collections::HashMap<&str, &str> = stdout
        .lines()
        .filter_map(|l| l.rsplit_once("probe ").map(|(_, r)| r))
        .filter_map(|l| l.split_once('='))
        .collect();
    let expect = [
        ("write_own_dir", "ok"),
        ("link_own_dir", "ok"),
        ("link_other_app", "denied"),
        ("link_outside", "denied"),
        ("cwd", "ok"),
        ("thread", "ok"),
        ("read_other_app", "denied"),
        ("read_etc_passwd", "denied"),
        ("list_root", "denied"),
        ("tcp_socket", "denied"),
        ("unix_socket", "denied"),
        ("unix_datagram", "denied"),
        ("admin_socket", "denied"),
        ("fork", "denied"),
        ("exec", "denied"),
        ("setuid_root", "denied"),
        ("ptrace", "denied"),
        ("kill_init", "denied"),
        ("unshare", "denied"),
        ("mmap_wx", "denied"),
        ("raise_nofile", "denied"),
        ("signal_owner", "denied"),
        ("sleep_survives_stop", "ok"),
    ];
    for (k, v) in expect {
        assert_eq!(results.get(k).copied(), Some(v), "{k}\n{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    }
    service_sock.set_nonblocking(true).unwrap();
    assert!(service_sock.recv(&mut [0; 8]).is_err(), "a datagram left the jail");
}

/// The jailed half of `jail_blocks_escapes`; a no-op when run directly.
/// The jail takes only a process with one thread, and the test harness has
/// two, so it runs in a forked child.
#[cfg(target_os = "linux")]
#[test]
fn jail_probe() {
    let Some(dir) = std::env::var_os("NANOVM_JAIL_PROBE").map(PathBuf::from) else {
        return;
    };
    let pid = unsafe { libc::fork() };
    if pid != 0 {
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0, "the probe failed: {status}");
        return;
    }
    let probed = std::panic::catch_unwind(|| jailed(&dir));
    use std::io::Write;
    let _ = std::io::stdout().flush();
    unsafe { libc::_exit(if probed.is_ok() { 0 } else { 1 }) }
}

#[cfg(target_os = "linux")]
fn jailed(dir: &Path) {
    let root = unsafe { libc::geteuid() } == 0;
    nanovm::jail::Jail {
        uid: root.then_some(200_099),
        parent: std::os::unix::process::parent_id(),
        read_write: vec![dir.join("mine")],
        read_only: vec![],
    }
    .enter()
    .unwrap();
    let report = |name: &str, ok: bool| println!("probe {name}={}", if ok { "ok" } else { "denied" });
    let sys = |r: i64| r >= 0;
    report("write_own_dir", std::fs::write(dir.join("mine/f"), "x").is_ok());
    report("link_own_dir", std::fs::hard_link(dir.join("mine/f"), dir.join("mine/link")).is_ok());
    report("link_other_app", std::fs::hard_link(dir.join("other/secret"), dir.join("mine/stolen")).is_ok());
    report("link_outside", std::fs::hard_link(dir.join("mine/f"), dir.join("other/injected")).is_ok());
    // A relative data_dir is canonicalized against it.
    report("cwd", std::env::current_dir().is_ok());
    report("thread", std::thread::spawn(|| 1).join().is_ok());
    report("read_other_app", std::fs::read(dir.join("other/secret")).is_ok());
    report("read_etc_passwd", std::fs::read("/etc/passwd").is_ok());
    report("list_root", std::fs::read_dir("/").is_ok());
    unsafe {
        report("tcp_socket", sys(libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) as i64));
        report("unix_socket", sys(libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) as i64));
        // A datagram pair can send to any socket on the host with a name.
        let service = std::env::var("NANOVM_JAIL_PROBE_SERVICE").unwrap();
        let sent = std::os::unix::net::UnixDatagram::pair().and_then(|(a, _)| {
            use std::os::linux::net::SocketAddrExt;
            a.send_to_addr(b"x", &std::os::unix::net::SocketAddr::from_abstract_name(&service)?)
        });
        report("unix_datagram", sent.is_ok());
        report("admin_socket", std::os::unix::net::UnixStream::connect(dir.join("admin.sock")).is_ok());
        let pid = libc::fork();
        if pid == 0 {
            libc::_exit(0);
        }
        report("fork", sys(pid as i64));
        report("exec", std::process::Command::new("/bin/true").status().is_ok());
        // Raw: glibc's setuid broadcasts to every thread of the process.
        report("setuid_root", sys(libc::syscall(libc::SYS_setuid, 0)) && libc::getuid() == 0);
        report("ptrace", sys(libc::ptrace(libc::PTRACE_ATTACH, 1, 0, 0)));
        report("kill_init", sys(libc::kill(1, 0) as i64));
        report("unshare", sys(libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) as i64));
        let p = libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        report("mmap_wx", p != libc::MAP_FAILED);
        let lim = libc::rlimit {
            rlim_cur: libc::RLIM_INFINITY,
            rlim_max: libc::RLIM_INFINITY,
        };
        report("raise_nofile", sys(libc::setrlimit(libc::RLIMIT_NOFILE, &lim) as i64));
        // Owning a descriptor's I/O signals would let it signal its supervisor.
        let f = std::fs::File::open(dir.join("mine/f")).unwrap();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&f);
        report("signal_owner", sys(libc::fcntl(fd, libc::F_SETOWN, libc::getppid()) as i64));
    }
    std::thread::sleep(Duration::from_millis(400));
    report("sleep_survives_stop", true);
}

/// Many requests through the supervisor reuse its links to the worker
/// without errors or restarts.
#[tokio::test(flavor = "multi_thread")]
async fn worker_connection_outlives_many_requests() {
    let (sup, addr) = supervised("worker_connection_outlives_many_requests").await;
    let counter = publish("counter", &app("counter")).await;
    // A worker belongs to the supervisor's control runtime, not whichever
    // front-end runtime happened to receive the request that started it.
    let (ready, port) = tokio::sync::oneshot::channel();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let front = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            ready.send(listener.local_addr().unwrap()).unwrap();
            tokio::spawn(nanovm::server::serve_http1(listener, move |req| sup.clone().handle(req)));
            stopped.await.unwrap();
        });
    });
    assert_eq!(app_get(port.await.unwrap(), &counter, "/c0/").await.0, 200);
    let pid = stats_json(addr).await["workers"][worker(&counter).as_str()]["pid"].clone();
    stop.send(()).unwrap();
    tokio::task::spawn_blocking(move || front.join().unwrap()).await.unwrap();
    let tasks: Vec<_> = (0..32)
        .map(|t| {
            let counter = counter.clone();
            tokio::spawn(async move {
                let mut bad = 0;
                for i in 0..400 {
                    if app_get(addr, &counter, &format!("/c{}/", (t + i) % 8)).await.0 != 200 {
                        bad += 1;
                    }
                }
                bad
            })
        })
        .collect();
    let mut bad = 0;
    for t in tasks {
        bad += t.await.unwrap();
    }
    assert_eq!(bad, 0);
    let v = stats_json(addr).await;
    assert_eq!(v["workers"][worker(&counter).as_str()]["pid"], pid, "front-end shutdown must not stop its worker");
    assert_eq!(v["workers"][worker(&counter).as_str()]["starts"], 1, "{v}");
    assert_eq!(v["workers"][worker(&counter).as_str()]["link_errors"], 0, "{v}");
}

/// A fresh test cgroup under the cgroup v2 mount, with `controllers` enabled
/// for its children, or why this host can't provide one.
fn test_cgroup(name: &str, controllers: &[&str]) -> Result<PathBuf, String> {
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let mount = mounts
        .lines()
        .find_map(|l| {
            let f: Vec<_> = l.split(' ').collect();
            (f.get(2) == Some(&"cgroup2")).then(|| PathBuf::from(f[1]))
        })
        .ok_or("no cgroup v2 hierarchy (scripts/vm-test.sh boots one)")?;
    let available = std::fs::read_to_string(mount.join("cgroup.controllers")).unwrap_or_default();
    if let Some(c) = controllers.iter().find(|c| !available.split_whitespace().any(|a| a == **c)) {
        return Err(format!(
            "the {c} controller is not available in {} (scripts/vm-test.sh boots a VM where it is)",
            mount.display()
        ));
    }
    let root = mount.join(format!("nanovm-{name}-{}", std::process::id()));
    std::fs::create_dir(&root).map_err(|e| format!("{}: {e}", mount.display()))?;
    let enable: Vec<_> = controllers.iter().map(|c| format!("+{c}")).collect();
    std::fs::write(root.join("cgroup.subtree_control"), enable.join(" ")).map_err(|e| e.to_string())?;
    Ok(root)
}

/// Runs `cfg`'s workers in the test cgroup `root`, under no limits but those
/// a test sets: the defaults need controllers a test cgroup may lack.
fn in_cgroup(cfg: &mut NodeConfig, root: &Path) {
    cfg.cgroup = root.to_str().unwrap().into();
    cfg.worker = nanovm::config::WorkerLimits { memory_mb: 0, cpus: 0.0, pids: 0 };
}

/// Kills what is left in a test cgroup and removes it.
async fn remove_cgroup(root: &Path) {
    for _ in 0..200 {
        for app in std::fs::read_dir(root).into_iter().flatten().flatten().filter(|e| e.path().is_dir()) {
            let _ = std::fs::write(app.path().join("cgroup.kill"), "1");
            let _ = std::fs::remove_dir(app.path());
        }
        if std::fs::remove_dir(root).is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn cgroup_stat(dir: &Path, file: &str, key: &str) -> u64 {
    let text = std::fs::read_to_string(dir.join(file)).unwrap_or_default();
    text.lines()
        .find_map(|l| l.strip_prefix(key)?.trim().parse().ok())
        .unwrap_or(0)
}

/// The cgroup `pid` runs in, under the cgroup v2 mount `root` is in.
fn cgroup_of(root: &Path, pid: u64) -> PathBuf {
    let cgroups = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    root.parent().unwrap().join(cgroups.lines().find_map(|l| l.strip_prefix("0::/")).unwrap())
}

/// Each worker runs in a cgroup of its own under `<cgroup>`, removed once
/// the worker has exited.
#[tokio::test(flavor = "multi_thread")]
async fn workers_are_placed_in_their_cgroups() {
    let root = match test_cgroup("placed", &[]) {
        Ok(r) => r,
        Err(why) => return eprintln!("skipped: {why}"),
    };
    let mut cfg = node_cfg("workers_are_placed_in_their_cgroups", 8);
    in_cgroup(&mut cfg, &root);
    let (sup, addr) = supervised_with(cfg).await;
    let (counter, hostile) = (publish("counter", &app("counter")).await, publish("hostile", &app("hostile")).await);
    for a in [&counter, &hostile] {
        assert_eq!(app_get(addr, a, "/x/").await.0, 200);
    }
    let v = stats_json(addr).await;
    let mut groups = Vec::new();
    for app in [worker(&counter), worker(&hostile)] {
        let pid = v["workers"][&app]["pid"].as_u64().unwrap();
        let group = cgroup_of(&root, pid);
        assert_eq!(group.parent(), Some(root.as_path()), "{app}");
        assert_eq!(std::fs::read_to_string(group.join("cgroup.procs")).unwrap().trim(), pid.to_string(), "{app}");
        groups.push((pid, group));
    }
    assert_ne!(groups[0].1, groups[1].1);
    let (pid, group) = &groups[0];
    unsafe { libc::kill(*pid as i32, libc::SIGKILL) };
    eventually("the exited worker's cgroup to go", || !group.exists()).await;
    drop(sup);
    remove_cgroup(&root).await;
}

/// By default a node runs its workers under the `[worker]` limits, sized
/// from the host: in a cgroup subtree it makes of its own where cgroup v2
/// lets it, and otherwise it refuses to start, saying how to run without.
#[tokio::test(flavor = "multi_thread")]
async fn workers_run_under_host_sized_limits_or_the_node_refuses_to_start() {
    let mut cfg = node_cfg("workers_run_under_host_sized_limits", 8);
    cfg.cgroup = "auto".into();
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_nanovm"));
    if let Err(e) = nanovm::supervisor::Supervisor::new(cfg.clone(), exe, 1, None) {
        let e = format!("{e:#}");
        assert!(e.contains("cgroup = \"auto\"") && e.contains("cgroup = \"off\""), "{e}");
        return eprintln!("refused, as it should be where no cgroup v2 subtree can be made: {e}");
    }
    {
        // Where it can, the supervisor moves itself into `<own>/supervisor`
        // and puts workers in `<own>/workers`, under the default limits.
        let (sup, addr) = supervised_with(cfg).await;
        let counter = publish("counter", &app("counter")).await;
        assert_eq!(app_get(addr, &counter, "/x/").await.0, 200);
        let pid = stats_json(addr).await["workers"][worker(&counter).as_str()]["pid"].as_u64().unwrap();
        let mine = std::fs::read_to_string("/proc/self/cgroup").unwrap();
        assert!(mine.trim_end().ends_with("/supervisor"), "{mine}");
        let theirs = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
        let group = PathBuf::from("/sys/fs/cgroup").join(theirs.lines().find_map(|l| l.strip_prefix("0::/")).unwrap());
        assert!(group.parent().unwrap().ends_with("workers"), "{}", group.display());
        let w = nanovm::config::WorkerLimits::default();
        assert_eq!(std::fs::read_to_string(group.join("memory.max")).unwrap().trim(), (w.memory_mb << 20).to_string());
        assert_eq!(std::fs::read_to_string(group.join("cpu.max")).unwrap().trim(), format!("{} 100000", w.cpus as u64 * 100_000));
        drop(sup);
    }
}

/// Waits until a worker spawned ahead has built its engine (its stacks and
/// their guard pages take at least two mappings a slot) and waits for its
/// app on stdin; returns its pid.
async fn spare_built(slots: usize) -> u64 {
    let me = std::process::id().to_string();
    let t0 = Instant::now();
    loop {
        for p in std::fs::read_dir("/proc").unwrap().flatten() {
            let stat = std::fs::read_to_string(p.path().join("stat")).unwrap_or_default();
            if stat.rsplit(") ").next().and_then(|s| s.split(' ').nth(1)) == Some(me.as_str())
                && std::fs::read_to_string(p.path().join("maps")).is_ok_and(|m| m.lines().count() > 2 * slots)
                && std::fs::read_to_string(p.path().join("syscall")).is_ok_and(|s| s.starts_with("0 0x0 "))
            {
                return p.file_name().to_str().unwrap().parse().unwrap();
            }
        }
        assert!(t0.elapsed() < Duration::from_secs(60), "no spare worker built its engine");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A spare worker builds its engine before it has an app: at 8,192 slots,
/// some 8 MiB of anonymous memory (and as much kernel memory for 33,000
/// mappings). It does so in a cgroup of its own, which gets its app's
/// limits when it is given its app, so those cover it: pages stay charged
/// to the cgroup a process was in when it touched them, wherever it moves.
#[tokio::test(flavor = "multi_thread")]
async fn review_spare_engine_memory_is_charged_to_the_app() {
    let root = match test_cgroup("charged", &["memory"]) {
        Ok(r) => r,
        Err(why) => return eprintln!("skipped: {why}"),
    };
    let slots = 8192;
    let mut cfg = node_cfg("review_spare_engine_memory_is_charged_to_the_app", slots);
    in_cgroup(&mut cfg, &root);
    cfg.worker.memory_mb = 512;
    let (sup, addr) = supervised_with(cfg).await;
    let counter = publish("counter", &app("counter")).await;
    let spare = spare_built(slots).await;
    assert_eq!(app_get(addr, &counter, "/x/").await.0, 200);
    let pid = stats_json(addr).await["workers"][worker(&counter).as_str()]["pid"].as_u64().unwrap();
    assert_eq!(pid, spare, "the start took the spare");
    let group = cgroup_of(&root, pid);
    assert_eq!(std::fs::read_to_string(group.join("memory.max")).unwrap().trim(), (512u64 << 20).to_string());
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let anon = status.lines().find_map(|l| l.strip_prefix("RssAnon:")).unwrap().trim().trim_end_matches(" kB");
    let anon = anon.parse::<u64>().unwrap() << 10;
    // memory.stat may lag by a batch of pages per CPU.
    let charged = cgroup_stat(&group, "memory.stat", "anon ");
    assert!(charged + (2 << 20) >= anon, "the worker has {anon} bytes of anonymous memory, its cgroup is charged {charged}");
    drop(sup);
    remove_cgroup(&root).await;
}

/// A worker over its `memory.max` is OOM-killed and restarted, while the
/// other app's worker keeps serving.
#[tokio::test(flavor = "multi_thread")]
async fn worker_memory_max_kills_only_that_worker() {
    let root = match test_cgroup("memory", &["memory"]) {
        Ok(r) => r,
        Err(why) => return eprintln!("skipped: {why}"),
    };
    // Each hostile object may hold 32 MiB; eight holding 30 MiB exceed 160.
    let mut cfg = node_cfg("worker_memory_max_kills_only_that_worker", 8);
    in_cgroup(&mut cfg, &root);
    cfg.worker.memory_mb = 160;
    let (sup, addr) = supervised_with(cfg).await;
    let mut big = app("hostile");
    big.limits.cpu_ms = 30_000;
    big.limits.request_timeout_ms = 30_000;
    let (counter, hostile) = (publish("counter", &app("counter")).await, publish("hostile", &big).await);
    assert_eq!(app_get(addr, &hostile, "/x/").await.0, 200);
    let pid = stats_json(addr).await["workers"][worker(&hostile).as_str()]["pid"].as_u64().unwrap();
    let group = cgroup_of(&root, pid);
    assert_eq!(std::fs::read_to_string(group.join("memory.max")).unwrap().trim(), (160u64 << 20).to_string());

    let mut statuses = Vec::new();
    for i in 0..8 {
        statuses.push(app_get(addr, &hostile, &format!("/m{i}/alloc?mb=30")).await.0);
    }
    // The killed worker's cgroup is gone; events count in its parent too.
    assert!(cgroup_stat(&root, "memory.events", "oom_kill ") >= 1, "no OOM kill: {statuses:?}");
    assert_eq!(app_get(addr, &counter, "/n/").await.0, 200, "the other app kept serving");
    // The next request starts a fresh worker.
    assert_eq!(app_get(addr, &hostile, "/after/").await.0, 200);
    let v = stats_json(addr).await;
    assert_ne!(v["workers"][worker(&hostile).as_str()]["pid"].as_u64().unwrap(), pid, "{v}");
    drop(sup);
    remove_cgroup(&root).await;
}

/// `cpu.max` throttles a worker's spinning guests, and the worker stays
/// within `pids.max`.
#[tokio::test(flavor = "multi_thread")]
async fn worker_cpu_and_pids_limits_hold() {
    let root = match test_cgroup("cpu", &["cpu", "pids"]) {
        Ok(r) => r,
        Err(why) => return eprintln!("skipped: {why}"),
    };
    let mut cfg = node_cfg("worker_cpu_and_pids_limits_hold", 8);
    in_cgroup(&mut cfg, &root);
    cfg.worker.cpus = 0.2;
    cfg.worker.pids = 64;
    let (sup, addr) = supervised_with(cfg).await;
    let hostile = publish("hostile", &app("hostile")).await;
    assert_eq!(app_get(addr, &hostile, "/x/").await.0, 200);
    let pid = stats_json(addr).await["workers"][worker(&hostile).as_str()]["pid"].as_u64().unwrap();
    let group = cgroup_of(&root, pid);
    assert_eq!(std::fs::read_to_string(group.join("cpu.max")).unwrap().trim(), "20000 100000");
    assert_eq!(std::fs::read_to_string(group.join("pids.max")).unwrap().trim(), "64");

    let spins: Vec<_> = (0..4)
        .map(|i| {
            let hostile = hostile.clone();
            tokio::spawn(async move { app_get(addr, &hostile, &format!("/spin{i}/spin")).await })
        })
        .collect();
    for s in spins {
        s.await.unwrap();
    }
    assert!(cgroup_stat(&group, "cpu.stat", "nr_throttled ") > 0, "cpu.max did not throttle");
    let pids = cgroup_stat(&group, "pids.current", "");
    assert!((1..=64).contains(&pids), "pids.current = {pids}");
    drop(sup);
    remove_cgroup(&root).await;
}

/// review2 (W1): at startup the supervisor is to remove the `worker-<n>`
/// cgroups an earlier run left. It removes every empty directory under
/// `cgroup` instead: a cgroup the operator made there (not yet holding a
/// process, say one with limits of its own) goes, and with `cgroup` set by
/// mistake to a directory that is no cgroup at all, so do its empty
/// subdirectories. Such a directory is refused, saying why.
#[tokio::test(flavor = "multi_thread")]
async fn review2_startup_removes_only_cgroups_it_made() {
    let root = match test_cgroup("foreign", &[]) {
        Ok(r) => r,
        Err(why) => return eprintln!("skipped: {why}"),
    };
    let theirs = root.join("reserved");
    std::fs::create_dir(&theirs).unwrap();
    let plain = Path::new(env!("CARGO_TARGET_TMPDIR")).join("review2-not-a-cgroup");
    let _ = std::fs::remove_dir_all(&plain);
    std::fs::create_dir_all(plain.join("keep")).unwrap();
    let (mut removed, mut started) = (vec![], vec![]);
    for (dir, kept) in [(&root, &theirs), (&plain, &plain.join("keep"))] {
        let mut cfg = node_cfg("review2_startup_removes_only_cgroups_it_made", 8);
        in_cgroup(&mut cfg, &dir);
        let sup = nanovm::supervisor::Supervisor::new(cfg, PathBuf::from(env!("CARGO_BIN_EXE_nanovm")), 1, None);
        if !kept.exists() {
            removed.push(kept.clone());
        }
        started.push(sup.map(drop).map_err(|e| format!("{e:#}")));
    }
    remove_cgroup(&root).await;
    let _ = std::fs::remove_dir_all(&plain);
    assert!(removed.is_empty(), "startup removed directories it did not make: {removed:?}");
    assert!(started[0].is_ok(), "{:?}", started[0]);
    let refused = started[1].as_ref().is_err_and(|e| e.ends_with(&format!("cgroup {} is not a cgroup v2 directory", plain.display())));
    assert!(refused, "a directory that is no cgroup: {:?}", started[1]);
}

/// Waits up to 10 s for `paths` to be gone.
async fn gone(paths: &[&Path]) {
    let t0 = Instant::now();
    while paths.iter().any(|p| p.exists()) && t0.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The `worker-<n>` cgroups an earlier run left go at startup, also one
/// whose worker is still there (exiting, say, when its supervisor was
/// restarted): it is killed, and its cgroup goes once it has exited,
/// rather than being left for good.
#[tokio::test(flavor = "multi_thread")]
async fn startup_removes_an_earlier_runs_worker_cgroups() {
    let root = match test_cgroup("earlier", &[]) {
        Ok(r) => r,
        Err(why) => return eprintln!("skipped: {why}"),
    };
    let (idle, busy) = (root.join("worker-1000"), root.join("worker-1001"));
    for dir in [&idle, &busy] {
        std::fs::create_dir(dir).unwrap();
    }
    let mut left = std::process::Command::new("sleep").arg("60").spawn().unwrap();
    std::fs::write(busy.join("cgroup.procs"), left.id().to_string()).unwrap();
    let mut cfg = node_cfg("startup_removes_an_earlier_runs_worker_cgroups", 8);
    in_cgroup(&mut cfg, &root);
    let (sup, _) = supervised_with(cfg).await;
    gone(&[&idle, &busy]).await;
    let kept: Vec<_> = [&idle, &busy].into_iter().filter(|d| d.exists()).cloned().collect();
    let killed = left.try_wait().unwrap();
    let _ = left.kill();
    drop(sup);
    remove_cgroup(&root).await;
    assert!(kept.is_empty(), "an earlier run's worker cgroups are left: {kept:?}");
    assert!(killed.is_some(), "an earlier run's worker still runs");
}

/// Two nodes can't run their workers in one cgroup: the second refuses to
/// start, rather than remove at startup the empty `worker-<n>` cgroups it
/// finds there, which can be the first's, made for a worker not yet in it.
#[tokio::test(flavor = "multi_thread")]
async fn two_nodes_cannot_share_a_cgroup() {
    let root = match test_cgroup("shared", &[]) {
        Ok(r) => r,
        Err(why) => return eprintln!("skipped: {why}"),
    };
    let mut cfg = node_cfg("two_nodes_cannot_share_a_cgroup", 8);
    in_cgroup(&mut cfg, &root);
    let (first, addr) = supervised_with(cfg).await;
    let counter = publish("counter", &app("counter")).await;
    let (status, body) = app_get(addr, &counter, "/x/").await;
    assert_eq!(status, 200, "{body}");
    let pid = stats_json(addr).await["workers"][worker(&counter).as_str()]["pid"].clone();
    // As the first node makes a worker's cgroup, before the worker is in it.
    let fresh = root.join("worker-999999");
    std::fs::create_dir(&fresh).unwrap();
    let mut cfg = node_cfg("two_nodes_cannot_share_a_cgroup-second", 8);
    in_cgroup(&mut cfg, &root);
    let second = nanovm::supervisor::Supervisor::new(cfg, PathBuf::from(env!("CARGO_BIN_EXE_nanovm")), 1, None);
    let second = second.map(drop).map_err(|e| format!("{e:#}"));
    let kept = fresh.exists();
    let served = app_get(addr, &counter, "/x/").await.0;
    let now = stats_json(addr).await["workers"][worker(&counter).as_str()]["pid"].clone();
    drop(first);
    remove_cgroup(&root).await;
    assert!(kept, "the second node removed the first's fresh worker cgroup");
    assert_eq!((served, now), (200, pid), "the first node's worker kept serving");
    let refused = second.as_ref().is_err_and(|e| e.ends_with(&format!("another node runs its workers in cgroup {}: lock acquisition failed because the operation would block", root.display())));
    assert!(refused, "{second:?}");
}

/// A worker's cgroup goes once the worker has exited, also when it is
/// dropped without being waited for: here its supervisor's runtime ends,
/// which drops the spare and cancels the task that waits for the app's
/// worker. Either is killed, but its cgroup outlived it.
#[test]
fn a_dropped_supervisors_worker_cgroups_go() {
    let root = match test_cgroup("dropped", &[]) {
        Ok(r) => r,
        Err(why) => return eprintln!("skipped: {why}"),
    };
    let workers = || std::fs::read_dir(&root).unwrap().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect::<Vec<_>>();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let made = rt.block_on(async {
        let mut cfg = node_cfg("a_dropped_supervisors_worker_cgroups_go", 8);
        in_cgroup(&mut cfg, &root);
        let (_sup, addr) = supervised_with(cfg).await;
        let counter = publish("counter", &app("counter")).await;
        let (status, body) = app_get(addr, &counter, "/x/").await;
        assert_eq!(status, 200, "{body}");
        // The app's worker and the next spare.
        eventually("a spare", || workers().len() == 2).await;
        workers()
    });
    drop(rt);
    let t0 = Instant::now();
    while made.iter().any(|d| d.exists()) && t0.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let left: Vec<_> = made.iter().filter(|d| d.exists()).collect();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(remove_cgroup(&root));
    assert!(left.is_empty(), "worker cgroups left 10 s after their supervisor was dropped: {left:?}");
}

/// A `[worker] memory_mb` below what a spare worker uses before it has
/// its app (its engine, mostly) is set when the spare is given its app, so
/// it would OOM-kill the spare, and the start fail with an unrelated error
/// (a broken pipe, say). The start is refused instead, saying why.
#[tokio::test(flavor = "multi_thread")]
async fn a_worker_memory_limit_below_a_spares_use_is_refused_clearly() {
    let root = match test_cgroup("small", &["memory"]) {
        Ok(r) => r,
        Err(why) => return eprintln!("skipped: {why}"),
    };
    let mut cfg = node_cfg("a_worker_memory_limit_below_a_spares_use_is_refused_clearly", 8);
    in_cgroup(&mut cfg, &root);
    cfg.worker.memory_mb = 1;
    let (sup, addr) = supervised_with(cfg).await;
    let counter = publish("counter", &app("counter")).await;
    spare_built(8).await;
    let (status, body) = app_get(addr, &counter, "/x/").await;
    let killed = cgroup_stat(&root, "memory.events", "oom_kill ");
    drop(sup);
    remove_cgroup(&root).await;
    assert_eq!((status, killed), (502, 0), "{body}");
    assert!(body.contains("[worker] memory_mb = 1 is below the"), "{body}");
}

/// review2 (W2): a jailed worker is the only owner of an export runtime,
/// and it never drops its telemetry: retiring it (a switch to another
/// version here) kills it outright, without asking it to flush first. What
/// it recorded since its last export (up to 1 s), here an egress-denied
/// event, never reaches the collector. Asked to stop, it flushes and exits
/// by itself, well before it would be killed (2 s; it would have exported
/// by then anyway).
#[tokio::test(flavor = "multi_thread")]
async fn review2_a_retired_workers_last_telemetry_reaches_the_collector() {
    let (_sup, addr) = supervised("review2_retired_worker_telemetry").await;
    let seen = |name: String| captures().lock()[&addr].1.lock().logs.iter().any(|log| log["app"] == name.as_str() && log["kind"] == "egress-denied");
    // Control: a live worker's event arrives.
    let live = publish("hostile", &app("hostile")).await;
    let (_, body) = app_get(addr, &live, "/s/fetch?url=http://169.254.169.254/").await;
    assert!(body.contains("HttpRequestDenied"), "{body}");
    stats_json(addr).await;
    assert!(seen(worker(&live)), "a live worker's egress-denied event is exported");
    let (mut lost, mut slow) = (vec![], vec![]);
    for round in 0..3 {
        let repo = format!("review2-retired-{round}");
        let v1 = publish_version(&repo, &app("hostile"), guest("hostile"), "1.0.0").await;
        let mut next = app("hostile");
        next.env.insert("NEXT".into(), "1".into());
        let v2 = publish_version(&repo, &next, guest("hostile"), "1.0.1").await;
        assert_ne!(v1, v2);
        let (_, body) = app_get(addr, &v1, "/s/fetch?url=http://169.254.169.254/").await;
        assert!(body.contains("HttpRequestDenied"), "{body}");
        // v1's worker has nothing in flight: it is retired at once.
        let t0 = Instant::now();
        assert_eq!(app_get(addr, &v2, "/x/").await.0, 200);
        if t0.elapsed() >= Duration::from_secs(2) {
            slow.push(t0.elapsed());
        }
        stats_json(addr).await;
        if !seen(worker(&v1)) {
            lost.push(round);
        }
    }
    assert!(lost.is_empty(), "rounds whose egress-denied event a retired worker never exported: {lost:?}");
    assert!(slow.is_empty(), "switches that waited for a retired worker to be killed: {slow:?}");
}

/// An upstream that answers with headers and the start of a large body,
/// then stalls, so each response body stays open. Returns its port.
async fn stalling_upstream() -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = upstream.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = upstream.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\nstart").await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
        }
    });
    port
}

/// `max_outbound_concurrency` (2 here) counts open response bodies, not just
/// requests waiting for headers, both in-process and through the supervisor.
#[tokio::test(flavor = "multi_thread")]
async fn open_response_bodies_count_against_outbound_concurrency() {
    let port = stalling_upstream().await;
    let path = format!("/hold?url=http://127.0.0.1:{port}/&n=3");
    let mut hostile = app("hostile");
    hostile.limits.egress.push(format!("127.0.0.1:{port}"));
    let node = node_with(node_cfg("open_bodies_in_process", 8), hostile.clone());
    let r = get(&node, "h", &path).await;
    assert_eq!(r.json()["held"], 2, "{}", r.body);
    assert!(r.body.contains("ConnectionLimitReached"), "{}", r.body);

    let (_sup, addr) = supervised("open_bodies_supervised").await;
    let (_, body) = app_get(addr, &publish("hostile", &hostile).await, &format!("/h{path}")).await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["held"], 2, "{body}");
    assert!(body.contains("ConnectionLimitReached"), "{body}");
}

// ---- registry apps -------------------------------------------------------
//
// Signature checks load the artifacts CI publishes to GHCR without running
// them. Worker drains use locally composed guests.

const SIGNER: &str = "https://token.actions.githubusercontent.com \
    https://github.com/carsonfarmer/nanovm/.github/workflows/publish-guests.yml@refs/heads/main";

async fn drain_versions(repo: &str) -> (String, String) {
    let mut hostile = app("hostile");
    hostile.limits.cpu_ms = 500;
    hostile.limits.request_timeout_ms = 2000;
    hostile.limits.post_return_timeout_ms = 1000;
    let old = publish_version(repo, &hostile, guest("hostile"), "1.0.0").await;
    let new = publish_version(repo, &hostile, guest("hostile"), "2.0.0").await;
    assert_ne!(old, new);
    (old, new)
}

/// A drain that runs out of time kills the old worker even while a client
/// still holds one of its responses unread, so it can't run beside the next
/// version. (The rollback from the newest version to the one before is a
/// switch like any other.)
#[tokio::test(flavor = "multi_thread")]
async fn a_drain_that_runs_out_of_time_kills_the_old_worker() {
    use tokio::io::AsyncWriteExt;
    let (old, new) = drain_versions("drain-timeout").await;
    let (_sup, addr) = supervised("drain_kills_the_old_worker").await;

    // A client that reads a little of a 7 MiB answer, then stops.
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.set_recv_buffer_size(4096).unwrap();
    let mut held = sock.connect(addr).await.unwrap();
    let head = format!(
        "GET /h/big?mb=7 HTTP/1.1\r\nHost: x\r\n{}\r\n",
        names(&new)
    );
    held.write_all(head.as_bytes()).await.unwrap();
    let mut response = [0; 64];
    let n = tokio::io::AsyncReadExt::read(&mut held, &mut response).await.unwrap();
    assert!(response[..n].starts_with(b"HTTP/1.1 200"), "{}", String::from_utf8_lossy(&response[..n]));
    let pid = stats_json(addr).await["workers"][worker(&new).as_str()]["pid"].as_u64().unwrap();

    let t0 = Instant::now();
    assert_eq!(app_get(addr, &old, "/x/").await.0, 200);
    // The switch waited out the drain (hostile's 2 s + 1 s), then killed it.
    assert!(t0.elapsed() >= Duration::from_secs(3), "{:?}", t0.elapsed());
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists(), "replacement answered before old worker was reaped");
    drop(held);
    let v = stats_json(addr).await;
    assert_eq!(v["workers"][worker(&old).as_str()]["version"], "1.0.0", "{v}");
    assert_ne!(v["workers"][worker(&old).as_str()]["pid"].as_u64().unwrap(), pid);
}

/// Published versions, oldest first, as `(version, ghcr.io/…@digest)`.
async fn published(name: &str) -> Result<Vec<(semver::Version, String)>, String> {
    let repo = format!("ghcr.io/carsonfarmer/nanovm-{name}");
    let oci = nanovm::registry::Oci::new(false).unwrap();
    let r = format!("{repo}:latest").parse().unwrap();
    let tags = oci.get(&r, "tags", "list", "application/json", 1 << 20).await.map_err(|e| format!("can't list {repo}: {e:#}"))?;
    let tags = tags.ok_or_else(|| format!("nothing published at {repo}"))?;
    let tags: serde_json::Value = serde_json::from_slice(&tags).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for tag in tags["tags"].as_array().into_iter().flatten().filter_map(|t| t.as_str()) {
        let Ok(v) = tag.parse::<semver::Version>() else { continue };
        let digest = oci.digest(&format!("{repo}:{tag}").parse().unwrap()).await.map_err(|e| format!("{e:#}"))?;
        out.push((v, format!("{repo}@{digest}")));
    }
    out.sort();
    if out.is_empty() {
        return Err(format!("nothing published at {repo}"));
    }
    Ok(out)
}

/// A supervisor that checks signatures.
async fn signed(test: &str) -> (Arc<nanovm::supervisor::Supervisor>, std::net::SocketAddr) {
    let mut cfg = node_cfg(test, 8);
    // Outside dev mode the operator's egress list names no private address.
    (cfg.dev, cfg.limits.egress) = (false, vec![]);
    supervised_with(cfg).await
}

/// A registry app loads from its signed artifact, and anything short of a
/// trusted signature over that digest at an allowed version is refused.
#[tokio::test(flavor = "multi_thread")]
async fn registry_apps_need_a_trusted_signature() {
    use nanovm::registry::{AppRef, Registry};
    let (version, app) = published("counter").await.expect("published signed counter is reachable").pop().unwrap();
    let cfg = node_cfg("registry_apps_need_a_trusted_signature", 8);
    let registry = Registry::new(cfg.data_dir.join("registry"), cfg.max_component_mb << 20, false).unwrap();
    let reference = |app: &str, trust: &str, min: Option<&str>| {
        let mut headers = http::HeaderMap::new();
        headers.insert("nanovm-app", app.parse().unwrap());
        headers.insert("nanovm-trust", trust.parse().unwrap());
        if let Some(min) = min {
            headers.insert("nanovm-min-version", min.parse().unwrap());
        }
        AppRef::from_headers(&headers).unwrap().unwrap()
    };
    let accepted = reference(&app, SIGNER, Some(&version.to_string()));
    let pulled = registry.load(&accepted).await.unwrap();
    assert_eq!(pulled.version, version);
    assert_eq!(pulled.spec.config["greeting"], "from the registry");
    assert!(std::fs::read(&pulled.component.1).unwrap().starts_with(b"\0asm"));

    let wrong_subject = SIGNER.replace("publish-guests.yml", "other.yml");
    let wrong_issuer = SIGNER.replace("token.actions", "evil");
    let unsigned = app.rsplit_once('@').unwrap().0.to_string() + "@sha256:" + &"0".repeat(64);
    let too_new = format!("{}.0.0", version.major + 1);
    // The cached artifact, under another policy, is checked again (twice:
    // a refusal is not remembered as an acceptance).
    for (name, a, trust, min, why) in [
        ("subject", app.as_str(), wrong_subject.as_str(), None, "not signed by a trusted identity"),
        ("subject", app.as_str(), wrong_subject.as_str(), None, "not signed by a trusted identity"),
        ("issuer", app.as_str(), wrong_issuer.as_str(), None, "not signed by a trusted identity"),
        ("floor", app.as_str(), SIGNER, Some(too_new.as_str()), "below the minimum"),
        ("digest", unsigned.as_str(), SIGNER, None, "not found"),
    ] {
        let error = registry.load(&reference(a, trust, min)).await.err().expect("untrusted artifact is refused");
        assert!(format!("{error:#}").contains(why), "{name}: {error:#}");
    }
    // Refusals do not poison an authorized reload.
    let reloaded = registry.load(&accepted).await.unwrap();
    assert_eq!(reloaded.version, version);
    assert_eq!(reloaded.component, pulled.component);
    // Trust is required at the HTTP boundary, before any instantiation.
    let (_sup, addr) = signed("registry_trust_header").await;
    let head = format!("GET /a/ HTTP/1.1\r\nHost: x\r\nnanovm-app: {app}\r\nConnection: close\r\n\r\n");
    assert_eq!(raw(addr, &head, b"").await.0, 400);
}

/// An app signed by an identity the proxy's operator doesn't list is
/// refused even though its record trusts that identity: the node hears only
/// the identities the operator lets deploy. One the operator lists, here by
/// a prefix, deploys.
#[tokio::test(flavor = "multi_thread")]
async fn only_the_operators_signers_deploy() {
    let (version, app) = published("counter").await.expect("published signed counter is reachable").pop().unwrap();
    let (_sup, node) = signed("only_the_operators_signers_deploy").await;
    let other = "https://accounts.example.com someone@example.com";
    let records = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let txt = format!("app={}:{version}; trust={SIGNER}; trust={other}", app.split_once('@').unwrap().0);
    records.lock().insert("_nanovm.counter.test".to_string(), vec![txt]);
    let resolver = doh(records).await;
    let github = "https://token.actions.githubusercontent.com https://github.com/carsonfarmer/*";
    for (signer, allowed) in [(other, false), (github, true)] {
        let cfg = nanovm::config::ProxyConfig {
            nodes: vec![node.to_string()], resolver: resolver.clone(), listen: "127.0.0.1:0".into(),
            hosts: vec!["counter.test".into()], signers: vec![signer.into()], ..Default::default()
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(nanovm::proxy::serve(nanovm::proxy::Proxy::new(cfg).unwrap(), listener));
        let (status, body) = raw(addr, "GET /a/ HTTP/1.1\r\nHost: counter.test\r\nConnection: close\r\n\r\n", b"").await;
        match allowed {
            true => assert_eq!(status, 200, "{body}"),
            false => assert!(status == 502 && body.contains("not signed by a trusted identity"), "{status} {body}"),
        }
    }
}

/// Naming a new version switches the app: requests in flight on the old
/// worker finish, the new worker serves the same volumes.
#[tokio::test(flavor = "multi_thread")]
async fn registry_app_switches_versions_after_a_drain() {
    let (old_app, new_app) = drain_versions("drain-switch").await;
    let (_sup, addr) = supervised("registry_app_switches_versions").await;

    // Cache both versions before testing the drain: downloading or
    // compiling the replacement could otherwise outlast the sleeping request.
    let (status, body) = app_get(addr, &new_app, "/warm/").await;
    assert_eq!(status, 200, "{body}");
    // Persistence needs only a tiny file and key; quotas have their own tests.
    let expected = serde_json::json!({"file": "before-switch", "key": "before-switch"});
    let (status, body) = app_get(addr, &old_app, "/w/commit?v=before-switch").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap(), expected);
    let old_worker = worker(&old_app);
    let old_pid = stats_json(addr).await["workers"][&old_worker]["pid"].as_u64().unwrap();
    let slow = tokio::spawn(async move { app_get(addr, &old_app, "/s/sleep?ms=1500").await });
    let started = Instant::now();
    loop {
        let v = stats_json(addr).await;
        if v["apps"][&old_worker]["phases"]["handling"] == 1 {
            break;
        }
        assert!(!slow.is_finished(), "old request finished before the switch: {v}");
        assert!(started.elapsed() < Duration::from_secs(2), "old request never entered handling: {v}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let (status, body) = app_get(addr, &new_app, "/w/commit").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap(), expected);
    // Observe worker lifetime, not the scheduling order of two HTTP clients.
    assert!(!Path::new(&format!("/proc/{old_pid}")).exists(), "old worker survived the switch");
    let (slow_status, slow_body) = slow.await.unwrap();
    assert_eq!(slow_status, 200, "the old version's request finished: {slow_body}");
    let v = stats_json(addr).await;
    assert_eq!(v["workers"][worker(&new_app).as_str()]["version"], "2.0.0", "{v}");
    assert_eq!(v["workers"][worker(&new_app).as_str()]["starts"], 3, "{v}");
}

// ---- composed spin:key-value --------------------------------------------

/// Values survive the instance, live in the object's own volume, and go away
/// when deleted.
#[tokio::test(flavor = "multi_thread")]
async fn kv_values_persist_in_the_volume() {
    let node = node("kv_values_persist", "counter", 8);
    assert_eq!(request(&node, "PUT", "k", "/kv/greeting", "hello").await.status, 204);
    assert_eq!(get(&node, "k", "/kv/greeting").await.body, "hello");
    assert_eq!(get(&node, "other", "/kv/greeting").await.status, 404, "objects don't share keys");

    node.shutdown().await;
    assert_eq!(get(&node, "k", "/kv/greeting").await.body, "hello", "a new instance sees it");
    let file = object_dir(&node.cfg.data_dir.join("objects/counter"), "k").join("data/.kv/default/Z3JlZXRpbmc");
    assert_eq!(std::fs::read(file).unwrap(), b"hello");

    assert_eq!(request(&node, "DELETE", "k", "/kv/greeting", "").await.status, 204);
    assert_eq!(get(&node, "k", "/kv/greeting").await.status, 404);
}

/// Keys count against the object's file limit and values against its disk
/// budget, which `/data` and `/tmp` share.
#[tokio::test(flavor = "multi_thread")]
async fn kv_counts_against_the_disk_quota() {
    let node = node_with(node_cfg("kv_quota", 8), filler());
    // An 8 MiB budget holds exactly eight 1 MiB values.
    let r = get(&node, "q", "/kv?op=fill&size=1048576&n=100").await;
    assert_eq!(r.json()["set"], 8, "{}", r.body);
    assert!(r.body.contains("InsufficientSpace"), "{}", r.body);
    // The filesystem sees the same budget as spent.
    let r = get(&node, "q", "/fill?mount=tmp&mb=2").await;
    assert!(r.body.contains("insufficient-space"), "{}", r.body);

    // 200 entries: the two bucket directories, then one file per key.
    let r = get(&node, "f", "/kv?op=fill&size=1").await;
    assert_eq!(r.json()["set"], 198, "{}", r.body);
    assert!(r.body.contains("Quota"), "{}", r.body);
    let r = get(&node, "f", "/kv?op=list").await;
    assert_eq!(r.json(), serde_json::json!({"keys":198}), "full stores remain readable: {}", r.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn kv_refuses_bad_names_and_streams_keys() {
    // Writing 1,500 keys takes real CPU, and on a busy disk more than 10 s
    // (N-32); give it room on a loaded host.
    let mut hostile = app("hostile");
    hostile.limits.cpu_ms = 5_000;
    hostile.limits.max_files = 3000;
    hostile.limits.request_timeout_ms = 60_000;
    let node = node_with(node_cfg("kv_refuses", 8), hostile);
    let call = |path: &'static str| {
        let node = node.clone();
        async move { get(&node, "b", path).await }
    };
    assert!(call("/kv?op=key&len=180").await.body.contains(r#""set":true"#));
    assert!(call("/kv?op=key&len=181").await.body.contains("keys are 1 to 180 bytes"));
    assert!(call("/kv?op=key&len=0").await.body.contains("keys are 1 to 180 bytes"));
    for name in ["../x", ".kv", "a/b", &"n".repeat(65)] {
        let r = get(&node, "b", &format!("/kv?name={name}")).await;
        assert!(r.body.contains("NoSuchStore"), "{name}: {}", r.body);
    }
    let r = get(&node, "p", "/kv?name=streamed&op=fill&size=1&n=1500").await;
    assert_eq!(r.json()["set"], 1500, "{}", r.body);
    let r = get(&node, "p", "/kv?name=streamed&op=list").await;
    assert_eq!(r.json(), serde_json::json!({"keys": 1500}), "{}", r.body);
    let r = get(&node, "p", "/kv?name=empty&op=list").await;
    assert_eq!(r.json(), serde_json::json!({"keys": 0}), "{}", r.body);
}

/// Key-value works inside the jail (Landlock and seccomp allow its file calls).
#[tokio::test(flavor = "multi_thread")]
async fn kv_works_in_a_jailed_worker() {
    let (_sup, addr) = supervised("kv_works_in_a_jailed_worker").await;
    let (counter, hostile) = (publish("counter", &app("counter")).await, publish("hostile", &app("hostile")).await);
    let put = format!("PUT /j/kv/a HTTP/1.1\r\nHost: x\r\n{}Content-Length: 2\r\nConnection: close\r\n\r\n", names(&counter));
    let (status, body) = raw(addr, &put, b"hi").await;
    assert_eq!(status, 204, "{body}");
    assert_eq!(app_get(addr, &counter, "/j/kv/a").await.1, "hi");
    let delete = format!("DELETE /j/kv/a HTTP/1.1\r\nHost: x\r\n{}Connection: close\r\n\r\n", names(&counter));
    assert_eq!(raw(addr, &delete, b"").await.0, 204);
    assert_eq!(app_get(addr, &counter, "/j/kv/a").await.0, 404);
    let (_, body) = app_get(addr, &hostile, "/j/kv?op=list").await;
    assert!(body.contains(r#""keys":0"#), "{body}");

    // A directory planted over a key's file: the set is refused, and leaves
    // nothing behind that the quota doesn't count.
    // A first write, not opening the bucket, now creates its directories.
    let (_, body) = app_get(addr, &hostile, "/p/kv?op=key&len=1").await;
    assert!(body.contains(r#""set":true"#), "{body}");
    let (_, body) = app_get(addr, &hostile, "/p/kv?op=plant").await;
    assert!(body.contains("IsDirectory"), "{body}");
    let bucket = std::fs::read_dir(object_dir(&node_dir("kv_works_in_a_jailed_worker").join("objects").join(worker(&hostile)), "p").join("data/.kv/default")).unwrap();
    let mut names: Vec<_> = bucket.flatten().map(|e| e.file_name()).collect();
    names.sort();
    assert_eq!(names, ["aw", "azA"], "only the seed and planted directory are there");
}

fn node_dir(test: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("nodes").join(test)
}

/// The provider bounds both writes and reads to 1 MiB per value, including
/// files written outside the KV interface.
#[tokio::test(flavor = "multi_thread")]
async fn kv_values_are_bounded() {
    let node = node("kv_bounded", "hostile", 8);
    let r = get(&node, "m", "/kv?op=fill&size=1048576&n=1").await;
    assert_eq!(r.json()["set"], 1, "{}", r.body);
    assert_eq!(get(&node, "m", "/kv?op=get").await.json()["bytes"], 1 << 20);
    let r = get(&node, "m", "/kv?op=fill&size=1048577&n=1").await;
    assert_eq!(r.json()["set"], 0, "{}", r.body);
    assert!(r.body.contains("value exceeds 1 MiB"), "{}", r.body);
    assert_eq!(get(&node, "m", "/kv?op=get").await.json()["bytes"], 1 << 20, "failed write keeps old value");

    node.shutdown().await;
    let file = object_dir(&node.cfg.data_dir.join("objects/hostile"), "m").join("data/.kv/default/azA");
    std::fs::write(file, vec![7u8; (1 << 20) + 1]).unwrap();
    let r = get(&node, "m", "/kv?op=get").await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("value exceeds 1 MiB"), "{}", r.body);
}

/// An app's `config` values are what its guests read through `wasi:config`.
#[tokio::test(flavor = "multi_thread")]
async fn wasi_config_serves_the_apps_values() {
    let mut counter = app("counter");
    counter.config.insert("greeting".into(), "hello from config".into());
    let node = node_with(node_cfg("wasi_config", 8), counter);
    let r = get(&node, "c", "/config/greeting").await;
    assert_eq!((r.status, r.body.as_str()), (200, "hello from config"));
    assert_eq!(get(&node, "c", "/config/missing").await.status, 404);
}

// ---- commits ---------------------------------------------------------------

/// Composing HTTP, KV and SQL must not reduce configured live-object capacity.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires SQLite build.sh --compose"]
async fn sqlite_composed_pool_capacity() {
    let cfg = node_cfg("sqlite_pool", 100);
    let mut app = app("counter");
    app.limits.memory_mb = 64;
    app.limits.cpu_ms = 2000;
    app.limits.request_timeout_ms = 5000;
    app.limits.idle_timeout_ms = 600_000;
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/composed.wasm")).unwrap();
    let node = Node::new(cfg, &app, &wasm, None).unwrap();
    for reused in [false, true] {
        for i in 0..100 {
            let r = get(&node, &format!("o{i}"), "/").await;
            assert_eq!(r.status, 200, "object {i}: {}", r.body);
            assert_eq!(r.json()["reused"], reused, "object {i}: {}", r.body);
        }
    }
    node.shutdown().await;
}

/// The SQLite engine is a separate component, not linked into the HTTP client.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires SQLite build.sh --compose and wac 0.12.0"]
async fn sqlite_composed_provider() {
    let cfg = node_cfg("sqlite_composed", 2);
    let mut app = app("counter");
    app.limits.memory_mb = 64;
    app.limits.cpu_ms = 2000;
    app.limits.request_timeout_ms = 5000;
    let client = std::fs::read(repo().join("target/sqlite-filesystem/wasm32-wasip2/release/sqlite_client.wasm")).unwrap();
    let why = Node::new(cfg.clone(), &app, &client, None).err().expect("uncomposed SQL must not link");
    assert!(format!("{why:#}").contains("spin:sqlite/sqlite@3.1.0"));
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/composed.wasm")).unwrap();
    let node = Node::new(cfg.clone(), &app, &wasm, None).unwrap();
    let expected = |rows: &[&str], marker: &str, reused: bool| {
        serde_json::json!({"rows": rows, "file": marker, "key": marker, "reused": reused})
    };
    let first = get(&node, "a", "/?v=first").await;
    assert_eq!(first.status, 200, "{:?}: {}", first.error, first.body);
    assert_eq!(first.json(), expected(&["first"], "first", false));
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first"], "first", true));
    let limits = get(&node, "a", "/limits").await;
    assert_eq!(limits.status, 200, "{}", limits.body);
    assert_eq!(limits.json(), expected(&["first"], "first", true));
    assert_eq!(get(&node, "a", "/nohooks").await.json(), expected(&["first"], "first", true));
    for then in ["error", "trap"] {
        let fail = get(&node, "a", &format!("/?v=discard&then={then}")).await;
        assert_eq!(fail.error.as_deref(), Some(if then == "error" { "guest-error" } else { "trap" }), "{}", fail.body);
        if then == "trap" {
            // Simulate an unwriteable database during host recovery. Neither
            // a failed startup nor its retry may discard the remaining undo.
            let base = node.cfg.app_dirs(&app.name)[0].clone();
            let db = object_dir(&base, "a").join("data/composed.db");
            let parked = base.join("parked.db");
            std::fs::rename(&db, &parked).unwrap();
            std::fs::create_dir(&db).unwrap();
            let log = object_dir(&base, "a").join("journal/log");
            for _ in 0..2 {
                let blocked = get(&node, "a", "/").await;
                assert!(blocked.status >= 500, "{}", blocked.body);
                assert!(std::fs::metadata(&log).unwrap().len() > 0, "keep undo for retry");
            }
            std::fs::remove_dir(&db).unwrap();
            std::fs::rename(parked, db).unwrap();
        }
        assert_eq!(get(&node, "a", "/").await.json(), expected(&["first"], "first", false));
    }
    let denials = node.metrics.fs_denied.load(Relaxed);
    let full = get(&node, "a", "/full?v=discard-full").await;
    assert_eq!(full.error.as_deref(), Some("trap"), "{}", full.body);
    assert!(full.body.contains("SQLite storage failure"));
    assert!(node.metrics.fs_denied.load(Relaxed) > denials);
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first"], "first", false));
    let limited = get(&node, "a", "/write-result-limit?v=discard-limit").await;
    assert_eq!(limited.error.as_deref(), Some("trap"), "{}", limited.body);
    assert!(limited.body.contains("SQLite write result limit"));
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first"], "first", false));
    let invalid = get(&node, "a", "/write-invalid-text?v=discard-text").await;
    assert_eq!(invalid.error.as_deref(), Some("trap"), "{}", invalid.body);
    assert!(invalid.body.contains("SQLite write result conversion"));
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first"], "first", false));
    let response = get(&node, "a", "/invalid-text").await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.json(), expected(&["first"], "first", true));
    let killed = get(&node, "a", "/cpu?v=discard-cpu").await;
    assert_eq!(killed.error.as_deref(), Some("cpu"), "{}", killed.body);
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first"], "first", false));
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first"], "first", true));
    let response = get(&node, "a", "/constraint?v=second&then=status").await;
    assert_eq!(response.status, 507);
    assert_eq!(response.json(), expected(&["first", "second"], "second", true));
    assert_eq!(get(&node, "a", "/close").await.json(), expected(&["first", "second"], "second", true));
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first", "second"], "second", false));
    assert_eq!(get(&node, "b", "/").await.json(), expected(&[], "", false));
    node.shutdown().await;
    drop(node);
    let node = Node::new(cfg, &app, &wasm, None).unwrap();
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first", "second"], "second", false));
    node.shutdown().await;
    println!("composed Spin SQLite: typed values, one connection, policy denial, drop/reopen, isolation, mixed rollback and restart passed");
}

/// Transfer only immutable captures, then remove the original node's entire root.
/// This exercises same-host recovery mechanics, not a remotely durable receipt.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires SQLite build.sh --compose"]
async fn sqlite_captures_recover_without_the_original_node() {
    use std::io::Read;
    use sha2::{Digest, Sha256};
    let mut cfg = node_cfg("sqlite_capture_source", 2);
    cfg.capture = Some(Default::default());
    let mut app = app("counter");
    app.limits.memory_mb = 64;
    app.limits.cpu_ms = 2000;
    app.limits.request_timeout_ms = 5000;
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/composed.wasm")).unwrap();
    let node = Node::new(cfg.clone(), &app, &wasm, None).unwrap();
    let expected = |rows: &[&str], marker: &str, reused: bool| {
        serde_json::json!({"rows": rows, "file": marker, "key": marker, "reused": reused})
    };
    assert_eq!(get(&node, "a", "/?v=first").await.json(), expected(&["first"], "first", false));
    assert_eq!(get(&node, "a", "/?v=rejected&then=trap").await.status, 500);
    assert_eq!(get(&node, "a", "/?v=second").await.json(), expected(&["first", "second"], "second", false));
    settle(&node).await;
    let source = object_dir(&cfg.app_dirs(&app.name)[0], "a").join("committed");
    let mut fresh = node_cfg("sqlite_capture_recovered", 2);
    fresh.capture = cfg.capture.clone();
    let objects = fresh.app_dirs(&app.name)[0].clone();
    let received = object_dir(&objects, "a").join("committed");
    std::fs::create_dir_all(&received).unwrap();
    let mut versions: Vec<_> = std::fs::read_dir(&source).unwrap().map(|e| e.unwrap().path()).collect();
    versions.sort();
    assert_eq!(versions.len(), 2, "rejected request must not enter history");
    let mut parent = String::new();
    for (index, path) in versions.iter().enumerate() {
        let bytes = std::fs::read(path).unwrap();
        let mut archive = tar::Archive::new(&bytes[..]);
        let mut entries = archive.entries().unwrap();
        let mut first = entries.next().unwrap().unwrap();
        assert_eq!(first.path().unwrap(), Path::new("nanovm.json"));
        let mut metadata = Vec::new();
        first.read_to_end(&mut metadata).unwrap();
        let version: serde_json::Value = serde_json::from_slice(&metadata).unwrap();
        assert_eq!(version["app"], app.name);
        assert_eq!(version["object"], "a");
        assert_eq!(version["sequence"], index + 1);
        assert_eq!(version["parent"], parent);
        assert_eq!(version["epoch"], 0, "one node: every lease its own, at epoch 0");
        parent = format!("{:x}", Sha256::digest(&bytes));
        let target = received.join(path.file_name().unwrap());
        std::fs::write(&target, &bytes).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), bytes);
        std::fs::File::open(&target).unwrap().sync_all().unwrap();
    }
    std::fs::File::open(&received).unwrap().sync_all().unwrap();
    // A later local success is deliberately not transferred. It cannot be
    // recovered and must never be advertised as remotely durable.
    assert_eq!(get(&node, "a", "/?v=unsent").await.status, 200);
    node.shutdown().await;
    drop(node);
    std::fs::remove_dir_all(&cfg.data_dir).unwrap();
    assert_eq!(nanovm::committed::restore(&received, &object_dir(&objects, "a").join("data")).unwrap(), 2);
    let node = Node::new(fresh.clone(), &app, &wasm, None).unwrap();
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first", "second"], "second", false));
    assert_eq!(get(&node, "a", "/?v=continued").await.json(), expected(&["first", "second", "continued"], "continued", true));
    settle(&node).await;
    assert_eq!(std::fs::read_dir(&received).unwrap().count(), 3, "restored history must continue, not restart");
    node.shutdown().await;
    drop(node);
    let node = Node::new(fresh, &app, &wasm, None).unwrap();
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first", "second", "continued"], "continued", false));
    node.shutdown().await;
    println!("source root removed: transferred SQL/file/KV recovered together; rejected and unsent writes absent; history continued across restart");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires SQLite build.sh --compose"]
async fn sqlite_composed_recovery_cuts() {
    let cfg = node_cfg("sqlite_composed_cuts", 2);
    let mut app = app("counter");
    app.limits.memory_mb = 64;
    app.limits.cpu_ms = 5000;
    app.limits.request_timeout_ms = 10_000;
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/composed-faults.wasm")).unwrap();
    let node = Node::new(cfg, &app, &wasm, None).unwrap();
    let overlap = get(&node, "overlap", "/overlap").await;
    assert_eq!(overlap.status, 200, "{}", overlap.body);
    assert_eq!(overlap.json()["scheduling"], "serialized");
    assert_eq!(get(&node, "timeout", "/?v=before").await.status, 200);
    let timed_out = get(&node, "timeout", "/timeout?v=discard-timeout").await;
    assert_eq!(timed_out.error.as_deref(), Some("timeout"), "{}", timed_out.body);
    let entered = object_dir(&node.cfg.app_dirs(&app.name)[0], "timeout").join("data/sql-wait");
    assert_eq!(std::fs::read(&entered).unwrap(), b"entered", "must reach SQLite's blocking wait");
    let recovered = get(&node, "timeout", "/").await;
    assert_eq!(recovered.json(), serde_json::json!({"rows":["before"],"file":"before","key":"before","reused":false}));
    assert!(!entered.exists(), "the interrupted SQL function's write must roll back too");
    let expected = |changed: bool| {
        let mut rows: Vec<_> = (1..=if changed {12} else {16}).map(|i| {
            serde_json::json!([i, i*3 + if changed {7} else {0}, if changed {8192} else {1024}])
        }).collect();
        if changed { rows.push(serde_json::json!([17,51,3072])); }
        rows
    };
    assert_eq!(get(&node, "control", "/cuts?op=init").await.status, 200);
    let control = get(&node, "control", "/cuts?op=mutate").await;
    assert_eq!(control.status, 200, "{}", control.body);
    assert_eq!(control.json()["rows"], serde_json::json!(expected(true)));
    assert_eq!(control.json()["file"], "after");
    assert_eq!(control.json()["key"], "after");
    let steps = control.json()["steps"].as_u64().unwrap();
    assert!(steps > 0 && steps < 1000);
    for cut in 1..=steps {
        let object = format!("cut-{cut}");
        assert_eq!(get(&node, &object, "/cuts?op=init").await.status, 200);
        let failed = get(&node, &object, &format!("/cuts?op=mutate&cut={cut}")).await;
        assert_eq!(failed.error.as_deref(), Some("trap"), "cut {cut}: {}", failed.body);
        assert!(failed.body.contains(&format!("SQLite VFS cut {cut} after")), "{}", failed.body);
        let recovered = get(&node, &object, "/cuts").await;
        assert_eq!(recovered.status, 200, "{}", recovered.body);
        assert_eq!(recovered.json()["rows"], serde_json::json!(expected(false)), "cut {cut}");
        assert_eq!(recovered.json()["file"], "before");
        assert_eq!(recovered.json()["key"], "before");
    }
    println!("composed provider: {steps} VFS cuts recovered exact SQL/file/KV state; concurrent calls serialized");
    node.shutdown().await;
}

/// Opt-in experiment: build both fixtures with experiments/sqlite-filesystem/build.sh.
/// No stock P2 filesystem import may bypass nanovm's journal to make SQLite work.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the separately built SQLite experiment (WASI SDK)"]
async fn sqlite_over_the_existing_filesystem() {
    let fixture = |name: &str| std::fs::read(repo().join("target/sqlite-filesystem").join(name)).unwrap();
    let cfg = node_cfg("sqlite_filesystem", 2);
    let mut app = app("counter");
    app.limits.cpu_ms = 2000;
    app.limits.request_timeout_ms = 5000;
    let err = Node::new(cfg.clone(), &app, &fixture("stock.wasm"), None)
        .err()
        .expect("P2 must be refused");
    let why = format!("{err:#}");
    assert!(
        why.contains("wasi:filesystem/") && why.contains("not permitted"),
        "{why}"
    );
    println!("stock SQLite rejected: {why}");
    let wasm = fixture("p3.wasm");
    let node = Node::new(cfg.clone(), &app, &wasm, None).unwrap();
    let expected = |rows: &[&str], marker: &str| {
        serde_json::json!({
            "rows": rows, "file": marker, "key": marker, "integrity": "ok", "blob_bytes": 0
        })
    };
    let r = get(&node, "a", "/?v=first").await;
    assert_eq!(r.status, 200, "{:?}: {}", r.error, r.body);
    assert_eq!(r.json(), expected(&["first"], "first"));
    for then in ["error", "panic", "spin", "sql", "sleep"] {
        let r = get(&node, "a", &format!("/?v=discard-{then}&then={then}")).await;
        assert_eq!(
            r.status,
            if then == "sleep" { 504 } else { 500 },
            "{then}: {:?}: {}",
            r.error,
            r.body
        );
        assert_eq!(
            r.error.as_deref(),
            Some(match then {
                "error" => "guest-error",
                "panic" => "trap",
                "spin" | "sql" => "cpu",
                "sleep" => "timeout",
                _ => unreachable!(),
            })
        );
        let read = get(&node, "a", "/").await;
        assert_eq!(read.json(), expected(&["first"], "first"), "after {then}");
        println!("{then}: SQL, file and KV rolled back; integrity_check=ok");
    }
    let denials = node.metrics.fs_denied.load(Relaxed);
    let r = get(&node, "a", "/full?v=discard-full").await;
    assert_eq!(r.status, 500, "{}", r.body);
    assert!(node.metrics.fs_denied.load(Relaxed) > denials, "the host must enforce its disk quota");
    assert!(
        r.body.contains("sqlite:"),
        "expected SQLite error, not unrelated kill: {}",
        r.body
    );
    assert_eq!(get(&node, "a", "/").await.json(), expected(&["first"], "first"));
    println!("quota refusal: SQL, file and KV rolled back; integrity_check=ok");
    let r = get(&node, "a", "/constraint?v=second&then=status").await;
    assert_eq!(r.status, 507, "{}", r.body);
    assert_eq!(r.json(), expected(&["first", "second"], "second"));
    assert_eq!(
        get(&node, "a", "/transaction").await.json(),
        expected(&["first", "second"], "second")
    );
    let database = object_dir(&cfg.app_dirs(&app.name)[0], "a").join("data/probe.db");
    let before = std::fs::metadata(&database).unwrap().len();
    let mut grown = expected(&["first", "second"], "second");
    grown["blob_bytes"] = 262144.into();
    assert_eq!(get(&node, "a", "/grow").await.json(), grown);
    let large = std::fs::metadata(&database).unwrap().len();
    assert!(large > before, "the blob must grow the file, not just SQLite's cache");
    assert_eq!(
        get(&node, "a", "/vacuum").await.json(),
        expected(&["first", "second"], "second")
    );
    let compact = std::fs::metadata(&database).unwrap().len();
    assert!(compact < large);
    println!("database growth and VACUUM: {before} -> {large} -> {compact} bytes; inner SQL rollback passed");
    assert_eq!(get(&node, "b", "/").await.json(), expected(&[], ""));
    settle(&node).await;
    assert_eq!(get(&node, "c", "/?v=other").await.status, 200);
    settle(&node).await;
    assert!(node.metrics.evicted_lru.load(Relaxed) > 0);
    assert_eq!(
        get(&node, "a", "/").await.json(),
        expected(&["first", "second"], "second")
    );
    node.shutdown().await;
    drop(node);
    let restarted = Node::new(cfg, &app, &wasm, None).unwrap();
    assert_eq!(
        get(&restarted, "a", "/").await.json(),
        expected(&["first", "second"], "second")
    );
    println!("handled constraint + HTTP 507 committed; VACUUM, isolation, eviction and node restart passed");
    restarted.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the separately built SQLite experiment (WASI SDK)"]
async fn sqlite_survives_a_jailed_worker_crash() {
    for fixture in ["p3", "composed"] {
        let mut cfg = node_cfg(&format!("sqlite_worker_crash_{fixture}"), 8);
        let mut app = app("counter");
        app.limits.memory_mb = 64;
        app.limits.cpu_ms = 2000;
        app.limits.request_timeout_ms = 30_000;
        // The supervisor clamps tenant budgets; this is a crash-recovery
        // test, not a test of the default 200 ms CPU ceiling.
        cfg.limits.cpu_ms = app.limits.cpu_ms;
        let (_sup, addr) = supervised_with(cfg.clone()).await;
        let wasm = std::fs::read(repo().join(format!("target/sqlite-filesystem/{fixture}.wasm"))).unwrap();
        let image = publish_wasm("sqlite-probe", &app, wasm).await;
        let (status, before) = app_get(addr, &image, "/a/?v=durable").await;
        assert_eq!(status, 200, "{before}");
        let name = worker(&image);
        let pid = stats_json(addr).await["workers"][&name]["pid"].as_u64().unwrap() as i32;
        let pending = tokio::spawn({
            let image = image.clone();
            async move { app_get(addr, &image, "/a/?v=uncommitted&then=sleep").await }
        });
        // Both clients write markers after SQLite's INSERT completes, before response commit.
        let marker = object_dir(&cfg.app_dirs(&name)[0], "a").join("data/marker");
        let key = object_dir(&cfg.app_dirs(&name)[0], "a").join("data/.kv/default/bWFya2Vy");
        eventually("SQLite, file and KV writes before SIGKILL", || {
            std::fs::read(&marker).is_ok_and(|v| v == b"uncommitted")
                && std::fs::read(&key).is_ok_and(|v| v == b"uncommitted")
        }).await;
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        let _ = pending.await;
        let start = Instant::now();
        while stats_json(addr).await["workers"][&name]["up"] == true {
            assert!(start.elapsed() < Duration::from_secs(5), "worker still up");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (status, after) = app_get(addr, &image, "/a/").await;
        assert_eq!(status, 200, "{after}");
        let mut expected = serde_json::json!({"rows": ["durable"], "file": "durable", "key": "durable"});
        if fixture == "p3" {
            expected["integrity"] = "ok".into();
            expected["blob_bytes"] = 0.into();
        } else {
            expected["reused"] = false.into();
        }
        assert_eq!(serde_json::from_str::<serde_json::Value>(&after).unwrap(), expected);
        println!("{fixture}: jailed worker SIGKILL after SQLite commit, before response: SQL/file/KV recovered together; integrity_check=ok");
    }
}

/// The node is lost after a real process crash; a fresh node restores the
/// app from the store its versions were shipped to, through `nanovm admin`,
/// and serves: what was committed, without what the crash cut short, then
/// new writes, which survive another restart.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the separately built SQLite experiment (WASI SDK)"]
async fn sqlite_restores_a_lost_node_from_the_store() {
    async fn start(cfg: &NodeConfig, path: &Path) -> (tokio::process::Child, std::net::SocketAddr) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut cfg = cfg.clone();
        cfg.listen = addr.to_string();
        std::fs::write(path, toml::to_string(&cfg).unwrap()).unwrap();
        drop(listener);
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_nanovm"))
            .args(["serve", "--config"]).arg(path).kill_on_drop(true)
            .stderr(std::fs::File::create(path.with_extension("log")).unwrap())
            .spawn().unwrap();
        let client = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(1)).build().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(child.try_wait().unwrap().is_none(), "supervisor failed; inspect {}", path.display());
            if client.get(format!("http://{addr}/_nanovm/ready")).send().await.is_ok_and(|r| r.status() == 204) {
                return (child, addr);
            }
            assert!(Instant::now() < deadline, "supervisor not ready");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    let mut cfg = node_cfg("sqlite_restore_source", 8);
    let workspace = node_cfg("sqlite_restore_drill", 8).data_dir;
    let store = workspace.join("store");
    cfg.capture = Some(nanovm::config::CaptureConfig { store: Some(store.to_str().unwrap().into()), ..Default::default() });
    let mut app = app("counter");
    app.limits.memory_mb = 64;
    app.limits.cpu_ms = 2000;
    app.limits.request_timeout_ms = 30_000;
    cfg.limits.cpu_ms = app.limits.cpu_ms;
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/composed.wasm")).unwrap();
    let image = publish_wasm("restore-probe", &app, wasm).await;
    let name = worker(&image);
    let shipped = |n: u64| std::fs::read(store.join(&name).join("a/head")).ok()
        .is_some_and(|h| serde_json::from_slice::<nanovm::ship::Head>(&h).unwrap().sequence >= n);
    let (mut source, addr) = start(&cfg, &workspace.join("source.toml")).await;
    let (status, body) = app_get(addr, &image, "/a/?v=durable").await;
    assert_eq!(status, 200, "{body}");
    let source_objects = cfg.data_dir.join("objects");
    eventually("the durable write shipped", || shipped(1) && nanovm::ship::backlog(&source_objects).versions == 0).await;
    let children: Vec<i32> = std::fs::read_dir(format!("/proc/{}/task", source.id().unwrap())).unwrap()
        .flat_map(|thread| std::fs::read_to_string(thread.unwrap().path().join("children")).unwrap()
            .split_whitespace().map(|pid| pid.parse().unwrap()).collect::<Vec<_>>())
        // A jailed worker runs under its seccomp filter; the spare spawned
        // ahead (N-224) has not been given an app or entered its jail yet.
        .filter(|pid| std::fs::read_to_string(format!("/proc/{pid}/status")).is_ok_and(|s| s.contains("Seccomp:\t2")))
        .collect();
    assert_eq!(children.len(), 1, "one jailed app worker");
    let pid = children[0];
    let pending = tokio::spawn({
        let image = image.clone();
        async move { app_get(addr, &image, "/a/?v=uncommitted&then=sleep").await }
    });
    let objects = cfg.app_dirs(&name)[0].clone();
    eventually("tentative SQL/file/KV changes", || {
        std::fs::read(object_dir(&objects, "a").join("data/marker")).is_ok_and(|v| v == b"uncommitted")
            && std::fs::read(object_dir(&objects, "a").join("data/.kv/default/bWFya2Vy")).is_ok_and(|v| v == b"uncommitted")
    }).await;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    let _ = pending.await;
    eventually("worker reaped", || !Path::new(&format!("/proc/{pid}")).exists()).await;
    source.kill().await.unwrap();
    assert!(object_dir(&objects, "a").join("journal/log").exists(), "the crash left undo");
    // The node is lost: a fresh one, on an empty root, restores the app.
    std::fs::remove_dir_all(&cfg.data_dir).unwrap();
    cfg.data_dir = workspace.join("fresh");
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (mut destination, addr) = start(&cfg, &workspace.join("restored.toml")).await;
    let (ok, out) = tokio::task::spawn_blocking({
        let (cfg, name) = (cfg.clone(), name.clone());
        move || admin(&cfg, &["restore", &name])
    }).await.unwrap();
    assert!(ok, "{out}");
    let restored_objects = cfg.app_dirs(&name)[0].clone();
    assert!(!object_dir(&restored_objects, "a").join("journal/log").exists());
    let (status, body) = app_get(addr, &image, "/a/").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"rows": ["durable"], "file": "durable", "key": "durable", "reused": false}));
    let (status, body) = app_get(addr, &image, "/a/?v=after-restore").await;
    assert_eq!(status, 200, "{body}");
    destination.kill().await.unwrap();
    // Parent-death cleanup releases admission before the second fresh-root start.
    eventually("restored worker storage released", || {
        std::fs::File::open(&restored_objects).unwrap().try_lock().is_ok()
    }).await;
    let (mut reopened, addr) = start(&cfg, &workspace.join("reopened.toml")).await;
    let (status, body) = app_get(addr, &image, "/a/").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"rows": ["durable", "after-restore"], "file": "after-restore", "key": "after-restore", "reused": false}));
    reopened.kill().await.unwrap();
    println!("lost node restored from the store: crash-left SQL/file/KV absent; new writes survived another restart");
}

/// Faults at every VFS mutation/sync boundary of a multi-database transaction.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the separately built SQLite experiment (WASI SDK)"]
async fn sqlite_journal_pressure_matrix() {
    let cfg = node_cfg("sqlite_pressure", 8);
    let mut app = app("counter");
    app.limits.cpu_ms = 5000;
    // 1,000 synced statements wait for a busy disk for up to 16 s while
    // using under 1 s of CPU.
    app.limits.request_timeout_ms = 60_000;
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/p3.wasm")).unwrap();
    let node = Node::new(cfg, &app, &wasm, None).unwrap();
    let expected = |changed: bool| {
        let rows: Vec<_> = (1..=16).map(|id| serde_json::json!([
            id, id*3 + if changed { 7 } else { 0 }, if changed { 2048 } else { 1024 }
        ])).collect();
        let marker = if changed { "after" } else { "before" };
        serde_json::json!({"rows": rows, "tally": if changed { 40 } else { 29 },
            "file": marker, "key": marker, "integrity": "ok", "aux_integrity": "ok"})
    };
    for mode in ["DELETE", "TRUNCATE", "PERSIST", "MEMORY", "WAL"] {
        let exclusive = if mode == "WAL" { "&exclusive=1" } else { "" };
        let url = |op: &str, cut: u64| format!("/pressure?mode={mode}&cache=1&op={op}&cut={cut}{exclusive}");
        let control = format!("{mode}-control");
        let init = get(&node, &control, &url("init", 0)).await;
        assert_eq!(init.status, 200, "{mode}: {}", init.body);
        assert_eq!(init.json()["state"], expected(false));
        let r = get(&node, &control, &url("mutate", 0)).await;
        assert_eq!(r.status, 200, "{mode}: {}", r.body);
        let result = r.json();
        assert_eq!(result["mode"], mode.to_lowercase());
        assert_eq!(result["aux_mode"], mode.to_lowercase());
        assert_eq!(result["state"], expected(true));
        assert_eq!(result["reused"], true, "must exercise a cached connection");
        let steps = result["steps"].as_u64().unwrap();
        assert!(steps > 0 && steps < 512, "unexpected workload size: {result}");
        assert_eq!(get(&node, &control, &url("read", 0)).await.json()["state"], expected(true));
        for cut in 1..=steps {
            let object = format!("{mode}-{cut}");
            let init = get(&node, &object, &url("init", 0)).await;
            assert_eq!(init.status, 200, "{mode} cut {cut}: {}", init.body);
            let failed = get(&node, &object, &url("mutate", cut)).await;
            assert_eq!(failed.error.as_deref(), Some("trap"), "{mode} cut {cut}: {}", failed.body);
            assert!(failed.body.contains(&format!("SQLite VFS cut {cut} after")), "{}", failed.body);
            let recovered = get(&node, &object, &url("read", 0)).await;
            assert_eq!(recovered.status, 200, "{mode} cut {cut}: {}", recovered.body);
            assert_eq!(recovered.json()["state"], expected(false), "{mode} cut {cut}");
            assert_eq!(recovered.json()["reused"], false, "failed instance must lose its SQLite cache");
        }
        println!("{mode}: {steps} VFS cuts recovered exact rows in both databases + file/KV; {} SQLite write bytes", result["write_bytes"]);
    }
    assert_eq!(get(&node, "wal", "/pressure?op=init").await.status, 200);
    let wal = get(&node, "wal", "/pressure?mode=WAL&op=mutate").await;
    assert_eq!(wal.error.as_deref(), Some("guest-error"), "{}", wal.body);
    assert!(wal.body.contains("disk I/O error"), "{}", wal.body);
    assert_eq!(get(&node, "wal", "/pressure").await.json()["state"], expected(false));
    println!("normal-locking WAL: disk I/O error with this VFS; previous state recovered");

    let init = get(&node, "churn", "/pressure?op=init&cache=1").await;
    assert_eq!(init.status, 200, "{}", init.body);
    let denials = node.metrics.fs_denied.load(Relaxed);
    let failed = get(&node, "churn", "/pressure?op=churn&n=1000&cache=1&full=insert").await;
    assert_eq!(failed.error.as_deref(), Some("guest-error"), "{}", failed.body);
    assert!(node.metrics.fs_denied.load(Relaxed) > denials);
    let recovered = get(&node, "churn", "/pressure?cache=1").await;
    assert_eq!(recovered.status, 200, "{}", recovered.body);
    assert_eq!(recovered.json()["state"], expected(false));
    println!("an oversized INSERT after repeated updates fails the volume quota; full recovery verified");
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the separately built SQLite experiment (WASI SDK)"]
async fn sqlite_handled_storage_exhaustion() {
    let cfg = node_cfg("sqlite_handled_exhaustion", 8);
    let mut app = app("counter");
    app.limits.cpu_ms = 5000;
    // As in the pressure matrix: the disk, not the CPU, sets the time.
    app.limits.request_timeout_ms = 60_000;
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/p3.wasm")).unwrap();
    let node = Node::new(cfg, &app, &wasm, None).unwrap();
    for object in ["insert", "bulk"] {
        assert_eq!(get(&node, object, "/pressure?op=init&cache=1").await.status, 200);
        let denials = node.metrics.fs_denied.load(Relaxed);
        let handled = get(&node, object, &format!("/pressure?op=churn&n=1000&full={object}&cache=1&handle=1")).await;
        assert_eq!(handled.status, 200, "handled exhaustion: {}", handled.body);
        assert!(node.metrics.fs_denied.load(Relaxed) > denials);
        let handled = handled.json();
        assert!(handled["handled"].is_string(), "{handled}");
        let completed = handled["completed"].as_i64().unwrap();
        assert_eq!(completed, 1000);
        let after = get(&node, object, "/pressure").await;
        println!("handled: {handled}; next read {}: {}", after.status, after.body);
        assert_eq!(after.status, 200, "{}", after.body);
        let after = after.json();
        let rows: Vec<_> = (1..=16).map(|id| serde_json::json!([
            id, id*3 + if id == 1 { completed } else { 0 }, 1024
        ])).collect();
        assert_eq!(after["state"], serde_json::json!({"rows": rows, "tally": 29,
            "file": "after", "key": "after", "integrity": "ok", "aux_integrity": "ok"}),
            "a failed statement must not commit, including partial multi-page updates");
        println!("handled exhaustion: {completed} successful statements committed without corruption");
    }
    node.shutdown().await;
}

/// Regression (N-640, N-600): a guest's waits for its own syncs aren't CPU.
/// 100 one-row transactions with `synchronous=FULL` each wait for several
/// fsyncs; at the default 200 ms budget they were killed for CPU while using
/// a fraction of it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the separately built SQLite experiment (WASI SDK)"]
async fn sqlite_sync_waits_are_not_charged_as_cpu() {
    let mut app = app("counter");
    app.limits.memory_mb = 64;
    app.limits.disk_mb = 8;
    app.limits.cpu_ms = 200;
    app.limits.request_timeout_ms = 30_000;
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/p3.wasm")).unwrap();
    let node = Node::new(node_cfg("sqlite_sync_waits", 8), &app, &wasm, None).unwrap();
    assert_eq!(get(&node, "o", "/bench?op=init").await.status, 200);
    for seq in 0..3 {
        let cpu = node.metrics.guest_cpu_ns.load(Relaxed);
        let r = get(&node, "o", &format!("/bench?op=autocommit&seq={seq}")).await;
        let cpu = Duration::from_nanos(node.metrics.guest_cpu_ns.load(Relaxed) - cpu);
        println!("100 synced transactions: {} after {cpu:?} of CPU in {:?}", r.status, r.elapsed);
        assert_eq!(r.status, 200, "{:?} {}", r.error, r.body);
    }
    node.shutdown().await;
}

/// Repeated overwrites and short-lived journals must fit without raising quotas.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the separately built SQLite experiment (WASI SDK)"]
async fn sqlite_repeated_updates_remain_bounded() {
    let cfg = node_cfg("sqlite_bounded_churn", 8);
    let mut app = app("counter");
    app.limits.cpu_ms = 10_000;
    app.limits.request_timeout_ms = 60_000;
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/p3.wasm")).unwrap();
    let node = Node::new(cfg, &app, &wasm, None).unwrap();
    for mode in ["DELETE", "TRUNCATE", "PERSIST", "MEMORY", "WAL"] {
        for rollback in [false, true] {
            let object = format!("{mode}-{rollback}");
            let policy = if mode == "WAL" { "&exclusive=1&autocheckpoint=16" } else { "" };
            let prefix = format!("/pressure?mode={mode}{policy}");
            let init = get(&node, &object, &format!("{prefix}&op=init&cache=1")).await;
            assert_eq!(init.status, 200, "{}", init.body);
            let before = init.json()["state"].clone();
            let then = if rollback { "&then=error" } else { "" };
            let result = get(&node, &object, &format!("{prefix}&op=churn&n=1000{then}")).await;
            if rollback {
                assert_eq!(result.error.as_deref(), Some("guest-error"), "{}", result.body);
                assert!(result.body.contains("after 1000 completed statements"), "{}", result.body);
                let dir = object_dir(&node.cfg.app_dirs(&app.name)[0], &object).join("journal");
                let log = std::fs::metadata(dir.join("log")).unwrap().len();
                let retained: u64 = std::fs::read_dir(&dir).unwrap().map(|entry| {
                    let entry = entry.unwrap();
                    if entry.file_name() == "log" { 0 } else { entry.metadata().unwrap().len() }
                }).sum();
                println!("{mode}: 1000 completed updates; undo log {log} bytes, retained files {retained} bytes");
                assert!(log < 256 * 1024 && retained < 256 * 1024,
                    "repeated changes must not approach the 1 MiB budget: log={log}, retained={retained}");
            } else {
                assert_eq!(result.status, 200, "{mode}: {}", result.body);
                assert_eq!(result.json()["completed"], 1000);
                assert!(result.json()["handled"].is_null());
            }
            let read = get(&node, &object, &prefix).await;
            assert_eq!(read.status, 200, "{}", read.body);
            let mut expected = before;
            if !rollback {
                expected["rows"][0][1] = 1003.into();
                expected["file"] = "after".into();
                expected["key"] = "after".into();
            }
            assert_eq!(read.json()["state"], expected, "{mode}, rollback={rollback}");
        }
    }
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the separately built SQLite experiment (WASI SDK)"]
async fn sqlite_exclusive_wal_checkpoints() {
    let cfg = node_cfg("sqlite_wal_checkpoints", 8);
    let mut app = app("counter");
    app.limits.cpu_ms = 10_000;
    app.limits.request_timeout_ms = 30_000;
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/p3.wasm")).unwrap();
    let node = Node::new(cfg, &app, &wasm, None).unwrap();
    let prefix = "/pressure?mode=WAL&exclusive=1&cache=1";
    for checkpoint in ["PASSIVE", "FULL", "RESTART", "TRUNCATE"] {
        let control = format!("{checkpoint}-control");
        assert_eq!(get(&node, &control, &format!("{prefix}&op=init")).await.status, 200);
        let changed = get(&node, &control, &format!("{prefix}&op=mutate")).await;
        assert_eq!(changed.status, 200, "{}", changed.body);
        let expected = changed.json()["state"].clone();
        let volume = object_dir(&node.cfg.app_dirs(&app.name)[0], &control).join("data");
        for db in ["pressure", "aux"] {
            assert!(std::fs::metadata(volume.join(format!("{db}.db-wal"))).unwrap().len() > 32);
            assert!(!volume.join(format!("{db}.db-shm")).exists());
        }
        let url = format!("{prefix}&op=checkpoint&checkpoint={checkpoint}");
        let done = get(&node, &control, &url).await;
        assert_eq!(done.status, 200, "{}", done.body);
        let done = done.json();
        assert_eq!(done["state"], expected);
        for result in done["checkpoints"].as_array().unwrap() {
            assert_eq!(result[0], 0, "checkpoint must not be busy");
            assert_eq!(result[1], result[2], "all WAL frames must be checkpointed");
            if checkpoint == "TRUNCATE" { assert_eq!(result[1], 0); }
            else { assert!(result[1].as_i64().unwrap() > 0); }
        }
        if checkpoint == "TRUNCATE" {
            for db in ["pressure", "aux"] {
                assert_eq!(std::fs::metadata(volume.join(format!("{db}.db-wal"))).unwrap().len(), 0);
            }
        }
        let steps = done["steps"].as_u64().unwrap();
        assert!(steps > 0 && steps < 128);
        for cut in 1..=steps {
            let object = format!("{checkpoint}-{cut}");
            assert_eq!(get(&node, &object, &format!("{prefix}&op=init")).await.status, 200);
            assert_eq!(get(&node, &object, &format!("{prefix}&op=mutate")).await.json()["state"], expected);
            let failed = get(&node, &object, &format!("{url}&cut={cut}")).await;
            assert_eq!(failed.error.as_deref(), Some("trap"), "{}", failed.body);
            assert!(failed.body.contains(&format!("SQLite VFS cut {cut} after")));
            let read = get(&node, &object, prefix).await;
            assert_eq!(read.status, 200, "{}", read.body);
            assert_eq!(read.json()["state"], expected, "{checkpoint} cut {cut}");
            assert_eq!(read.json()["reused"], false);
        }
        println!("exclusive WAL {checkpoint}: {steps} checkpoint cuts; exact SQL/file/KV state recovered");
    }
    let before = get(&node, "default-checkpoint", &format!("{prefix}&op=init")).await;
    assert_eq!(before.status, 200, "{}", before.body);
    let denials = node.metrics.fs_denied.load(Relaxed);
    let failed = get(&node, "default-checkpoint", &format!("{prefix}&op=churn&n=1000")).await;
    assert_eq!(failed.error.as_deref(), Some("guest-error"), "{}", failed.body);
    assert!(node.metrics.fs_denied.load(Relaxed) > denials);
    let wal = object_dir(&node.cfg.app_dirs(&app.name)[0], "default-checkpoint").join("data/pressure.db-wal");
    println!("default WAL autocheckpoint exhausts the 1 MiB volume: WAL {} bytes", std::fs::metadata(wal).unwrap().len());
    let recovered = get(&node, "default-checkpoint", prefix).await;
    assert_eq!(recovered.status, 200, "{}", recovered.body);
    assert_eq!(recovered.json()["state"], before.json()["state"]);
    node.shutdown().await;
}

/// Manual benchmark, deliberately outside the `sqlite_` correctness-test filter.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual SQLite benchmark; requires the separately built experiment"]
async fn benchmark_database_modes() {
    let cfg = node_cfg("sqlite_benchmark", 8);
    let mut app = app("counter");
    app.limits.memory_mb = 64;
    app.limits.disk_mb = 16;
    app.limits.cpu_ms = 10_000;
    app.limits.request_timeout_ms = 30_000;
    let wasm = std::fs::read(repo().join("target/sqlite-filesystem/p3.wasm")).unwrap();
    let node = Node::new(cfg, &app, &wasm, None).unwrap();
    let variants = [("DELETE", 1000), ("WAL", 16), ("WAL", 1000)];
    let mut results = vec![];
    for round in 0..3 {
        for workload in ["point", "scan", "write", "batch", "autocommit", "blob"] {
            for cache in [true, false] {
                for j in 0..3 {
                    let (mode, pages) = variants[(j + round) % 3];
                    let object = format!("{round}-{workload}-{cache}-{mode}-{pages}");
                    let prefix = format!("/bench?mode={mode}&pages={pages}&cache={}", u8::from(cache));
                    let init = get(&node, &object, &format!("{prefix}&op=init")).await;
                    assert_eq!(init.status, 200, "{}", init.body);
                    let mut samples = vec![];
                    let (mut bytes, mut syncs, mut peak) = (0u64, 0u64, 0u64);
                    let writes = match workload { "batch" | "autocommit" => 100, "write" | "blob" => 1, _ => 0 };
                    for seq in 0..28 {
                        let start = Instant::now();
                        let response = get(&node, &object, &format!("{prefix}&op={workload}&seq={seq}")).await;
                        let us = start.elapsed().as_secs_f64() * 1_000_000.0;
                        assert_eq!(response.status, 200, "{object}: {}", response.body);
                        let body = response.json();
                        let expected = match workload { "point" => 512, "scan" => 1024*512, _ => writes };
                        assert_eq!(body["value"], expected);
                        if seq >= 3 {
                            samples.push(us);
                            bytes += body["sqlite_write_bytes"].as_u64().unwrap();
                            syncs += body["sqlite_syncs"].as_u64().unwrap();
                        }
                        let volume = object_dir(&node.cfg.app_dirs(&app.name)[0], &object).join("data");
                        let live: u64 = std::fs::read_dir(volume).unwrap()
                            .map(|e| e.unwrap().metadata().unwrap().len()).sum();
                        peak = peak.max(live);
                    }
                    // Include deferred WAL cleanup in the lifecycle cost, not in p50/p95.
                    let start = Instant::now();
                    let close = get(&node, &object, &format!("{prefix}&op=close")).await;
                    let close_us = start.elapsed().as_secs_f64()*1_000_000.0;
                    assert_eq!(close.status, 200, "{}", close.body);
                    let closed = close.json();
                    let verify = get(&node, &object, &format!("{prefix}&op=verify")).await;
                    assert_eq!(verify.status, 200, "{}", verify.body);
                    assert_eq!(verify.json()["value"], 28*writes);
                    assert_eq!(verify.json()["integrity"], "ok");
                    let result = serde_json::json!({"round": round, "workload": workload, "cached": cache,
                        "mode": mode, "checkpoint_pages": pages, "samples_us": samples,
                        "close_us": close_us, "sqlite_write_bytes": bytes, "sqlite_syncs": syncs,
                        "close_write_bytes": closed["sqlite_write_bytes"], "close_syncs": closed["sqlite_syncs"],
                        "peak_live_file_bytes": peak});
                    println!("benchmark {object}: {} us mean including close", (samples.iter().sum::<f64>()+close_us)/25.0);
                    results.push(result);
                }
            }
        }
    }
    std::fs::write(repo().join("target/sqlite-filesystem/benchmark-results.json"),
        serde_json::to_vec_pretty(&results).unwrap()).unwrap();
    node.shutdown().await;
}

/// A request's writes, to files and to key-value alike, take effect only if
/// it answers, whatever the status; one that fails, traps or runs out of CPU
/// leaves nothing behind.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_requests_writes_are_rolled_back() {
    let node = node("a_failed_requests_writes_are_rolled_back", "hostile", 8);
    let read = async || get(&node, "c", "/commit").await.json();
    let r = get(&node, "c", "/commit?v=one").await;
    assert_eq!(r.status, 200, "{}", r.body);
    for then in ["error", "panic", "spin"] {
        let r = get(&node, "c", &format!("/commit?v=two-{then}&then={then}")).await;
        assert_eq!(r.status, 500, "{}", r.body);
        assert_eq!(read().await, serde_json::json!({"file": "one", "key": "one"}), "after {then}");
    }
    assert_eq!(get(&node, "c", "/commit?v=three&then=fail").await.status, 507);
    assert_eq!(read().await, serde_json::json!({"file": "three", "key": "three"}));
}

/// A commit outlives a request killed at its deadline (here it waits for
/// the one blocking thread past the post-return deadline), and it still owns
/// the volume: the object's next instance recovers only once it has
/// finished. Before, that instance recovered at once, and the key the late
/// commit then wrote voided the records of the next request, which failed
/// and kept its writes (S1). The killed request's own outcome is the
/// commit's, as for any commit whose answer is lost: here it committed.
#[test]
fn review_cancelled_commit_cannot_outlive_recovery() {
    let rt = tokio::runtime::Builder::new_multi_thread().max_blocking_threads(1).enable_all().build().unwrap();
    rt.block_on(async {
        let node = node("review_cancelled_commit_cannot_outlive_recovery", "hostile", 8);
        let (release, held) = std::sync::mpsc::channel::<()>();
        let busy = tokio::task::spawn_blocking(move || held.recv());
        let r = get(&node, "c", "/commit?v=one").await;
        assert_eq!(r.status, 504, "{}", r.body);
        let mut failing = tokio::spawn({
            let node = node.clone();
            async move { get(&node, "c", "/commit?v=two&then=error").await }
        });
        let early = tokio::time::timeout(Duration::from_millis(500), &mut failing).await.ok();
        let waited = early.is_none();
        release.send(()).unwrap();
        busy.await.unwrap().unwrap();
        // Time for a late commit to end, were it still running.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let failed = match early {
            Some(r) => r.unwrap(),
            None => failing.await.unwrap(),
        };
        assert_eq!(failed.status, 500, "{}", failed.body);
        let r = get(&node, "c", "/commit").await;
        assert_eq!(r.json(), serde_json::json!({"file": "one", "key": "one"}), "a failed request's writes stayed");
        assert!(waited, "the next instance started while the commit was still running");
        node.shutdown().await;
    });
}

/// The journal is out of a guest's reach, and commits work with a relative
/// `data_dir` (the demo's).
#[tokio::test(flavor = "multi_thread")]
async fn the_journal_is_out_of_reach_and_works_from_a_relative_data_dir() {
    let mut cfg = node_cfg("journal_out_of_reach", 8);
    let rel = Path::new("../../target/journal-rel");
    let _ = std::fs::remove_dir_all(rel);
    std::fs::create_dir_all(rel).unwrap();
    std::os::unix::fs::symlink(cfg.data_dir.join("cache"), rel.join("cache")).unwrap();
    cfg.data_dir = rel.to_path_buf();
    let node = node_with(cfg, app("hostile"));
    let r = get(&node, "c", "/commit?v=one").await;
    assert_eq!(r.status, 200, "{}", r.body);
    let r = get(&node, "c", "/rename?mount=data&from=../journal&to=j").await;
    assert_eq!(r.status, 507, "{}", r.body);
    assert_eq!(get(&node, "c", "/commit?v=two&then=panic").await.status, 500);
    let r = get(&node, "c", "/commit").await;
    assert_eq!(r.json(), serde_json::json!({"file": "one", "key": "one"}), "{}", r.body);
}

/// A request in flight when its worker dies is rolled back when the object
/// next starts.
#[tokio::test(flavor = "multi_thread")]
async fn a_crash_mid_request_is_rolled_back() {
    let (_sup, addr) = supervised("a_crash_mid_request_is_rolled_back").await;
    let hostile = publish("hostile", &app("hostile")).await;
    assert_eq!(app_get(addr, &hostile, "/c/commit?v=one").await.0, 200);
    let pid = stats_json(addr).await["workers"][worker(&hostile).as_str()]["pid"].as_u64().unwrap() as i32;
    let crashing = tokio::spawn({
        let hostile = hostile.clone();
        async move { app_get(addr, &hostile, "/c/commit?v=two&then=sleep").await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let _ = crashing.await;
    let t0 = Instant::now();
    while stats_json(addr).await["workers"][worker(&hostile).as_str()]["up"] == true {
        assert!(t0.elapsed() < Duration::from_secs(2), "worker still up");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (status, body) = app_get(addr, &hostile, "/c/commit").await;
    assert_eq!((status, body.as_str()), (200, r#"{"file":"one","key":"one"}"#));
}

// ---- calls between objects ---------------------------------------------

/// An object calls another of its app at `http://nanovm/objects/{id}/…`. The
/// callee serves the call from its own queue and volume, and commits before
/// the caller hears back. A call that would wait on its own caller, directly
/// or through another call, is refused at once with 508 instead of
/// deadlocking until a deadline kills both.
#[tokio::test(flavor = "multi_thread")]
async fn objects_call_objects() {
    let node = node("objects_call_objects", "hostile", 8);
    let enc = |s: &str| s.replace('%', "%25").replace('&', "%26").replace('?', "%3F").replace('=', "%3D");
    let call = async |from: &str, url: &str| {
        let r = get(&node, from, &format!("/fetch?url={}", enc(url))).await;
        assert_eq!(r.status, 200, "{}", r.body);
        r.json()
    };

    let r = call("a", "http://nanovm/objects/b/commit?v=via-a").await;
    assert_eq!((r["status"].as_u64(), r["body"].as_str()), (Some(200), Some(r#"{"file":"via-a","key":"via-a"}"#)));
    let r = get(&node, "b", "/commit").await;
    assert_eq!(r.body, r#"{"file":"via-a","key":"via-a"}"#, "the callee's volume has it");
    assert_eq!(get(&node, "a", "/commit").await.body, r#"{"file":"","key":""}"#, "the caller's doesn't");

    let t0 = Instant::now();
    let r = call("a", "http://nanovm/objects/a/").await;
    assert_eq!(r["status"], 508, "{r}");
    let inner = format!("http://nanovm/objects/a/fetch?url={}", enc("http://nanovm/objects/b/"));
    let r = call("b", &inner).await;
    assert!(r["body"].as_str().unwrap().contains(r#""status":508"#), "{r}");
    assert!(t0.elapsed() < Duration::from_secs(1), "refused at once: {:?}", t0.elapsed());

    // Answered calls wait on nothing: b may now call a.
    assert_eq!(call("b", "http://nanovm/objects/a/").await["status"], 200);
}

/// What an object posts to `http://nanovm/` is held for its sockets only at
/// `/publish`, and only if well formed; anything else is refused at once, as
/// a supervisor would refuse it. An in-process node holds publishes too: it
/// no longer sends them to its egress policy, which denied them.
#[tokio::test(flavor = "multi_thread")]
async fn only_well_formed_publishes_are_held() {
    let node = node("only_well_formed_publishes_are_held", "hostile", 8);
    for (url, status) in [("http://nanovm/elsewhere", 404), ("http://nanovm/publish/", 400)] {
        let r = get(&node, "p", &format!("/fetch?method=POST&url={url}")).await;
        assert_eq!(r.json()["status"], status, "{url}: {}", r.body);
    }
}

// ---- Secrets ---------------------------------------------------------------

/// A secrets directory for `test` that only this user can use, outside the
/// node's data directory, as an operator keeps one.
fn secrets_dir(test: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("secrets").join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// Gives the app named `name` on the node `secrets`, as an operator should:
/// a new file renamed into place.
fn give(dir: &Path, name: &str, secrets: &str) {
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, secrets).unwrap();
    std::fs::rename(tmp, dir.join(format!("{name}.json"))).unwrap();
}

/// The counter guest's manifest, needing the secrets `needs`.
fn needing(needs: &[&str]) -> serde_json::Value {
    serde_json::json!({ "limits": app("counter").limits, "secrets": needs })
}

/// Polls `GET path` on `app` until `done` holds of its answer, for up to 10 s.
async fn until(addr: std::net::SocketAddr, app: &str, path: &str, done: impl Fn(u16, &str) -> bool) -> (u16, String) {
    let t0 = Instant::now();
    loop {
        let (status, body) = app_get(addr, app, path).await;
        if done(status, &body) {
            return (status, body);
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "still {status} {body}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// An app's secrets reach its own jailed worker, which reads them with its
/// config (`wasi:config`), and no other app: not through its config, not
/// through the file (the directory is root's alone, and a worker runs as a
/// uid of its own), and not through the worker's arguments or environment,
/// which other processes may read.
#[tokio::test(flavor = "multi_thread")]
async fn secrets_reach_only_their_apps_worker() {
    let test = "secrets_reach_only_their_apps_worker";
    let (mut cfg, dir) = (node_cfg(test, 8), secrets_dir(test));
    cfg.secrets = Some(dir.clone());
    let (_sup, addr) = supervised_with(cfg).await;
    let a = publish_spec("secret-a", needing(&["api_key"]), guest("counter"), "1.0.0").await;
    let b = publish_spec("secret-b", needing(&[]), guest("counter"), "1.0.0").await;
    let key = "sk-a-0123456789abcdef";
    give(&dir, &worker(&a), &format!(r#"{{"api_key": "{key}", "greeting": "from the operator"}}"#));
    assert_eq!(app_get(addr, &a, "/x/config/api_key").await, (200, key.into()));
    assert_eq!(app_get(addr, &b, "/x/config/api_key").await.0, 404, "another app's secret");
    // b's own, once it has one: b's worker restarts with it, and a's keeps its.
    give(&dir, &worker(&b), r#"{"api_key": "b's own"}"#);
    until(addr, &b, "/x/config/api_key", |s, body| (s, body) == (200, "b's own")).await;
    assert_eq!(app_get(addr, &a, "/x/config/api_key").await, (200, key.into()));

    let pid = stats_json(addr).await["workers"][worker(&a).as_str()]["pid"].as_u64().unwrap();
    for file in ["cmdline", "environ"] {
        let seen = std::fs::read(format!("/proc/{pid}/{file}")).unwrap();
        assert!(!seen.windows(key.len()).any(|w| w == key.as_bytes()), "the worker's {file} holds the secret");
    }
    if unsafe { libc::geteuid() } == 0 {
        use std::os::unix::{fs::MetadataExt, process::CommandExt};
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let uid: u32 = status.lines().find_map(|l| l.strip_prefix("Uid:")).unwrap().split_whitespace().next().unwrap().parse().unwrap();
        assert!(uid >= 200_000, "the worker runs as uid {uid}");
        // Its uid can't read the file (nor can its jail reach the directory).
        let read = std::process::Command::new("cat").arg(dir.join(format!("{}.json", worker(&a)))).uid(uid).gid(uid).output().unwrap();
        assert!(!read.status.success() && read.stdout.is_empty(), "uid {uid} read the secrets file");
        // It is not dumpable: no core dump holds its secrets, and no
        // process of its uid may trace it or read its memory.
        for file in ["environ", "mem"] {
            assert_eq!(std::fs::metadata(format!("/proc/{pid}/{file}")).unwrap().uid(), 0, "{file}: the worker is dumpable");
        }
    }
}

/// A change to an app's secrets restarts its worker with them. A file the
/// node can't use stops the app (fail closed): its requests get 502 naming
/// the file and what is wrong, never what the file holds. So does a
/// missing secret the app needs. A secrets directory others can use stops
/// the node from starting.
#[tokio::test(flavor = "multi_thread")]
async fn changed_or_bad_secrets_restart_or_stop_the_app() {
    use std::os::unix::fs::PermissionsExt;
    let test = "changed_or_bad_secrets_restart_or_stop_the_app";
    let (mut cfg, dir) = (node_cfg(test, 8), secrets_dir(test));
    cfg.secrets = Some(dir.clone());
    let (_sup, addr) = supervised_with(cfg.clone()).await;
    let a = publish_spec("secret-change", needing(&["api_key"]), guest("counter"), "1.0.0").await;
    let name = worker(&a);
    let (status, body) = app_get(addr, &a, "/x/config/api_key").await;
    assert_eq!(status, 502, "{body}");
    assert!(body.contains(&format!("the app's secrets, from the node's secrets directory: the app needs secrets api_key; {name}.json doesn't give them")), "{body}");

    give(&dir, &name, r#"{"api_key": "one"}"#);
    until(addr, &a, "/x/config/api_key", |s, body| (s, body) == (200, "one")).await;
    let pid = |v: serde_json::Value| v["workers"][name.as_str()]["pid"].as_u64().unwrap();
    let first = pid(stats_json(addr).await);
    give(&dir, &name, r#"{"api_key": "two"}"#);
    until(addr, &a, "/x/config/api_key", |s, body| (s, body) == (200, "two")).await;
    assert_ne!(pid(stats_json(addr).await), first, "a new worker");

    // serde_json alone would say `invalid type: integer `4242…``.
    give(&dir, &name, r#"{"api_key": 4242424242424242}"#);
    let (_, body) = until(addr, &a, "/x/config/api_key", |s, _| s == 502).await;
    assert!(body.contains(&format!("{name}.json is not a JSON object of strings (line 1, column")) && !body.contains("4242"), "{body}");
    std::fs::remove_file(dir.join(format!("{name}.json"))).unwrap();
    std::fs::create_dir(dir.join(format!("{name}.json"))).unwrap();
    until(addr, &a, "/x/config/api_key", |s, body| s == 502 && body.contains(&format!("{name}.json is not a file"))).await;
    std::fs::remove_dir(dir.join(format!("{name}.json"))).unwrap();
    give(&dir, &name, r#"{"api_key": "three"}"#);
    until(addr, &a, "/x/config/api_key", |s, body| (s, body) == (200, "three")).await;

    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_nanovm"));
    let e = nanovm::supervisor::Supervisor::new(cfg, exe, 1, None).err().expect("a directory others can read");
    assert!(e.to_string().contains("only its owner, the user nanovm runs as, may use it"), "{e:#}");
}

/// A guest that writes its secret to stdout and stderr, puts it in the path
/// of a request out, and then fails with it in its error or its panic. The
/// host reports all of that (the failure, with the guest's last output, to
/// the client; the output, the request out and its refusal to telemetry),
/// but masked: nothing the collector receives, and no answer, holds the
/// secret.
#[tokio::test(flavor = "multi_thread")]
async fn secrets_stay_out_of_telemetry_and_error_text() {
    use http_body_util::BodyExt;
    let test = "secrets_stay_out_of_telemetry_and_error_text";
    let (mut cfg, dir) = (node_cfg(test, 8), secrets_dir(test));
    cfg.secrets = Some(dir.clone());
    // Every batch the collector is sent, as sent.
    let received = Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
    let collector = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    cfg.telemetry_endpoint = format!("http://{}", collector.local_addr().unwrap());
    let r = received.clone();
    tokio::spawn(nanovm::server::serve_http1(collector, move |req: http::Request<hyper::body::Incoming>| {
        let r = r.clone();
        async move {
            let body = req.into_body().collect().await.unwrap().to_bytes();
            r.lock().extend(body);
            http::Response::new(http_body_util::Full::new(bytes::Bytes::new()).map_err(|e| match e {}).boxed_unsync())
        }
    }));
    let (sup, addr) = supervised_with(cfg).await;
    let leaky = serde_json::json!({ "limits": app("hostile").limits, "secrets": ["api_key"] });
    let leaky = publish_spec("secret-leak", leaky, probe(), "1.0.0").await;
    let key = "sk-leak-0123456789abcdef";
    give(&dir, &worker(&leaky), &format!(r#"{{"api_key": "{key}"}}"#));
    let masked = "*".repeat(key.len());

    let (status, body) = app_get(addr, &leaky, "/x/leak?key=api_key").await;
    assert_eq!(status, 500, "{body}");
    assert!(body.contains(r#""error":"trap""#) && body.contains(&format!("panic {masked}")) && body.contains(&format!("stderr {masked}")), "{body}");
    assert!(!body.contains(key), "{body}");
    let (status, body) = app_get(addr, &leaky, "/y/leak?key=api_key&then=error").await;
    assert_eq!(status, 500, "{body}");
    assert!(body.contains(r#""error":"guest-error""#) && body.contains(&format!("error {masked}")) && !body.contains(key), "{body}");

    let t0 = Instant::now();
    while let Err(e) = sup.flush_telemetry().await {
        assert!(t0.elapsed() < Duration::from_secs(2), "telemetry flush failed: {e:#}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let received = received.lock().clone();
    let holds = |s: &str| received.windows(s.len()).any(|w| w == s.as_bytes());
    for reported in [format!("/leak/{masked}"), format!("stdout {masked}"), format!("stderr {masked}"), format!("panic {masked}")] {
        assert!(holds(&reported), "telemetry should report {reported:?}");
    }
    assert!(!holds(key), "telemetry holds the secret");
}

// ---- WebSockets ----------------------------------------------------------

mod sockets {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{Error, Message, protocol::frame::coding::CloseCode};

    type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
    type Records = Arc<parking_lot::Mutex<std::collections::HashMap<String, Vec<String>>>>;

    async fn front(label: &str, mut cfg: nanovm::config::ProxyConfig, apps: &[(&str, AppConfig)]) -> (Arc<nanovm::supervisor::Supervisor>, std::net::SocketAddr, std::net::SocketAddr, Records) {
        let control = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut node_cfg = node_cfg(label, 8);
        node_cfg.proxy = format!("http://{}", control.local_addr().unwrap());
        let (sup, node) = supervised_with(node_cfg).await;
        let records = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        for (host, app) in apps {
            let published = publish(&format!("{label}-{host}"), app).await;
            records.lock().insert(format!("_nanovm.{host}"), vec![format!("app={}:latest; trust=dev dev", published.split_once('@').unwrap().0)]);
        }
        cfg.listen = "127.0.0.1:0".into();
        cfg.nodes = vec![node.to_string()];
        cfg.hosts.extend(apps.iter().map(|(host, _)| host.to_string()));
        cfg.signers = vec!["dev dev".into()];
        cfg.resolver = doh(records.clone()).await;
        cfg.tag_ttl_s = 0;
        cfg.dev = true;
        let proxy = nanovm::proxy::Proxy::new(cfg).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(nanovm::proxy::serve(proxy.clone(), listener));
        tokio::spawn(nanovm::proxy::serve_publish(proxy, control));
        (sup, node, addr, records)
    }

    async fn app_get(addr: std::net::SocketAddr, host: &str, path: &str) -> (u16, String) {
        raw(addr, &format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"), b"").await
    }

    async fn ended(addr: std::net::SocketAddr, host: &str, object: &str, n: usize) {
        let t0 = Instant::now();
        while app_get(addr, host, &format!("/{object}/kv/ws-ends")).await != (200, n.to_string()) {
            assert!(t0.elapsed() < Duration::from_secs(10), "{host}/{object} never ended {n} sockets");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A handshake for `path` on `app`.
    fn handshake(addr: std::net::SocketAddr, app: &str, path: &str) -> http::Request<()> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut req = format!("ws://{addr}{path}").into_client_request().unwrap();
        req.headers_mut().insert("host", app.parse().unwrap());
        req
    }

    /// The socket, or the refusal's status and body.
    async fn open(addr: std::net::SocketAddr, app: &str, path: &str) -> Result<Ws, (u16, String)> {
        match tokio_tungstenite::connect_async(handshake(addr, app, path)).await {
            Ok((ws, _)) => Ok(ws),
            Err(Error::Http(res)) => {
                Err((res.status().as_u16(), String::from_utf8_lossy(res.body().as_deref().unwrap_or(&[])).into()))
            }
            Err(e) => panic!("{path}: {e}"),
        }
    }

    fn refused(r: Result<Ws, (u16, String)>) -> (u16, String) {
        r.err().expect("refused")
    }

    /// A socket whose client takes little before the kernel stops sending.
    async fn open_slow(addr: std::net::SocketAddr, app: &str, path: &str) -> Ws {
        let sock = tokio::net::TcpSocket::new_v4().unwrap();
        sock.set_recv_buffer_size(4096).unwrap();
        let tcp = tokio_tungstenite::MaybeTlsStream::Plain(sock.connect(addr).await.unwrap());
        tokio_tungstenite::client_async(handshake(addr, app, path), tcp).await.unwrap().0
    }

    /// The next data message or close frame.
    async fn next(ws: &mut Ws) -> Message {
        loop {
            let m = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.expect("a message in time");
            match m.expect("the socket is open").expect("a frame") {
                Message::Ping(_) | Message::Pong(_) => continue,
                m => return m,
            }
        }
    }

    async fn say(ws: &mut Ws, text: &str) {
        ws.send(Message::text(text)).await.unwrap();
    }

    async fn post(addr: std::net::SocketAddr, app: &str, path: &str) -> u16 {
        let head = format!("POST {path} HTTP/1.1\r\nHost: {app}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        raw(addr, &head, b"").await.0
    }

    /// An existing socket uses the new digest, retains its subscriptions,
    /// and cannot switch the node back to its OPEN version.
    #[tokio::test(flavor = "multi_thread")]
    async fn sockets_follow_current_versions() {
        let (_sup, node, addr, _) = front("socket_versions", Default::default(), &[("counter", app("counter"))]).await;
        let mut ws = open(addr, "counter", "/room/ws").await.unwrap();
        let mut changed = app("counter");
        changed.config.insert("greeting".into(), "new version".into());
        let next_version = publish("socket_versions-counter", &changed).await;
        say(&mut ws, "pub after deploy").await;
        assert_eq!(next(&mut ws).await, Message::text("after deploy"));
        let name = nanovm::registry::app_name("counter", &next_version.parse().unwrap());
        assert_eq!(stats_json(node).await["workers"][&name]["digest"], next_version.split_once('@').unwrap().1);
        assert_eq!(app_get(addr, "counter", "/room/config/greeting").await, (200, "new version".into()));
        say(&mut ws, "still new").await;
        assert_eq!(next(&mut ws).await, Message::text("echo still new"));
        assert_eq!(stats_json(node).await["workers"][&name]["starts"], 2);
    }

    /// Owners, not request hosts or repositories, scope channels and caps.
    #[tokio::test(flavor = "multi_thread")]
    async fn sockets_stay_with_their_record_owner_and_repository() {
        let hosts = ["other", "alias", "alias2"].map(String::from).into();
        let cfg = nanovm::config::ProxyConfig { max_sockets_per_app: 2, hosts, ..Default::default() };
        let (_sup, _node, addr, records) = front("socket_owners", cfg, &[("counter", app("counter")), ("replacement", app("counter"))]).await;
        {
            let mut r = records.lock();
            let same = vec![format!("{}; alias=alias; alias=alias2", r["_nanovm.counter"][0])];
            r.insert("_nanovm.counter".into(), same.clone());
            r.insert("_nanovm.other".into(), same);
            for alias in ["alias", "alias2"] {
                r.insert(format!("_nanovm.{alias}"), vec!["CNAME _nanovm.counter".into()]);
            }
        }
        let mut a = open(addr, "counter", "/room/ws").await.unwrap();
        let mut b = open(addr, "alias", "/room/ws").await.unwrap();
        assert_eq!(refused(open(addr, "alias2", "/room/ws").await).0, 503);
        let mut other = open(addr, "other", "/room/ws").await.unwrap();
        say(&mut a, "pub shared").await;
        assert_eq!(next(&mut a).await, Message::text("shared"));
        assert_eq!(next(&mut b).await, Message::text("shared"));
        say(&mut other, "isolated").await;
        assert_eq!(next(&mut other).await, Message::text("echo isolated"));

        // A changed owner, even on the same repository, is another app.
        records.lock().insert("_nanovm.alias".into(), vec!["CNAME _nanovm.other".into()]);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        say(&mut b, "pub must not arrive").await;
        let Message::Close(Some(f)) = next(&mut b).await else { panic!("no close frame") };
        assert_eq!(f.code, CloseCode::Error);
        say(&mut other, "still isolated").await;
        assert_eq!(next(&mut other).await, Message::text("echo still isolated"));
        assert_eq!(app_get(addr, "other", "/room/kv/ws-ends").await.0, 404);

        // The same owner naming a different repository is also another app.
        let replacement = records.lock()["_nanovm.replacement"].clone();
        records.lock().insert("_nanovm.counter".into(), replacement);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        say(&mut a, "pub must not arrive either").await;
        let Message::Close(Some(f)) = next(&mut a).await else { panic!("no close frame") };
        assert_eq!(f.code, CloseCode::Error);
        assert_eq!(app_get(addr, "counter", "/room/kv/ws-ends").await.0, 404);
    }

    /// Events reach the object, which answers on the socket and publishes to
    /// the sockets of its own channel (from an event or a plain request), and
    /// learns how each socket ended unless it closed it itself. Events wait
    /// for room in a full queue, only sockets send them, and handshakes are
    /// checked.
    #[tokio::test(flavor = "multi_thread")]
    async fn websockets_reach_their_object_and_fan_out_within_it() {
        let mut counter = app("counter");
        counter.limits.max_queue = 2;
        let (_sup, _node, addr, _) = front("websockets_fan_out", Default::default(), &[("counter", counter.clone()), ("other", counter), ("hostile", app("hostile"))]).await;
        let (counter, hostile) = ("counter", "hostile");
        let (mut a, mut b) = (open(addr, &counter, "/room/ws").await.unwrap(), open(addr, &counter, "/room/ws").await.unwrap());
        let mut other = open(addr, &counter, "/elsewhere/ws").await.unwrap();
        let mut other_app = open(addr, "other", "/room/ws").await.unwrap();

        say(&mut a, "hello").await;
        assert_eq!(next(&mut a).await, Message::text("echo hello"));
        a.send(Message::binary(vec![0, 1, 2])).await.unwrap();
        assert_eq!(next(&mut a).await, Message::binary(vec![0, 1, 2]));
        say(&mut a, "id").await;
        let Message::Text(id) = next(&mut a).await else { panic!() };
        say(&mut b, "id").await;
        assert_ne!(next(&mut b).await, Message::Text(id));

        say(&mut a, "pub hi").await;
        assert_eq!(next(&mut a).await, Message::text("hi"));
        assert_eq!(next(&mut b).await, Message::text("hi"));
        assert_eq!(post(addr, &counter, "/room/publish?msg=yo").await, 200);
        assert_eq!(next(&mut a).await, Message::text("yo"));
        assert_eq!(next(&mut b).await, Message::text("yo"));
        // What a request publishes is sent only if it answers.
        say(&mut b, "pubpanic lost").await;
        let Message::Close(Some(f)) = next(&mut b).await else { panic!("no close frame") };
        assert_eq!(f.code, CloseCode::Error);
        b = open(addr, &counter, "/room/ws").await.unwrap();
        say(&mut a, "pub kept").await;
        assert_eq!(next(&mut a).await, Message::text("kept"));
        assert_eq!(next(&mut b).await, Message::text("kept"));
        // Another object's sockets heard none of it.
        say(&mut other, "x").await;
        assert_eq!(next(&mut other).await, Message::text("echo x"));
        say(&mut other_app, "x").await;
        assert_eq!(next(&mut other_app).await, Message::text("echo x"), "another app hears no publish even with the same object and channel");

        say(&mut b, "close").await;
        let Message::Close(Some(f)) = next(&mut b).await else { panic!("no close frame") };
        assert_eq!(f.code, CloseCode::from(4000));
        a.close(None).await.unwrap();
        drop(other);
        ended(addr, counter, "room", 2).await;
        ended(addr, counter, "elsewhere", 1).await;
        // `a`, and the `b` whose event failed; not the `b` the object closed.
        assert_eq!(app_get(addr, &counter, "/room/kv/ws-ends").await, (200, "2".into()));
        assert_eq!(app_get(addr, &counter, "/elsewhere/kv/ws-ends").await, (200, "1".into()));

        // Eight sockets' events at once wait for room in a queue of two.
        let mut many = Vec::new();
        for _ in 0..8 {
            many.push(open(addr, &counter, "/busy/ws").await.unwrap());
        }
        futures::future::join_all(many.iter_mut().map(|ws| say(ws, "hello"))).await;
        for ws in &mut many {
            assert_eq!(next(ws).await, Message::text("echo hello"));
        }
        drop(many);
        ended(addr, counter, "busy", 8).await;
        assert_eq!(app_get(addr, &counter, "/busy/kv/ws-ends").await, (200, "8".into()));

        // An object that doesn't answer `OPEN` refuses the socket, with its
        // own status if it gave one.
        assert_eq!(refused(open(addr, &hostile, "/x/").await).0, 502);
        assert_eq!(refused(open(addr, &counter, "/room/nope").await).0, 404);
        // A client can't send events, and handshakes are checked.
        let forged = format!(
            "POST /room/ws HTTP/1.1\r\nHost: {counter}\r\nContent-Type: application/websocket-events\r\n\
            Connection-Id: 1\r\nContent-Length: 12\r\nConnection: close\r\n\r\nDISCONNECT\r\n",
        );
        assert_eq!(raw(addr, &forged, b"").await.0, 400);
        let old = format!(
            "GET /room/ws HTTP/1.1\r\nHost: {counter}\r\nUpgrade: websocket\r\nConnection: Upgrade, close\r\n\
            Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 8\r\n\r\n",
        );
        assert_eq!(raw(addr, &old, b"").await.0, 426);
        assert_eq!(app_get(addr, &counter, "/room/kv/ws-ends").await, (200, "2".into()));
    }

    /// Sockets per object, per app and per node are capped, a message is
    /// bounded by the request limit, a client that falls behind is dropped,
    /// and a worker's sockets close with it.
    #[tokio::test(flavor = "multi_thread")]
    async fn websocket_limits_hold() {
        let cfg = nanovm::config::ProxyConfig { max_sockets: 4, max_sockets_per_app: 3, max_sockets_per_object: 2,
            max_request_body_kb: 16, max_response_body_kb: 64, ..Default::default() };
        let mut counter = app("counter");
        let limits = &mut counter.limits;
        (limits.max_request_body_kb, limits.max_response_body_kb) = (16, 64);
        let (_sup, node, addr, _) = front("websocket_limits_hold", cfg, &[("counter", counter.clone()), ("chat", counter)]).await;
        let (counter, chat) = ("counter", "chat");

        let a1 = open_slow(addr, &counter, "/a/ws").await;
        let _a2 = open_slow(addr, &counter, "/a/ws").await;
        let (status, body) = refused(open(addr, &counter, "/a/ws").await);
        assert_eq!((status, body.contains("object has max_sockets")), (503, true), "{body}");
        let mut big = open(addr, &counter, "/b/ws").await.unwrap();
        let (status, body) = refused(open(addr, &counter, "/c/ws").await);
        assert_eq!((status, body.contains("app has max_sockets")), (503, true), "{body}");
        let other_app = open(addr, &chat, "/x/ws").await.unwrap();
        let (status, body) = refused(open(addr, &chat, "/y/ws").await);
        assert_eq!((status, body.contains("proxy has max_sockets")), (503, true), "{body}");
        drop(other_app);
        ended(addr, chat, "x", 1).await;

        big.send(Message::text("x".repeat(16 << 10))).await.unwrap();
        let Message::Close(Some(f)) = next(&mut big).await else { panic!("no close frame") };
        assert_eq!(f.code, CloseCode::Size);
        ended(addr, counter, "b", 1).await;

        // `a1` and `a2` never read; what is published to them piles up until
        // they are dropped.
        let msg = "y".repeat(1000);
        for i in 0.. {
            assert_eq!(post(addr, &counter, &format!("/a/publish?msg={msg}&n=50")).await, 200);
            if app_get(addr, counter, "/a/kv/ws-ends").await == (200, "2".into()) {
                break;
            }
            assert!(i < 2000, "a socket that never reads was never dropped");
        }
        assert_eq!(app_get(addr, &counter, "/a/kv/ws-ends").await, (200, "2".into()));
        drop(a1);

        // A client that gives up while its `OPEN` waits on the object still
        // has its socket ended for the object, which accepted it.
        let stats = stats_json(node).await;
        let pid = stats["workers"].as_object().unwrap().iter().find(|(name, _)| name.starts_with("counter_")).unwrap().1["pid"].as_u64().unwrap() as i32;
        unsafe { libc::kill(pid, libc::SIGSTOP) };
        let upgrade = format!(
            "GET /e/ws HTTP/1.1\r\nHost: {counter}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
            Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        );
        let mut gave_up = tokio::net::TcpStream::connect(addr).await.unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut gave_up, upgrade.as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(gave_up);
        unsafe { libc::kill(pid, libc::SIGCONT) };
        let t0 = Instant::now();
        while app_get(addr, &counter, "/e/kv/ws-ends").await != (200, "1".into()) {
            assert!(t0.elapsed() < Duration::from_secs(5), "the object never heard the socket end");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // A worker exit doesn't close its sockets; their next event starts it.
        let mut s = open(addr, &counter, "/d/ws").await.unwrap();
        unsafe { libc::kill(pid, libc::SIGKILL) };
        tokio::time::sleep(Duration::from_millis(100)).await;
        say(&mut s, "survived").await;
        assert_eq!(next(&mut s).await, Message::text("echo survived"));
    }
}

/// Under an address-space limit (`ulimit -v`) smaller than the pooling
/// allocator reserves, `nanovm serve` refuses to start and says what it
/// needs, instead of starting and then failing every app's worker with
/// "failed to create memory pool mapping" (N-671).
#[test]
fn serve_explains_an_address_space_limit() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("address_space_limit");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("nanovm.toml");
    let toml = format!("listen = \"127.0.0.1:0\"\ndata_dir = {:?}\nmax_live_objects = 8\nmemory_reservation_mb = 64\n", dir.join("data"));
    std::fs::write(&cfg, toml).unwrap();
    // 4 GiB: plenty for the supervisor, a third of what 40 slots reserve.
    let cmd = format!("ulimit -v {} && exec {} serve --config {}", 4 << 20, env!("CARGO_BIN_EXE_nanovm"), cfg.display());
    let mut child = std::process::Command::new("sh").args(["-c", &cmd]).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let t0 = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if t0.elapsed() > Duration::from_secs(10) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("nanovm serve started under ulimit -v 4 GiB");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("reserves about 12 GiB of address space, but RLIMIT_AS is 4096 MiB") && stderr.contains("ulimit -v unlimited"), "{stderr}");
}

// ---- the proxy -------------------------------------------------------------

/// A DNS-over-HTTPS stub answering TXT queries from `records`, by name.
async fn doh(records: Arc<parking_lot::Mutex<std::collections::HashMap<String, Vec<String>>>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(nanovm::server::serve_http1(listener, move |req: http::Request<hyper::body::Incoming>| {
        let q = req.uri().query().unwrap_or("");
        let mut name = q.split('&').find_map(|kv| kv.strip_prefix("name=")).unwrap_or("").to_string();
        let mut answer = Vec::new();
        // A record `CNAME <target>` answers as resolvers do: the alias, then
        // the target's records under the target's name.
        let target = records.lock().get(&name).and_then(|r| r.first()?.strip_prefix("CNAME ").map(String::from));
        if let Some(target) = target {
            answer.push(serde_json::json!({ "name": name, "type": 5, "TTL": 1, "data": format!("{target}.") }));
            name = target;
        }
        answer.extend(records.lock().get(&name).into_iter().flatten().map(|txt| {
            serde_json::json!({ "name": format!("{name}."), "type": 16, "TTL": 1, "data": format!("\"{txt}\"") })
        }));
        async move { nanovm::server::ok_json(serde_json::json!({ "Status": 0, "Answer": answer })) }
    }));
    format!("http://{addr}/dns-query")
}

/// A dev-mode proxy in front of `node`, serving `*.test` to apps the dev
/// identity signs, asking `resolver`, and asking the registry for a tag's
/// digest every time.
async fn proxied(node: std::net::SocketAddr, resolver: String) -> std::net::SocketAddr {
    let cfg = nanovm::config::ProxyConfig {
        nodes: vec![node.to_string()],
        hosts: vec!["*.test".into()],
        signers: vec!["dev dev".into()],
        resolver,
        tag_ttl_s: 0,
        dev: true,
        listen: "127.0.0.1:0".into(),
        ..Default::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(nanovm::proxy::serve(nanovm::proxy::Proxy::new(cfg).unwrap(), listener));
    addr
}

/// A proxy in front of `node` serving `counter` at `counter.test`, with
/// `clients` its client limits.
async fn gated(test: &str, clients: nanovm::config::ClientLimits) -> (Arc<nanovm::supervisor::Supervisor>, std::net::SocketAddr) {
    let (sup, node) = supervised(test).await;
    let app = publish(&format!("{test}-counter"), &app("counter")).await;
    let record = vec![format!("app={}:latest; trust=dev dev", app.split_once('@').unwrap().0)];
    let records = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::from([("_nanovm.counter.test".to_string(), record)])));
    let cfg = nanovm::config::ProxyConfig {
        nodes: vec![node.to_string()], resolver: doh(records).await, dev: true, listen: "127.0.0.1:0".into(), clients,
        // The operator serves the host and lets the dev identity deploy.
        hosts: vec!["counter.test".into()], signers: vec!["dev dev".into()],
        ..Default::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(nanovm::proxy::serve(nanovm::proxy::Proxy::new(cfg).unwrap(), listener));
    (sup, addr)
}

/// A client gets `connections_per_client` connections, past which one is
/// answered 503 with a retry-after and closed; past `connections` in all, a
/// new one waits until another closes.
#[tokio::test(flavor = "multi_thread")]
async fn connections_are_capped_per_client_and_in_all() {
    use nanovm::config::ClientLimits;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let request = b"GET /a/ HTTP/1.1\r\nHost: counter.test\r\nConnection: close\r\n\r\n";
    let answer = async |s: &mut tokio::net::TcpStream| {
        s.write_all(request).await.unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out).await;
        String::from_utf8_lossy(&out).to_ascii_lowercase()
    };
    let limits = ClientLimits { connections_per_client: 2, trusted: vec![], ..ClientLimits::default() };
    let (_sup, proxy) = gated("connections_per_client", limits).await;
    let connect = || tokio::net::TcpStream::connect(proxy);
    let (a, mut b) = (connect().await.unwrap(), connect().await.unwrap());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let refused = answer(&mut connect().await.unwrap()).await;
    assert!(refused.starts_with("http/1.1 503") && refused.contains("retry-after: 1") && refused.contains("too-many-connections"), "{refused}");
    assert!(answer(&mut b).await.starts_with("http/1.1 200"));
    drop(a);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(answer(&mut connect().await.unwrap()).await.starts_with("http/1.1 200"));

    let limits = ClientLimits { connections: 2, connections_per_client: 0, ..ClientLimits::default() };
    let (_sup, proxy) = gated("connections_in_all", limits).await;
    let connect = || tokio::net::TcpStream::connect(proxy);
    let (a, _b) = (connect().await.unwrap(), connect().await.unwrap());
    let mut third = connect().await.unwrap();
    let waiting = tokio::spawn(async move { answer(&mut third).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!waiting.is_finished(), "served past the cap");
    drop(a);
    let served = tokio::time::timeout(Duration::from_secs(5), waiting).await.unwrap().unwrap();
    assert!(served.starts_with("http/1.1 200"), "{served}");
}

/// The proxy serves each host the app its `_nanovm` record names, at the
/// version its tag names now; a client can name no app itself, nor reach
/// the node's own routes; WebSockets pass through.
#[tokio::test(flavor = "multi_thread")]
async fn the_proxy_routes_hosts_by_dns() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
    let (_sup, node) = supervised("the_proxy_routes_hosts_by_dns").await;
    let records = Arc::new(parking_lot::Mutex::default());
    let proxy = proxied(node, doh(records.clone()).await).await;
    let (counter, hostile) = (publish("counter", &app("counter")).await, publish("hostile", &app("hostile")).await);
    let tagged = format!("{}:latest", counter.split_once('@').unwrap().0);
    let record = |host: &str, txt: &[&str]| {
        records.lock().insert(format!("_nanovm.{host}"), txt.iter().map(|t| t.to_string()).collect());
    };
    record("counter.test", &[&format!("app={tagged}; trust=dev dev; alias=www.counter.test")]);
    let at = |host: &str, app: &str| nanovm::registry::app_name(host, &app.parse().unwrap());
    let get = async |host: &str, path: &str, extra: &str| {
        raw(proxy, &format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{extra}Connection: close\r\n\r\n"), b"").await
    };

    let (status, body) = get("counter.test", "/a/", "").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(&format!(r#""app":"{}","object":"a""#, at("counter.test", &counter))), "{body}");
    // What a client says its app is goes unheard.
    let (status, body) = get("Counter.Test:8443", "/a/", &names(&hostile)).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(&at("counter.test", &counter)), "{body}");
    assert_eq!(get("counter.test", "/_nanovm/stats", "").await.0, 400);
    // Nor can it claim to be a socket's event (and pick its `Connection-Id`),
    // however it spells the type: counter's own parsing reads the last one.
    for types in ["text/plain\r\nContent-Type: application/websocket-events", "Application/WebSocket-Events; charset=utf-8"] {
        let head = format!("POST /room/ws HTTP/1.1\r\nHost: counter.test\r\nContent-Type: {types}\r\n\
            Connection-Id: 7\r\nContent-Length: 12\r\nConnection: close\r\n\r\n");
        let (status, body) = raw(proxy, &head, b"TEXT 2\r\nid\r\n").await;
        assert_eq!(status, 400, "{types}: {body}");
    }
    // Another host naming the same repository runs an app of its own: it
    // can choose the version it runs, but not the one counter.test runs.
    record("other.test", &[&format!("app={tagged}; trust=dev dev")]);
    let (status, body) = get("other.test", "/a/", "").await;
    assert!(status == 200 && body.contains(&at("other.test", &counter)), "{body}");
    // An alias (its record a CNAME of counter.test's, which lists it) is
    // counter.test's app; a name counter.test's record doesn't list serves
    // nothing, nor does a host the proxy doesn't serve, or a record trusting
    // only identities the proxy doesn't let deploy.
    record("www.counter.test", &["CNAME _nanovm.counter.test"]);
    let (status, body) = get("WWW.counter.test", "/a/", "").await;
    assert!(status == 200 && body.contains(&at("counter.test", &counter)), "{body}");
    record("evil.test", &["CNAME _nanovm.counter.test"]);
    record("counter.example", &[&format!("app={tagged}; trust=dev dev")]);
    record("unsigned.test", &[&format!("app={tagged}; trust=other signer")]);
    for (host, why) in [("evil.test", "doesn't list it"), ("counter.example", "doesn't serve"), ("unsigned.test", "trusts no identity")] {
        let (status, body) = get(host, "/a/", "").await;
        assert!(status == 403 && body.contains(why), "{host}: {status} {body}");
    }

    // No record is no app; a bad record, or two, is the proxy's error.
    assert_eq!(get("nowhere.test", "/a/", "").await.0, 404);
    record("bad.test", &["app=x"]);
    record("two.test", &[&format!("app={tagged}; trust=dev dev"), &format!("app={tagged}; trust=dev dev")]);
    for host in ["bad.test", "two.test"] {
        assert_eq!(get(host, "/a/", "").await.0, 502, "{host}");
    }

    // Moving the tag deploys: the next request runs the new version.
    let mut next = app("counter");
    next.config.insert("greeting".into(), "deployed".into());
    let next = publish("counter", &next).await;
    assert_eq!(get("counter.test", "/a/config/greeting", "").await, (200, "deployed".into()));
    let digest = next.split_once('@').unwrap().1;
    assert_eq!(stats_json(node).await["workers"][at("counter.test", &counter).as_str()]["digest"], digest);

    let mut req = format!("ws://{proxy}/room/ws").into_client_request().unwrap();
    req.headers_mut().insert("host", "counter.test".parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(Message::text("hello")).await.unwrap();
    let echo = async {
        loop {
            match ws.next().await.unwrap().unwrap() {
                Message::Ping(_) => continue,
                m => return m,
            }
        }
    };
    assert_eq!(tokio::time::timeout(Duration::from_secs(5), echo).await.unwrap(), Message::text("echo hello"));
}

// ---- apps from the registry, in dev mode ----------------------------------

/// The node downloads no component over `max_component_mb`, and no config
/// (or manifest, or signature) over 4 MiB.
#[tokio::test(flavor = "multi_thread")]
async fn registry_downloads_are_bounded() {
    let mut cfg = node_cfg("registry_downloads_are_bounded", 8);
    cfg.max_component_mb = 0;
    let (_sup, addr) = supervised_with(cfg).await;
    let (status, body) = app_get(addr, &publish("counter", &app("counter")).await, "/a/").await;
    assert_eq!(status, 502, "{body}");
    assert!(body.contains("the component") && body.contains("over 0"), "{body}");

    let (_sup, addr) = supervised("registry_downloads_are_bounded_config").await;
    let mut huge = app("counter");
    huge.config.insert("k".into(), "x".repeat(4 << 20));
    let (status, body) = app_get(addr, &publish("counter", &huge).await, "/a/").await;
    assert_eq!(status, 502, "{body}");
    assert!(body.contains(&format!("over {}", 4 << 20)), "{body}");
}

/// A running version is served under a new policy only once that policy
/// accepts it; a refusal is not remembered as an acceptance, and leaves the
/// version running.
#[tokio::test(flavor = "multi_thread")]
async fn a_running_version_is_checked_against_each_policy() {
    let (_sup, addr) = supervised("a_running_version_is_checked_against_each_policy").await;
    let counter = publish("counter", &app("counter")).await;
    let with_floor = |min: &str| {
        format!(
            "GET /a/ HTTP/1.1\r\nHost: x\r\n{}nanovm-min-version: {min}\r\nConnection: close\r\n\r\n",
            names(&counter)
        )
    };
    assert_eq!(app_get(addr, &counter, "/a/").await.0, 200);
    for _ in 0..2 {
        let (status, body) = raw(addr, &with_floor("2.0.0"), b"").await;
        assert_eq!(status, 502, "{body}");
        assert!(body.contains("below the minimum 2.0.0"), "{body}");
    }
    assert_eq!(raw(addr, &with_floor("1.0.0"), b"").await.0, 200);
    let v = stats_json(addr).await;
    assert_eq!(v["workers"][worker(&counter).as_str()]["starts"], 1, "{v}");
    // Ever-new policies don't grow what the supervisor remembers.
    for i in 0..20 {
        assert_eq!(raw(addr, &with_floor(&format!("0.0.{i}")), b"").await.0, 200);
    }
    let v = stats_json(addr).await;
    let w = &v["workers"][worker(&counter).as_str()];
    assert!(w["policies"].as_u64().unwrap() <= 16 && w["starts"] == 1, "{v}");
}

/// Nothing a registry sends reaches the node's disk before its app is
/// authorized: an app refused (here for its version floor) leaves the
/// registry cache empty, so unsigned manifests can't fill the disk.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_app_leaves_nothing_on_disk() {
    let test = "a_refused_app_leaves_nothing_on_disk";
    let (_sup, addr) = supervised(test).await;
    let counter = publish("counter", &app("counter")).await;
    let head = format!(
        "GET /a/ HTTP/1.1\r\nHost: x\r\n{}nanovm-min-version: 9.0.0\r\nConnection: close\r\n\r\n",
        names(&counter)
    );
    let (status, body) = raw(addr, &head, b"").await;
    assert!(status == 502 && body.contains("below the minimum"), "{status} {body}");
    let cache = Path::new(env!("CARGO_TARGET_TMPDIR")).join("nodes").join(test).join("registry");
    let kept: Vec<_> = std::fs::read_dir(&cache).into_iter().flatten().flatten().map(|e| e.file_name()).collect();
    assert!(kept.is_empty(), "{kept:?}");
}

/// Test-only components from `crates/nanovm/tests/refused` (outside
/// guests/**): `allowed` imports only what nanovm offers; each other one
/// adds one import nanovm refuses, or has no `wasi:http/handler`.
fn refused(name: &str) -> Vec<u8> {
    static BUILT: std::sync::Once = std::sync::Once::new();
    BUILT.call_once(|| {
        let build = std::process::Command::new("cargo").args([
            "build", "--locked", "--release", "--target", "wasm32-wasip2", "--manifest-path",
            "crates/nanovm/tests/refused/Cargo.toml", "--target-dir", "target/test-guest", "--lib", "--examples",
        ]).current_dir(repo()).status().unwrap();
        assert!(build.success());
    });
    let out = repo().join("target/test-guest/wasm32-wasip2/release");
    std::fs::read(match name {
        "allowed" => out.join("refused_imports.wasm"),
        _ => out.join("examples").join(format!("{name}.wasm")),
    })
    .unwrap()
}

/// Components that import what nanovm refuses (sockets of either WASI
/// version, the WASIp2 filesystem, an unknown interface) or export no HTTP
/// handler are refused when loaded, before any object exists, and the error
/// names the import. The node's linker doesn't define WASIp2 sockets or
/// files either, so the import check isn't the only thing in the way.
#[tokio::test(flavor = "multi_thread")]
async fn components_importing_refused_interfaces_are_rejected_by_name() {
    let load = |fixture: &str| Node::new(node_cfg(&format!("refused_{fixture}"), 2), &app("counter"), &refused(fixture), None);
    let control = load("allowed").unwrap_or_else(|e| panic!("the control was refused: {e:#}"));
    let res = get(&control, "a", "/").await;
    assert_eq!((res.status, res.body.as_str()), (200, "allowed"));

    for (fixture, named) in [
        ("p2_sockets", "import `wasi:sockets/"),
        ("p3_sockets", "import `wasi:sockets/types@0.3"),
        ("p2_filesystem", "import `wasi:filesystem/"),
        ("custom_import", "import `acme:backdoor/shell"),
        ("command", "wasi:http/handler"),
    ] {
        let Err(e) = load(fixture) else { panic!("{fixture} was loaded") };
        let e = format!("{e:#}");
        assert!(e.contains(named) && e.contains(r#"app "counter""#), "{fixture}: {e}");
        if fixture != "command" {
            assert!(e.contains("is not permitted"), "{fixture}: {e}");
        }
        if fixture == "p2_filesystem" {
            assert!(e.contains("@0.2"), "{fixture}: {e}");
        }
    }

    // Without the check, WASIp2 sockets and files (outside the quota) still
    // don't link: the linker defines only what the check allows (N-500).
    let engine = nanovm::engine::build(&node_cfg("refused_linking", 2)).unwrap();
    let linker = nanovm::app::linker(&engine).unwrap();
    for (fixture, missing) in [("p2_sockets", "wasi:sockets/"), ("p2_filesystem", "wasi:filesystem/")] {
        let component = wasmtime::component::Component::new(&engine, refused(fixture)).unwrap();
        let Err(e) = linker.instantiate_pre(&component) else { panic!("{fixture} links against app::linker") };
        assert!(format!("{e:#}").contains(missing), "{fixture}: {e:#}");
        assert!(nanovm::app::check_imports(&engine, &component).is_err(), "{fixture}");
    }
    let allowed = wasmtime::component::Component::new(&engine, refused("allowed")).unwrap();
    linker.instantiate_pre(&allowed).unwrap();
}

/// The same refusal through a registry and a jailed worker: the request is
/// answered `worker-down`, naming the import, and so is the next one.
#[tokio::test(flavor = "multi_thread")]
async fn a_jailed_worker_refuses_a_component_importing_sockets() {
    let (_sup, addr) = supervised("a_jailed_worker_refuses_sockets").await;
    let sockets = publish_wasm("refused-sockets", &app("counter"), refused("p3_sockets")).await;
    for _ in 0..2 {
        let (status, body) = app_get(addr, &sockets, "/a/").await;
        assert_eq!(status, 502, "{body}");
        assert!(body.contains("worker-down") && body.contains("import `wasi:sockets/types@0.3"), "{body}");
    }
}

/// A switch, once begun, finishes even if the request that began it goes
/// away mid-drain: the old worker is killed and the new one serves, so the
/// app never runs in two.
#[tokio::test(flavor = "multi_thread")]
async fn a_switch_finishes_when_its_request_goes_away() {
    use tokio::io::AsyncWriteExt;
    let (_sup, addr) = supervised("a_switch_finishes_when_its_request_goes_away").await;
    let old = publish("hostile", &app("hostile")).await;
    let mut next = app("hostile");
    next.config.insert("v".into(), "2".into());
    let new = publish("hostile", &next).await;
    assert_eq!(app_get(addr, &old, "/x/").await.0, 200);
    let slow = tokio::spawn({
        let old = old.clone();
        async move { app_get(addr, &old, "/s/sleep?ms=800").await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let pid = stats_json(addr).await["workers"][worker(&old).as_str()]["pid"].as_u64().unwrap();
    // A client asks for the new version, then hangs up during the drain.
    let mut gone = tokio::net::TcpStream::connect(addr).await.unwrap();
    let head = format!("GET /x/ HTTP/1.1\r\nHost: x\r\n{}\r\n", names(&new));
    gone.write_all(head.as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(gone);
    assert_eq!(slow.await.unwrap().0, 200);
    let t0 = Instant::now();
    loop {
        let v = stats_json(addr).await;
        let w = &v["workers"][worker(&new).as_str()];
        if w["digest"] == new.split_once('@').unwrap().1 && w["up"] == true {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(5), "the switch never finished: {v}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
    assert!(stat.map_or(true, |s| s.contains(") Z ")), "the old worker still runs");
}

/// At most `max_apps` workers run. A new app takes the slot of the least
/// recently used idle app, never a busy one's; with none idle it is refused,
/// and served once one is.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_runs_at_most_max_apps() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut cfg = node_cfg("a_node_runs_at_most_max_apps", 8);
    cfg.max_apps = 2;
    // Admission, not deadlines: keep observed requests pending until released.
    cfg.limits.request_timeout_ms = 30_000;
    let (_sup, addr) = supervised_with(cfg).await;
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    let mut tenant = app("hostile");
    tenant.limits.request_timeout_ms = 30_000;
    tenant.limits.egress.push(upstream_addr.to_string());
    let mut apps = Vec::new();
    for repo in ["apps-a", "apps-b", "apps-c"] {
        apps.push(publish(repo, &tenant).await);
    }
    let [a, b, c] = &apps[..] else { unreachable!() };
    let busy = async |app: &String| {
        let app = app.clone();
        let request = tokio::spawn(async move { app_get(addr, &app, &format!("/s/fetch?url=http://{upstream_addr}/")).await });
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), upstream.accept()).await.unwrap().unwrap();
        let mut bytes = [0; 4096];
        assert!(tokio::time::timeout(Duration::from_secs(5), socket.read(&mut bytes)).await.unwrap().unwrap() > 0);
        (request, socket)
    };
    let up = async |app: &str| stats_json(addr).await["workers"][worker(app).as_str()]["up"] == true;

    assert_eq!(app_get(addr, a, "/x/").await.0, 200);
    assert_eq!(app_get(addr, b, "/x/").await.0, 200);
    // `a` is idle and least recently used; `b` is busy.
    let (slow_b, mut gate_b) = busy(b).await;
    assert_eq!(app_get(addr, c, "/x/").await.0, 200);
    assert!(!up(a).await && up(b).await && up(c).await, "{}", stats_json(addr).await);
    // Both running apps busy: `a` is refused, and neither is disturbed.
    let (slow_c, mut gate_c) = busy(c).await;
    let (status, body) = app_get(addr, a, "/x/").await;
    assert!(status == 502 && body.contains("most apps (2)"), "{status} {body}");
    let answer = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    gate_b.write_all(answer).await.unwrap();
    assert_eq!(slow_b.await.unwrap().0, 200);
    gate_c.write_all(answer).await.unwrap();
    assert_eq!(slow_c.await.unwrap().0, 200);
    // Once one is idle (and the refusal's backoff has passed), `a` runs.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(app_get(addr, a, "/x/").await.0, 200);
    assert!(up(a).await && !up(b).await && up(c).await, "{}", stats_json(addr).await);
    // Apps that never start are forgotten once there are 2 × max_apps.
    for i in 0..6 {
        let bogus = format!("127.0.0.1:1/nothing/n{i}@sha256:{}", "a".repeat(64));
        assert_eq!(app_get(addr, &bogus, "/x/").await.0, 502);
    }
    let v = stats_json(addr).await;
    let known = v["workers"].as_object().unwrap();
    assert!(known.len() <= 4, "{v}");
    assert_eq!(known.values().filter(|w| w["up"] == true).count(), 2, "{v}");
}

/// A registry that answers a byte at a time can't hold the node: at most
/// four fetches run at once, across apps, and each ends at
/// `load_timeout_ms`.
#[tokio::test(flavor = "multi_thread")]
async fn slow_registries_are_bounded() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let slow = listener.local_addr().unwrap();
    let (open, most) = (Arc::new(std::sync::atomic::AtomicUsize::new(0)), Arc::new(std::sync::atomic::AtomicUsize::new(0)));
    let (o, m) = (open, most.clone());
    tokio::spawn(async move {
        loop {
            let (mut conn, _) = listener.accept().await.unwrap();
            let (o, m) = (o.clone(), m.clone());
            tokio::spawn(async move {
                m.fetch_max(o.fetch_add(1, Relaxed) + 1, Relaxed);
                let _ = conn.read(&mut [0; 4096]).await;
                let mut out = b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\n".to_vec();
                while conn.write_all(&out).await.is_ok() {
                    out = b"x".to_vec();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                o.fetch_sub(1, Relaxed);
            });
        }
    });
    let mut cfg = node_cfg("slow_registries_are_bounded", 8);
    cfg.load_timeout_ms = 1000;
    let (_sup, addr) = supervised_with(cfg).await;
    let t0 = Instant::now();
    let loads: Vec<_> = (0..8)
        .map(|i| {
            let app = format!("{slow}/slow/app{i}@sha256:{}", "a".repeat(64));
            tokio::spawn(async move { app_get(addr, &app, "/x/").await })
        })
        .collect();
    // Before any deadline, so dropped connections can't be counted.
    tokio::time::sleep(Duration::from_millis(900)).await;
    let most = most.load(Relaxed);
    assert!((1..=4).contains(&most), "{most} fetches at once");
    for load in loads {
        let (status, body) = tokio::time::timeout(Duration::from_secs(10), load).await.expect("a load hangs").unwrap();
        assert!(status == 502 && body.contains("took over 1s"), "{status} {body}");
    }
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
}

/// The public proxy outlasts clients that hold connections: one that never
/// finishes its headers is hung up on after 10 s, and
/// running out of file descriptors pauses accepting rather than ending the
/// process.
#[tokio::test(flavor = "multi_thread")]
async fn the_proxy_outlasts_idle_and_excess_connections() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("proxy_outlasts");
    std::fs::create_dir_all(&dir).unwrap();
    let port = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    // A proxy whose resolver is unreachable: every request is its 502.
    let start = |name: &str| {
        let addr = port();
        let cfg = dir.join(format!("{name}.toml"));
        // Its own private port too: the default, 8444, may be another's.
        let toml = format!("listen = \"{addr}\"\npublish_listen = \"{}\"\nresolver = \"http://127.0.0.1:1/dns-query\"\nhosts = [\"x\"]\n", port());
        std::fs::write(&cfg, toml).unwrap();
        let cmd = format!("ulimit -n 64 && exec {} proxy --config {}", env!("CARGO_BIN_EXE_nanovm"), cfg.display());
        let child = tokio::process::Command::new("sh").args(["-c", &cmd]).kill_on_drop(true).spawn().unwrap();
        (child, addr)
    };
    let connect = async |addr| {
        for _ in 0..100 {
            if let Ok(c) = tokio::net::TcpStream::connect(addr).await {
                return c;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the proxy never listened on {addr}");
    };
    // Waits for the server to hang up, and says how long that took.
    let hung_up = async |mut c: tokio::net::TcpStream| {
        let t0 = Instant::now();
        let n = tokio::time::timeout(Duration::from_secs(20), c.read(&mut [0; 64])).await;
        assert!(matches!(n, Ok(Ok(0)) | Ok(Err(_))), "still open: {n:?}");
        t0.elapsed()
    };

    let (mut plain, addr) = start("plain");
    let mut slow = connect(addr).await;
    slow.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n").await.unwrap();
    let headers = tokio::spawn(async move { hung_up(slow).await });
    // Past the fd limit, then back under it: the proxy still answers.
    let mut excess = Vec::new();
    for _ in 0..100 {
        excess.push(connect(addr).await);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(excess);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(plain.try_wait().unwrap().is_none(), "the proxy exited");
    let (status, body) = raw(addr, "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n", b"").await;
    assert_eq!(status, 502, "{body}");

    let took = headers.await.unwrap();
    assert!(took >= Duration::from_secs(9), "{took:?}");
}

/// Sends `head`, then the `len` bytes of body its Content-Length promised,
/// `chunk` bytes every `every`, while reading the answer. Returns its
/// status and body (what arrived before any reset) and when it came.
async fn trickle(addr: std::net::SocketAddr, head: String, len: usize, chunk: usize, every: Duration) -> (u16, String, Duration) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut rx, mut tx) = tokio::net::TcpStream::connect(addr).await.unwrap().into_split();
    let t0 = Instant::now();
    tx.write_all(head.as_bytes()).await.unwrap();
    let writer = tokio::spawn(async move {
        for sent in (0..len).step_by(chunk) {
            if tx.write_all(&vec![b'x'; chunk.min(len - sent)]).await.is_err() {
                return;
            }
            tokio::time::sleep(every).await;
        }
        std::future::pending::<()>().await;
    });
    let mut out = Vec::new();
    let read = async {
        let mut buf = [0; 4096];
        while let Ok(n @ 1..) = rx.read(&mut buf).await {
            out.extend_from_slice(&buf[..n]);
        }
    };
    let ended = tokio::time::timeout(Duration::from_secs(40), read).await;
    let took = t0.elapsed();
    assert!(ended.is_ok(), "{addr}: still open after {took:?}: {:?}", String::from_utf8_lossy(&out));
    writer.abort();
    let text = String::from_utf8_lossy(&out).to_string();
    let status = text.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    (status, text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default(), took)
}

/// A client trickling a request body (a byte every 200 ms) holds nothing for
/// long at any entry point (an in-process node, the supervisor, the proxy,
/// and a proxy whose node never reads): after about `CLIENT_TIMEOUT` it is
/// answered 408 `body-timeout` and its connection is closed. An upload sent
/// steadily at 8 KiB/s through the proxy takes longer than that and is not
/// cut (N-505).
#[tokio::test(flavor = "multi_thread")]
async fn slow_request_bodies_are_cut_at_every_entry_point() {
    let node = node("slow_bodies", "counter", 8);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let direct = listener.local_addr().unwrap();
    let n = node.clone();
    tokio::spawn(nanovm::server::serve_http1(listener, move |req| nanovm::server::handle(n.clone(), req)));
    let (_sup, sup) = supervised("slow_bodies_supervised").await;
    let counter = publish("counter", &app("counter")).await;
    let records = Arc::new(parking_lot::Mutex::default());
    let proxy = proxied(sup, doh(records.clone()).await).await;
    let tagged = format!("{}:latest", counter.split_once('@').unwrap().0);
    records.lock().insert("_nanovm.slow.test".to_string(), vec![format!("app={tagged}; trust=dev dev")]);
    let entries = [(direct, String::new()), (sup, names(&counter)), (proxy, String::new())];
    let head = |extra: &str, method: &str, path: &str, len: usize| {
        format!("{method} {path} HTTP/1.1\r\nHost: slow.test\r\n{extra}Content-Length: {len}\r\n\r\n")
    };
    // Ordinary uploads first, which also start the worker.
    for (addr, extra) in &entries {
        let (status, body) = raw(*addr, &head(extra, "PUT", "/a/kv/k", 5).replace("\r\n\r\n", "\r\nConnection: close\r\n\r\n"), b"hello").await;
        assert_eq!(status, 204, "{addr}: {body}");
    }
    // A node that takes requests and never reads them: the proxy's own rule.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let deaf = listener.local_addr().unwrap();
    tokio::spawn(nanovm::server::serve_http1(listener, |req: http::Request<hyper::body::Incoming>| async move {
        let _held = req;
        std::future::pending().await
    }));
    let entries = [&entries[..], &[(proxied(deaf, doh(records.clone()).await).await, String::new())]].concat();
    let slow = entries.iter().map(|(addr, extra)| tokio::spawn(trickle(*addr, head(extra, "PUT", "/a/kv/k", 1000), 1000, 1, Duration::from_millis(200))));
    let steady = tokio::spawn(trickle(proxy, head("", "PUT", "/a/kv/big", 128 << 10), 128 << 10, 1024, Duration::from_millis(125)));
    for (s, (addr, _)) in slow.collect::<Vec<_>>().into_iter().zip(&entries) {
        let (status, body, took) = s.await.unwrap();
        eprintln!("{addr}: {status} after {took:?}");
        assert_eq!(status, 408, "{addr}: {body}");
        assert!(body.contains("body-timeout"), "{addr}: {body}");
        let limit = nanovm::server::CLIENT_TIMEOUT;
        assert!(took >= limit && took < limit + Duration::from_secs(3), "{addr}: {took:?}");
    }
    let (status, body, took) = steady.await.unwrap();
    assert_eq!(status, 204, "{body}");
    assert!(took > Duration::from_secs(15), "{took:?}");
    let (status, body) = raw(proxy, "GET /a/kv/big HTTP/1.1\r\nHost: slow.test\r\nConnection: close\r\n\r\n", b"").await;
    assert_eq!((status, body.len()), (200, 128 << 10));
}

/// Hop-by-hop headers, and the ones a client's `Connection` lists, stay on
/// the client's connection; the rest reach the node.
#[tokio::test(flavor = "multi_thread")]
async fn the_proxy_drops_hop_by_hop_headers() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo = listener.local_addr().unwrap();
    tokio::spawn(nanovm::server::serve_http1(listener, |req: http::Request<hyper::body::Incoming>| {
        let names: Vec<_> = req.headers().keys().map(|k| k.to_string()).collect();
        async move { nanovm::server::ok_json(serde_json::json!(names)) }
    }));
    let records = Arc::new(parking_lot::Mutex::default());
    let proxy = proxied(echo, doh(records.clone()).await).await;
    let counter = publish("counter", &app("counter")).await;
    let txt = format!("app={}:latest; trust=dev dev", counter.split_once('@').unwrap().0);
    records.lock().insert("_nanovm.hop.test".to_string(), vec![txt]);
    let head = "GET /a/ HTTP/1.1\r\nHost: hop.test\r\nConnection: close, x-private\r\nx-private: 1\r\n\
        Keep-Alive: timeout=5\r\nTE: trailers\r\nProxy-Authorization: Basic eA==\r\nx-kept: 1\r\n\r\n";
    let (status, body) = raw(proxy, head, b"").await;
    assert_eq!(status, 200, "{body}");
    let names: Vec<String> = serde_json::from_str(&body).unwrap();
    assert!(names.contains(&"x-kept".to_string()) && names.contains(&"nanovm-app".to_string()), "{names:?}");
    for gone in ["x-private", "keep-alive", "te", "proxy-authorization", "connection"] {
        assert!(!names.contains(&gone.to_string()), "{gone} reached the node: {names:?}");
    }
    // WebSocket upgrades terminate here, not at the node; malformed ones fail.
    let head = "GET /a/ HTTP/1.1\r\nHost: hop.test\r\nConnection: Upgrade, close, x-private\r\nUpgrade: websocket\r\n\r\n";
    assert_eq!(raw(proxy, head, b"").await.0, 400);
}

#[tokio::test(flavor = "multi_thread")]
async fn front_door_events_cover_failures_and_link_to_worker() {
    use http_body_util::BodyExt;
    use opentelemetry_proto::tonic::{collector::trace::v1::ExportTraceServiceRequest, common::v1::any_value::Value};
    use prost::Message;
    let spans = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", sink.local_addr().unwrap());
    let captured = spans.clone();
    let collector = tokio::spawn(nanovm::server::serve_http1(sink, move |req| {
        let spans = captured.clone();
        async move {
            let trace = req.uri().path() == "/v1/traces";
            let body = req.into_body().collect().await.unwrap().to_bytes();
            if trace {
                assert!(!body.windows(7).any(|s| s == b"PRIVATE"));
                spans.lock().extend(ExportTraceServiceRequest::decode(body).unwrap().resource_spans
                    .into_iter().flat_map(|r| r.scope_spans).flat_map(|s| s.spans));
            }
            nanovm::server::ok_json(serde_json::json!({}))
        }
    }));
    let mut cfg = node_cfg("front_door_events", 8);
    cfg.telemetry_endpoint = endpoint.clone();
    let (sup, node) = supervised_with(cfg).await;
    let counter = publish("front-door", &app("counter")).await;
    let records = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    records.lock().insert("_nanovm.good.test".into(), vec![format!("app={}:latest; trust=dev dev", counter.split_once('@').unwrap().0)]);
    let cfg = nanovm::config::ProxyConfig {
        nodes: vec![node.to_string()], resolver: doh(records).await, telemetry_endpoint: endpoint,
        listen: "127.0.0.1:0".into(), dev: true, hosts: vec!["*.test".into()], signers: vec!["dev dev".into()], ..Default::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap();
    let service = tokio::spawn(nanovm::proxy::serve(nanovm::proxy::Proxy::new(cfg).unwrap(), listener));
    let missing = format!("{}@sha256:{}", counter.split_once('@').unwrap().0, "0".repeat(64));
    let cases = [
        (node, "x", "/obj/", String::new(), 400),
        (node, "x", "/obj/", "nanovm-app: invalid\r\n".into(), 400),
        (node, "x", "/../", names(&counter), 400),
        (node, "x", "/obj/", names(&missing), 502),
        (proxy, "absent.test", "/obj/", String::new(), 404),
        (proxy, "good.test", "/obj/", "Content-Type: application/websocket-events\r\n".into(), 400),
        (proxy, "good.test", "/obj/", String::new(), 200),
    ];
    for (i, (addr, host, path, headers, status)) in cases.iter().enumerate() {
        let head = format!("GET {path}?secret=PRIVATE HTTP/1.1\r\nHost: {host}\r\n{headers}traceparent: 00-{:032x}-1111111111111111-01\r\nConnection: close\r\n\r\n", i + 1);
        assert_eq!(raw(*addr, &head, b"").await.0, *status, "case {i}");
    }
    sup.flush_telemetry().await.unwrap();
    eventually("front-door exports", || spans.lock().len() >= 9).await;
    let spans = spans.lock();
    assert_eq!(spans.len(), 9, "six rejections, one complete three-hop request");
    for (i, (_, _, path, _, status)) in cases.iter().enumerate() {
        let mut trace = vec![0; 16];
        trace[15] = (i + 1) as u8;
        let group: Vec<_> = spans.iter().filter(|s| s.trace_id == trace).collect();
        let root = group.iter().find(|s| s.parent_span_id == [0x11; 8]).unwrap();
        assert_eq!(group.len(), if *status == 200 { 3 } else { 1 });
        assert_eq!(root.kind, 2);
        assert_eq!(root.status.as_ref().map_or(0, |s| s.code), if *status == 502 { 2 } else { 0 });
        for (key, value) in [("url.path", Value::StringValue((*path).into())),
            ("http.response.status_code", Value::IntValue(*status as i64))] {
            assert!(root.attributes.iter().any(|a| a.key == key && a.value.as_ref().and_then(|v| v.value.as_ref()) == Some(&value)));
        }
        if *status == 200 {
            let supervisor = group.iter().find(|s| s.name == "supervisor.request").unwrap();
            let worker = group.iter().find(|s| s.name == "request").unwrap();
            assert_eq!(root.name, "proxy.request");
            assert_eq!(supervisor.parent_span_id, root.span_id);
            assert_eq!(worker.parent_span_id, supervisor.span_id);
        }
    }
    service.abort();
    collector.abort();
}

/// A private fixture, outside guests/** and its publishing workflow.
fn probe() -> Vec<u8> {
    let build = std::process::Command::new("cargo").args([
        "build", "--locked", "--release", "--target", "wasm32-wasip2", "--manifest-path",
        "crates/nanovm/tests/post-return/Cargo.toml", "--target-dir", "target/test-guest",
    ]).current_dir(repo()).status().unwrap();
    assert!(build.success());
    std::fs::read(repo().join("target/test-guest/wasm32-wasip2/release/post_return_probe.wasm")).unwrap()
}

/// `crates/nanovm/tests/sdk-after`: an app on nanovm-sdk whose routes do
/// work after the answer and fail in it, served on a node of its own.
fn sdk_after(test: &str) -> Arc<Node> {
    let (app, wasm) = sdk_after_app();
    Node::new(node_cfg(test, 8), &app, &wasm, None).unwrap()
}

/// The `sdk-after` app and its component.
fn sdk_after_app() -> (AppConfig, Vec<u8>) {
    let build = std::process::Command::new("cargo").args([
        "build", "--locked", "--release", "--target", "wasm32-wasip2", "--manifest-path",
        "crates/nanovm/tests/sdk-after/Cargo.toml", "--target-dir", "target/test-guest",
    ]).current_dir(repo()).status().unwrap();
    assert!(build.success());
    let wasm = std::fs::read(repo().join("target/test-guest/wasm32-wasip2/release/sdk_after_probe.wasm")).unwrap();
    let mut app = app("counter");
    (app.name, app.limits.memory_mb, app.limits.post_return_timeout_ms) = ("sdk".into(), 32, 2000);
    (app, wasm)
}

/// Work after the answer (`after`: 500 ms, then a write) is part of its
/// request, so retiring its worker (a switch to another version here) lets
/// it finish and commit, as the drain lets requests finish, rather than
/// roll it back.
#[tokio::test(flavor = "multi_thread")]
async fn a_retired_workers_work_after_the_answer_commits() {
    let (mut app, wasm) = sdk_after_app();
    let (_sup, addr) = supervised("a_retired_workers_work_after_the_answer_commits").await;
    let v1 = publish_version("retired-after", &app, wasm.clone(), "1.0.0").await;
    app.env.insert("NEXT".into(), "1".into());
    let v2 = publish_version("retired-after", &app, wasm, "1.0.1").await;
    assert_eq!(app_get(addr, &v1, "/a/slow").await, (200, "answered".to_string()));
    let (status, files) = app_get(addr, &v2, "/a/files").await;
    assert_eq!(status, 200, "{files}");
    let files: serde_json::Value = serde_json::from_str(&files).unwrap();
    assert_eq!(files["later"], true, "work after the answer was cut off by the switch: {files}");
}

/// What `/files` says: requests the instance has served, and which files the
/// object holds.
fn files(served: u32, written: &[&str]) -> serde_json::Value {
    let mut v = serde_json::json!({"served": served});
    for name in ["answered", "later", "nested", "streamed"] {
        v[name] = written.contains(&name).into();
    }
    v
}

/// `after` work starts once the client has the whole answer, so nothing it
/// does can fail the answer. Work that panics, at once or after writing,
/// rolls back what it wrote and replaces the instance; the answer and what
/// the handler wrote stand.
#[tokio::test(flavor = "multi_thread")]
async fn after_work_that_panics_leaves_the_answer_standing() {
    let node = sdk_after("sdk_after_panics");
    for (id, path) in [("a", "/panic-at-once"), ("b", "/panic-later")] {
        let r = request(&node, "POST", id, path, "").await;
        assert_eq!((r.status, r.body.as_str()), (200, "answered"), "{path}: {:?}", r.error);
        assert_eq!(get(&node, id, "/files").await.json(), files(1, &["answered"]), "{path}");
    }
}

/// Work that `after` work passes to `after` runs before the object's next
/// request, and commits or rolls back with the rest of the work after the
/// answer.
#[tokio::test(flavor = "multi_thread")]
async fn after_work_passed_on_by_after_work_runs_before_the_next_request() {
    let node = sdk_after("sdk_after_nested");
    let r = request(&node, "POST", "a", "/nested", "").await;
    assert_eq!(r.status, 200, "{:?}", r.error);
    assert_eq!(get(&node, "a", "/files").await.json(), files(2, &["answered", "nested"]));
    let r = request(&node, "POST", "b", "/nested-panic", "").await;
    assert_eq!(r.status, 200, "{:?}", r.error);
    assert_eq!(get(&node, "b", "/files").await.json(), files(1, &["answered"]));
}

/// A client slow to read a long answer still gets it whole, though the work
/// after it fails at once: that work waits until the answer is out.
#[tokio::test(flavor = "multi_thread")]
async fn after_work_that_panics_lets_a_slow_client_read_the_whole_answer() {
    use http_body_util::BodyExt;
    let node = sdk_after("sdk_after_big");
    let res = node.dispatch("a", http::Request::post("http://localhost/big-panic").body(bytes::Bytes::new()).unwrap()).await;
    assert_eq!(res.status(), 200);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let body = res.into_body().collect().await.expect("the answer arrives whole").to_bytes();
    assert_eq!(body.len(), 1 << 20);
    assert_eq!(get(&node, "a", "/files").await.json(), files(1, &["answered"]));
}

/// A streamed body whose writer returns an error breaks off for the client,
/// rather than ending as if whole, and what the writer wrote rolls back, as
/// on a panic.
#[tokio::test(flavor = "multi_thread")]
async fn a_streamed_body_whose_writer_fails_breaks_off_and_rolls_back() {
    use http_body_util::BodyExt;
    let node = sdk_after("sdk_after_cut");
    let res = node.dispatch("a", http::Request::get("http://localhost/cut").body(bytes::Bytes::new()).unwrap()).await;
    assert_eq!(res.status(), 200);
    let (mut body, mut data) = (res.into_body(), Vec::new());
    let end = loop {
        match body.frame().await {
            Some(Ok(frame)) => data.extend_from_slice(&frame.into_data().unwrap_or_default()),
            end => break end.map(|e| e.map(drop)),
        }
    };
    assert_eq!(data, b"partial");
    assert!(matches!(end, Some(Err(_))), "the body ended as if whole: {end:?}");
    assert_eq!(get(&node, "a", "/files").await.json(), files(1, &["answered"]));
}

/// What the host holds for a guest's header maps stays within its memory
/// limit: a map holds at most 64 KiB (its own, or a copy of one it was sent;
/// bigger heads are refused before it), and an instance holds no more
/// resources than its memory limit has room for at that size.
#[tokio::test(flavor = "multi_thread")]
async fn guest_header_maps_are_bounded_by_the_memory_limit() {
    let mut app = app("hostile");
    app.limits.cpu_ms = 2000;
    app.limits.request_timeout_ms = 5000;
    // 32 MiB: room for 512 maps of 64 KiB, of its 2,000 resources.
    let node = Node::new(node_cfg("guest_header_maps", 8), &app, &probe(), None).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(nanovm::server::serve_http1(listener, move |req| nanovm::server::handle(node.clone(), req)));
    let get = async |path: &str, header: usize| {
        let head = format!("GET {path} HTTP/1.1\r\nHost: x\r\nx-big: {}\r\nConnection: close\r\n\r\n", "x".repeat(header));
        raw(addr, &head, b"").await
    };
    // 600 maps of 60 KiB, new or copies of a request's, are over 32 MiB.
    for (path, header) in [("/a/headers?n=600&kb=60", 0), ("/b/headers?n=600", 60 << 10)] {
        let (status, body) = get(path, header).await;
        assert!(status == 500 && body.contains("resource-table"), "{path}: {status} {body}");
    }
    assert_eq!(get("/c/headers?n=1&kb=100", 0).await.0, 500, "a map over 64 KiB");
    assert_eq!(get("/d/headers?n=0", 100 << 10).await.0, 431, "a head over 64 KiB");
    assert_eq!(get("/e/headers?n=100&kb=60", 50 << 10).await, (200, "100".into()), "within the limits");
}

/// A guest that wakes itself in a tight loop (each wake goes back to the
/// host, which runs it again at once) holds its thread at most a tick at a
/// time, as a spinner does: on a one-thread runtime a neighbour's requests
/// get through quickly, and it is killed at its CPU budget.
#[tokio::test(flavor = "current_thread")]
async fn a_self_waking_guest_yields_its_thread_at_each_tick() {
    let mut app = app("hostile");
    app.limits.cpu_ms = 500;
    app.limits.request_timeout_ms = 5000;
    let node = Node::new(node_cfg("a_self_waking_guest", 8), &app, &probe(), None).unwrap();
    assert_eq!(get(&node, "neighbour", "/callee").await.status, 200);
    let t0 = Instant::now();
    // Spawned: the test's own future runs only once the queue is empty.
    let n = node.clone();
    let waker = tokio::spawn(async move { (get(&n, "waker", "/selfwake").await, t0.elapsed()) });
    let neighbour = tokio::spawn(async move {
        let mut slowest = Duration::ZERO;
        for _ in 0..5 {
            let r = get(&node, "neighbour", "/callee").await;
            assert_eq!(r.status, 200, "{}", r.body);
            slowest = slowest.max(r.elapsed);
        }
        (slowest, t0.elapsed())
    });
    let ((woke, woke_at), (slowest, neighbour_at)) = (waker.await.unwrap(), neighbour.await.unwrap());
    assert_eq!(woke.error.as_deref(), Some("cpu"), "{}", woke.body);
    assert!(
        neighbour_at < woke_at && slowest < Duration::from_millis(50),
        "the neighbour's slowest request took {slowest:?}, all five {neighbour_at:?}; the waker ran {woke_at:?}"
    );
}

/// Nor does a method, authority or path a guest sets on its requests take
/// what the host holds for them past its memory limit (N-691): a request's
/// head (those, its scheme and its headers) is at most 64 KiB, as every head
/// the node is sent, so a resource still holds at most that.
#[tokio::test(flavor = "multi_thread")]
async fn guest_request_heads_are_bounded_by_the_memory_limit() {
    let mut app = app("hostile");
    app.limits.memory_mb = 16;
    app.limits.cpu_ms = 2000;
    app.limits.request_timeout_ms = 5000;
    let node = Node::new(node_cfg("guest_request_heads", 8), &app, &probe(), None).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(nanovm::server::serve_http1(listener, move |req| nanovm::server::handle(node.clone(), req)));
    let get = async |path: &str| raw(addr, &format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"), b"").await;
    // Each is more than 16 MiB: 32 methods or authorities of 1 MiB, or 200
    // paths of 60 KiB beside 60 KiB of headers.
    for (path, n) in [("/a/strings?part=method&len=1048576&n=32", 32), ("/b/strings?part=authority&len=1048576&n=32", 32),
        ("/c/strings?part=path&len=61440&hlen=61440&n=200", 200)] {
        let (status, body) = get(path).await;
        let counts = body.split_once(' ').and_then(|(h, r)| Some((h.parse::<usize>().ok()?, r.parse::<usize>().ok()?)));
        let (held, refused) = counts.unwrap_or_else(|| panic!("{path}: {status} {body}"));
        assert!(status == 200 && held <= 16 << 20, "{path}: the host holds {held} bytes of them");
        assert_eq!(refused, n, "{path}");
    }
    // Real ones fit: an extension method, an 8 KiB path with 8 KiB of
    // headers, a 60 KiB path alone, the longest host name.
    for (path, held) in [("/d/strings?part=method&len=20&n=10", 200), ("/e/strings?part=path&len=8192&hlen=8192&n=10", 163840),
        ("/f/strings?part=path&len=61440&n=10", 614400), ("/g/strings?part=authority&len=253&n=10", 2530)] {
        assert_eq!(get(path).await, (200, format!("{held} 0")), "{path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn post_return_work_keeps_context_and_reports_commit_failure() {
    use http_body_util::BodyExt;
    use opentelemetry_proto::tonic::{collector, common::v1::any_value::Value};
    use prost::Message;
    let wasm = probe();
    let spans = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let logs = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = node_cfg("post_return_context", 8);
    cfg.capture = Some(Default::default());
    cfg.telemetry_endpoint = format!("http://{}", sink.local_addr().unwrap());
    let (captured, captured_logs) = (spans.clone(), logs.clone());
    let collector = tokio::spawn(nanovm::server::serve_http1(sink, move |req| {
        let (spans, logs) = (captured.clone(), captured_logs.clone());
        async move {
            let trace = req.uri().path() == "/v1/traces";
            let body = req.into_body().collect().await.unwrap().to_bytes();
            if trace {
                spans.lock().extend(collector::trace::v1::ExportTraceServiceRequest::decode(body).unwrap()
                    .resource_spans.into_iter().flat_map(|r| r.scope_spans).flat_map(|s| s.spans));
            } else {
                logs.lock().extend(collector::logs::v1::ExportLogsServiceRequest::decode(body).unwrap()
                    .resource_logs.into_iter().flat_map(|r| r.scope_logs).flat_map(|s| s.log_records));
            }
            nanovm::server::ok_json(serde_json::json!({}))
        }
    }));
    let gate = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = gate.local_addr().unwrap();
    let begin = Arc::new(tokio::sync::Semaphore::new(0));
    let finish = Arc::new(tokio::sync::Semaphore::new(0));
    let (arrived, mut arrivals) = tokio::sync::mpsc::channel(4);
    let (b, f) = (begin.clone(), finish.clone());
    let server = tokio::spawn(nanovm::server::serve_http1(gate, move |req| {
        let gate = if req.uri().path() == "/begin" { b.clone() } else { f.clone() };
        let arrived = arrived.clone();
        async move {
            arrived.send(req.uri().path().to_owned()).await.unwrap();
            gate.acquire().await.unwrap().forget();
            nanovm::server::ok_json(serde_json::json!({}))
        }
    }));
    let mut app = app("hostile");
    app.limits.egress = vec![address.to_string()];
    app.limits.post_return_timeout_ms = 5000;
    app.limits.cpu_ms = 1000;
    let objects = cfg.app_dirs(&app.name)[0].clone();
    let node = Node::new(cfg.clone(), &app, &wasm, None).unwrap();
    for (i, fail) in [(1u8, false), (2, true)] {
        let id = format!("parent{i}");
        let req = http::Request::builder().uri(format!("/?gate={address}"))
            .header("traceparent", format!("00-{i:032x}-1111111111111111-01"))
            .body(bytes::Bytes::new()).unwrap();
        let response = node.dispatch(&id, req).await;
        assert_eq!(response.status(), 200);
        let first = captured_files(&objects, &id, 1);
        assert_eq!(first[Path::new("data/before.txt")], b"before");
        assert!(!first.contains_key(Path::new("data/after.txt")), "post-return bytes entered the response capture");
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), arrivals.recv()).await.unwrap().unwrap(), "/begin");
        begin.add_permits(1);
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), arrivals.recv()).await.unwrap().unwrap(), "/finish");
        let after = object_dir(&objects, &id).join("data/after.txt");
        assert!(after.exists(), "post-return work wrote");
        if fail {
            // A file the commit can't sync, as on a failing disk.
            std::fs::rename(&after, objects.join(format!("{id}-after"))).unwrap();
            std::os::unix::fs::symlink("after.txt", &after).unwrap();
        }
        let mut queued = if !fail {
            let node = node.clone();
            let id = id.clone();
            Some(tokio::spawn(async move {
                node.dispatch(&id, http::Request::builder().uri("/callee").body(bytes::Bytes::new()).unwrap()).await
            }))
        } else { None };
        if let Some(next) = &mut queued {
            assert!(tokio::time::timeout(Duration::from_millis(100), next).await.is_err(), "next request entered during post-return work");
        }
        finish.add_permits(1);
        assert_eq!(response.into_body().collect().await.unwrap().to_bytes(), "finished");
        if let Some(next) = queued {
            let next = tokio::time::timeout(Duration::from_secs(5), next).await.unwrap().unwrap();
            assert_eq!(next.into_body().collect().await.unwrap().to_bytes(), "callee");
        }
        eventually("final commit", || node.phases().keys().all(|p| *p == "idle" || *p == "cold")).await;
        if !fail {
            assert_eq!(captured_files(&objects, &id, 2)[Path::new("data/after.txt")], b"after");
        } else {
            assert!(!object_dir(&objects, &id).join(format!("committed/{:020}.tar", 2)).exists());
        }
        assert_eq!(captured_files(&objects, &id, 1), first, "later work changed a published version");
    }
    node.shutdown().await;
    let telemetry = node.telemetry.clone();
    drop(node);
    // The injected failure happened before the log's new key: its undo
    // records must retain the first commit and reject only later work.
    // This is not a claim that every ambiguous commit failure rolls back.
    for _ in 0..2 {
        let reopened = Node::new(cfg.clone(), &app, &wasm, None).unwrap();
        for id in ["parent1", "parent2"] {
            let response = reopened.dispatch(id, http::Request::builder().uri("/callee").body(bytes::Bytes::new()).unwrap()).await;
            assert_eq!(response.status(), 200);
            response.into_body().collect().await.unwrap();
            assert_eq!(std::fs::read(object_dir(&objects, id).join("data/before.txt")).unwrap(), b"before");
        }
        assert_eq!(std::fs::read(object_dir(&objects, "parent1").join("data/after.txt")).unwrap(), b"after");
        assert!(!object_dir(&objects, "parent2").join("data/after.txt").exists());
        assert!(!object_dir(&objects, "parent2").join("committed/00000000000000000002.tar").exists(), "recovery published rejected post-return work");
        reopened.shutdown().await;
    }
    tokio::task::spawn_blocking(move || telemetry.flush().unwrap()).await.unwrap();
    let spans = spans.lock();
    let logs = logs.lock();
    let attr = |attrs: &[opentelemetry_proto::tonic::common::v1::KeyValue], key: &str| {
        attrs.iter().find(|a| a.key == key).and_then(|a| a.value.as_ref()).and_then(|v| v.value.clone())
    };
    for (i, fail) in [(1u8, false), (2, true)] {
        let mut trace = vec![0; 16];
        trace[15] = i;
        let parent = spans.iter().find(|s| s.trace_id == trace && s.name == "request"
            && attr(&s.attributes, "nanovm.object") == Some(Value::StringValue(format!("parent{i}")))).unwrap();
        assert_eq!(attr(&parent.attributes, "http.response.status_code"), Some(Value::IntValue(200)));
        assert_eq!(parent.status.as_ref().map_or(0, |s| s.code), if fail { 2 } else { 0 });
        assert_eq!(attr(&parent.attributes, "error.type"), fail.then(|| Value::StringValue("post-return-commit-failed".into())));
        let call = spans.iter().find(|s| s.trace_id == trace && s.name == "call").unwrap();
        let child = spans.iter().find(|s| s.trace_id == trace && s.name == "request" && s.span_id != parent.span_id).unwrap();
        assert_eq!(call.parent_span_id, parent.span_id);
        assert_eq!(child.parent_span_id, call.span_id);
        let response_ms = match attr(&parent.attributes, "nanovm.response.duration_ms").unwrap() {
            Value::DoubleValue(ms) => ms, other => panic!("{other:?}"),
        };
        assert!(call.start_time_unix_nano > parent.start_time_unix_nano + (response_ms * 1e6) as u64);
        assert!(parent.end_time_unix_nano >= call.end_time_unix_nano);
        for (key, value) in [("log.iostream", "stderr")].into_iter()
            .chain(fail.then_some(("event.name", "post-return-commit-failed"))) {
            let log = logs.iter().find(|l| l.trace_id == trace && attr(&l.attributes, key) == Some(Value::StringValue(value.into()))).unwrap();
            assert_eq!(log.span_id, parent.span_id);
            assert!(matches!(log.body.as_ref().and_then(|v| v.value.as_ref()), Some(Value::StringValue(s)) if !s.is_empty() && s.len() <= 4096));
        }
    }
    server.abort();
    collector.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn jailed_workers_export_otel_without_guest_network_access() {
    use http_body_util::BodyExt;
    use opentelemetry_proto::tonic::{collector, common::v1::any_value::Value};
    use prost::Message;
    let batches = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let failures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let unavailable = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let received = batches.clone();
    let (down, rejected) = (unavailable.clone(), failures.clone());
    let collector = tokio::spawn(nanovm::server::serve_http1(listener, move |req| {
        let received = received.clone();
        let (down, rejected) = (down.clone(), rejected.clone());
        async move {
            let path = req.uri().path().to_string();
            let body = req.into_body().collect().await.unwrap().to_bytes();
            if down.load(Relaxed) {
                rejected.fetch_add(1, Relaxed);
                return nanovm::object::error_response(503, "collector-down", "");
            }
            received.lock().push((path, body));
            nanovm::server::ok_json(serde_json::json!({}))
        }
    }));
    let mut cfg = node_cfg("jailed_workers_export_otel_without_guest_network_access", 8);
    cfg.telemetry_endpoint = endpoint;
    let (_sup, addr) = supervised_with(cfg).await;
    let counter = publish("otel-counter", &app("counter")).await;
    let hostile = publish("otel-hostile", &app("hostile")).await;
    let head = format!("GET /room/ HTTP/1.1\r\nHost: x\r\n{}traceparent: 00-0123456789abcdef0123456789abcdef-0123456789abcdef-00\r\nConnection: close\r\n\r\n", names(&counter));
    assert_eq!(raw(addr, &head, b"").await.0, 200);
    let head = format!("GET /other/panic HTTP/1.1\r\nHost: x\r\n{}traceparent: 00-0123456789abcdef0123456789abcdef-0123456789abcdef-01\r\nConnection: close\r\n\r\n", names(&hostile));
    assert_eq!(raw(addr, &head, b"").await.0, 500);
    let (_, denied) = app_get(addr, &hostile, "/other/fetch?url=http://nanovm/telemetry/logs").await;
    assert!(denied.contains("HttpRequestDenied"), "{denied}");

    let attr = |attrs: &[opentelemetry_proto::tonic::common::v1::KeyValue], key: &str| {
        attrs.iter().find(|kv| kv.key == key).and_then(|kv| kv.value.as_ref()).and_then(|v| match &v.value {
            Some(Value::StringValue(s)) => Some(s.clone()), _ => None,
        }).unwrap_or_default()
    };
    let mut spans = Vec::new();
    let mut logs = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            for (path, body) in std::mem::take(&mut *batches.lock()) {
                match path.as_str() {
                    "/v1/traces" => spans.extend(collector::trace::v1::ExportTraceServiceRequest::decode(body).unwrap().resource_spans),
                    "/v1/logs" => logs.extend(collector::logs::v1::ExportLogsServiceRequest::decode(body).unwrap().resource_logs),
                    other => panic!("unexpected collector route {other}"),
                }
            }
            let complete = [&counter, &hostile].iter().all(|app| {
                let matches = |r: &Option<opentelemetry_proto::tonic::resource::v1::Resource>| {
                    r.as_ref().is_some_and(|r| attr(&r.attributes, "nanovm.app") == worker(app))
                };
                spans.iter().any(|r| matches(&r.resource) && r.scope_spans.iter().flat_map(|s| &s.spans).any(|s| s.name == "request"))
                    && logs.iter().any(|r| matches(&r.resource) && r.scope_logs.iter().flat_map(|s| &s.log_records)
                        .any(|l| attr(&l.attributes, "event.name") == "nanovm.snapshot"))
            });
            if complete && logs.iter().filter(|r| attr(&r.resource.as_ref().unwrap().attributes, "nanovm.app") == worker(&hostile))
                .flat_map(|r| &r.scope_logs).flat_map(|s| &s.log_records)
                .any(|l| attr(&l.attributes, "event.name") == "trap") { break }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("request and infrastructure events from real jailed workers");
    {
        for app in [&counter, &hostile] {
            let resource = spans.iter().find(|r| attr(&r.resource.as_ref().unwrap().attributes, "nanovm.app") == worker(app)).unwrap();
            assert_eq!(attr(&resource.resource.as_ref().unwrap().attributes, "service.version"), app.split_once('@').unwrap().1);
        }
        let counter_spans = spans.iter().filter(|r| attr(&r.resource.as_ref().unwrap().attributes, "nanovm.app") == worker(&counter))
            .flat_map(|r| &r.scope_spans).flat_map(|s| &s.spans).collect::<Vec<_>>();
        assert_eq!(counter_spans.len(), 1, "one rich event, not separate phase spans");
        let request = counter_spans.iter().find(|s| s.name == "request").unwrap();
        assert_eq!(request.trace_id, [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef].repeat(2));
        let hop = spans.iter().flat_map(|r| &r.scope_spans).flat_map(|s| &s.spans)
            .find(|s| s.name == "supervisor.request" && s.trace_id == request.trace_id
                && attr(&s.attributes, "nanovm.object") == "room").unwrap();
        assert_eq!(request.parent_span_id, hop.span_id);
        assert_eq!(hop.parent_span_id, [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
        assert_eq!(attr(&request.attributes, "nanovm.object"), "room");
        assert_eq!(attr(&request.attributes, "http.request.method"), "GET");
        assert_eq!(attr(&request.attributes, "url.path"), "/");
        assert!(request.attributes.iter().any(|a| a.key == "http.response.status_code"
            && a.value.as_ref().and_then(|v| v.value.as_ref()) == Some(&Value::IntValue(200))));
        for name in ["queue", "instantiate", "guest", "commit", "post_return", "post_return_commit"] {
            assert!(request.attributes.iter().any(|a| a.key == format!("nanovm.{name}.duration_ms")
                && matches!(a.value.as_ref().and_then(|v| v.value.as_ref()), Some(Value::DoubleValue(v)) if *v >= 0.0)), "{name}");
        }
    }
    let hostile_spans = spans.iter().filter(|r| attr(&r.resource.as_ref().unwrap().attributes, "nanovm.app") == worker(&hostile))
        .flat_map(|r| &r.scope_spans).flat_map(|s| &s.spans).collect::<Vec<_>>();
    let panic = hostile_spans.iter().find(|s| attr(&s.attributes, "url.path") == "/panic").unwrap();
    assert_eq!(panic.status.as_ref().unwrap().code, 2);
    assert_eq!(attr(&panic.attributes, "error.type"), "trap");
    let trap = logs.iter().flat_map(|r| &r.scope_logs).flat_map(|s| &s.log_records)
        .find(|l| attr(&l.attributes, "event.name") == "trap").unwrap();
    assert_eq!(trap.trace_id, panic.trace_id, "runtime diagnostics carry causal context too");
    assert_eq!(trap.span_id, panic.span_id);
    let restarted = hostile_spans.iter().find(|s| attr(&s.attributes, "url.path") == "/fetch").unwrap();
    assert_ne!(panic.trace_id, restarted.trace_id, "restart must use the next request's context");
    assert!(restarted.attributes.iter().any(|a| a.key == "nanovm.cold_start"
        && a.value.as_ref().and_then(|v| v.value.as_ref()) == Some(&Value::BoolValue(true))));
    assert!(logs.iter().filter(|r| attr(&r.resource.as_ref().unwrap().attributes, "nanovm.app") == worker(&hostile))
        .flat_map(|r| &r.scope_logs).flat_map(|s| &s.log_records).any(|l| {
            attr(&l.attributes, "nanovm.object") == "other" && attr(&l.attributes, "log.iostream") == "stderr"
                && l.trace_id == [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef].repeat(2)
                && l.span_id == panic.span_id
                && matches!(l.body.as_ref().and_then(|v| v.value.as_ref()), Some(Value::StringValue(s)) if s.contains("hostile guest panicked"))
        }));
    for (app, field, expected) in [(&counter, "nanovm.objects.phase.idle", 1),
        (&hostile, "nanovm.poisons.trap", 1), (&hostile, "nanovm.objects.limit", 8)] {
        assert!(logs.iter().filter(|r| attr(&r.resource.as_ref().unwrap().attributes, "nanovm.app") == worker(app))
            .flat_map(|r| &r.scope_logs).flat_map(|s| &s.log_records).any(|l|
                attr(&l.attributes, "event.name") == "nanovm.snapshot" && l.attributes.iter().any(|a|
                    a.key == field && a.value.as_ref().and_then(|v| v.value.as_ref()) == Some(&Value::IntValue(expected)))), "{field}");
    }
    // Concurrent requests queue on one caller. Each object's outbound call
    // must inherit that request, not another queued request's context.
    futures::future::join_all((1..=6).map(|i| {
        let app = &hostile;
        async move {
            let target = match i {
                4 => "callee4/panic".to_owned(),
                5 => "caller/".to_owned(),
                6 => "callee6/missing".to_owned(),
                _ => format!("callee{i}/sleep?ms=5"),
            };
            let head = format!("GET /caller/fetch?url=http://nanovm/objects/{target} HTTP/1.1\r\nHost: x\r\n{}traceparent: 00-{i:032x}-1111111111111111-01\r\nConnection: close\r\n\r\n", names(app));
            let (status, body) = raw(addr, &head, b"").await;
            assert_eq!(status, 200, "{body}");
            let expected = match i { 4 => 500, 5 => 508, 6 => 404, _ => 200 };
            assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["status"], expected);
        }
    })).await;
    let mut calls = Vec::new();
    let mut hops = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            _sup.flush_telemetry().await.unwrap();
            for (path, body) in std::mem::take(&mut *batches.lock()) {
                if path != "/v1/traces" { continue }
                for resource in collector::trace::v1::ExportTraceServiceRequest::decode(body).unwrap().resource_spans {
                    if attr(&resource.resource.as_ref().unwrap().attributes, "nanovm.app") == "_supervisor" {
                        hops.extend(resource.scope_spans.into_iter().flat_map(|s| s.spans));
                        continue;
                    }
                    assert_eq!(attr(&resource.resource.as_ref().unwrap().attributes, "nanovm.app"), worker(&hostile));
                    calls.extend(resource.scope_spans.into_iter().flat_map(|s| s.spans));
                }
            }
            if calls.len() >= 17 && hops.len() >= 6 { break }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("all queued calls exported");
    assert_eq!(calls.len(), 17);
    for i in 1..=6 {
        let mut trace = vec![0; 16];
        trace[15] = i;
        let group: Vec<_> = calls.iter().filter(|s| s.trace_id == trace).collect();
        assert_eq!(group.len(), if i == 5 { 2 } else { 3 }, "a rejected cycle has no callee request");
        let caller = group.iter().find(|s| s.name == "request" && attr(&s.attributes, "nanovm.object") == "caller").unwrap();
        let call = group.iter().find(|s| s.name == "call").unwrap();
        let hop = hops.iter().find(|s| s.trace_id == trace).unwrap();
        assert_eq!(caller.parent_span_id, hop.span_id);
        assert_eq!(hop.parent_span_id, [0x11; 8]);
        assert_eq!(call.parent_span_id, caller.span_id);
        assert_eq!(caller.kind, 2, "SERVER request");
        assert_eq!(call.kind, 3, "CLIENT call");
        let expected = match i { 4 => 500, 5 => 508, 6 => 404, _ => 200 };
        assert!(call.attributes.iter().any(|a| a.key == "http.response.status_code"
            && a.value.as_ref().and_then(|v| v.value.as_ref()) == Some(&Value::IntValue(expected))));
        assert_eq!(attr(&call.attributes, "nanovm.target_object"), if i == 5 { "caller".to_owned() } else { format!("callee{i}") });
        assert_eq!(attr(&call.attributes, "http.request.method"), "GET");
        let path = match i { 4 => "/objects/callee4/panic".to_owned(), 5 => "/objects/caller/".to_owned(), 6 => "/objects/callee6/missing".to_owned(), _ => format!("/objects/callee{i}/sleep") };
        assert_eq!(attr(&call.attributes, "url.path"), path, "query values are not exported");
        assert_eq!(call.status.as_ref().map_or(0, |s| s.code), if i >= 4 { 2 } else { 0 });
        assert_eq!(attr(&call.attributes, "error.type"), match i { 4 => "trap", 5 => "call-cycle", 6 => "404", _ => "" });
        assert_eq!(caller.status.as_ref().map_or(0, |s| s.code), 0, "the caller handled the dependency failure");
        if i != 5 {
            let callee = group.iter().find(|s| s.name == "request" && attr(&s.attributes, "nanovm.object") == format!("callee{i}")).unwrap();
            assert_eq!(callee.parent_span_id, call.span_id);
            assert_eq!(callee.kind, 2);
            assert_eq!(callee.status.as_ref().map_or(0, |s| s.code), if i == 4 { 2 } else { 0 }, "404 is not a server failure");
        }
    }
    // Refuse exports on existing connections too, and prove an export failed
    // before checking that requests still succeed.
    unavailable.store(true, Relaxed);
    tokio::time::timeout(Duration::from_secs(5), async {
        while failures.load(Relaxed) == 0 { tokio::time::sleep(Duration::from_millis(20)).await; }
    }).await.expect("collector must actually refuse an export");
    assert_eq!(tokio::time::timeout(Duration::from_secs(2), app_get(addr, &counter, "/room/")).await.unwrap().0, 200);
    collector.abort();
}

/// A request to an object names, in `nanovm-callers`, the objects waiting on
/// it, the caller last, and only nodes say so: a client's forged header never
/// reaches the guest, nor does one a guest puts on its own call (W-602). One
/// node, behind the proxy; `nodes::callers_are_named_by_nodes_only_across_
/// nodes` is the same across two.
#[tokio::test(flavor = "multi_thread")]
async fn callers_are_named_by_nodes_only() {
    let (_sup, node) = supervised("callers_named_by_nodes").await;
    let records = Arc::new(parking_lot::Mutex::default());
    let proxy = proxied(node, doh(records.clone()).await).await;
    let published = publish_wasm("callers-probe", &app("hostile"), probe()).await;
    let tagged = format!("{}:latest", published.split_once('@').unwrap().0);
    records.lock().insert("_nanovm.callers.test".to_string(), vec![format!("app={tagged}; trust=dev dev")]);
    let get = async |path: &str| {
        raw(proxy, &format!("GET {path} HTTP/1.1\r\nHost: callers.test\r\nnanovm-callers: forged\r\nConnection: close\r\n\r\n"), b"").await
    };
    assert_eq!(get("/x/callers").await, (200, String::new()), "a client names no caller");
    assert_eq!(get("/x/call/y").await, (200, "x".into()), "the node names the calling object");
}

// ---- many nodes --------------------------------------------------------------

mod nodes {
    use super::*;
    use object_store::{ObjectStore, ObjectStoreExt, memory::InMemory, path::Path as Key};
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

    type Records = Arc<parking_lot::Mutex<std::collections::HashMap<String, Vec<String>>>>;

    const LEASE_MS: u64 = 500;

    /// The lease store as one node sees it: the shared one, until the node
    /// is cut off from it.
    #[derive(Debug)]
    struct Cut(Arc<InMemory>, Arc<AtomicBool>);

    impl std::fmt::Display for Cut {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "Cut")
        }
    }

    impl Cut {
        fn check(&self) -> object_store::Result<()> {
            match self.1.load(SeqCst) {
                true => Err(object_store::Error::Generic { store: "cut", source: "cut off".into() }),
                false => Ok(()),
            }
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for Cut {
        async fn put_opts(&self, l: &Key, p: object_store::PutPayload, o: object_store::PutOptions) -> object_store::Result<object_store::PutResult> {
            self.check()?;
            self.0.put_opts(l, p, o).await
        }
        async fn put_multipart_opts(&self, l: &Key, o: object_store::PutMultipartOptions) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
            self.check()?;
            self.0.put_multipart_opts(l, o).await
        }
        async fn get_opts(&self, l: &Key, o: object_store::GetOptions) -> object_store::Result<object_store::GetResult> {
            self.check()?;
            self.0.get_opts(l, o).await
        }
        fn delete_stream(&self, l: futures::stream::BoxStream<'static, object_store::Result<Key>>) -> futures::stream::BoxStream<'static, object_store::Result<Key>> {
            self.0.delete_stream(l)
        }
        fn list(&self, p: Option<&Key>) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
            self.0.list(p)
        }
        async fn list_with_delimiter(&self, p: Option<&Key>) -> object_store::Result<object_store::ListResult> {
            self.0.list_with_delimiter(p).await
        }
        async fn copy_opts(&self, f: &Key, t: &Key, o: object_store::CopyOptions) -> object_store::Result<()> {
            self.0.copy_opts(f, t, o).await
        }
    }

    /// The node holding `object`'s lease, and its epoch.
    async fn holder(store: &InMemory, app: &str, object: &str) -> (String, u64) {
        let rec = store.get(&Key::from_iter(["objects", app, object])).await.unwrap().bytes().await.unwrap();
        let rec: serde_json::Value = serde_json::from_slice(&rec).unwrap();
        (rec["node"].as_str().unwrap().to_owned(), rec["epoch"].as_u64().unwrap())
    }

    /// Listeners for `n` nodes, and their names.
    fn listeners(n: usize) -> (Vec<std::net::TcpListener>, Vec<String>) {
        let ls: Vec<_> = (0..n).map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap()).collect();
        let names = ls.iter().map(|l| l.local_addr().unwrap().to_string()).collect();
        (ls, names)
    }

    /// A node's config: named for `listener`, taking leases, calling through
    /// the proxy's private endpoint `private`, shipping to a store the nodes
    /// of a test share (`<test>_a` and `<test>_b` share `<test>`'s) and
    /// answering writes once shipped, as a config file's default is.
    fn leased(test: &str, listener: &std::net::TcpListener, private: std::net::SocketAddr) -> NodeConfig {
        let mut cfg = node_cfg(test, 8);
        (cfg.name, cfg.leases, cfg.lease_ms) = (listener.local_addr().unwrap().to_string(), "memory:".into(), LEASE_MS);
        cfg.proxy = format!("http://{private}");
        cfg.limits.post_return_timeout_ms = 20_000;
        let shared = test.strip_suffix("_a").or(test.strip_suffix("_b")).unwrap_or(test);
        let store = Path::new(env!("CARGO_TARGET_TMPDIR")).join("nodes-store").join(shared);
        let _ = std::fs::remove_dir_all(&store);
        cfg.capture = Some(nanovm::config::CaptureConfig { store: Some(store.to_str().unwrap().into()), remote_ack: true, ..Default::default() });
        cfg.validate().unwrap();
        cfg
    }

    async fn serve(cfg: NodeConfig, listener: std::net::TcpListener, leases: Arc<dyn ObjectStore>) -> Arc<nanovm::supervisor::Supervisor> {
        listener.set_nonblocking(true).unwrap();
        supervised_at(cfg, tokio::net::TcpListener::from_std(listener).unwrap(), Some(leases)).await
    }

    /// A dev-mode proxy placing objects on `nodes`, with its private
    /// endpoint on `private`; and the name of the app `host` serves, its
    /// record naming `published`.
    async fn proxy(nodes: &[String], private: tokio::net::TcpListener, host: &str, published: &str) -> (std::net::SocketAddr, String) {
        let records: Records = Arc::default();
        let tagged = format!("{}:latest", published.split_once('@').unwrap().0);
        records.lock().insert(format!("_nanovm.{host}"), vec![format!("app={tagged}; trust=dev dev")]);
        let cfg = nanovm::config::ProxyConfig {
            nodes: nodes.to_vec(), resolver: doh(records).await, tag_ttl_s: 0, dev: true, listen: "127.0.0.1:0".into(),
            failover_ms: 20_000, hosts: vec![host.into()], signers: vec!["dev dev".into()], ..Default::default()
        };
        let proxy = nanovm::proxy::Proxy::new(cfg).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(nanovm::proxy::serve(proxy.clone(), listener));
        tokio::spawn(nanovm::proxy::serve_publish(proxy, private));
        (addr, nanovm::registry::app_name(host, &tagged.parse().unwrap()))
    }

    /// An object of `app` whose home is `node`.
    fn homed(nodes: &[String], app: &str, node: &str) -> String {
        (0..).map(|i| format!("o{i}")).find(|o| nanovm::proxy::rank(nodes, app, o)[0] == node).unwrap()
    }

    async fn get(proxy: std::net::SocketAddr, host: &str, path: &str) -> (u16, String) {
        raw(proxy, &format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"), b"").await
    }

    /// Node A serves an object; A stops; node B takes the object's lease, at
    /// the next epoch, once A's has run out, and serves the object, with what
    /// A committed. The nodes share a data root, standing in for W-9's
    /// stream of committed versions.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_object_moves_to_another_node_when_its_node_stops() {
        let store = Arc::new(InMemory::new());
        let private = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (mut ls, names) = listeners(2);
        let (lb, la) = (ls.pop().unwrap(), ls.pop().unwrap());
        let cfg_a = leased("an_object_moves", &la, private.local_addr().unwrap());
        let cfg_b = NodeConfig { name: names[1].clone(), ..cfg_a.clone() };
        // A runs on a runtime of its own, so that stopping it stops all of
        // it: its server, its session's renewals and its workers.
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let leases: Arc<dyn ObjectStore> = store.clone();
        let a = std::thread::spawn(move || {
            tokio::runtime::Runtime::new().unwrap().block_on(async move {
                let _a = serve(cfg_a, la, leases).await;
                let _ = stopped.await;
            })
        });
        let _b = serve(cfg_b, lb, store.clone()).await;
        let published = publish("nodes-moves", &app("hostile")).await;
        let (proxy, app) = proxy(&names, private, "moves.test", &published).await;
        let object = homed(&names, &app, &names[0]);

        let written = get(proxy, "moves.test", &format!("/{object}/commit?v=one")).await;
        assert_eq!(written, (200, r#"{"file":"one","key":"one"}"#.into()));
        assert_eq!(holder(&store, &app, &object).await, (names[0].clone(), 1));
        stop.send(()).unwrap();
        a.join().unwrap();
        let t0 = Instant::now();
        let read = loop {
            // B refuses until A's worker has gone from the shared root.
            let read = get(proxy, "moves.test", &format!("/{object}/commit")).await;
            if read.0 == 200 || t0.elapsed() > Duration::from_secs(15) {
                break read;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert_eq!(read, (200, r#"{"file":"one","key":"one"}"#.into()));
        assert_eq!(holder(&store, &app, &object).await, (names[1].clone(), 2));
        assert!(t0.elapsed() >= Duration::from_millis(LEASE_MS), "taken over before A's lease ran out: {:?}", t0.elapsed());
    }

    /// Nodes on disks of their own, sharing only the stores: A answers a
    /// write once its version counts in the store; A stops; B takes the
    /// object over and serves what A wrote, restored from the store, never
    /// the empty object its own disk has.
    #[tokio::test(flavor = "multi_thread")]
    async fn review_failover_with_separate_disks_preserves_remote_acknowledged_state() {
        let store = Arc::new(InMemory::new());
        let shipped = Path::new(env!("CARGO_TARGET_TMPDIR")).join("nodes-store/separate_disks");
        let _ = std::fs::remove_dir_all(&shipped);
        let private = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (mut ls, names) = listeners(2);
        let (lb, la) = (ls.pop().unwrap(), ls.pop().unwrap());
        let capture = nanovm::config::CaptureConfig { store: Some(shipped.to_str().unwrap().into()), remote_ack: true, ..Default::default() };
        let mut cfg_a = leased("separate_disks_a", &la, private.local_addr().unwrap());
        let mut cfg_b = leased("separate_disks_b", &lb, private.local_addr().unwrap());
        (cfg_a.capture, cfg_b.capture) = (Some(capture.clone()), Some(capture));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let leases: Arc<dyn ObjectStore> = store.clone();
        let a = std::thread::spawn(move || {
            tokio::runtime::Runtime::new().unwrap().block_on(async move {
                let _a = serve(cfg_a, la, leases).await;
                let _ = stopped.await;
            })
        });
        let _b = serve(cfg_b, lb, store.clone()).await;
        let published = publish("nodes-separate-disks", &app("hostile")).await;
        let (proxy, app) = proxy(&names, private, "disks.test", &published).await;
        let object = homed(&names, &app, &names[0]);
        let written = get(proxy, "disks.test", &format!("/{object}/commit?v=one")).await;
        assert_eq!(written, (200, r#"{"file":"one","key":"one"}"#.into()));
        let head = || std::fs::read(shipped.join(&app).join(&object).join("head")).ok().map(|h| serde_json::from_slice::<serde_json::Value>(&h).unwrap());
        assert_eq!(head().unwrap()["sequence"], 1, "answered once it counts in the store");
        stop.send(()).unwrap();
        a.join().unwrap();
        let t0 = Instant::now();
        let read = loop {
            let read = get(proxy, "disks.test", &format!("/{object}/commit")).await;
            if read.0 == 200 || t0.elapsed() > Duration::from_secs(15) {
                break read;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert_eq!(read, (200, r#"{"file":"one","key":"one"}"#.into()), "B serves the object from its own disk");
        assert_eq!(holder(&store, &app, &object).await, (names[1].clone(), 2));
        // B's history goes on from A's.
        assert_eq!(get(proxy, "disks.test", &format!("/{object}/commit?v=two")).await.0, 200);
        let head = head().unwrap();
        assert_eq!((head["sequence"].as_u64(), head["node"].as_str()), (Some(2), Some(names[1].as_str())));
    }

    /// Acknowledging remotely, a write whose version doesn't count in the
    /// store within the commit's wait is answered 500, though it stands on
    /// its node. The node then serves the object nothing (503, Retry-After)
    /// until its shipper has caught up: here it stops first, and the node
    /// taking the object over serves what it last served, never older
    /// (before: A served the write it had answered 500 for, then B the
    /// state before it).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_answered_500_is_served_only_once_it_counts() {
        let store = Arc::new(InMemory::new());
        let shipped = Path::new(env!("CARGO_TARGET_TMPDIR")).join("nodes-store/unacknowledged");
        let (_, _) = (std::fs::remove_file(&shipped), std::fs::remove_dir_all(shipped.with_extension("away")));
        let private = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (mut ls, names) = listeners(2);
        let (lb, la) = (ls.pop().unwrap(), ls.pop().unwrap());
        // Data roots of their own, one store to ship to (see `leased`).
        let cfg_a = leased("unacknowledged_a", &la, private.local_addr().unwrap());
        let cfg_b = leased("unacknowledged_b", &lb, private.local_addr().unwrap());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let leases: Arc<dyn ObjectStore> = store.clone();
        let a = std::thread::spawn(move || {
            tokio::runtime::Runtime::new().unwrap().block_on(async move {
                let _a = serve(cfg_a, la, leases).await;
                let _ = stopped.await;
            })
        });
        let _b = serve(cfg_b, lb, store.clone()).await;
        let published = publish("nodes-unacknowledged", &app("hostile")).await;
        let (proxy, app) = proxy(&names, private, "unacked.test", &published).await;
        let object = homed(&names, &app, &names[0]);
        let one = (200, r#"{"file":"one","key":"one"}"#.to_string());
        assert_eq!(get(proxy, "unacked.test", &format!("/{object}/commit?v=one")).await, one);
        // The store goes away (a file where its directory was).
        let away = shipped.with_extension("away");
        std::fs::rename(&shipped, &away).unwrap();
        std::fs::write(&shipped, "").unwrap();
        // Answered 500 once the wait for the store (2 s) is over, whatever
        // the app's deadlines (B-507); the commit stands. A read queued
        // behind it finds the object's instance reset, and its next one
        // waits for the store, in vain. (Once the object's actor has
        // retired, a request needs a new claim, which the store being down
        // refuses too: A serves nothing either way.)
        let path = format!("/{object}/commit?v=two");
        let write = tokio::spawn(async move { get(proxy, "unacked.test", &path).await });
        tokio::time::sleep(Duration::from_millis(500)).await;
        let read = {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut s = tokio::net::TcpStream::connect(proxy).await.unwrap();
            let head = format!("GET /{object}/commit HTTP/1.1\r\nHost: unacked.test\r\nConnection: close\r\n\r\n");
            s.write_all(head.as_bytes()).await.unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).await.unwrap();
            out.to_lowercase()
        };
        let written = write.await.unwrap();
        assert_eq!(written.0, 500, "{written:?}");
        assert!(read.starts_with("http/1.1 503") && read.contains("retry-after: 1") && read.contains("not-stored"), "A serves what it answered 500 for: {read}");
        // A stops, then the store is back; B takes the object over.
        stop.send(()).unwrap();
        a.join().unwrap();
        std::fs::remove_file(&shipped).unwrap();
        std::fs::rename(&away, &shipped).unwrap();
        let t0 = Instant::now();
        let read = loop {
            let read = get(proxy, "unacked.test", &format!("/{object}/commit")).await;
            if read.0 == 200 || t0.elapsed() > Duration::from_secs(15) {
                break read;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert_eq!(read, one, "B serves what A last served");
        assert_eq!(holder(&store, &app, &object).await, (names[1].clone(), 2));
    }

    /// With leases, an object is restored on the node holding it: the restore
    /// takes its lease and claims its head, going back to an older version as
    /// the version after the head. The other node refuses to restore it while
    /// it is held. The proxy then serves the restored object, and its history
    /// goes on from the restore.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_object_is_restored_on_the_node_holding_it() {
        let store = Arc::new(InMemory::new());
        let private = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (mut ls, names) = listeners(2);
        let (lb, la) = (ls.pop().unwrap(), ls.pop().unwrap());
        let cfg_a = leased("restore_leased_a", &la, private.local_addr().unwrap());
        let cfg_b = leased("restore_leased_b", &lb, private.local_addr().unwrap());
        let shipped = PathBuf::from(cfg_a.capture.as_ref().unwrap().store.clone().unwrap());
        let a = serve(cfg_a, la, store.clone()).await;
        let b = serve(cfg_b, lb, store.clone()).await;
        // A write waits for its version to count in the store (remote ack)
        // in its post-return phase: hostile's 500 ms is short for a busy disk.
        let mut hostile = app("hostile");
        hostile.limits.post_return_timeout_ms = 5_000;
        let published = publish("nodes-restore", &hostile).await;
        let (proxy, app) = proxy(&names, private, "restore.test", &published).await;
        let object = homed(&names, &app, &names[0]);
        let head = || serde_json::from_slice::<nanovm::ship::Head>(&std::fs::read(shipped.join(&app).join(&object).join("head")).unwrap()).unwrap();
        for v in ["one", "two", "three"] {
            let written = get(proxy, "restore.test", &format!("/{object}/commit?v={v}")).await;
            assert_eq!(written.0, 200, "{written:?}");
        }
        let before = head();
        assert_eq!(before.node, names[0]);
        let args = |a: &[&str]| a.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let e = b.command(&args(&["restore", &app, &object, "1"])).await.unwrap_err();
        assert!(format!("{e:#}").contains(&format!("Held(\"{}\")", names[0])), "{e:#}");
        let done = a.command(&args(&["restore", &app, &object, "1"])).await.unwrap();
        assert_eq!((&done[0]["version"], &done[0]["head"]), (&1.into(), &(before.sequence + 1).into()), "{done}");
        assert_eq!(head().sequence, before.sequence + 1, "shipped under the restore's lease");
        assert_eq!(get(proxy, "restore.test", &format!("/{object}/commit")).await, (200, r#"{"file":"one","key":"one"}"#.into()));
        let written = get(proxy, "restore.test", &format!("/{object}/commit?v=four")).await;
        assert_eq!(written.0, 200, "{written:?}");
        let after = head();
        assert_eq!((after.sequence, after.node.as_str()), (before.sequence + 2, names[0].as_str()));
        assert!(after.lease > before.lease, "taken again after the restore: {after:?}");
    }

    /// Node A answers a request and works on after answering; meanwhile it
    /// is cut off from the lease store. Its lease runs out, so it refuses the
    /// object's requests, and node B takes the lease and serves them. A's
    /// late commit of its work after answering is refused: it stays
    /// uncommitted, to be rolled back.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_node_that_lost_an_objects_lease_cannot_commit_it() {
        let wasm = probe();
        let gate = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = gate.local_addr().unwrap();
        let (begin, finish) = (Arc::new(tokio::sync::Semaphore::new(0)), Arc::new(tokio::sync::Semaphore::new(0)));
        let (arrived, mut arrivals) = tokio::sync::mpsc::channel(4);
        let (b, f) = (begin.clone(), finish.clone());
        tokio::spawn(nanovm::server::serve_http1(gate, move |req| {
            let gate = if req.uri().path() == "/begin" { b.clone() } else { f.clone() };
            let arrived = arrived.clone();
            async move {
                arrived.send(req.uri().path().to_owned()).await.unwrap();
                gate.acquire().await.unwrap().forget();
                nanovm::server::ok_json(serde_json::json!({}))
            }
        }));
        let store = Arc::new(InMemory::new());
        let cut = Arc::new(AtomicBool::new(false));
        let private = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (mut ls, names) = listeners(2);
        let (lb, la) = (ls.pop().unwrap(), ls.pop().unwrap());
        let cfg_a = leased("lost_lease_a", &la, private.local_addr().unwrap());
        let cfg_b = leased("lost_lease_b", &lb, private.local_addr().unwrap());
        let objects_a = cfg_a.data_dir.join("objects");
        let a = serve(cfg_a, la, Arc::new(Cut(store.clone(), cut.clone()))).await;
        let _b = serve(cfg_b, lb, store.clone()).await;
        let mut probe = app("hostile");
        probe.limits.egress = vec![address.to_string()];
        (probe.limits.post_return_timeout_ms, probe.limits.cpu_ms) = (20_000, 1000);
        let published = publish_wasm("nodes-lost-lease", &probe, wasm).await;
        let (proxy, app) = proxy(&names, private, "probe.test", &published).await;
        let object = homed(&names, &app, &names[0]);

        let path = format!("/{object}/?gate={address}");
        let first = tokio::spawn(async move { get(proxy, "probe.test", &path).await });
        assert_eq!(tokio::time::timeout(Duration::from_secs(10), arrivals.recv()).await.unwrap().unwrap(), "/begin");
        assert_eq!(holder(&store, &app, &object).await, (names[0].clone(), 1));
        // Another of A's objects, idle; and a request queued behind A's work.
        let idle = (0..).map(|i| format!("idle{i}")).find(|o| nanovm::proxy::rank(&names, &app, o)[0] == names[0]).unwrap();
        assert_eq!(get(proxy, "probe.test", &format!("/{idle}/callee")).await, (200, "callee".into()));
        let queued = format!("/{object}/callee");
        let queued = tokio::spawn(async move { get(proxy, "probe.test", &queued).await });
        cut.store(true, SeqCst);
        // Past the lease A last renewed, A refuses the object, and evicts
        // the idle one.
        tokio::time::sleep(Duration::from_millis(LEASE_MS)).await;
        let a_addr = names[0].parse().unwrap();
        let t0 = Instant::now();
        while stats_json(a_addr).await["apps"][&app]["live_objects"] != 1 {
            assert!(t0.elapsed() < Duration::from_secs(5), "{}", stats_json(a_addr).await);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let t0 = Instant::now();
        loop {
            match get(proxy, "probe.test", &format!("/{object}/callee")).await {
                (200, body) => break assert_eq!(body, "callee"),
                other => assert!(t0.elapsed() < Duration::from_secs(10), "{other:?}"),
            }
        }
        assert_eq!(holder(&store, &app, &object).await, (names[1].clone(), 2));
        assert!(object_dir(&objects_a.join(&app), &object).join("data/before.txt").exists(), "A committed what it wrote before answering");

        // A's work after answering calls another object (through the proxy,
        // to B, since A can take no lease) and writes; its commit is refused.
        (begin.add_permits(1), finish.add_permits(1));
        assert_eq!(tokio::time::timeout(Duration::from_secs(10), arrivals.recv()).await.unwrap().unwrap(), "/finish");
        let (status, body) = first.await.unwrap();
        assert!(status == 200 && body.contains("finished"), "{status} {body}");
        // The request that waited on A is not run there, but on B.
        assert_eq!(queued.await.unwrap(), (200, "callee".into()));
        let t0 = Instant::now();
        let refused = loop {
            a.flush_telemetry().await.unwrap();
            let logs = captures().lock()[&names[0].parse().unwrap()].1.lock().logs.clone();
            if let Some(e) = logs.iter().find(|e| e["kind"] == "post-return-commit-failed") {
                break e.clone();
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "no refused commit in {logs:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert!(refused["detail"].as_str().unwrap().contains("no longer holds the object's lease"), "{refused}");
        assert!(object_dir(&objects_a.join(&app), &object).join("data/after.txt").exists(), "A wrote after answering");
        assert!(object_dir(&objects_a.join(&app), &object).join("journal/log").exists(), "the write is uncommitted");
    }

    /// A guest answering as a node that doesn't hold its object (421, naming
    /// another) is answered as it is: were it taken at its word, the proxy
    /// would run the request again on every node until `failover_ms`.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_guest_cannot_pose_as_a_node_refusing_its_object() {
        let store = Arc::new(InMemory::new());
        let private = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (mut ls, names) = listeners(2);
        let (lb, la) = (ls.pop().unwrap(), ls.pop().unwrap());
        let _a = serve(leased("forged_refusal_a", &la, private.local_addr().unwrap()), la, store.clone()).await;
        let _b = serve(leased("forged_refusal_b", &lb, private.local_addr().unwrap()), lb, store.clone()).await;
        let mut probe_app = app("hostile");
        probe_app.limits.idle_timeout_ms = 200;
        let published = publish_wasm("nodes-forged-refusal", &probe_app, probe()).await;
        let (proxy, app) = proxy(&names, private, "forged.test", &published).await;
        let object = homed(&names, &app, &names[0]);
        let t0 = Instant::now();
        let (status, body) = get(proxy, "forged.test", &format!("/{object}/misdirect/{}", names[1])).await;
        assert_eq!((status, body.as_str()), (421, "forged"));
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
        assert_eq!(holder(&store, &app, &object).await, (names[0].clone(), 1), "run once, on its home");
        // Once idle, its node gives up its lease.
        let t0 = Instant::now();
        while holder(&store, &app, &object).await != (String::new(), 1) {
            assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", holder(&store, &app, &object).await);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A WebSocket's events reach its object on the object's node, which
    /// alone holds its lease.
    #[tokio::test(flavor = "multi_thread")]
    async fn socket_events_reach_their_objects_node() {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
        let store = Arc::new(InMemory::new());
        let private = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (mut ls, names) = listeners(2);
        let (lb, la) = (ls.pop().unwrap(), ls.pop().unwrap());
        let _a = serve(leased("socket_events_a", &la, private.local_addr().unwrap()), la, store.clone()).await;
        let _b = serve(leased("socket_events_b", &lb, private.local_addr().unwrap()), lb, store.clone()).await;
        let published = publish("nodes-sockets", &app("counter")).await;
        let (proxy, app) = proxy(&names, private, "sockets.test", &published).await;
        let object = homed(&names, &app, &names[1]);
        let mut req = format!("ws://{proxy}/{object}/ws").into_client_request().unwrap();
        req.headers_mut().insert("host", "sockets.test".parse().unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        ws.send(Message::text("hello")).await.unwrap();
        let echo = async {
            loop {
                match ws.next().await.unwrap().unwrap() {
                    Message::Ping(_) => continue,
                    m => return m,
                }
            }
        };
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), echo).await.unwrap(), Message::text("echo hello"));
        assert_eq!(holder(&store, &app, &object).await, (names[1].clone(), 1));
    }

    /// As `callers_are_named_by_nodes_only`, with the caller and the callee
    /// on two nodes: the call crosses the proxy's private route, which keeps
    /// only the header the calling node set.
    #[tokio::test(flavor = "multi_thread")]
    async fn callers_are_named_by_nodes_only_across_nodes() {
        let store = Arc::new(InMemory::new());
        let private = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (mut ls, names) = listeners(2);
        let (lb, la) = (ls.pop().unwrap(), ls.pop().unwrap());
        let _a = serve(leased("callers_across_a", &la, private.local_addr().unwrap()), la, store.clone()).await;
        let _b = serve(leased("callers_across_b", &lb, private.local_addr().unwrap()), lb, store.clone()).await;
        let published = publish_wasm("nodes-callers", &app("hostile"), probe()).await;
        let (proxy, app) = proxy(&names, private, "callers.test", &published).await;
        let (x, y) = (homed(&names, &app, &names[0]), homed(&names, &app, &names[1]));
        let get = async |path: &str| {
            raw(proxy, &format!("GET {path} HTTP/1.1\r\nHost: callers.test\r\nnanovm-callers: forged\r\nConnection: close\r\n\r\n"), b"").await
        };
        assert_eq!(get(&format!("/{y}/callers")).await, (200, String::new()), "a client names no caller");
        assert_eq!(get(&format!("/{x}/call/{y}")).await, (200, x.clone()), "the calling node names the caller");
        assert_eq!(holder(&store, &app, &y).await.0, names[1], "the call crossed to y's node");
    }

    /// A call to an object's root with a query and no slash after its id
    /// (`http://nanovm/objects/{id}?…`) reaches the object on another node,
    /// as it does on one node.
    #[tokio::test(flavor = "multi_thread")]
    async fn review_cross_node_root_calls_accept_a_query_without_a_trailing_slash() {
        let store = Arc::new(InMemory::new());
        let private = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (mut ls, names) = listeners(2);
        let (lb, la) = (ls.pop().unwrap(), ls.pop().unwrap());
        let _a = serve(leased("root_calls_a", &la, private.local_addr().unwrap()), la, store.clone()).await;
        let _b = serve(leased("root_calls_b", &lb, private.local_addr().unwrap()), lb, store.clone()).await;
        let published = publish("nodes-root-calls", &app("hostile")).await;
        let (proxy, app) = proxy(&names, private, "root.test", &published).await;
        let (x, y) = (homed(&names, &app, &names[0]), homed(&names, &app, &names[1]));
        let url = format!("http://nanovm/objects/{y}?v=1").replace('?', "%3F").replace('=', "%3D");
        let (status, body) = get(proxy, "root.test", &format!("/{x}/fetch?url={url}")).await;
        assert_eq!(status, 200, "{body}");
        let r: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(r["status"], 200, "{r}");
        assert!(r["body"].as_str().unwrap().contains(&format!(r#""object":"{y}""#)), "{r}");
        assert_eq!(holder(&store, &app, &y).await.0, names[1], "the call crossed to y's node");
    }

    /// An object calls an object on another node, through the proxy, which
    /// commits there; a call back to the caller is refused at once (508),
    /// though the wait it would close is on another node.
    #[tokio::test(flavor = "multi_thread")]
    async fn calls_reach_objects_on_other_nodes_and_cycles_across_nodes_are_refused() {
        let store = Arc::new(InMemory::new());
        let private = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (mut ls, names) = listeners(2);
        let (lb, la) = (ls.pop().unwrap(), ls.pop().unwrap());
        let cfg_a = leased("calls_across_a", &la, private.local_addr().unwrap());
        let cfg_b = leased("calls_across_b", &lb, private.local_addr().unwrap());
        let objects_b = cfg_b.data_dir.join("objects");
        let _a = serve(cfg_a, la, store.clone()).await;
        let _b = serve(cfg_b, lb, store.clone()).await;
        let published = publish("nodes-calls", &app("hostile")).await;
        let (proxy, app) = proxy(&names, private, "calls.test", &published).await;
        let (x, y) = (homed(&names, &app, &names[0]), homed(&names, &app, &names[1]));
        let enc = |s: &str| s.replace('%', "%25").replace('&', "%26").replace('?', "%3F").replace('=', "%3D");
        let call = async |from: &str, url: &str| {
            let (status, body) = get(proxy, "calls.test", &format!("/{from}/fetch?url={}", enc(url))).await;
            assert_eq!(status, 200, "{body}");
            serde_json::from_str::<serde_json::Value>(&body).unwrap()
        };

        let r = call(&x, &format!("http://nanovm/objects/{y}/commit?v=via-x")).await;
        assert_eq!((r["status"].as_u64(), r["body"].as_str()), (Some(200), Some(r#"{"file":"via-x","key":"via-x"}"#)), "{r}");
        assert_eq!(std::fs::read_to_string(object_dir(&objects_b.join(&app), &y).join("data/c.txt")).unwrap(), "via-x");
        assert_eq!(holder(&store, &app, &y).await.0, names[1]);

        let t0 = Instant::now();
        let back = format!("http://nanovm/objects/{y}/fetch?url={}", enc(&format!("http://nanovm/objects/{x}/")));
        let r = call(&x, &back).await;
        assert!(r["body"].as_str().unwrap().contains(r#""status":508"#), "{r}");
        assert!(t0.elapsed() < Duration::from_secs(1), "refused at once: {:?}", t0.elapsed());

        // B renews its leases as long as it lives: y keeps its epoch.
        tokio::time::sleep(Duration::from_millis(3 * LEASE_MS)).await;
        assert_eq!(call(&x, &format!("http://nanovm/objects/{y}/commit")).await["status"], 200);
        assert_eq!(holder(&store, &app, &y).await, (names[1].clone(), 1));
    }
}
