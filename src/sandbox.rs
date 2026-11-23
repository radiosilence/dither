//! What an ffmpeg or ffprobe run may touch beyond the file it was given.
//!
//! The arguments `video::Tools` passes keep ffmpeg to one container, the file
//! protocol and a list of decoders, but those are ffmpeg's own checks: a file
//! that exploits a bug in its parsers runs code that ignores them, as the
//! worker, in the worker's container, beside the database and every stored
//! image. So each run is also confined from the outside, by the kernel:
//!
//! - Landlock: it reads the programs' and libraries' directories, the files
//!   it was given, and the one directory it writes in; nothing else on the
//!   filesystem, `/proc` included, so not the worker's environment either. No
//!   TCP, and no signals or abstract sockets beyond its own processes.
//! - seccomp: no sockets at all, no reaching into other processes (`ptrace`,
//!   `process_vm_*`, `pidfd_getfd`), no new namespaces, and none of the
//!   kernel interfaces exploits reach for and ffmpeg never uses (io_uring,
//!   BPF, perf events, userfaultfd, keyrings).
//! - Limits: no file larger than it is allowed (the kernel stops it with
//!   `SIGXFSZ`), and no core dumps.
//!
//! Landlock applies as far as the kernel supports it; [`landlock_abi`] says
//! how far, for the log at startup. The rest applies on any Linux. Elsewhere
//! (a laptop) nothing is confined.
//!
//! The rules are built before the fork and applied between fork and exec
//! ([`Prepared::apply`]), where only system calls are safe: the worker is
//! multi-threaded, so the child must not allocate or take a lock.

use std::io;
use std::path::{Path, PathBuf};

/// What one run may do.
#[derive(Debug, Clone, Default)]
pub struct Sandbox {
    /// Files it reads.
    read: Vec<PathBuf>,
    /// The directory it writes in, if any.
    write: Option<PathBuf>,
    /// The largest file it may write; `None` for no limit of its own.
    file_size: Option<u64>,
}

impl Sandbox {
    /// A run that reads `files` and writes no file: a probe, or a frame sent
    /// down a pipe.
    pub fn reading(files: &[&Path]) -> Self {
        Self {
            read: files.iter().map(|f| f.to_path_buf()).collect(),
            write: None,
            file_size: Some(0),
        }
    }

    /// A run that reads `files` and writes into `dir`, no file larger than
    /// `file_size` bytes (`None`: no limit of its own).
    pub fn writing(files: &[&Path], dir: &Path, file_size: Option<u64>) -> Self {
        Self {
            read: files.iter().map(|f| f.to_path_buf()).collect(),
            write: Some(dir.to_path_buf()),
            file_size,
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::landlock_abi;

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::os::fd::{AsRawFd as _, OwnedFd};
    use std::path::{Path, PathBuf};

    use landlock::{
        ABI, Access as _, AccessFs, AccessNet, PathBeneath, PathFd, Ruleset, RulesetAttr as _,
        RulesetCreatedAttr as _, Scope,
    };

    use super::Sandbox;

    /// The Landlock ABI the rules are written for: 6 adds the scopes. A
    /// kernel with less applies what it knows.
    const ABI_TARGET: ABI = ABI::V6;

    /// Where programs, and the libraries a dynamically linked build loads,
    /// live: read and executed. In the image ffmpeg is static and only
    /// `/usr/local/bin` and `/etc/ssl` exist; a development machine's ffmpeg
    /// needs the rest.
    const SYSTEM: [&str; 6] = ["/usr", "/lib", "/lib64", "/lib32", "/etc", "/opt"];

    /// Read, not programs: randomness, and the CPU count some builds read
    /// rather than ask the kernel for.
    const DEVICES: [&str; 2] = ["/dev/urandom", "/sys/devices/system/cpu"];

    impl Sandbox {
        /// The rules for running `program`, built now, before the fork.
        pub fn prepare(&self, program: &Path) -> io::Result<Prepared> {
            Ok(Prepared {
                landlock: self.ruleset(program)?,
                filter: filter(),
                file_size: self
                    .file_size
                    .map_or(libc::RLIM_INFINITY, |n| n as libc::rlim_t),
            })
        }

        /// The Landlock ruleset's descriptor, or `None` where the kernel has
        /// no Landlock.
        fn ruleset(&self, program: &Path) -> io::Result<Option<OwnedFd>> {
            let read = AccessFs::from_read(ABI_TARGET);
            let mut ruleset = Ruleset::default()
                .handle_access(AccessFs::from_all(ABI_TARGET))
                .and_then(|r| r.handle_access(AccessNet::from_all(ABI_TARGET)))
                .and_then(|r| r.scope(Scope::from_all(ABI_TARGET)))
                .and_then(|r| r.create())
                .map_err(io::Error::other)?;
            let add = |ruleset: landlock::RulesetCreated,
                       path: &Path,
                       access: landlock::BitFlags<AccessFs>| {
                let fd = PathFd::new(path).map_err(io::Error::other)?;
                ruleset
                    .add_rule(PathBeneath::new(fd, access))
                    .map_err(io::Error::other)
            };
            // Wherever the program is, it may be read and run: a static
            // ffmpeg outside the usual directories needs nothing else.
            let own = program.parent().map(Path::to_path_buf);
            for dir in SYSTEM.iter().map(PathBuf::from).chain(own) {
                if dir.exists() {
                    ruleset = add(ruleset, &dir, read)?;
                }
            }
            for path in DEVICES.map(Path::new) {
                if path.is_dir() {
                    ruleset = add(ruleset, path, AccessFs::ReadFile | AccessFs::ReadDir)?;
                } else if path.exists() {
                    ruleset = add(ruleset, path, AccessFs::ReadFile.into())?;
                }
            }
            for file in &self.read {
                ruleset = add(ruleset, file, AccessFs::ReadFile.into())?;
            }
            if let Some(dir) = &self.write {
                let access = AccessFs::ReadFile
                    | AccessFs::ReadDir
                    | AccessFs::WriteFile
                    | AccessFs::MakeReg
                    | AccessFs::RemoveFile
                    | AccessFs::Truncate;
                ruleset = add(ruleset, dir, access)?;
            }
            Ok(ruleset.into())
        }
    }

    /// A [`Sandbox`] ready to apply in the child.
    #[derive(Debug)]
    pub struct Prepared {
        landlock: Option<OwnedFd>,
        filter: Vec<libc::sock_filter>,
        file_size: libc::rlim_t,
    }

    impl Prepared {
        /// Confine the calling process. Runs between fork and exec, so it
        /// makes system calls and nothing else.
        pub fn apply(&self) -> io::Result<()> {
            fn check(r: libc::c_long) -> io::Result<()> {
                if r < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            }
            let limit = |n| libc::rlimit {
                rlim_cur: n,
                rlim_max: n,
            };
            let (core, file) = (limit(0), limit(self.file_size));
            // SAFETY: every call passes values that outlive it, and none of
            // them allocates.
            unsafe {
                check(libc::setrlimit(libc::RLIMIT_CORE, &core).into())?;
                check(libc::setrlimit(libc::RLIMIT_FSIZE, &file).into())?;
                let (one, zero): (libc::c_ulong, libc::c_ulong) = (1, 0);
                check(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, one, zero, zero, zero).into())?;
                if let Some(fd) = &self.landlock {
                    check(libc::syscall(
                        libc::SYS_landlock_restrict_self,
                        fd.as_raw_fd(),
                        0 as libc::c_uint,
                    ))?;
                }
                if !self.filter.is_empty() {
                    let program = libc::sock_fprog {
                        len: self.filter.len() as libc::c_ushort,
                        filter: self.filter.as_ptr().cast_mut(),
                    };
                    check(libc::syscall(
                        libc::SYS_seccomp,
                        libc::SECCOMP_SET_MODE_FILTER,
                        0 as libc::c_uint,
                        &raw const program,
                    ))?;
                }
            }
            Ok(())
        }
    }

    /// The Landlock ABI this kernel offers, or `None` without Landlock.
    pub fn landlock_abi() -> Option<u32> {
        const VERSION: libc::c_uint = 1;
        // SAFETY: asking for the version passes no attributes and creates
        // nothing.
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<libc::c_void>(),
                0usize,
                VERSION,
            )
        };
        u32::try_from(abi).ok().filter(|&v| v > 0)
    }

    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: Option<u32> = Some(0xC000_003E);
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: Option<u32> = Some(0xC000_00B7);
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    const AUDIT_ARCH: Option<u32> = None;

    /// System calls a run is refused: the network, other processes, and the
    /// kernel's more exposed interfaces, none of which ffmpeg uses on files.
    const DENIED: [libc::c_long; 16] = [
        libc::SYS_socket,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_pidfd_getfd,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_unshare,
        libc::SYS_setns,
    ];

    /// `clone` flags that make namespaces, which is how an unprivileged
    /// process reaches the parts of the kernel that otherwise need
    /// privileges. Threads are clones without them.
    const CLONE_NAMESPACES: u32 = (libc::CLONE_NEWNS
        | libc::CLONE_NEWCGROUP
        | libc::CLONE_NEWUTS
        | libc::CLONE_NEWIPC
        | libc::CLONE_NEWUSER
        | libc::CLONE_NEWPID
        | libc::CLONE_NEWNET) as u32;

    /// The seccomp filter: refusals over everything else allowed, the
    /// container's own profile being the allowlist outside it. A system call
    /// of another architecture (a 32-bit `int 0x80`) kills the process, as
    /// does an x32 one, which would otherwise pass under numbers this list
    /// does not name. `clone3` says it does not exist, so threads fall back
    /// to `clone`, whose flags can be read. Empty on an architecture it has
    /// no number for, where only Landlock and the limits apply.
    pub(super) fn filter() -> Vec<libc::sock_filter> {
        const LD: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
        const JEQ: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
        const JGE: u16 = (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16;
        const JSET: u16 = (libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K) as u16;
        const RET: u16 = (libc::BPF_RET | libc::BPF_K) as u16;
        // Offsets into `struct seccomp_data`: the number, the architecture,
        // then the arguments, the first's low half first on these
        // little-endian machines.
        const NR: u32 = 0;
        const ARCH: u32 = 4;
        const ARG0: u32 = 16;
        const X32: u32 = 0x4000_0000;
        let Some(arch) = AUDIT_ARCH else {
            return Vec::new();
        };
        let op = |code, k, jt, jf| libc::sock_filter { code, jt, jf, k };
        let ret = |k: u32| op(RET, k, 0, 0);
        let errno = |e: i32| ret(libc::SECCOMP_RET_ERRNO | e as u32);

        let mut f = vec![
            op(LD, ARCH, 0, 0),
            op(JEQ, arch, 1, 0),
            ret(libc::SECCOMP_RET_KILL_PROCESS),
            op(LD, NR, 0, 0),
        ];
        if cfg!(target_arch = "x86_64") {
            f.push(op(JGE, X32, 0, 1));
            f.push(ret(libc::SECCOMP_RET_KILL_PROCESS));
        }
        for nr in DENIED {
            f.push(op(JEQ, nr as u32, 0, 1));
            f.push(errno(libc::EPERM));
        }
        f.push(op(JEQ, libc::SYS_clone3 as u32, 0, 1));
        f.push(errno(libc::ENOSYS));
        f.push(op(JEQ, libc::SYS_clone as u32, 0, 3));
        f.push(op(LD, ARG0, 0, 0));
        f.push(op(JSET, CLONE_NAMESPACES, 0, 1));
        f.push(errno(libc::EPERM));
        f.push(ret(libc::SECCOMP_RET_ALLOW));
        f
    }
}

/// Where nothing is confined: the same calls, doing nothing.
#[cfg(not(target_os = "linux"))]
mod other {
    use std::io;
    use std::path::Path;

    use super::Sandbox;

    #[derive(Debug)]
    pub struct Prepared;

    impl Sandbox {
        pub fn prepare(&self, _program: &Path) -> io::Result<Prepared> {
            Ok(Prepared)
        }
    }

    impl Prepared {
        pub fn apply(&self) -> io::Result<()> {
            Ok(())
        }
    }

    pub fn landlock_abi() -> Option<u32> {
        None
    }
}

#[cfg(not(target_os = "linux"))]
pub use other::landlock_abi;

/// Whether runs are confined at all on this system.
pub fn confines() -> bool {
    cfg!(target_os = "linux")
}

/// `program` found as a shell would: by its path when it names a directory,
/// else in `PATH`. Landlock needs to know where it is to let it run.
pub fn locate(program: &Path) -> io::Result<PathBuf> {
    let found = if program.components().count() > 1 {
        Some(program.to_path_buf())
    } else {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(program))
                .find(|p| p.is_file())
        })
    };
    match found {
        Some(p) => std::fs::canonicalize(p),
        None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} is not on PATH", program.display()),
        )),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::process::Command;

    /// Run `program args` confined by `sandbox`: whether it succeeded, and
    /// what it wrote to stderr.
    fn confined(sandbox: &Sandbox, program: &str, args: &[&str]) -> (bool, String) {
        use std::os::unix::process::CommandExt as _;
        let path = locate(Path::new(program)).unwrap();
        let prepared = sandbox.prepare(&path).unwrap();
        let mut cmd = Command::new(&path);
        cmd.args(args);
        // SAFETY: `apply` makes system calls and nothing else.
        unsafe {
            cmd.pre_exec(move || prepared.apply());
        }
        let out = cmd.output().expect("runs");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    #[test]
    fn every_jump_in_the_filter_lands_inside_it() {
        let f = linux::filter();
        for (i, ins) in f.iter().enumerate() {
            let class = u32::from(ins.code) & 0x07;
            if class == libc::BPF_RET {
                continue;
            }
            let far = i + 1 + usize::from(ins.jt.max(ins.jf));
            assert!(far < f.len(), "instruction {i} jumps past the end");
        }
        assert_eq!(
            u32::from(f.last().unwrap().code) & 0x07,
            libc::BPF_RET,
            "it ends by deciding"
        );
    }

    #[test]
    fn a_confined_program_runs_and_reads_what_it_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("given");
        std::fs::write(&file, "hello").unwrap();
        let (ok, err) = confined(
            &Sandbox::reading(&[&file]),
            "cat",
            &[file.to_str().unwrap()],
        );
        assert!(ok, "{err}");
    }

    #[test]
    fn a_confined_program_reads_nothing_else() {
        if landlock_abi().is_none() {
            eprintln!("skipped: this kernel has no Landlock");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("other");
        std::fs::write(&other, "secret").unwrap();
        let (ok, _) = confined(&Sandbox::reading(&[]), "cat", &[other.to_str().unwrap()]);
        assert!(!ok, "read a file it was not given");
        // Nor the environment of the process that started it.
        let environ = format!("/proc/{}/environ", std::process::id());
        let (ok, _) = confined(&Sandbox::reading(&[]), "cat", &[&environ]);
        assert!(!ok, "read its parent's environment");
    }

    #[test]
    fn a_confined_program_writes_only_where_it_may_and_no_more_than_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let sandbox = Sandbox::writing(&[], dir.path(), Some(10));
        let write = |text: &str| {
            let script = format!("printf '%s' {text} > {}", out.display());
            confined(&sandbox, "sh", &["-c", &script]).0
        };
        assert!(write("short"));
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "short");
        assert!(!write("much-longer-than-ten-bytes"), "wrote past its limit");
        if landlock_abi().is_some() {
            let elsewhere = tempfile::tempdir().unwrap();
            let target = elsewhere.path().join("x");
            let script = format!("echo x > {}", target.display());
            let (ok, _) = confined(&sandbox, "sh", &["-c", &script]);
            assert!(!ok && !target.exists(), "wrote outside its directory");
        }
    }

    #[test]
    fn a_confined_program_has_no_network() {
        let (ok, err) = confined(
            &Sandbox::reading(&[]),
            "python3",
            &["-I", "-c", "import socket; socket.socket()"],
        );
        assert!(!ok);
        assert!(err.contains("Operation not permitted"), "{err}");
    }

    #[test]
    fn a_program_is_found_on_the_path() {
        assert!(locate(Path::new("sh")).unwrap().is_absolute());
        assert!(locate(Path::new("no-such-program-here")).is_err());
    }
}
