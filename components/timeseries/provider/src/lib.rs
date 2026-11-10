//! Numeric time series with hour and day rollups, over the object's filesystem.
//!
//! A series is `/data/.ts/<name>/`:
//! - `r<day>`: raw points of one UTC day, 16 bytes each (time ms, value), in
//!   write order. Writes only append, so the host's undo journal records the
//!   old size, not data.
//! - `h<n>`: hour rollups for hours `768n..768n+768` (32 days), 32-byte slots
//!   (count, sum, min, max), updated in place; the journal keeps the old 32
//!   bytes. Files grow only as far as the last slot written.
//! - `d<n>`: day rollups for days `1024n..1024n+1024`, likewise.
//!
//! No internal write-ahead log: the host commits or rolls back each request's
//! changes as a whole. A write touches several files, so it keeps an undo list
//! in memory for the call alone: if any step is refused it puts back what the
//! earlier steps changed before returning the error, and traps if it cannot,
//! so the whole request rolls back.
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use futures::lock::Mutex;
use util::wasip3::filesystem::types::{DescriptorFlags, ErrorCode, OpenFlags};

mod bindings {
    wit_bindgen::generate!({ path: "../wit", world: "provider" });
}
use bindings::exports::nanovm::timeseries::series::{
    Bucket, Error, Guest, GuestSeries, Level, Point, Series as Handle,
};
bindings::export!(Provider with_types_in bindings);
struct Provider;

const ROOT: &str = ".ts";
const HOUR: u64 = 3_600_000;
const DAY: u64 = 24 * HOUR;
const HOURS_PER_FILE: u64 = 768;
const DAYS_PER_FILE: u64 = 1024;
const POINT: usize = 16;
const SLOT: usize = 32;
const MAX_TIME: u64 = 1 << 53;
const MAX_POINTS: usize = 10_000;
const MAX_BUCKETS: u64 = 10_000;
const MAX_HANDLES: usize = 64;
const MAX_CACHED: usize = 256;

thread_local! {
    static SERIES: RefCell<HashMap<String, Rc<Mutex<State>>>> = RefCell::new(HashMap::new());
    static HANDLES: Cell<usize> = const { Cell::new(0) };
}

#[derive(Clone, Copy, Default)]
struct Agg {
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
}

impl Agg {
    fn add(&mut self, o: Agg) {
        if o.count == 0 {
            return;
        }
        if self.count == 0 {
            *self = o;
            return;
        }
        self.count += o.count;
        self.sum += o.sum;
        self.min = self.min.min(o.min);
        self.max = self.max.max(o.max);
    }

    fn of(v: f64) -> Agg {
        Agg { count: 1, sum: v, min: v, max: v }
    }

    fn decode(b: &[u8]) -> Agg {
        let f = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        Agg { count: f(0), sum: f64::from_bits(f(8)), min: f64::from_bits(f(16)), max: f64::from_bits(f(24)) }
    }

    fn encode(&self, out: &mut [u8]) {
        out[0..8].copy_from_slice(&self.count.to_le_bytes());
        out[8..16].copy_from_slice(&self.sum.to_le_bytes());
        out[16..24].copy_from_slice(&self.min.to_le_bytes());
        out[24..32].copy_from_slice(&self.max.to_le_bytes());
    }
}

/// One rollup level: `unit` ms per slot, `per_file` slots per file.
#[derive(Clone, Copy)]
struct Rollup {
    prefix: char,
    unit: u64,
    per_file: u64,
}

const HOURS: Rollup = Rollup { prefix: 'h', unit: HOUR, per_file: HOURS_PER_FILE };
const DAYS: Rollup = Rollup { prefix: 'd', unit: DAY, per_file: DAYS_PER_FILE };

/// How to put back one change if a later step of the same call fails.
enum Undo {
    Unlink(String),
    Truncate(String, u64),
    Restore(String, u64, Vec<u8>),
}

async fn undo(steps: Vec<Undo>) {
    let trap = |e: ErrorCode| -> ! { panic!("timeseries: cannot undo a refused write ({e:?}); the request must roll back") };
    for step in steps.into_iter().rev() {
        match step {
            Undo::Unlink(path) => match util::mount("/data").unwrap().unlink_file_at(path).await {
                Ok(()) | Err(ErrorCode::NoEntry) => (),
                Err(e) => trap(e),
            },
            Undo::Truncate(path, len) => {
                let fd = util::open("/data", &path, OpenFlags::empty(), DescriptorFlags::WRITE).await.unwrap_or_else(|e| trap(e));
                fd.set_size(len).await.unwrap_or_else(|e| trap(e));
            }
            Undo::Restore(path, at, bytes) => {
                let fd = util::open("/data", &path, OpenFlags::empty(), DescriptorFlags::WRITE).await.unwrap_or_else(|e| trap(e));
                util::write_all(&fd, bytes, at).await.unwrap_or_else(|e| trap(e));
            }
        }
    }
}

/// What files a series has, loaded on first use and shared by every handle,
/// across requests (a failed request ends the instance; calls update this
/// only after their change succeeded).
struct State {
    dir: String,
    loaded: bool,
    created: bool,
    raw: BTreeSet<u64>,
    hours: BTreeSet<u64>,
    days: BTreeSet<u64>,
}

impl State {
    fn files(&mut self, prefix: char) -> &mut BTreeSet<u64> {
        match prefix {
            'r' => &mut self.raw,
            'h' => &mut self.hours,
            _ => &mut self.days,
        }
    }

    fn path(&self, prefix: char, n: u64) -> String {
        format!("{}/{prefix}{n}", self.dir)
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
        let (mut entries, done) = dir.read_directory();
        while let Some(entry) = entries.next().await {
            let mut name = entry.name.chars();
            if let (Some(p @ ('r' | 'h' | 'd')), Ok(n)) = (name.next(), name.as_str().parse::<u64>()) {
                self.files(p).insert(n);
            }
        }
        drop(entries);
        done.await.map_err(error)?;
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

    /// Opens `prefix n` for writing, creating it if new; its length before.
    async fn open_for_write(
        &self,
        prefix: char,
        n: u64,
        steps: &mut Vec<Undo>,
        added: &mut Vec<(char, u64)>,
    ) -> Result<(util::Fd, u64), Error> {
        let path = self.path(prefix, n);
        let exists = match prefix {
            'r' => self.raw.contains(&n),
            'h' => self.hours.contains(&n),
            _ => self.days.contains(&n),
        };
        if exists {
            let fd = util::open("/data", &path, OpenFlags::empty(), DescriptorFlags::READ | DescriptorFlags::WRITE)
                .await
                .map_err(error)?;
            let len = fd.stat().await.map_err(error)?.size;
            Ok((fd, len))
        } else {
            let fd = util::open(
                "/data",
                &path,
                OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
                DescriptorFlags::READ | DescriptorFlags::WRITE,
            )
            .await
            .map_err(error)?;
            steps.push(Undo::Unlink(path));
            added.push((prefix, n));
            Ok((fd, 0))
        }
    }

    async fn write(&mut self, points: &[Point]) -> Result<(), Error> {
        self.create_dirs().await?;
        let (mut steps, mut added) = (Vec::new(), Vec::new());
        match self.write_steps(points, &mut steps, &mut added).await {
            Ok(()) => {
                for (prefix, n) in added {
                    self.files(prefix).insert(n);
                }
                Ok(())
            }
            Err(e) => {
                undo(steps).await;
                Err(e)
            }
        }
    }

    async fn write_steps(
        &self,
        points: &[Point],
        steps: &mut Vec<Undo>,
        added: &mut Vec<(char, u64)>,
    ) -> Result<(), Error> {
        // Raw points first: they are the bulk of the bytes, so a full quota
        // most likely refuses them before any rollup changed.
        let mut by_day: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        for p in points {
            let bytes = by_day.entry(p.time_ms / DAY).or_default();
            bytes.extend_from_slice(&p.time_ms.to_le_bytes());
            bytes.extend_from_slice(&p.value.to_le_bytes());
        }
        for (day, bytes) in by_day {
            let (fd, len) = self.open_for_write('r', day, steps, added).await?;
            if len > 0 {
                steps.push(Undo::Truncate(self.path('r', day), len));
            }
            util::write_all(&fd, bytes, len).await.map_err(error)?;
        }
        for rollup in [HOURS, DAYS] {
            // Per file, per slot.
            let mut files: BTreeMap<u64, BTreeMap<u64, Agg>> = BTreeMap::new();
            for p in points {
                let unit = p.time_ms / rollup.unit;
                files.entry(unit / rollup.per_file).or_default().entry(unit % rollup.per_file).or_default().add(Agg::of(p.value));
            }
            for (n, slots) in files {
                let (fd, len) = self.open_for_write(rollup.prefix, n, steps, added).await?;
                let first = *slots.keys().next().unwrap();
                let last = *slots.keys().next_back().unwrap();
                let at = first * SLOT as u64;
                let mut span = read_at(&fd, at, (last - first + 1) * SLOT as u64).await?;
                let old = span.clone();
                span.resize((last - first + 1) as usize * SLOT, 0);
                for (slot, agg) in slots {
                    let i = (slot - first) as usize * SLOT;
                    let mut cur = Agg::decode(&span[i..i + SLOT]);
                    cur.add(agg);
                    cur.encode(&mut span[i..i + SLOT]);
                }
                if len > 0 {
                    let path = self.path(rollup.prefix, n);
                    if at + span.len() as u64 > len {
                        steps.push(Undo::Truncate(path.clone(), len));
                    }
                    if !old.is_empty() {
                        steps.push(Undo::Restore(path, at, old));
                    }
                }
                util::write_all(&fd, span, at).await.map_err(error)?;
            }
        }
        Ok(())
    }

    /// The points of `day`, in write order.
    async fn day(&self, day: u64) -> Result<Vec<Point>, Error> {
        let bytes = match util::read_file("/data", &self.path('r', day)).await {
            Ok(b) => b,
            Err(ErrorCode::NoEntry) => return Ok(Vec::new()),
            Err(e) => return Err(error(e)),
        };
        Ok(bytes
            .as_chunks::<POINT>()
            .0
            .iter()
            .map(|c| Point {
                time_ms: u64::from_le_bytes(c[..8].try_into().unwrap()),
                value: f64::from_le_bytes(c[8..].try_into().unwrap()),
            })
            .collect())
    }

    async fn range(&self, from: u64, to: u64, limit: u32) -> Result<Vec<Point>, Error> {
        if !(1..=MAX_POINTS as u32).contains(&limit) {
            return Err(Error::InvalidArgument(format!("limit must be 1 to {MAX_POINTS}")));
        }
        let mut out = Vec::new();
        if from >= to {
            return Ok(out);
        }
        let days: Vec<u64> = self.raw.range(from / DAY..=(to - 1) / DAY).copied().collect();
        for day in days {
            let mut points: Vec<Point> =
                self.day(day).await?.into_iter().filter(|p| (from..to).contains(&p.time_ms)).collect();
            points.sort_by_key(|p| p.time_ms);
            out.extend(points);
            if out.len() >= limit as usize {
                out.truncate(limit as usize);
                break;
            }
        }
        Ok(out)
    }

    async fn aggregate(&self, from: u64, to: u64, step: u64) -> Result<Vec<Bucket>, Error> {
        if step == 0 {
            return Err(Error::InvalidArgument("step must be at least 1 ms".into()));
        }
        if from >= to {
            return Ok(Vec::new());
        }
        if (to - 1) / step - from / step >= MAX_BUCKETS {
            return Err(Error::InvalidArgument(format!("more than {MAX_BUCKETS} buckets; narrow the range")));
        }
        let mut buckets: BTreeMap<u64, Agg> = BTreeMap::new();
        let rollup = match step {
            s if s % DAY == 0 => Some((DAYS, &self.days)),
            s if s % HOUR == 0 => Some((HOURS, &self.hours)),
            _ => None,
        };
        match rollup {
            Some((rollup, files)) => {
                let (first, last) = (from / rollup.unit, (to - 1) / rollup.unit);
                for &n in files.range(first / rollup.per_file..=last / rollup.per_file) {
                    let base = n * rollup.per_file;
                    let lo = first.max(base) - base;
                    let hi = last.min(base + rollup.per_file - 1) - base;
                    let path = self.path(rollup.prefix, n);
                    let fd = util::open("/data", &path, OpenFlags::empty(), DescriptorFlags::READ).await.map_err(error)?;
                    let span = read_at(&fd, lo * SLOT as u64, (hi - lo + 1) * SLOT as u64).await?;
                    for (i, slot) in span.as_chunks::<SLOT>().0.iter().enumerate() {
                        let agg = Agg::decode(slot);
                        if agg.count > 0 {
                            let start = (base + lo + i as u64) * rollup.unit;
                            buckets.entry(start / step * step).or_default().add(agg);
                        }
                    }
                }
            }
            None => {
                let days: Vec<u64> = self.raw.range(from / DAY..=(to - 1) / DAY).copied().collect();
                for day in days {
                    for p in self.day(day).await? {
                        if (from..to).contains(&p.time_ms) {
                            buckets.entry(p.time_ms / step * step).or_default().add(Agg::of(p.value));
                        }
                    }
                }
            }
        }
        Ok(buckets
            .into_iter()
            .map(|(start_ms, a)| Bucket { start_ms, count: a.count, sum: a.sum, min: a.min, max: a.max })
            .collect())
    }

    async fn trim(&mut self, level: Level, before: u64) -> Result<(), Error> {
        let (prefix, span) = match level {
            Level::Raw => ('r', DAY),
            Level::Hour => ('h', HOURS.unit * HOURS.per_file),
            Level::Day => ('d', DAYS.unit * DAYS.per_file),
        };
        let doomed: Vec<u64> = self.files(prefix).iter().copied().take_while(|n| (n + 1) * span <= before).collect();
        let data = util::mount("/data").unwrap();
        for (i, n) in doomed.into_iter().enumerate() {
            // Each file goes whole or not at all; stopping early leaves a
            // consistent series that a later trim continues.
            match data.unlink_file_at(self.path(prefix, n)).await {
                Ok(()) => {
                    self.files(prefix).remove(&n);
                }
                Err(e) if i == 0 => return Err(error(e)),
                Err(_) => break,
            }
        }
        Ok(())
    }
}

/// Up to `len` bytes at `offset`: fewer at the end of the file.
async fn read_at(fd: &util::Fd, offset: u64, len: u64) -> Result<Vec<u8>, Error> {
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
    Ok(buf)
}

fn error(e: ErrorCode) -> Error {
    match e {
        ErrorCode::InsufficientSpace | ErrorCode::Quota => Error::Full,
        e => Error::Other(format!("{e:?}")),
    }
}

pub struct Series {
    state: Rc<Mutex<State>>,
}

impl Drop for Series {
    fn drop(&mut self) {
        HANDLES.set(HANDLES.get() - 1);
    }
}

impl Series {
    async fn loaded(&self) -> Result<futures::lock::MutexGuard<'_, State>, Error> {
        let mut state = self.state.lock().await;
        state.load().await?;
        Ok(state)
    }
}

impl Guest for Provider {
    type Series = Series;
}

impl GuestSeries for Series {
    async fn open(name: String) -> Result<Handle, Error> {
        if !(1..=64).contains(&name.len()) || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(Error::InvalidName);
        }
        if HANDLES.get() >= MAX_HANDLES {
            return Err(Error::TableFull);
        }
        let state = SERIES.with_borrow_mut(|all| {
            if all.len() >= MAX_CACHED && !all.contains_key(&name) {
                all.retain(|_, s| Rc::strong_count(s) > 1);
            }
            all.entry(name)
                .or_insert_with_key(|name| {
                    Rc::new(Mutex::new(State {
                        dir: format!("{ROOT}/{name}"),
                        loaded: false,
                        created: false,
                        raw: BTreeSet::new(),
                        hours: BTreeSet::new(),
                        days: BTreeSet::new(),
                    }))
                })
                .clone()
        });
        HANDLES.set(HANDLES.get() + 1);
        Ok(Handle::new(Series { state }))
    }

    async fn write(&self, points: Vec<Point>) -> Result<(), Error> {
        if points.is_empty() || points.len() > MAX_POINTS {
            return Err(Error::InvalidArgument(format!("write 1 to {MAX_POINTS} points")));
        }
        if let Some(p) = points.iter().find(|p| p.time_ms > MAX_TIME || !p.value.is_finite()) {
            return Err(Error::InvalidArgument(format!("bad point at {} ms: {}", p.time_ms, p.value)));
        }
        let mut state = self.loaded().await?;
        // Once started, a write runs to completion or restoration even if the
        // caller stops waiting; the request's rollback covers traps.
        wit_bindgen::block_on(state.write(&points))
    }

    async fn range(&self, from_ms: u64, to_ms: u64, limit: u32) -> Result<Vec<Point>, Error> {
        self.loaded().await?.range(from_ms, to_ms, limit).await
    }

    async fn aggregate(&self, from_ms: u64, to_ms: u64, step_ms: u64) -> Result<Vec<Bucket>, Error> {
        self.loaded().await?.aggregate(from_ms, to_ms, step_ms).await
    }

    async fn latest(&self) -> Result<Option<Point>, Error> {
        let state = self.loaded().await?;
        let Some(&day) = state.raw.last() else {
            return Ok(None);
        };
        let points = state.day(day).await?;
        Ok(points.into_iter().reduce(|best, p| if p.time_ms >= best.time_ms { p } else { best }))
    }

    async fn trim(&self, level: Level, before_ms: u64) -> Result<(), Error> {
        let mut state = self.loaded().await?;
        wit_bindgen::block_on(state.trim(level, before_ms))
    }
}
