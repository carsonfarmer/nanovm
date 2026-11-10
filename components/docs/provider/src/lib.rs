//! JSON document collections with secondary indexes, over the object's
//! filesystem.
//!
//! A collection is `/data/.docs/<name>/`: `log`, an append-only sequence of
//! put and delete records, and `indexes`, the index definitions. The current
//! version of each document and every index live in memory (Bitcask's
//! "keydir"): rebuilt from the log on first use, then cached for the
//! instance's life. A change appends one record, so the host's undo journal
//! records only the log's old size; once more than half of a log over 1 MiB is
//! garbage, the change also rewrites it (see `State::compact`).
//!
//! No internal write-ahead log: the host commits or rolls back each request's
//! changes as a whole. Each call is all-or-nothing within the request: checks
//! come before the one append, a failed append is truncated away, and if that
//! undo fails the component traps so the whole request rolls back. Memory is
//! updated only after the file change succeeds; a failed request ends the
//! instance, so the cache never outlives a rollback.
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Bound;
use std::rc::Rc;

use futures::lock::Mutex;
use serde_json::{Value, json};
use util::wasip3::filesystem::types::{Descriptor, DescriptorFlags, ErrorCode, OpenFlags};

mod bindings {
    wit_bindgen::generate!({ path: "../wit", world: "provider" });
}
use bindings::exports::nanovm::docs::store::{
    Bound as WitBound, Collection as Handle, Document, Error, Guest, GuestCollection, Hit, Index as WitIndex, Page,
    Query, Stats,
};
bindings::export!(Provider with_types_in bindings);
struct Provider;

const ROOT: &str = ".docs";
const MAX_BODY: usize = 1 << 20;
const MAX_ID: usize = 256;
const MAX_FIELD: usize = 128;
const MAX_INDEXES: usize = 16;
const MAX_LIMIT: u32 = 1000;
/// A page stops once its bodies pass this (always at least one document).
const PAGE_BYTES: usize = 1 << 20;
const COMPACT_MIN: u64 = 1 << 20;
/// op (1 byte), id length and body length (u32 LE each).
const HEADER: u64 = 9;
const PUT: u8 = 1;
const DELETE: u8 = 2;
/// A put record turned into a delete record in place, when a full volume has
/// no room to append one.
const DEAD: u8 = 3;
const MAX_HANDLES: usize = 64;
const MAX_CACHED: usize = 64;

thread_local! {
    static COLLECTIONS: RefCell<HashMap<String, Rc<Mutex<State>>>> = RefCell::new(HashMap::new());
    static HANDLES: Cell<usize> = const { Cell::new(0) };
}

/// An indexed value. Order: null < false < true < numbers < strings.
#[derive(Clone, Debug)]
enum Key {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
}

impl Key {
    fn scalar(v: &Value) -> Option<Key> {
        Some(match v {
            Value::Null => Key::Null,
            Value::Bool(b) => Key::Bool(*b),
            Value::Number(n) => Key::Num(n.as_f64()?),
            Value::String(s) => Key::Str(s.clone()),
            Value::Array(_) | Value::Object(_) => return None,
        })
    }

    fn rank(&self) -> u8 {
        match self {
            Key::Null => 0,
            Key::Bool(_) => 1,
            Key::Num(_) => 2,
            Key::Str(_) => 3,
        }
    }

    fn json(&self) -> Value {
        match self {
            Key::Null => Value::Null,
            Key::Bool(b) => json!(b),
            Key::Num(n) => json!(n),
            Key::Str(s) => json!(s),
        }
    }

    fn parse(text: &str) -> Result<Key, Error> {
        let v: Value = serde_json::from_str(text).map_err(|e| Error::InvalidJson(e.to_string()))?;
        Key::scalar(&v).ok_or_else(|| Error::InvalidJson("a bound must be a JSON scalar".into()))
    }
}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Key::Bool(a), Key::Bool(b)) => a.cmp(b),
            (Key::Num(a), Key::Num(b)) => a.total_cmp(b),
            (Key::Str(a), Key::Str(b)) => a.cmp(b),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}
impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Key {}

/// The values of `path` in `doc`: a scalar, or each distinct scalar of an array.
fn keys_of(doc: &Value, path: &[String]) -> Vec<Key> {
    let mut v = doc;
    for segment in path {
        match v.get(segment) {
            Some(next) => v = next,
            None => return Vec::new(),
        }
    }
    match v {
        Value::Array(items) => {
            let mut keys: Vec<Key> = items.iter().filter_map(Key::scalar).collect();
            keys.sort();
            keys.dedup();
            keys
        }
        v => Key::scalar(v).into_iter().collect(),
    }
}

/// Lowercased runs of letters and digits, up to 64 characters each.
fn terms(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty() && t.chars().count() <= 64)
        .map(str::to_lowercase)
}

/// BM25's usual parameters.
const K1: f64 = 1.2;
const B: f64 = 0.75;

/// Sorts after every id (ids are at most 256 bytes).
fn after_all_ids() -> String {
    "\u{10FFFF}".repeat(MAX_ID / 4 + 1)
}

struct Entry {
    /// Where its record starts in the log.
    at: u64,
    id_len: u32,
    body_len: u32,
    /// Its values in each index, in `State::indexes` order.
    keys: Vec<Vec<Key>>,
}

impl Entry {
    fn record_len(&self) -> u64 {
        HEADER + u64::from(self.id_len) + u64::from(self.body_len)
    }

    fn body_at(&self) -> u64 {
        self.at + HEADER + u64::from(self.id_len)
    }
}

struct Index {
    field: String,
    path: Vec<String>,
    unique: bool,
    tree: BTreeSet<(Key, String)>,
    /// A full-text index: `postings` and `lengths` instead of `tree`.
    text: bool,
    /// Term -> document -> occurrences.
    postings: HashMap<String, HashMap<String, u32>>,
    /// Document -> terms in its field; and their sum.
    lengths: HashMap<String, u32>,
    total: u64,
}

impl Index {
    fn new(field: &str, unique: bool, text: bool) -> Index {
        Index {
            field: field.into(),
            path: field.split('.').map(String::from).collect(),
            unique,
            tree: BTreeSet::new(),
            text,
            postings: HashMap::new(),
            lengths: HashMap::new(),
            total: 0,
        }
    }

    /// A document's keys in this index: its values, or for a text index
    /// every term, repeated as often as it occurs, sorted.
    fn keys(&self, doc: &Value) -> Vec<Key> {
        if !self.text {
            return keys_of(doc, &self.path);
        }
        let mut v = doc;
        for segment in &self.path {
            match v.get(segment) {
                Some(next) => v = next,
                None => return Vec::new(),
            }
        }
        let texts: Vec<&str> = match v {
            Value::String(s) => vec![s],
            Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        let mut terms: Vec<Key> = texts.into_iter().flat_map(terms).map(Key::Str).collect();
        terms.sort();
        terms
    }

    /// Indexes `id` under `keys`; what its entry keeps to remove it later.
    fn add(&mut self, id: &str, keys: Vec<Key>) -> Vec<Key> {
        if !self.text {
            for k in &keys {
                self.tree.insert((k.clone(), id.to_string()));
            }
            return keys;
        }
        let mut distinct = Vec::new();
        for run in keys.chunk_by(|a, b| a == b) {
            if let Key::Str(term) = &run[0] {
                self.postings
                    .entry(term.clone())
                    .or_default()
                    .insert(id.to_string(), run.len() as u32);
                distinct.push(run[0].clone());
            }
        }
        self.lengths.insert(id.to_string(), keys.len() as u32);
        self.total += keys.len() as u64;
        distinct
    }

    fn remove(&mut self, id: &str, keys: Vec<Key>) {
        if !self.text {
            for k in keys {
                self.tree.remove(&(k, id.to_string()));
            }
            return;
        }
        for k in keys {
            if let Key::Str(term) = k
                && let Some(docs) = self.postings.get_mut(&term)
            {
                docs.remove(id);
                if docs.is_empty() {
                    self.postings.remove(&term);
                }
            }
        }
        self.total -= u64::from(self.lengths.remove(id).unwrap_or(0));
    }

    /// Whether a document other than `id` already has one of `keys`.
    fn taken(&self, keys: &[Key], id: &str) -> bool {
        keys.iter().any(|k| {
            self.tree
                .range((Bound::Included((k.clone(), String::new())), Bound::Unbounded))
                .take_while(|(other, _)| other == k)
                .any(|(_, other)| other != id)
        })
    }
}

struct State {
    dir: String,
    loaded: bool,
    created: bool,
    log_exists: bool,
    docs: BTreeMap<String, Entry>,
    indexes: Vec<Index>,
    log_len: u64,
    live: u64,
    /// A read descriptor on the log, reopened after a rewrite.
    reader: Option<Descriptor>,
}

impl State {
    fn log(&self) -> String {
        format!("{}/log", self.dir)
    }

    async fn load(&mut self) -> Result<(), Error> {
        if self.loaded {
            return Ok(());
        }
        match util::open("/data", &self.dir, OpenFlags::DIRECTORY, DescriptorFlags::READ).await {
            Ok(_) => self.created = true,
            Err(ErrorCode::NoEntry) => {
                self.loaded = true;
                return Ok(());
            }
            Err(e) => return Err(error(e)),
        }
        if let Some(bytes) = read_all(&format!("{}/indexes", self.dir)).await? {
            let defs: Vec<Value> = serde_json::from_slice(&bytes).map_err(|e| corrupt("indexes", e))?;
            for def in defs {
                let (Some(field), Some(unique)) = (def["field"].as_str(), def["unique"].as_bool()) else {
                    return Err(corrupt("indexes", "bad definition"));
                };
                let text = def["text"].as_bool().unwrap_or(false);
                self.indexes.push(Index::new(field, unique, text));
            }
        }
        let Some(mut log) = LogReader::open(&self.log()).await? else {
            self.loaded = true;
            return Ok(());
        };
        self.log_exists = true;
        while let Some(rec) = log.next().await? {
            match rec.op {
                PUT => {
                    let keys = if self.indexes.is_empty() {
                        Vec::new()
                    } else {
                        let doc: Value = serde_json::from_slice(&rec.body).map_err(|e| corrupt("log", e))?;
                        self.indexes.iter().map(|ix| ix.keys(&doc)).collect()
                    };
                    self.apply_put(rec.id, rec.at, rec.body.len() as u32, keys);
                }
                DELETE | DEAD => {
                    self.apply_delete(&rec.id);
                }
                op => return Err(corrupt("log", format!("record type {op}"))),
            }
        }
        self.log_len = log.end().await?;
        self.loaded = true;
        Ok(())
    }

    fn apply_put(&mut self, id: String, at: u64, body_len: u32, keys: Vec<Vec<Key>>) {
        self.apply_delete(&id);
        let keys = self
            .indexes
            .iter_mut()
            .zip(keys)
            .map(|(ix, ks)| ix.add(&id, ks))
            .collect();
        let entry = Entry {
            at,
            id_len: id.len() as u32,
            body_len,
            keys,
        };
        self.live += entry.record_len();
        self.docs.insert(id, entry);
    }

    fn apply_delete(&mut self, id: &str) -> bool {
        let Some(old) = self.docs.remove(id) else {
            return false;
        };
        self.live -= old.record_len();
        for (ix, ks) in self.indexes.iter_mut().zip(old.keys) {
            ix.remove(id, ks);
        }
        true
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

    /// Appends one record, or changes nothing.
    async fn append(&mut self, op: u8, id: &str, body: &[u8]) -> Result<u64, Error> {
        self.create_dirs().await?;
        let path = self.log();
        let fd = util::open("/data", &path, OpenFlags::CREATE, DescriptorFlags::WRITE)
            .await
            .map_err(error)?;
        let mut rec = Vec::with_capacity(HEADER as usize + id.len() + body.len());
        rec.push(op);
        rec.extend_from_slice(&(id.len() as u32).to_le_bytes());
        rec.extend_from_slice(&(body.len() as u32).to_le_bytes());
        rec.extend_from_slice(id.as_bytes());
        rec.extend_from_slice(body);
        let at = self.log_len;
        if let Err(e) = util::write_all(&fd, rec, at).await {
            if self.log_exists {
                fd.set_size(at)
                    .await
                    .expect("docs: cannot undo a partial append; the request must roll back");
            } else {
                drop(fd);
                unlink_or_trap(&path).await;
            }
            return Err(error(e));
        }
        self.log_exists = true;
        self.log_len = at + HEADER + (id.len() + body.len()) as u64;
        Ok(at)
    }

    async fn put(&mut self, id: String, body: String) -> Result<(), Error> {
        let doc: Value = serde_json::from_str(&body).map_err(|e| Error::InvalidJson(e.to_string()))?;
        if !doc.is_object() {
            return Err(Error::InvalidJson("a document must be a JSON object".into()));
        }
        let keys: Vec<Vec<Key>> = self.indexes.iter().map(|ix| ix.keys(&doc)).collect();
        for (ix, ks) in self.indexes.iter().zip(&keys) {
            if ix.unique && ix.taken(ks, &id) {
                return Err(Error::Conflict(ix.field.clone()));
            }
        }
        let at = self.append(PUT, &id, body.as_bytes()).await?;
        self.apply_put(id, at, body.len() as u32, keys);
        self.maybe_compact().await;
        Ok(())
    }

    async fn delete(&mut self, id: String) -> Result<bool, Error> {
        if !self.docs.contains_key(&id) {
            return Ok(false);
        }
        match self.append(DELETE, &id, &[]).await {
            Ok(_) => {
                self.apply_delete(&id);
                self.maybe_compact().await;
            }
            // A full volume must still let documents go: turn the document's
            // own record into a delete record, in place (one byte).
            Err(Error::Full) => {
                let fd = util::open("/data", &self.log(), OpenFlags::empty(), DescriptorFlags::WRITE)
                    .await
                    .map_err(error)?;
                util::write_all(&fd, vec![DEAD], self.docs[&id].at)
                    .await
                    .map_err(error)?;
                self.apply_delete(&id);
            }
            Err(e) => return Err(e),
        }
        Ok(true)
    }

    async fn maybe_compact(&mut self) {
        if self.log_len > COMPACT_MIN && self.log_len - self.live > self.live {
            // A refused rewrite (a full quota) leaves the log as it was.
            let _ = self.compact().await;
        }
    }

    /// Rewrites the log with only current versions. The old log is unlinked
    /// first (the host keeps it, uncounted, until commit) and read through an
    /// open descriptor, so this needs no free space: only room for the old
    /// log among the request's removed files. A refusal there changes
    /// nothing; any later failure traps, rolling the request back.
    async fn compact(&mut self) -> Result<(), Error> {
        if !self.log_exists || self.log_len == self.live {
            return Ok(());
        }
        let path = self.log();
        let old = util::open("/data", &path, OpenFlags::empty(), DescriptorFlags::READ)
            .await
            .map_err(error)?;
        let data = util::mount("/data").unwrap();
        data.unlink_file_at(path.clone()).await.map_err(error)?;
        self.reader = None;
        let trap = |e: Error| -> ! { panic!("docs: compaction failed ({e:?}); the request must roll back") };
        let out = util::open(
            "/data",
            &path,
            OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
            DescriptorFlags::WRITE,
        )
        .await
        .unwrap_or_else(|e| trap(error(e)));
        let mut log = LogReader::new(&old);
        let mut moved = Vec::with_capacity(self.docs.len());
        let (mut buf, mut written) = (Vec::with_capacity(1 << 20), 0u64);
        while let Some(rec) = log.next().await.unwrap_or_else(|e| trap(e)) {
            if rec.op != PUT || self.docs.get(&rec.id).is_none_or(|e| e.at != rec.at) {
                continue;
            }
            moved.push((rec.id.clone(), written + buf.len() as u64));
            buf.push(PUT);
            buf.extend_from_slice(&(rec.id.len() as u32).to_le_bytes());
            buf.extend_from_slice(&(rec.body.len() as u32).to_le_bytes());
            buf.extend_from_slice(rec.id.as_bytes());
            buf.extend_from_slice(&rec.body);
            if buf.len() >= 1 << 20 {
                let n = buf.len() as u64;
                util::write_all(&out, std::mem::take(&mut buf), written)
                    .await
                    .unwrap_or_else(|e| trap(error(e)));
                written += n;
            }
        }
        let n = buf.len() as u64;
        util::write_all(&out, buf, written)
            .await
            .unwrap_or_else(|e| trap(error(e)));
        written += n;
        for (id, at) in moved {
            self.docs.get_mut(&id).unwrap().at = at;
        }
        self.log_len = written;
        Ok(())
    }

    async fn bodies<'a>(&mut self, ids: impl Iterator<Item = &'a String>) -> Result<Vec<Document>, Error> {
        let ids: Vec<String> = ids.cloned().collect();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let body = self.body(&id).await?;
            out.push(Document { id, body });
        }
        Ok(out)
    }

    async fn body(&mut self, id: &str) -> Result<String, Error> {
        let (at, len) = {
            let e = &self.docs[id];
            (e.body_at(), e.body_len)
        };
        if self.reader.is_none() {
            self.reader = Some(
                util::open("/data", &self.log(), OpenFlags::empty(), DescriptorFlags::READ)
                    .await
                    .map_err(error)?,
            );
        }
        let bytes = read_at(self.reader.as_ref().unwrap(), at, u64::from(len)).await?;
        if bytes.len() != len as usize {
            return Err(corrupt("log", "short read"));
        }
        String::from_utf8(bytes).map_err(|e| corrupt("log", e))
    }

    /// Ids up to `limit` and about `PAGE_BYTES` of bodies.
    fn take<'a>(&self, ids: impl Iterator<Item = &'a String>, limit: u32) -> (Vec<&'a String>, bool) {
        let mut out = Vec::new();
        let mut bytes = 0;
        for id in ids {
            if out.len() == limit as usize || (!out.is_empty() && bytes >= PAGE_BYTES) {
                return (out, true);
            }
            bytes += self.docs[id].body_len as usize;
            out.push(id);
        }
        (out, false)
    }

    async fn query(&mut self, q: Query) -> Result<Page, Error> {
        check_limit(q.limit)?;
        let i = self.index(&q.field)?;
        if self.indexes[i].text {
            return Err(Error::InvalidArgument(format!(
                "{} is a text index: use search",
                q.field
            )));
        }
        let lower = match &q.lower {
            WitBound::Unbounded => Bound::Unbounded,
            WitBound::Included(v) => Bound::Included((Key::parse(v)?, String::new())),
            WitBound::Excluded(v) => Bound::Excluded((Key::parse(v)?, after_all_ids())),
        };
        let upper = match &q.upper {
            WitBound::Unbounded => None,
            WitBound::Included(v) => Some((Key::parse(v)?, true)),
            WitBound::Excluded(v) => Some((Key::parse(v)?, false)),
        };
        let start = match &q.after {
            None => lower,
            Some(cursor) => {
                let bad = || Error::InvalidArgument("bad cursor".into());
                let v: Value = serde_json::from_str(cursor).map_err(|_| bad())?;
                let key = v.get(0).and_then(Key::scalar).ok_or_else(bad)?;
                let id = v.get(1).and_then(Value::as_str).ok_or_else(bad)?.to_string();
                let after = (key, id);
                match lower {
                    Bound::Included(l) if l > after => Bound::Included(l),
                    Bound::Excluded(l) if l > after => Bound::Excluded(l),
                    _ => Bound::Excluded(after),
                }
            }
        };
        let tree = &self.indexes[i].tree;
        let within = |k: &Key| match &upper {
            None => true,
            Some((u, true)) => k <= u,
            Some((u, false)) => k < u,
        };
        let matching: Vec<&(Key, String)> = tree
            .range((start, Bound::Unbounded))
            .take_while(|(k, _)| within(k))
            .take(q.limit as usize + 1)
            .collect();
        let (ids, more) = self.take(matching.iter().map(|(_, id)| id), q.limit);
        let cursor = more.then(|| {
            let (k, id) = matching[ids.len() - 1];
            json!([k.json(), id]).to_string()
        });
        let ids: Vec<String> = ids.into_iter().cloned().collect();
        Ok(Page {
            documents: self.bodies(ids.iter()).await?,
            cursor,
        })
    }

    /// Documents matching any term of `query` in text index `field`, best
    /// first by BM25 (ties by id).
    async fn search(&mut self, field: &str, query: &str, limit: u32) -> Result<Vec<Hit>, Error> {
        check_limit(limit)?;
        let ix = &self.indexes[self.index(field)?];
        if !ix.text {
            return Err(Error::InvalidArgument(format!("{field} is not a text index")));
        }
        let mut wanted: Vec<String> = terms(query).collect();
        wanted.sort();
        wanted.dedup();
        let n = ix.lengths.len() as f64;
        let average = ix.total as f64 / n.max(1.0);
        let mut scores: HashMap<&String, f64> = HashMap::new();
        for term in &wanted {
            let Some(docs) = ix.postings.get(term) else { continue };
            let df = docs.len() as f64;
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
            for (id, &tf) in docs {
                let (tf, len) = (f64::from(tf), f64::from(ix.lengths[id]));
                *scores.entry(id).or_default() += idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * len / average));
            }
        }
        let mut ranked: Vec<(&String, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        ranked.truncate(limit as usize);
        let ranked: Vec<(String, f64)> = ranked.into_iter().map(|(id, s)| (id.clone(), s)).collect();
        let mut hits = Vec::with_capacity(ranked.len());
        for (id, score) in ranked {
            let body = self.body(&id).await?;
            hits.push(Hit {
                document: Document { id, body },
                score,
            });
        }
        Ok(hits)
    }

    fn index(&self, field: &str) -> Result<usize, Error> {
        self.indexes
            .iter()
            .position(|ix| ix.field == field)
            .ok_or_else(|| Error::NoSuchIndex(field.into()))
    }

    async fn write_indexes(&mut self, defs: Value) -> Result<(), Error> {
        self.create_dirs().await?;
        let (tmp, path) = (format!("{}/indexes.new", self.dir), format!("{}/indexes", self.dir));
        if let Err(e) = util::write_file("/data", &tmp, defs.to_string().into_bytes()).await {
            unlink_or_trap(&tmp).await;
            return Err(error(e));
        }
        let data = util::mount("/data").unwrap();
        if let Err(e) = data.rename_at(tmp.clone(), data, path).await {
            unlink_or_trap(&tmp).await;
            return Err(error(e));
        }
        Ok(())
    }

    fn defs(&self) -> Vec<Value> {
        self.indexes
            .iter()
            .map(|ix| json!({"field": ix.field, "unique": ix.unique, "text": ix.text}))
            .collect()
    }

    async fn create_index(&mut self, field: String, unique: bool, text: bool) -> Result<(), Error> {
        if field.is_empty() || field.len() > MAX_FIELD || field.split('.').any(str::is_empty) {
            return Err(Error::InvalidArgument(format!(
                "a field is 1-{MAX_FIELD} bytes of dot-separated names"
            )));
        }
        if let Ok(i) = self.index(&field) {
            return match (self.indexes[i].unique, self.indexes[i].text) == (unique, text) {
                true => Ok(()),
                false => Err(Error::InvalidArgument(format!(
                    "{field} is already indexed differently"
                ))),
            };
        }
        if self.indexes.len() == MAX_INDEXES {
            return Err(Error::InvalidArgument(format!("at most {MAX_INDEXES} indexes")));
        }
        let mut ix = Index::new(&field, unique, text);
        let mut keys = HashMap::with_capacity(self.docs.len());
        if let Some(mut log) = LogReader::open(&self.log()).await? {
            while let Some(rec) = log.next().await? {
                if rec.op != PUT || self.docs.get(&rec.id).is_none_or(|e| e.at != rec.at) {
                    continue;
                }
                let doc: Value = serde_json::from_slice(&rec.body).map_err(|e| corrupt("log", e))?;
                let ks = ix.keys(&doc);
                if unique && ix.taken(&ks, &rec.id) {
                    return Err(Error::Conflict(field));
                }
                let kept = ix.add(&rec.id, ks);
                keys.insert(rec.id, kept);
            }
        }
        let mut defs = self.defs();
        defs.push(json!({"field": field, "unique": unique, "text": text}));
        self.write_indexes(Value::Array(defs)).await?;
        for (id, entry) in self.docs.iter_mut() {
            entry.keys.push(keys.remove(id).unwrap_or_default());
        }
        self.indexes.push(ix);
        Ok(())
    }

    async fn drop_index(&mut self, field: String) -> Result<(), Error> {
        let i = self.index(&field)?;
        let mut defs = self.defs();
        defs.remove(i);
        self.write_indexes(Value::Array(defs)).await?;
        self.indexes.remove(i);
        for entry in self.docs.values_mut() {
            entry.keys.remove(i);
        }
        Ok(())
    }
}

struct Record {
    at: u64,
    op: u8,
    id: String,
    body: Vec<u8>,
}

/// Streams a log's records in order.
struct LogReader {
    stream: wit_bindgen::rt::async_support::StreamReader<u8>,
    done: Option<wit_bindgen::rt::async_support::FutureReader<Result<(), ErrorCode>>>,
    buf: Vec<u8>,
    /// Bytes of `buf` already returned.
    pos: usize,
    /// Log offset of `buf[0]`.
    base: u64,
    eof: bool,
}

impl LogReader {
    async fn open(path: &str) -> Result<Option<LogReader>, Error> {
        match util::open("/data", path, OpenFlags::empty(), DescriptorFlags::READ).await {
            Ok(fd) => Ok(Some(LogReader::new(&fd))),
            Err(ErrorCode::NoEntry) => Ok(None),
            Err(e) => Err(error(e)),
        }
    }

    fn new(fd: &Descriptor) -> LogReader {
        let (stream, done) = fd.read_via_stream(0);
        LogReader {
            stream,
            done: Some(done),
            buf: Vec::new(),
            pos: 0,
            base: 0,
            eof: false,
        }
    }

    async fn next(&mut self) -> Result<Option<Record>, Error> {
        loop {
            let rest = &self.buf[self.pos..];
            let mut need = HEADER as usize;
            if rest.len() >= need {
                let len = |i: usize| u32::from_le_bytes(rest[i..i + 4].try_into().unwrap()) as usize;
                let (id_len, body_len) = (len(1), len(5));
                need += id_len + body_len;
                if rest.len() >= need {
                    let h = HEADER as usize;
                    let id = String::from_utf8(rest[h..h + id_len].to_vec()).map_err(|e| corrupt("log", e))?;
                    let rec = Record {
                        at: self.base + self.pos as u64,
                        op: rest[0],
                        id,
                        body: rest[h + id_len..need].to_vec(),
                    };
                    self.pos += need;
                    return Ok(Some(rec));
                }
            }
            if self.eof {
                return match rest.is_empty() {
                    true => Ok(None),
                    false => Err(corrupt("log", "truncated record")),
                };
            }
            // Drop what was returned, then read at least the rest of this record.
            self.buf.drain(..self.pos);
            self.base += self.pos as u64;
            self.pos = 0;
            self.buf.reserve(need.max(256 << 10));
            let before = self.buf.len();
            let (_, buf) = self.stream.read(std::mem::take(&mut self.buf)).await;
            self.buf = buf;
            if self.buf.len() == before {
                self.eof = true;
                if let Some(done) = self.done.take() {
                    done.await.map_err(error)?;
                }
            }
        }
    }

    /// The log's length, once every record has been read.
    async fn end(mut self) -> Result<u64, Error> {
        assert!(self.next().await?.is_none());
        Ok(self.base + self.pos as u64)
    }
}

async fn read_at(fd: &Descriptor, offset: u64, len: u64) -> Result<Vec<u8>, Error> {
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

async fn read_all(path: &str) -> Result<Option<Vec<u8>>, Error> {
    match util::read_file("/data", path).await {
        Ok(b) => Ok(Some(b)),
        Err(ErrorCode::NoEntry) => Ok(None),
        Err(e) => Err(error(e)),
    }
}

async fn unlink_or_trap(path: &str) {
    match util::mount("/data").unwrap().unlink_file_at(path.to_string()).await {
        Ok(()) | Err(ErrorCode::NoEntry) => (),
        Err(e) => panic!("docs: cannot remove {path} after a failed change ({e:?}); the request must roll back"),
    }
}

fn error(e: ErrorCode) -> Error {
    match e {
        ErrorCode::InsufficientSpace | ErrorCode::Quota => Error::Full,
        e => Error::Other(format!("{e:?}")),
    }
}

fn corrupt(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Other(format!("corrupt {what}: {e}"))
}

fn check_limit(limit: u32) -> Result<(), Error> {
    match (1..=MAX_LIMIT).contains(&limit) {
        true => Ok(()),
        false => Err(Error::InvalidArgument(format!("limit must be 1 to {MAX_LIMIT}"))),
    }
}

fn check_id(id: &str) -> Result<(), Error> {
    match (1..=MAX_ID).contains(&id.len()) {
        true => Ok(()),
        false => Err(Error::InvalidArgument(format!("an id is 1 to {MAX_ID} bytes"))),
    }
}

pub struct Collection {
    state: Rc<Mutex<State>>,
}

impl Drop for Collection {
    fn drop(&mut self) {
        HANDLES.set(HANDLES.get() - 1);
    }
}

impl Collection {
    async fn loaded(&self) -> Result<futures::lock::MutexGuard<'_, State>, Error> {
        let mut state = self.state.lock().await;
        state.load().await?;
        Ok(state)
    }
}

impl Guest for Provider {
    type Collection = Collection;
}

impl GuestCollection for Collection {
    async fn open(name: String) -> Result<Handle, Error> {
        if !(1..=64).contains(&name.len())
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(Error::InvalidName);
        }
        if HANDLES.get() >= MAX_HANDLES {
            return Err(Error::TableFull);
        }
        let state = COLLECTIONS.with_borrow_mut(|all| {
            if all.len() >= MAX_CACHED && !all.contains_key(&name) {
                all.retain(|_, s| Rc::strong_count(s) > 1);
            }
            all.entry(name)
                .or_insert_with_key(|name| {
                    Rc::new(Mutex::new(State {
                        dir: format!("{ROOT}/{name}"),
                        loaded: false,
                        created: false,
                        log_exists: false,
                        docs: BTreeMap::new(),
                        indexes: Vec::new(),
                        log_len: 0,
                        live: 0,
                        reader: None,
                    }))
                })
                .clone()
        });
        HANDLES.set(HANDLES.get() + 1);
        Ok(Handle::new(Collection { state }))
    }

    async fn get(&self, id: String) -> Result<Option<String>, Error> {
        let mut state = self.loaded().await?;
        if !state.docs.contains_key(&id) {
            return Ok(None);
        }
        state.body(&id).await.map(Some)
    }

    async fn put(&self, id: String, body: String) -> Result<(), Error> {
        check_id(&id)?;
        if body.len() > MAX_BODY {
            return Err(Error::InvalidArgument("a document exceeds 1 MiB".into()));
        }
        let mut state = self.loaded().await?;
        // Once started, a change runs to completion or restoration even if the
        // caller stops waiting; the request's rollback covers traps.
        wit_bindgen::block_on(state.put(id, body))
    }

    async fn delete(&self, id: String) -> Result<bool, Error> {
        let mut state = self.loaded().await?;
        wit_bindgen::block_on(state.delete(id))
    }

    async fn scan(&self, after: Option<String>, limit: u32) -> Result<Vec<Document>, Error> {
        check_limit(limit)?;
        let mut state = self.loaded().await?;
        let start = after.map_or(Bound::Unbounded, Bound::Excluded);
        let (ids, _) = state.take(
            state
                .docs
                .range::<String, _>((start, Bound::Unbounded))
                .map(|(id, _)| id),
            limit,
        );
        let ids: Vec<String> = ids.into_iter().cloned().collect();
        state.bodies(ids.iter()).await
    }

    async fn query(&self, q: Query) -> Result<Page, Error> {
        self.loaded().await?.query(q).await
    }

    async fn create_index(&self, field: String, unique: bool) -> Result<(), Error> {
        let mut state = self.loaded().await?;
        wit_bindgen::block_on(state.create_index(field, unique, false))
    }

    async fn create_text_index(&self, field: String) -> Result<(), Error> {
        let mut state = self.loaded().await?;
        wit_bindgen::block_on(state.create_index(field, false, true))
    }

    async fn search(&self, field: String, query: String, limit: u32) -> Result<Vec<Hit>, Error> {
        self.loaded().await?.search(&field, &query, limit).await
    }

    async fn drop_index(&self, field: String) -> Result<(), Error> {
        let mut state = self.loaded().await?;
        wit_bindgen::block_on(state.drop_index(field))
    }

    async fn indexes(&self) -> Result<Vec<WitIndex>, Error> {
        let state = self.loaded().await?;
        Ok(state
            .indexes
            .iter()
            .map(|ix| WitIndex {
                field: ix.field.clone(),
                unique: ix.unique,
                text: ix.text,
            })
            .collect())
    }

    async fn stats(&self) -> Result<Stats, Error> {
        let state = self.loaded().await?;
        Ok(Stats {
            documents: state.docs.len() as u64,
            live_bytes: state.live,
            log_bytes: state.log_len,
        })
    }

    async fn compact(&self) -> Result<(), Error> {
        let mut state = self.loaded().await?;
        wit_bindgen::block_on(state.compact())
    }
}
