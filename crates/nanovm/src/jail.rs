//! The jail a worker puts itself in before it compiles or runs any guest
//! code: its own uid, Landlock confining the filesystem to the app's own
//! directories, and a seccomp allowlist without sockets, exec or namespaces.
//! It is entered while the process still has one thread, so every thread
//! started later inherits it.

use std::path::PathBuf;

/// What a worker may touch.
pub struct Jail {
    /// Run as this uid and gid (set when the supervisor is root).
    pub uid: Option<u32>,
    pub read_write: Vec<PathBuf>,
    pub read_only: Vec<PathBuf>,
}

#[cfg(not(target_os = "linux"))]
impl Jail {
    /// Off Linux the worker runs unjailed; the process split still holds.
    pub fn enter(self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Jail {
    pub fn enter(self) -> anyhow::Result<()> {
        use anyhow::{Context, ensure};
        use landlock::{
            ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
        };

        // SAFETY: plain syscalls on this process's own credentials.
        unsafe {
            if let Some(id) = self.uid {
                ensure!(libc::setgroups(0, std::ptr::null()) == 0, "setgroups failed");
                ensure!(libc::setresgid(id, id, id) == 0, "setresgid({id}) failed");
                ensure!(libc::setresuid(id, id, id) == 0, "setresuid({id}) failed");
            }
            // Changing uid clears the parent-death signal, so it is set here
            // rather than before exec; a supervisor already gone means init
            // adopted us.
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            ensure!(libc::getppid() != 1, "supervisor exited");
        }

        // Built by hand: the crate's `path_beneath_rules` silently skips a
        // path it cannot open, and a worker that cannot reach its own
        // directories should say so.
        let abi = ABI::V6;
        let rule = |p: &PathBuf, access| -> anyhow::Result<_> {
            let fd = PathFd::new(p).with_context(|| format!("{} is not accessible to the worker", p.display()))?;
            Ok(PathBeneath::new(fd, access))
        };
        let status = Ruleset::default()
            .handle_access(AccessFs::from_all(abi))?
            .create()?
            .add_rules(self.read_write.iter().map(|p| rule(p, AccessFs::from_all(abi))))?
            .add_rules(self.read_only.iter().map(|p| rule(p, AccessFs::from_read(abi))))?
            // Also sets no_new_privs.
            .restrict_self()
            .context("landlock")?;
        ensure!(status.ruleset != RulesetStatus::NotEnforced, "this kernel does not enforce Landlock");

        let filter: seccompiler::BpfProgram = seccomp_filter()?.try_into()?;
        seccompiler::apply_filter(&filter).context("seccomp")?;
        Ok(())
    }
}

/// The syscalls Wasmtime, tokio, the WASI filesystem host and the links to
/// the supervisor make (traced on jailed workers through startup and the
/// demo), plus the file calls `wasi:filesystem` can reach that the demo
/// doesn't exercise.
/// Anything else fails with `ENOSYS`, which also sends glibc's thread
/// creation from `clone3` back to `clone`, whose flags can be checked.
#[cfg(target_os = "linux")]
fn seccomp_filter() -> anyhow::Result<seccompiler::SeccompFilter> {
    use seccompiler::{SeccompAction, SeccompCmpArgLen::*, SeccompCmpOp::*, SeccompCondition as Cond, SeccompRule};

    let mut allow: Vec<i64> = vec![
        // memory
        libc::SYS_brk,
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_madvise,
        // Wasmtime flushes the instruction cache with this on aarch64.
        libc::SYS_membarrier,
        // threads, signals, time
        libc::SYS_futex,
        libc::SYS_set_robust_list,
        libc::SYS_set_tid_address,
        libc::SYS_rseq,
        libc::SYS_sched_yield,
        libc::SYS_sched_getaffinity,
        libc::SYS_gettid,
        libc::SYS_getpid,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_sigaltstack,
        libc::SYS_clock_gettime,
        libc::SYS_clock_getres,
        libc::SYS_clock_nanosleep,
        // The kernel resumes a sleep interrupted by a signal through this.
        libc::SYS_restart_syscall,
        libc::SYS_getrandom,
        libc::SYS_exit,
        libc::SYS_exit_group,
        // event loop and the two inherited sockets
        libc::SYS_epoll_create1,
        libc::SYS_epoll_ctl,
        libc::SYS_epoll_pwait,
        libc::SYS_eventfd2,
        libc::SYS_sendto,
        libc::SYS_recvfrom,
        libc::SYS_sendmsg,
        libc::SYS_recvmsg,
        libc::SYS_shutdown,
        // files, confined by Landlock
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_readv,
        libc::SYS_writev,
        libc::SYS_pread64,
        libc::SYS_pwrite64,
        libc::SYS_preadv,
        libc::SYS_pwritev,
        libc::SYS_lseek,
        libc::SYS_close,
        libc::SYS_openat,
        libc::SYS_openat2,
        libc::SYS_fstat,
        libc::SYS_newfstatat,
        libc::SYS_statx,
        libc::SYS_faccessat2,
        libc::SYS_getdents64,
        libc::SYS_mkdirat,
        libc::SYS_unlinkat,
        libc::SYS_renameat2,
        libc::SYS_readlinkat,
        libc::SYS_ftruncate,
        libc::SYS_fsync,
        libc::SYS_fdatasync,
        libc::SYS_utimensat,
        // Canonicalizing a relative data_dir (the demo's) reads the cwd.
        libc::SYS_getcwd,
    ];
    #[cfg(target_arch = "x86_64")]
    allow.extend([
        libc::SYS_epoll_wait,
        libc::SYS_access,
        libc::SYS_mkdir,
        libc::SYS_rename,
        libc::SYS_renameat,
        // std's remove_file, remove_dir and read_link on x86_64 glibc.
        libc::SYS_unlink,
        libc::SYS_rmdir,
        libc::SYS_readlink,
    ]);
    let mut rules: std::collections::BTreeMap<i64, Vec<SeccompRule>> = allow.into_iter().map(|s| (s, vec![])).collect();
    let rule = |conds: Vec<Cond>| SeccompRule::new(conds);

    // Never writable and executable at once.
    let wx = (libc::PROT_WRITE | libc::PROT_EXEC) as u64;
    let prot = || -> anyhow::Result<Vec<SeccompRule>> {
        [0, libc::PROT_WRITE as u64, libc::PROT_EXEC as u64]
            .into_iter()
            .map(|v| Ok(rule(vec![Cond::new(2, Dword, MaskedEq(wx), v)?])?))
            .collect()
    };
    rules.insert(libc::SYS_mmap, prot()?);
    rules.insert(libc::SYS_mprotect, prot()?);
    // Threads only: no new processes.
    let thread = libc::CLONE_THREAD as u64;
    rules.insert(libc::SYS_clone, vec![rule(vec![Cond::new(0, Qword, MaskedEq(thread), thread)?])?]);
    // Thread names.
    rules.insert(
        libc::SYS_prctl,
        vec![
            rule(vec![Cond::new(0, Dword, Eq, libc::PR_SET_NAME as u64)?])?,
            rule(vec![Cond::new(0, Dword, Eq, libc::PR_GET_NAME as u64)?])?,
        ],
    );
    // Reading limits, never raising them.
    rules.insert(libc::SYS_prlimit64, vec![rule(vec![Cond::new(2, Qword, Eq, 0)?])?]);
    // Descriptor flags only. Not F_SETOWN or F_SETSIG: with O_ASYNC they have
    // the kernel signal any process of the same uid.
    let fcntl = [libc::F_GETFD, libc::F_SETFD, libc::F_GETFL, libc::F_SETFL, libc::F_DUPFD_CLOEXEC];
    let fcntl = fcntl.map(|c| Ok(rule(vec![Cond::new(1, Dword, Eq, c as u64)?])?));
    rules.insert(libc::SYS_fcntl, fcntl.into_iter().collect::<anyhow::Result<_>>()?);
    // Making a received socket non-blocking (std uses this ioctl for it).
    rules.insert(libc::SYS_ioctl, vec![rule(vec![Cond::new(1, Dword, Eq, libc::FIONBIO)?])?]);
    // tokio's signal driver wakes itself through a local socketpair, which
    // reaches nothing outside the process.
    rules.insert(libc::SYS_socketpair, vec![rule(vec![Cond::new(0, Dword, Eq, libc::AF_UNIX as u64)?])?]);
    // abort() signals only this process.
    let pid = std::process::id() as u64;
    rules.insert(libc::SYS_tgkill, vec![rule(vec![Cond::new(0, Dword, Eq, pid)?])?]);

    Ok(seccompiler::SeccompFilter::new(
        rules,
        SeccompAction::Errno(libc::ENOSYS as u32),
        SeccompAction::Allow,
        std::env::consts::ARCH.try_into()?,
    )?)
}
