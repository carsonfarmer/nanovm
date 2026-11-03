//! Minimal single-connection P3 port, not a production VFS. Every persistent
//! operation uses the existing WASI filesystem, never nanovm internals.
use std::cell::Cell;
use std::io::{self, ErrorKind};
use std::time::Duration;

use sqlite_vfs::{DatabaseHandle, LockKind, OpenAccess, OpenOptions, Vfs, WalDisabled};
use nanovm_sdk::{data, wasip3};
use wasip3::filesystem::types::{Descriptor, DescriptorFlags, ErrorCode, OpenFlags, PathFlags};
use wasip3::wit_bindgen::block_on;

thread_local! {
    static CUT: Cell<u64> = const { Cell::new(0) };
    static STEPS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
    static SYNCS: Cell<u64> = const { Cell::new(0) };
}

pub fn arm(cut: u64) {
    CUT.set(cut);
    STEPS.set(0);
    BYTES.set(0);
    SYNCS.set(0);
}

pub fn syncs() -> u64 {
    SYNCS.get()
}

pub fn counts() -> (u64, u64) {
    (STEPS.get(), BYTES.get())
}

// A deterministic guest trap AFTER a real filesystem operation returns.
// No unwinding or SQLite cleanup runs: nanovm must recover its outer journal.
fn checkpoint(operation: &str, bytes: usize) {
    let n = STEPS.get() + 1;
    STEPS.set(n);
    BYTES.set(BYTES.get() + bytes as u64);
    assert!(CUT.get() != n, "SQLite VFS cut {n} after {operation}");
}

fn error(e: ErrorCode) -> io::Error {
    io::Error::new(
        match e {
            ErrorCode::NoEntry => ErrorKind::NotFound,
            ErrorCode::InsufficientSpace | ErrorCode::Quota => ErrorKind::StorageFull,
            _ => ErrorKind::Other,
        },
        format!("{e:?}"),
    )
}

struct Files;
pub struct File {
    fd: Descriptor,
    lock: LockKind,
}

// SQLite calls these when built with SQLITE_OS_OTHER. Registration itself
// does not open a file or perform any async work.
#[unsafe(no_mangle)]
pub extern "C" fn sqlite3_os_init() -> i32 {
    match sqlite_vfs::register("wasi-p3", Files, true) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn sqlite3_os_end() -> i32 {
    0
}

impl Vfs for Files {
    type Handle = File;

    fn open(&self, name: &str, opts: OpenOptions) -> io::Result<File> {
        let (oflags, flags) = match opts.access {
            OpenAccess::Read => (OpenFlags::empty(), DescriptorFlags::READ),
            OpenAccess::Write => (OpenFlags::empty(), DescriptorFlags::READ | DescriptorFlags::WRITE),
            OpenAccess::Create => (OpenFlags::CREATE, DescriptorFlags::READ | DescriptorFlags::WRITE),
            OpenAccess::CreateNew => (
                OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
                DescriptorFlags::READ | DescriptorFlags::WRITE,
            ),
        };
        let fd = block_on(data().open(name, oflags, flags)).map_err(error)?;
        checkpoint("open", 0);
        Ok(File {
            fd,
            lock: LockKind::None,
        })
    }
    fn delete(&self, name: &str) -> io::Result<()> {
        block_on(data().remove(name)).map_err(error)?;
        checkpoint("delete", 0);
        Ok(())
    }
    fn exists(&self, name: &str) -> io::Result<bool> {
        match block_on(
            data()
                .descriptor()
                .stat_at(PathFlags::empty(), name.to_string()),
        ) {
            Ok(_) => Ok(true),
            Err(ErrorCode::NoEntry) => Ok(false),
            Err(e) => Err(error(e)),
        }
    }
    fn temporary_name(&self) -> String {
        "temporary.db".into()
    }
    fn random(&self, buffer: &mut [i8]) {
        let bytes = wasip3::random::random::get_random_bytes(buffer.len() as u64);
        for (out, byte) in buffer.iter_mut().zip(bytes) {
            *out = byte as i8;
        }
    }
    fn sleep(&self, duration: Duration) -> Duration {
        block_on(wasip3::clocks::monotonic_clock::wait_for(duration.as_nanos() as u64));
        duration
    }
}

impl DatabaseHandle for File {
    type WalIndex = WalDisabled;
    fn size(&self) -> io::Result<u64> {
        block_on(self.fd.stat()).map(|s| s.size).map_err(error)
    }
    fn read_exact_at(&mut self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        block_on(async {
            let (mut rx, done) = self.fd.read_via_stream(offset);
            let mut n = 0;
            while n < buf.len() {
                let (_, bytes) = rx.read(Vec::with_capacity(buf.len() - n)).await;
                if bytes.is_empty() {
                    break;
                }
                buf[n..n + bytes.len()].copy_from_slice(&bytes);
                n += bytes.len();
            }
            drop(rx);
            done.await.map_err(error)?;
            if n != buf.len() {
                buf[n..].fill(0);
                Err(io::Error::from(ErrorKind::UnexpectedEof))
            } else {
                Ok(())
            }
        })
    }
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        block_on(nanovm_sdk::fs::write_at(&self.fd, buf.to_vec(), offset)).map_err(error)?;
        checkpoint("write", buf.len());
        Ok(())
    }
    fn sync(&mut self, data_only: bool) -> io::Result<()> {
        block_on(async {
            if data_only {
                self.fd.sync_data().await
            } else {
                self.fd.sync().await
            }
        })
        .map_err(error)?;
        SYNCS.set(SYNCS.get() + 1);
        checkpoint("sync", 0);
        Ok(())
    }
    fn set_len(&mut self, size: u64) -> io::Result<()> {
        block_on(self.fd.set_size(size)).map_err(error)?;
        checkpoint("truncate", 0);
        Ok(())
    }
    // The fixture has exactly one connection per object, never shared. This
    // is not sufficient locking for a general SQLite library.
    fn lock(&mut self, lock: LockKind) -> io::Result<bool> {
        self.lock = lock;
        Ok(true)
    }
    fn reserved(&mut self) -> io::Result<bool> {
        Ok(matches!(
            self.lock,
            LockKind::Reserved | LockKind::Pending | LockKind::Exclusive
        ))
    }
    fn current_lock(&self) -> io::Result<LockKind> {
        Ok(self.lock)
    }
    fn wal_index(&self, _: bool) -> io::Result<WalDisabled> {
        Err(io::Error::from(ErrorKind::Unsupported))
    }
}
