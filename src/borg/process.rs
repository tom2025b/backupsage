//! Linux-only supervisor. All allocation/string construction occurs before fork;
//! fork children use only raw syscalls/libc wrappers and _exit, never Rust locks,
//! unwinding, destructors, Command::spawn, or allocator-dependent operations.
use super::{Cancellation, Error, Limits, Result};
use std::{
    ffi::CString,
    fs::File,
    io::Read,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    os::unix::ffi::OsStrExt,
    time::Instant,
};

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(Error::Spawn);
    }
    // Reserve low descriptors for child stdio, control=3, repository pin=9.
    let original = unsafe { [OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])] };
    let duplicate = |fd: &OwnedFd| -> Result<OwnedFd> {
        let n = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 20) };
        if n < 0 {
            Err(Error::Spawn)
        } else {
            Ok(unsafe { OwnedFd::from_raw_fd(n) })
        }
    };
    Ok((duplicate(&original[0])?, duplicate(&original[1])?))
}

fn nonblocking(fd: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(Error::Pipe);
    }
    Ok(())
}

struct Supervisor {
    pid: libc::pid_t,
    control: Option<OwnedFd>,
    status: Option<i32>,
}
impl Supervisor {
    fn cancel(&mut self) {
        self.control.take();
    }
    fn poll(&mut self) -> Result<Option<i32>> {
        if self.status.is_none() {
            let mut status = 0;
            let rc = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
            if rc == self.pid {
                self.status = Some(status);
            } else if rc < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                return Err(Error::Pipe);
            }
        }
        Ok(self.status)
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        self.cancel();
        if self.status.is_none() {
            let mut status = 0;
            loop {
                let rc = unsafe { libc::waitpid(self.pid, &mut status, 0) };
                if rc >= 0
                    || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
                    break;
                }
            }
        }
    }
}

// The supervisor has its own session and is a subreaper. Borg leads a separate
// process group in that session; killing Borg's group leaves the supervisor
// alive to reap orphaned grandchildren. No process-wide parent subreaper flag.
// Confinement against deliberate session/group escape belongs to ADR 0008.
unsafe fn supervise(
    fds: [RawFd; 5],
    executable: *const libc::c_char,
    argv: *const *const libc::c_char,
    env: *const *const libc::c_char,
) -> ! {
    unsafe {
        for (src, dst) in fds.into_iter().zip([0, 1, 2, 3, 9]) {
            if libc::dup2(src, dst) < 0 {
                libc::_exit(120);
            }
        }
        // close_range is required; no best-effort leaked-descriptor fallback.
        if libc::syscall(libc::SYS_close_range, 4u32, 8u32, 0u32) != 0
            || libc::syscall(libc::SYS_close_range, 10u32, u32::MAX, 0u32) != 0
            || libc::setsid() < 0
            || libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) < 0
        {
            libc::_exit(120);
        }
        libc::umask(0o077);
        // Do not inherit caller-blocked termination signals into Borg/helpers.
        let mut signals: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut signals);
        if libc::sigprocmask(libc::SIG_SETMASK, &signals, std::ptr::null_mut()) < 0 {
            libc::_exit(120);
        }
        for signal in [
            libc::SIGCHLD,
            libc::SIGTERM,
            libc::SIGINT,
            libc::SIGHUP,
            libc::SIGPIPE,
        ] {
            libc::signal(signal, libc::SIG_DFL);
        }
        // A fixed cwd prevents accidental inherited directory authority.
        if libc::chdir(c"/".as_ptr()) < 0 {
            libc::_exit(120);
        }
        let leader = libc::fork();
        if leader < 0 {
            libc::_exit(120);
        }
        if leader == 0 {
            if libc::setpgid(0, 0) < 0 {
                libc::_exit(120);
            }
            libc::close(3);
            libc::execve(executable, argv, env);
            libc::_exit(120);
        }
        // Both sides setpgid, eliminating cancellation-before-group-creation.
        libc::setpgid(leader, leader);
        let mut leader_ok = false;
        let mut stopping = false;
        let mut ticks = 0;
        loop {
            let mut status = 0;
            let mut no_children = false;
            loop {
                let pid = libc::waitpid(-1, &mut status, libc::WNOHANG);
                if pid == leader {
                    leader_ok = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
                    stopping = true;
                }
                if pid > 0 {
                    continue;
                }
                if pid < 0 && *libc::__errno_location() == libc::ECHILD {
                    no_children = true;
                }
                break;
            }
            if no_children {
                libc::_exit(if leader_ok { 0 } else { 121 });
            }
            let mut byte = 0u8;
            let rc = libc::read(3, (&mut byte as *mut u8).cast(), 1);
            if rc >= 0 {
                stopping = true;
            } // parent closes control on any failure/drop
            if stopping {
                libc::kill(
                    -leader,
                    if ticks < 5 {
                        libc::SIGTERM
                    } else {
                        libc::SIGKILL
                    },
                );
                ticks += 1;
            }
            libc::poll(std::ptr::null_mut(), 0, 10);
        }
    }
}

// Internal transport input; only Runtime's fixed profiles reach this in a
// production build. The fake executable seam exists solely in unit tests.
pub(super) struct Invocation<'a> {
    pub executable: &'a std::path::Path,
    pub argv: &'a [String],
    pub pin: &'a File,
}

/// Callback sees stdout only, in <=64KiB chunks, and must return promptly.
/// Returning Err aborts/reaps; panic unwinding also triggers supervisor Drop.
/// Raw stderr is concurrently drained/discarded and never becomes an error.
pub(super) fn run(
    invocation: Invocation<'_>,
    environment: &super::BorgEnvironment<'_>,
    limits: &Limits,
    cancel: &Cancellation,
    stdout_limit: u64,
    consume: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let Invocation {
        executable,
        argv,
        pin,
    } = invocation;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let start = Instant::now();
    let exe = CString::new(executable.as_os_str().as_bytes()).map_err(|_| Error::InvalidInput)?;
    let args: Vec<CString> = std::iter::once(exe.clone())
        .chain(
            argv.iter()
                .map(|s| CString::new(s.as_bytes()))
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|_| Error::InvalidInput)?,
        )
        .collect();
    let env: Vec<CString> = environment
        .as_map()
        .iter()
        .map(|(k, v)| {
            let mut bytes = k.as_os_str().as_bytes().to_vec();
            bytes.push(b'=');
            bytes.extend(v.as_os_str().as_bytes());
            CString::new(bytes).map_err(|_| Error::InvalidInput)
        })
        .collect::<Result<_>>()?;
    let mut arg_ptrs: Vec<_> = args.iter().map(|s| s.as_ptr()).collect();
    arg_ptrs.push(std::ptr::null());
    let mut env_ptrs: Vec<_> = env.iter().map(|s| s.as_ptr()).collect();
    env_ptrs.push(std::ptr::null());
    let (out_read, out_write) = pipe()?;
    let (err_read, err_write) = pipe()?;
    let (ctrl_read, ctrl_write) = pipe()?;
    nonblocking(ctrl_read.as_raw_fd())?;
    nonblocking(out_read.as_raw_fd())?;
    nonblocking(err_read.as_raw_fd())?;
    let null = File::open("/dev/null").map_err(|_| Error::Spawn)?;
    // Duplicate pin/null above reserved FDs too, avoiding dup2 overlap.
    let dup = |fd: RawFd| -> Result<OwnedFd> {
        let n = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 20) };
        if n < 0 {
            Err(Error::Spawn)
        } else {
            Ok(unsafe { OwnedFd::from_raw_fd(n) })
        }
    };
    let null = dup(null.as_raw_fd())?;
    let pin = dup(pin.as_raw_fd())?;
    let fds = [
        null.as_raw_fd(),
        out_write.as_raw_fd(),
        err_write.as_raw_fd(),
        ctrl_read.as_raw_fd(),
        pin.as_raw_fd(),
    ];
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(Error::Spawn);
    }
    if pid == 0 {
        unsafe { supervise(fds, exe.as_ptr(), arg_ptrs.as_ptr(), env_ptrs.as_ptr()) }
    }
    let mut supervisor = Supervisor {
        pid,
        control: Some(ctrl_write),
        status: None,
    };
    drop((out_write, err_write, ctrl_read));
    let mut stdout = File::from(out_read);
    let mut stderr = File::from(err_read);
    let mut eof = [false; 2];
    let mut buf = [0u8; 65536];
    let mut total = 0u64;
    let mut failure = None;
    loop {
        if failure.is_none() {
            if cancel.is_cancelled() {
                failure = Some(Error::Cancelled);
            } else if start.elapsed() >= limits.timeout {
                failure = Some(Error::Timeout);
            }
        }
        if failure.is_some() {
            supervisor.cancel();
        }
        // One read per stream per iteration: a continuously noisy stderr cannot
        // starve stdout, cancellation, deadline checks or child status handling.
        for (i, stream) in [&mut stdout, &mut stderr].into_iter().enumerate() {
            if eof[i] {
                continue;
            }
            match stream.read(&mut buf) {
                Ok(0) => eof[i] = true,
                Ok(n) if i == 0 && failure.is_none() => {
                    total = total.saturating_add(n as u64);
                    if total > stdout_limit {
                        failure = Some(Error::OutputLimit);
                    } else if consume(&buf[..n]).is_err() {
                        failure = Some(Error::Consumer);
                    }
                }
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => {
                    failure = Some(Error::Pipe);
                    eof[i] = true;
                }
            }
        }
        if let Some(status) = supervisor.poll()? {
            if eof.iter().all(|v| *v) {
                return match failure {
                    Some(error) => Err(error),
                    None if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 => Ok(()),
                    None => Err(Error::ChildFailed),
                };
            }
        }
        let mut pollfds = [
            libc::pollfd {
                fd: stdout.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stderr.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        for (fd, done) in pollfds.iter_mut().zip(eof) {
            if done {
                fd.fd = -1;
            }
        }
        unsafe {
            libc::poll(pollfds.as_mut_ptr(), 2, 10);
        }
    }
}
