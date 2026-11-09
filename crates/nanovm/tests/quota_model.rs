//! Model-based test of the disk quota and the journal (D-46): random
//! sequences of writes, truncations, renames, unlinks, mkdirs and rmdirs on
//! an object's `/data`, in requests that commit or roll back, against a
//! model of the volume. After every request the volume on disk must be the
//! model's committed state, and (probed through the guest) exactly as many
//! bytes and entries must still fit as the model says: the quota's usage
//! always equals the model's.
//!
//! The test component is `crates/nanovm/tests/fsops` (outside guests/**).
//! Notes, coverage and the mutations it kills: `notes/quality.md` (D-46).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use nanovm::config::{AppConfig, NodeConfig};
use nanovm::table::Node;
use proptest::prelude::*;

#[path = "../support/load.rs"]
#[allow(dead_code)]
mod load;

const MAX_BYTES: u64 = 1 << 20;
const MAX_FILES: u64 = 4;
/// How close the request's undo log may come to its budget (the quota)
/// before the model can't tell where the journal refuses a write, as a full
/// disk: that depends on how the write's stream is chunked.
const LOG_SLACK: u64 = 64 << 10;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn guest() -> Vec<u8> {
    let build = std::process::Command::new("cargo").args([
        "build", "--locked", "--release", "--target", "wasm32-wasip2", "--manifest-path",
        "crates/nanovm/tests/fsops/Cargo.toml", "--target-dir", "target/test-guest",
    ]).current_dir(repo()).status().unwrap();
    assert!(build.success());
    std::fs::read(repo().join("target/test-guest/wasm32-wasip2/release/fsops_probe.wasm")).unwrap()
}

#[derive(Clone, Debug)]
enum Op {
    Write(String, u64, u64, u8),
    Truncate(String, u64),
    Rename(String, String),
    Unlink(String),
    Mkdir(String),
    Rmdir(String),
}

impl Op {
    fn line(&self) -> String {
        match self {
            Op::Write(p, off, len, b) => format!("write {p} {off} {len} {b}"),
            Op::Truncate(p, n) => format!("truncate {p} {n}"),
            Op::Rename(a, b) => format!("rename {a} {b}"),
            Op::Unlink(p) => format!("unlink {p}"),
            Op::Mkdir(p) => format!("mkdir {p}"),
            Op::Rmdir(p) => format!("rmdir {p}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Entry {
    Dir,
    /// An identity (kept by renames, for the journal's per-file saves) and
    /// the contents.
    File(u64, Vec<u8>),
}

/// The volume: paths (`a`, `d/a`) to entries.
#[derive(Clone, Debug, Default)]
struct Model {
    tree: BTreeMap<String, Entry>,
    next_id: u64,
}

/// What the request's journal has saved so far, to tell a full undo log
/// (refused as a full disk) from a quota bug.
#[derive(Default)]
struct Log {
    /// Files present when the request began: their size then, and the byte
    /// ranges of it saved.
    original: HashMap<u64, (u64, Vec<(u64, u64)>)>,
    /// Old bytes saved in the undo log.
    bytes: u64,
    /// Bytes of removed files the journal holds (their own budget, also the
    /// quota): a file present when the request began, until it ends.
    removed: u64,
}

impl Log {
    fn begin(m: &Model) -> Log {
        let original = m.tree.values().filter_map(|e| match e {
            Entry::File(id, c) => Some((*id, (c.len() as u64, vec![]))),
            Entry::Dir => None,
        }).collect();
        Log { original, bytes: 0, removed: 0 }
    }

    /// Saves what of `[at, end)` of file `id` was there when the request
    /// began and isn't saved yet; the bytes this adds to the log.
    fn save(&mut self, id: u64, at: u64, end: u64) -> u64 {
        let Some((len, ranges)) = self.original.get_mut(&id) else { return 0 };
        let end = end.min(*len);
        let mut added = 0;
        let mut from = at;
        let mut covered: Vec<_> = ranges.iter().copied().filter(|r| r.1 > at && r.0 < end).collect();
        covered.sort();
        for (s, e) in covered {
            if s > from { added += s - from; }
            from = from.max(e);
        }
        if from < end { added += end - from; }
        if end > at { ranges.push((at, end)); }
        self.bytes += added;
        added
    }

    fn near_full(&self) -> bool {
        self.bytes + LOG_SLACK >= MAX_BYTES
    }

    /// Removes file `id` of `len` bytes into the journal, if its budget has
    /// room; a file created in this request is held only while removing.
    fn remove(&mut self, id: u64, len: u64) -> bool {
        if self.removed + len > MAX_BYTES {
            return false;
        }
        if self.original.contains_key(&id) {
            self.removed += len;
        }
        true
    }
}

fn parent(p: &str) -> &str {
    p.rsplit_once('/').map_or("", |(d, _)| d)
}

impl Model {
    fn usage(&self) -> (u64, u64) {
        let bytes = self.tree.values().map(|e| match e { Entry::File(_, c) => c.len() as u64, Entry::Dir => 0 }).sum();
        (bytes, self.tree.len() as u64)
    }

    fn dir_ok(&self, p: &str) -> bool {
        p.is_empty() || self.tree.get(p) == Some(&Entry::Dir)
    }

    fn children(&self, p: &str) -> Vec<String> {
        let prefix = format!("{p}/");
        self.tree.keys().filter(|k| k.starts_with(&prefix)).cloned().collect()
    }

    /// Applies `op` as the node should, returning the result the guest
    /// should see (`ok`, `insufficient-space`, `quota` or `error`).
    fn apply(&mut self, op: &Op, log: &mut Log) -> &'static str {
        let (used, files) = self.usage();
        let room = MAX_BYTES.saturating_sub(used);
        match op {
            Op::Write(p, off, len, byte) => {
                match self.tree.get(p) {
                    Some(Entry::Dir) => return "error",
                    Some(Entry::File(..)) => {}
                    None if files >= MAX_FILES => return "quota",
                    None if !self.dir_ok(parent(p)) => return "error",
                    None => {
                        self.next_id += 1;
                        self.tree.insert(p.clone(), Entry::File(self.next_id, vec![]));
                    }
                }
                if *len == 0 {
                    return "ok";
                }
                let Some(Entry::File(id, c)) = self.tree.get_mut(p) else { unreachable!() };
                let before = c.len() as u64;
                let end = off + len;
                let allowed = end.min(before + room);
                if allowed <= *off {
                    return "insufficient-space";
                }
                log.save(*id, *off, allowed);
                if c.len() < allowed as usize {
                    c.resize(allowed as usize, 0);
                }
                c[*off as usize..allowed as usize].fill(*byte);
                if allowed < end { "insufficient-space" } else { "ok" }
            }
            Op::Truncate(p, size) => {
                let Some(Entry::File(id, c)) = self.tree.get_mut(p) else { return "error" };
                let before = c.len() as u64;
                if *size > before + room {
                    return "insufficient-space";
                }
                log.save(*id, *size, before);
                c.resize(*size as usize, 0);
                "ok"
            }
            Op::Rename(a, b) => {
                if a == b {
                    return "ok";
                }
                let Some(src) = self.tree.get(a).cloned() else { return "error" };
                match (&src, self.tree.get(b)) {
                    (Entry::Dir, Some(Entry::File(..))) | (Entry::File(..), Some(Entry::Dir)) => return "error",
                    (Entry::Dir, Some(Entry::Dir)) if !self.children(b).is_empty() => return "error",
                    (_, None) if !self.dir_ok(parent(b)) => return "error",
                    _ => {}
                }
                if matches!(src, Entry::Dir) && b.starts_with(&format!("{a}/")) {
                    return "error";
                }
                if let Some(Entry::File(id, c)) = self.tree.get(b)
                    && !log.remove(*id, c.len() as u64)
                {
                    return "insufficient-space";
                }
                // Moves `a` and, for a directory, everything under it.
                let moved: Vec<String> = std::iter::once(a.clone()).chain(self.children(a)).collect();
                let entries: Vec<(String, Entry)> =
                    moved.iter().map(|k| (format!("{b}{}", &k[a.len()..]), self.tree.remove(k).unwrap())).collect();
                self.tree.extend(entries);
                "ok"
            }
            Op::Unlink(p) => match self.tree.get(p) {
                // A new file without open descriptors is just deleted.
                Some(Entry::File(id, c)) if !log.original.contains_key(id) || log.remove(*id, c.len() as u64) => {
                    self.tree.remove(p);
                    "ok"
                }
                Some(Entry::File(..)) => "insufficient-space",
                _ => "error",
            },
            Op::Mkdir(p) => {
                if files >= MAX_FILES {
                    return "quota";
                }
                if self.tree.contains_key(p) || !self.dir_ok(parent(p)) {
                    return "error";
                }
                self.tree.insert(p.clone(), Entry::Dir);
                "ok"
            }
            Op::Rmdir(p) => match self.tree.get(p) {
                Some(Entry::Dir) if self.children(p).is_empty() => {
                    self.tree.remove(p);
                    "ok"
                }
                _ => "error",
            },
        }
    }
}

/// A file's contents as runs, `byte x count`, for readable differences.
fn runs(c: &[u8]) -> String {
    let mut out: Vec<String> = vec![];
    let mut i = 0;
    while i < c.len() {
        let n = c[i..].iter().take_while(|&&b| b == c[i]).count();
        out.push(format!("{}x{n}", c[i]));
        i += n;
    }
    format!("[{}]", out.join(" "))
}

/// What is on disk under `volume`: each file's runs, or `dir`.
fn on_disk(volume: &Path) -> BTreeMap<String, String> {
    raw(volume).into_iter().map(|(k, v)| (k, v.map_or("dir".into(), |c| runs(&c)))).collect()
}

fn raw(volume: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![volume.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            let rel = e.path().strip_prefix(volume).unwrap().to_string_lossy().into_owned();
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
                out.insert(rel, None);
            } else {
                out.insert(rel, Some(std::fs::read(e.path()).unwrap()));
            }
        }
    }
    out
}

fn expected(m: &Model) -> BTreeMap<String, String> {
    m.tree.iter().map(|(k, e)| (k.clone(), match e { Entry::Dir => "dir".into(), Entry::File(_, c) => runs(c) })).collect()
}

struct Fixture {
    rt: tokio::runtime::Runtime,
    node: Arc<Node>,
    objects: PathBuf,
    next: AtomicUsize,
}

fn fixture() -> &'static Fixture {
    static F: std::sync::OnceLock<Fixture> = std::sync::OnceLock::new();
    F.get_or_init(|| {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
        let dir = root.join("nodes").join("quota_model");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = NodeConfig { data_dir: dir, max_live_objects: 4, memory_reservation_mb: 64, dev: true, ..NodeConfig::default() };
        cfg.validate().unwrap();
        let limits = serde_json::json!({
            "memory_mb": 32, "cpu_ms": 5000, "request_timeout_ms": 30000, "post_return_timeout_ms": 5000,
            "idle_timeout_ms": 600000, "disk_mb": MAX_BYTES >> 20, "max_files": MAX_FILES,
        });
        let app = AppConfig {
            name: "fsops".into(), digest: "test".into(), env: Default::default(), config: Default::default(),
            limits: serde_json::from_value(limits).unwrap(), worker: Default::default(),
        };
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        nanovm::engine::preflight(&cfg).unwrap();
        let objects = cfg.app_dirs(&app.name)[0].clone();
        let node = rt.block_on(async { Node::new(cfg, &app, &guest(), None) }).unwrap();
        Fixture { rt, node, objects, next: AtomicUsize::new(0) }
    })
}

/// How often each op gave each result, and requests by kind: printed at
/// the end, to show the cases cover quota and journal refusals.
static STATS: std::sync::Mutex<BTreeMap<String, usize>> = std::sync::Mutex::new(BTreeMap::new());

#[derive(Clone, Debug)]
struct Step {
    ops: Vec<Op>,
    commit: bool,
    probe: bool,
}

/// More than `MAX_FILES` paths, mostly at the top or in `d`, so entries,
/// renames across directories and the entry limit all come up.
fn path() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["a", "b", "c", "d", "d", "d/a", "d/b", "d/e", "d/e/a"]).prop_map(String::from)
}

fn op() -> impl Strategy<Value = Op> {
    let offset = prop::sample::select(vec![0u64, 1, 4096, 100_000, 700_000, 1_048_575, 1_200_000]);
    let len = prop::sample::select(vec![0u64, 1, 100, 4096, 65_536, 300_000, 1_100_000]);
    let size = prop::sample::select(vec![0u64, 1, 4096, 300_000, 1_048_576, 2_000_000]);
    prop_oneof![
        4 => (path(), offset, len, any::<u8>()).prop_map(|(p, o, l, b)| Op::Write(p, o, l, b)),
        2 => (path(), size).prop_map(|(p, s)| Op::Truncate(p, s)),
        3 => (path(), path()).prop_map(|(a, b)| Op::Rename(a, b)),
        2 => path().prop_map(Op::Unlink),
        2 => path().prop_map(Op::Mkdir),
        2 => path().prop_map(Op::Rmdir),
    ]
}

fn step() -> impl Strategy<Value = Step> {
    (prop::collection::vec(op(), 1..6), prop::bool::weighted(0.75), any::<bool>())
        .prop_map(|(ops, commit, probe)| Step { ops, commit, probe })
}

/// Runs `steps` on a fresh object; panics (with the step) on any difference.
fn run(steps: &[Step]) {
    let f = fixture();
    let id = format!("o{}", f.next.fetch_add(1, Relaxed));
    let volume = f.objects.join(&id);
    let mut model = Model::default();
    f.rt.block_on(async {
        let send = async |method: &str, path: &str, body: &str| load::request(&f.node, method, &id, path, body).await;
        for (n, s) in steps.iter().enumerate() {
            let mut next = model.clone();
            let mut log = Log::begin(&model);
            // Each op's expected result. Once the undo log is nearly full,
            // where the journal refuses a write is unknown: that op may also
            // give `insufficient-space`, its file is read back afterwards,
            // and the request's later ops are left out.
            let (mut lines, mut unsure) = (vec![], None);
            for op in &s.ops {
                let result = next.apply(op, &mut log);
                if let (true, Op::Write(p, ..) | Op::Truncate(p, _)) = (log.near_full(), op) {
                    lines.push(format!("{} => {result}|insufficient-space", op.line()));
                    unsure = Some(p.clone());
                    break;
                }
                lines.push(format!("{} => {result}", op.line()));
            }
            for l in &lines {
                let (op, result) = l.split_once(" => ").unwrap();
                let key = format!("{} {result}", op.split(' ').next().unwrap());
                *STATS.lock().unwrap().entry(key).or_default() += 1;
            }
            *STATS.lock().unwrap().entry(format!("request commit={} probe={}", s.commit, s.probe)).or_default() += 1;
            let r = send("POST", &format!("/ops?commit={}", s.commit as u8), &lines.join("\n")).await;
            let committed = match (r.status, r.body.as_str()) {
                (200, "matched") => true,
                (500, body) if body.contains("matched") && !body.contains("mismatch") && !s.commit => false,
                _ => panic!("step {n}:\n{}\n{} {}\nsteps: {steps:#?}", lines.join("\n"), r.status, r.body),
            };
            if committed {
                if let Some(p) = unsure
                    && let Some(Entry::File(_, c)) = next.tree.get_mut(&p)
                {
                    *c = std::fs::read(volume.join(&p)).unwrap();
                }
                model = next;
            }
            if s.probe {
                // Runs in this instance if the request committed, else in a
                // fresh one that recovered it and rescanned the volume.
                let (used, files) = model.usage();
                let r = send("POST", &format!("/probe?bytes={}&files={}", MAX_BYTES - used, MAX_FILES - files), "").await;
                assert!(r.body.contains("probed"), "step {n}: usage {used} B, {files} entries: {}\nsteps: {steps:#?}", r.body);
            }
            if s.probe || !committed {
                // A failed request is rolled back when its object restarts.
                let r = send("GET", "/", "").await;
                assert_eq!(r.status, 200, "{}", r.body);
            }
            assert_eq!(on_disk(&volume), expected(&model), "step {n}: {}\nsteps: {steps:#?}", lines.join("\n"));
        }
    });
}

proptest! {
    // QUOTA_MODEL_CASES=1000 for a longer run.
    #![proptest_config(ProptestConfig {
        cases: std::env::var("QUOTA_MODEL_CASES").ok().and_then(|c| c.parse().ok()).unwrap_or(64),
        max_shrink_iters: 200, failure_persistence: None, ..ProptestConfig::default()
    })]

    #[test]
    fn quota_usage_always_matches_the_model(steps in prop::collection::vec(step(), 1..20)) {
        run(&steps);
    }
}

#[test]
fn zz_print_coverage() {
    // Runs after the model test with RUST_TEST_THREADS=1 (alphabetical).
    eprintln!("{:#?}", STATS.lock().unwrap());
}
