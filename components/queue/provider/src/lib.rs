//! An append-only log with consumer positions, over the object's filesystem.
//!
//! A topic is `/data/.queue/<name>/`: segments `<base>.dat` (payloads back to
//! back) and `<base>.idx` (16 bytes per message: end position in `.dat`, then
//! timestamp, both little-endian u64), `head` (the oldest retained offset once
//! trimmed) and `c.<consumer>` (a consumer's next offset). Appends only ever
//! extend files, so the host's undo journal records just their old sizes.
//!
//! No internal write-ahead log: the host commits or rolls back each request's
//! changes as a whole. Each call is all-or-nothing within the request: on an
//! error it restores what it changed before returning, and if it cannot, it
//! traps so the whole request rolls back.
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use futures::lock::Mutex;
use util::wasip3::filesystem::types::{Descriptor, DescriptorFlags, DescriptorType, ErrorCode, OpenFlags};

mod bindings {
    wit_bindgen::generate!({ path: "../wit", world: "provider" });
}
use bindings::exports::nanovm::queue::log::{Error, Guest, GuestTopic, Message, Topic as Handle};
bindings::export!(Provider with_types_in bindings);
struct Provider;

const ROOT: &str = ".queue";
/// A segment takes no more appends once its data or index reaches these.
const SEGMENT_BYTES: u64 = 1 << 20;
const SEGMENT_MESSAGES: u64 = 16_384;
const MAX_MESSAGE: usize = 1 << 20;
const MAX_BATCH: usize = 4096;
const MAX_BATCH_BYTES: usize = 4 << 20;
const MAX_READ: u32 = 1024;
const READ_BYTES: u64 = 1 << 20;
const ENTRY: u64 = 16;
const MAX_HANDLES: usize = 64;
/// Topics whose state stays cached with no handle open.
const MAX_CACHED: usize = 256;

thread_local! {
    static TOPICS: RefCell<HashMap<String, Rc<Mutex<State>>>> = RefCell::new(HashMap::new());
    static HANDLES: Cell<usize> = const { Cell::new(0) };
}

#[derive(Clone, Copy)]
struct Segment {
    base: u64,
    count: u64,
    bytes: u64,
}

/// What a topic's files say, loaded on first use and shared by every handle
/// to the same name, across requests: a failed request ends the instance, so
/// this never outlives a rollback, and every call updates it only once its
/// change has succeeded.
struct State {
    dir: String,
    loaded: bool,
    created: bool,
    head: u64,
    /// In offset order. Only the last one's `count` and `bytes` are kept
    /// exact; earlier ones end where the next begins.
    segments: Vec<Segment>,
}

impl State {
    fn next(&self) -> u64 {
        self.segments.last().map_or(self.head, |s| s.base + s.count)
    }

    fn oldest(&self) -> u64 {
        self.segments.first().map_or(self.head, |s| s.base.max(self.head))
    }

    /// Where segment `i` ends.
    fn end(&self, i: usize) -> u64 {
        self.segments.get(i + 1).map_or_else(|| self.next(), |s| s.base)
    }

    fn path(&self, base: u64, ext: &str) -> String {
        format!("{}/{base:020}.{ext}", self.dir)
    }

    async fn load(&mut self) -> Result<(), Error> {
        if self.loaded {
            return Ok(());
        }
        let dir = match util::open("/data", &self.dir, OpenFlags::DIRECTORY, DescriptorFlags::READ).await {
            Ok(dir) => dir,
            Err(ErrorCode::NoEntry) => {
                self.loaded = true;
                return Ok(());
            }
            Err(e) => return Err(error(e)),
        };
        let mut bases = Vec::new();
        let (mut entries, done) = dir.read_directory();
        while let Some(entry) = entries.next().await {
            if let Some(base) = entry.name.strip_suffix(".idx").and_then(|n| n.parse::<u64>().ok()) {
                bases.push(base);
            }
        }
        drop(entries);
        done.await.map_err(error)?;
        bases.sort_unstable();
        self.head = match read_range(&format!("{}/head", self.dir), 0, 8).await? {
            Some(b) if b.len() == 8 => u64::from_le_bytes(b.try_into().unwrap()),
            None => 0,
            Some(_) => return Err(Error::Other("corrupt head".into())),
        };
        self.segments = bases
            .into_iter()
            .map(|base| Segment {
                base,
                count: 0,
                bytes: 0,
            })
            .collect();
        if let Some(&Segment { base, .. }) = self.segments.last() {
            let idx = size(&self.path(base, "idx")).await?;
            let dat = size(&self.path(base, "dat")).await?;
            if idx % ENTRY != 0 {
                return Err(Error::Other(format!("corrupt index for segment {base}")));
            }
            *self.segments.last_mut().unwrap() = Segment {
                base,
                count: idx / ENTRY,
                bytes: dat,
            };
        }
        self.created = true;
        self.loaded = true;
        Ok(())
    }

    async fn create_dirs(&mut self) -> Result<(), Error> {
        if self.created {
            return Ok(());
        }
        let data = util::mount("/data").ok_or(Error::Other("no /data".into()))?;
        for name in [ROOT.to_owned(), self.dir.clone()] {
            match data.create_directory_at(name).await {
                Ok(()) | Err(ErrorCode::Exist) => (),
                Err(e) => return Err(error(e)),
            }
        }
        self.created = true;
        Ok(())
    }

    async fn read(&self, offset: u64, max: u32) -> Result<Vec<Message>, Error> {
        if !(1..=MAX_READ).contains(&max) {
            return Err(Error::InvalidArgument(format!("max-count must be 1 to {MAX_READ}")));
        }
        let mut offset = offset.max(self.oldest());
        let mut out = Vec::new();
        let mut budget = READ_BYTES;
        let Some(mut i) = self.segments.iter().rposition(|s| s.base <= offset) else {
            return Ok(out);
        };
        while i < self.segments.len() && offset < self.next() && out.len() < max as usize && budget > 0 {
            let base = self.segments[i].base;
            let k = offset - base;
            let n = (self.end(i) - offset).min(max as u64 - out.len() as u64);
            if n == 0 {
                i += 1;
                continue;
            }
            // The entry before `k` gives where message `k` starts.
            let from = k.saturating_sub(1) * ENTRY;
            let want = (n + u64::from(k > 0)) * ENTRY;
            let idx = read_range(&self.path(base, "idx"), from, want)
                .await?
                .filter(|b| b.len() as u64 == want)
                .ok_or_else(|| Error::Other(format!("short index in segment {base}")))?;
            let mut entries = idx.as_chunks::<8>().0.iter().map(|b| u64::from_le_bytes(*b));
            let start = if k > 0 {
                let end = entries.next().unwrap();
                entries.next();
                end
            } else {
                0
            };
            // At least one message per call, even one larger than the budget.
            let mut ends = Vec::with_capacity(n as usize);
            while let (Some(end), Some(ts)) = (entries.next(), entries.next()) {
                if !(out.is_empty() && ends.is_empty()) && end - start > budget {
                    break;
                }
                ends.push((end, ts));
            }
            let short = (ends.len() as u64) < n;
            let stop = ends.last().unwrap().0;
            let data = read_range(&self.path(base, "dat"), start, stop - start)
                .await?
                .filter(|b| b.len() as u64 == stop - start)
                .ok_or_else(|| Error::Other(format!("short data in segment {base}")))?;
            let mut at = 0;
            for (end, ts) in ends {
                let len = (end - start - at) as usize;
                out.push(Message {
                    offset,
                    timestamp_ms: ts,
                    payload: data[at as usize..at as usize + len].to_vec(),
                });
                at += len as u64;
                offset += 1;
            }
            budget = budget.saturating_sub(stop - start);
            if short {
                break;
            }
            i += 1;
        }
        Ok(out)
    }

    async fn append(&mut self, payloads: Vec<Vec<u8>>) -> Result<u64, Error> {
        self.create_dirs().await?;
        let first = self.next();
        let last = self.segments.last().copied();
        let fresh = match last {
            Some(s) => s.bytes >= SEGMENT_BYTES || s.count >= SEGMENT_MESSAGES,
            None => true,
        };
        let seg = if fresh {
            Segment {
                base: first,
                count: 0,
                bytes: 0,
            }
        } else {
            last.unwrap()
        };
        let (idx_path, dat_path) = (self.path(seg.base, "idx"), self.path(seg.base, "dat"));
        let oflags = if fresh {
            OpenFlags::CREATE | OpenFlags::EXCLUSIVE
        } else {
            OpenFlags::empty()
        };
        let idx = util::open("/data", &idx_path, oflags, DescriptorFlags::WRITE)
            .await
            .map_err(error)?;
        // A data file without its index is garbage a failed trim left: reuse it.
        let dat_flags = if fresh {
            OpenFlags::CREATE | OpenFlags::TRUNCATE
        } else {
            OpenFlags::empty()
        };
        let dat = match util::open("/data", &dat_path, dat_flags, DescriptorFlags::WRITE).await {
            Ok(fd) => fd,
            Err(e) => {
                if fresh {
                    unlink_or_trap(&idx_path).await;
                }
                return Err(error(e));
            }
        };
        let ts = now_ms();
        let mut index = Vec::with_capacity(payloads.len() * ENTRY as usize);
        let mut data = Vec::with_capacity(payloads.iter().map(Vec::len).sum());
        for p in &payloads {
            data.extend_from_slice(p);
            index.extend_from_slice(&(seg.bytes + data.len() as u64).to_le_bytes());
            index.extend_from_slice(&ts.to_le_bytes());
        }
        let added = data.len() as u64;
        let mut result = util::write_all(&dat, data, seg.bytes).await;
        if result.is_ok() {
            result = util::write_all(&idx, index, seg.count * ENTRY).await;
        }
        if let Err(e) = result {
            // Put both files back as they were, so a caught error commits nothing.
            if fresh {
                drop((idx, dat));
                unlink_or_trap(&dat_path).await;
                unlink_or_trap(&idx_path).await;
            } else {
                restore(&dat, seg.bytes).await;
                restore(&idx, seg.count * ENTRY).await;
            }
            return Err(error(e));
        }
        let seg = Segment {
            base: seg.base,
            count: seg.count + payloads.len() as u64,
            bytes: seg.bytes + added,
        };
        if fresh {
            self.segments.push(seg);
        } else {
            *self.segments.last_mut().unwrap() = seg;
        }
        Ok(first)
    }

    async fn position(&self, consumer: &str) -> Result<u64, Error> {
        let stored = match read_range(&format!("{}/c.{consumer}", self.dir), 0, 8).await? {
            Some(b) if b.len() == 8 => u64::from_le_bytes(b.try_into().unwrap()),
            None => 0,
            Some(_) => return Err(Error::Other(format!("corrupt position for {consumer}"))),
        };
        Ok(stored.max(self.oldest()).min(self.next()))
    }

    /// Writes an 8-byte file in place, or creates it; on error, as it was.
    async fn put_u64(&mut self, name: &str, value: u64) -> Result<(), Error> {
        self.create_dirs().await?;
        let path = format!("{}/{name}", self.dir);
        let (fd, created) = match util::open("/data", &path, OpenFlags::empty(), DescriptorFlags::WRITE).await {
            Ok(fd) => (fd, false),
            Err(ErrorCode::NoEntry) => (
                util::open(
                    "/data",
                    &path,
                    OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
                    DescriptorFlags::WRITE,
                )
                .await
                .map_err(error)?,
                true,
            ),
            Err(e) => return Err(error(e)),
        };
        // An in-place overwrite of 8 bytes either happens or is refused whole:
        // the host journals the old bytes before it writes.
        if let Err(e) = util::write_all(&fd, value.to_le_bytes().to_vec(), 0).await {
            drop(fd);
            if created {
                unlink_or_trap(&path).await;
            }
            return Err(error(e));
        }
        Ok(())
    }

    async fn trim(&mut self, before: u64) -> Result<(), Error> {
        if before > self.next() {
            return Err(Error::InvalidArgument("trim past the end of the log".into()));
        }
        if before <= self.oldest() {
            return Ok(());
        }
        self.put_u64("head", before).await?;
        self.head = before;
        // Whole segments below the new head go. A segment that fails to go is
        // still consistent: reads start at `head`; a later trim retries it.
        while !self.segments.is_empty() && self.end(0) <= before {
            let base = self.segments[0].base;
            let data = util::mount("/data").ok_or(Error::Other("no /data".into()))?;
            if data.unlink_file_at(self.path(base, "idx")).await.is_err() {
                break;
            }
            // Without its index the data file is invisible; drop it best effort.
            let _ = data.unlink_file_at(self.path(base, "dat")).await;
            self.segments.remove(0);
        }
        Ok(())
    }
}

/// Reads `len` bytes at `offset` (fewer at the end of the file); `None` if absent.
async fn read_range(path: &str, offset: u64, len: u64) -> Result<Option<Vec<u8>>, Error> {
    let fd = match util::open("/data", path, OpenFlags::empty(), DescriptorFlags::READ).await {
        Ok(fd) => fd,
        Err(ErrorCode::NoEntry) => return Ok(None),
        Err(e) => return Err(error(e)),
    };
    let (mut stream, done) = fd.read_via_stream(offset);
    let mut buf = Vec::with_capacity(len as usize);
    while (buf.len() as u64) < len {
        let before = buf.len();
        let (_, b) = stream.read(buf).await;
        buf = b;
        if buf.len() == before {
            break;
        }
    }
    let complete = buf.len() as u64 >= len;
    drop(stream);
    if !complete {
        done.await.map_err(error)?;
    }
    buf.truncate(len as usize);
    Ok(Some(buf))
}

async fn size(path: &str) -> Result<u64, Error> {
    let fd = util::open("/data", path, OpenFlags::empty(), DescriptorFlags::READ)
        .await
        .map_err(error)?;
    Ok(fd.stat().await.map_err(error)?.size)
}

async fn restore(fd: &Descriptor, len: u64) {
    fd.set_size(len)
        .await
        .expect("queue: cannot undo a partial append; the request must roll back");
}

async fn unlink_or_trap(path: &str) {
    match util::mount("/data").unwrap().unlink_file_at(path.to_string()).await {
        Ok(()) | Err(ErrorCode::NoEntry) => (),
        Err(e) => panic!("queue: cannot remove {path} after a failed change ({e:?}); the request must roll back"),
    }
}

fn now_ms() -> u64 {
    let t = util::wasip3::clocks::system_clock::now();
    (t.seconds.max(0) as u64) * 1000 + u64::from(t.nanoseconds) / 1_000_000
}

fn error(e: ErrorCode) -> Error {
    match e {
        ErrorCode::InsufficientSpace | ErrorCode::Quota => Error::Full,
        e => Error::Other(format!("{e:?}")),
    }
}

fn valid(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn consumer_name(name: &str) -> Result<(), Error> {
    if valid(name) { Ok(()) } else { Err(Error::InvalidName) }
}

pub struct Topic {
    state: Rc<Mutex<State>>,
}

impl Drop for Topic {
    fn drop(&mut self) {
        HANDLES.set(HANDLES.get() - 1);
    }
}

impl Guest for Provider {
    type Topic = Topic;
}

impl GuestTopic for Topic {
    async fn open(name: String) -> Result<Handle, Error> {
        if !valid(&name) {
            return Err(Error::InvalidName);
        }
        if HANDLES.get() >= MAX_HANDLES {
            return Err(Error::TableFull);
        }
        let state = TOPICS.with_borrow_mut(|topics| {
            if topics.len() >= MAX_CACHED && !topics.contains_key(&name) {
                topics.retain(|_, s| Rc::strong_count(s) > 1);
            }
            topics
                .entry(name)
                .or_insert_with_key(|name| {
                    Rc::new(Mutex::new(State {
                        dir: format!("{ROOT}/{name}"),
                        loaded: false,
                        created: false,
                        head: 0,
                        segments: Vec::new(),
                    }))
                })
                .clone()
        });
        HANDLES.set(HANDLES.get() + 1);
        Ok(Handle::new(Topic { state }))
    }

    async fn append(&self, payloads: Vec<Vec<u8>>) -> Result<u64, Error> {
        if payloads.is_empty() || payloads.len() > MAX_BATCH {
            return Err(Error::InvalidArgument(format!("append 1 to {MAX_BATCH} messages")));
        }
        if payloads.iter().any(|p| p.len() > MAX_MESSAGE) {
            return Err(Error::InvalidArgument("a message exceeds 1 MiB".into()));
        }
        if payloads.iter().map(Vec::len).sum::<usize>() > MAX_BATCH_BYTES {
            return Err(Error::InvalidArgument("a batch exceeds 4 MiB".into()));
        }
        let mut state = self.state.lock().await;
        state.load().await?;
        // Once started, a change runs to completion or restoration even if the
        // caller stops waiting; the request's rollback covers traps.
        wit_bindgen::block_on(state.append(payloads))
    }

    async fn read(&self, offset: u64, max_count: u32) -> Result<Vec<Message>, Error> {
        let mut state = self.state.lock().await;
        state.load().await?;
        state.read(offset, max_count).await
    }

    async fn bounds(&self) -> Result<(u64, u64), Error> {
        let mut state = self.state.lock().await;
        state.load().await?;
        Ok((state.oldest(), state.next()))
    }

    async fn position(&self, consumer: String) -> Result<u64, Error> {
        consumer_name(&consumer)?;
        let mut state = self.state.lock().await;
        state.load().await?;
        state.position(&consumer).await
    }

    async fn seek(&self, consumer: String, offset: u64) -> Result<(), Error> {
        consumer_name(&consumer)?;
        let mut state = self.state.lock().await;
        state.load().await?;
        if offset > state.next() {
            return Err(Error::InvalidArgument("seek past the end of the log".into()));
        }
        wit_bindgen::block_on(state.put_u64(&format!("c.{consumer}"), offset))
    }

    async fn next(&self, consumer: String, max_count: u32) -> Result<Vec<Message>, Error> {
        consumer_name(&consumer)?;
        let mut state = self.state.lock().await;
        state.load().await?;
        let from = state.position(&consumer).await?;
        let messages = state.read(from, max_count).await?;
        if let Some(last) = messages.last() {
            wit_bindgen::block_on(state.put_u64(&format!("c.{consumer}"), last.offset + 1))?;
        }
        Ok(messages)
    }

    async fn consumers(&self) -> Result<Vec<(String, u64)>, Error> {
        let mut state = self.state.lock().await;
        state.load().await?;
        let mut names = Vec::new();
        if let Ok(dir) = util::open("/data", &state.dir, OpenFlags::DIRECTORY, DescriptorFlags::READ).await {
            let (mut entries, done) = dir.read_directory();
            while let Some(entry) = entries.next().await {
                if let Some(name) = entry.name.strip_prefix("c.")
                    && matches!(entry.type_, DescriptorType::RegularFile)
                    && valid(name)
                {
                    names.push(name.to_string());
                }
            }
            drop(entries);
            done.await.map_err(error)?;
        }
        names.sort_unstable();
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let at = state.position(&name).await?;
            out.push((name, at));
        }
        Ok(out)
    }

    async fn remove_consumer(&self, consumer: String) -> Result<(), Error> {
        consumer_name(&consumer)?;
        let state = self.state.lock().await;
        let path = format!("{}/c.{consumer}", state.dir);
        wit_bindgen::block_on(async {
            match util::mount("/data").unwrap().unlink_file_at(path).await {
                Ok(()) | Err(ErrorCode::NoEntry) => Ok(()),
                Err(e) => Err(error(e)),
            }
        })
    }

    async fn trim(&self, before: u64) -> Result<(), Error> {
        let mut state = self.state.lock().await;
        state.load().await?;
        wit_bindgen::block_on(state.trim(before))
    }
}
