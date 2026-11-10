//! Process plumbing: a self-pipe that signal handlers, the panic hook, the
//! stdout writer and [`crate::AbortSink::fail`] write to, parent-death
//! detection, `/proc` identities, raw stdout writes and `poll`.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

/// Write end of the wake pipe; -1 until [`WakePipe::install`] ran. Never
/// closed: it lives as long as the process (a handler may still hold it).
static WAKE_WR: AtomicI32 = AtomicI32::new(-1);

/// Wake codes that are not signal numbers (signals are < 65).
pub(crate) const WAKE_FAIL: u8 = 0xF0;
pub(crate) const WAKE_PANIC: u8 = 0xF1;
pub(crate) const WAKE_STDOUT: u8 = 0xF2;

/// The signals that end the watcher with cleanup.
pub(crate) const EXIT_SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// Optional pid of the process the watcher serves (Claude's main process).
pub const PARENT_PID_ENV: &str = "CLAUDE_CU_PARENT_PID";

extern "C" fn on_signal(sig: libc::c_int) {
    // Async-signal-safe: one write(2) to a non-blocking pipe, errno preserved.
    // SAFETY: __errno_location returns this thread's errno slot.
    unsafe {
        let errno = *libc::__errno_location();
        wake(sig as u8);
        *libc::__errno_location() = errno;
    }
}

/// Wake the control loop. Async-signal-safe.
pub(crate) fn wake(code: u8) {
    let fd = WAKE_WR.load(Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: fd is the pipe's write end, valid for the process lifetime.
        // A full pipe (EAGAIN) loses the byte, but the pipe is then already
        // readable, which is all the reader needs to wake up.
        unsafe {
            libc::write(fd, (&raw const code).cast(), 1);
        }
    }
}

fn set_disposition(sig: libc::c_int, handler: libc::sighandler_t) -> io::Result<()> {
    // SAFETY: plain sigaction; the only custom handler is async-signal-safe.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        if libc::sigaction(sig, &sa, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Put SIGTERM/SIGINT/SIGHUP back to their default action (after cleanup).
pub(crate) fn restore_default_signals() {
    for sig in EXIT_SIGNALS {
        let _ = set_disposition(sig, libc::SIG_DFL);
    }
}

pub(crate) struct WakePipe {
    rd: OwnedFd,
}

impl WakePipe {
    /// Create the pipe, route SIGTERM/SIGINT/SIGHUP into it and ignore
    /// SIGPIPE (a closed stdout must be an EPIPE error, not a silent death).
    /// The handlers are process-wide, so they work whichever thread the
    /// kernel picks (including tokio workers the bridge created earlier).
    ///
    /// Meant once per process: a second call creates a new pipe and leaks
    /// the previous write end on purpose (a handler may be writing to it).
    pub(crate) fn install() -> io::Result<Self> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: fds has room for two descriptors.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: pipe2 just returned these two fresh descriptors.
        let rd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        WAKE_WR.store(fds[1], Ordering::SeqCst);
        set_disposition(libc::SIGPIPE, libc::SIG_IGN)?;
        for sig in EXIT_SIGNALS {
            set_disposition(sig, on_signal as *const () as libc::sighandler_t)?;
        }
        // Unblock them on this thread in case the bridge blocked them.
        // SAFETY: sigset manipulation on a local set.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for sig in EXIT_SIGNALS {
                libc::sigaddset(&mut set, sig);
            }
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        }
        Ok(Self { rd })
    }

    pub(crate) fn fd(&self) -> RawFd {
        self.rd.as_raw_fd()
    }

    /// Every pending code, oldest first.
    pub(crate) fn drain(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            // SAFETY: reading into a local buffer of the given length.
            let n = unsafe { libc::read(self.fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                out.extend_from_slice(&buf[..n as usize]);
                continue;
            }
            if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return out;
        }
    }
}

/// `(state, starttime)` from a `/proc/<pid>/stat` line: fields 3 and 22,
/// counted after the last `)` since comm may contain spaces and parentheses.
pub(crate) fn parse_stat(stat: &str) -> Option<(char, u64)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let start = fields.nth(18)?.parse().ok()?;
    Some((state, start))
}

/// Start time of a RUNNING process: `None` when it does not exist or is a
/// zombie (`Z`) / dead (`X`), i.e. killed but not reaped yet.
pub(crate) fn proc_start_time(pid: u32) -> Option<u64> {
    let (state, start) = parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)?;
    (!matches!(state, 'Z' | 'X')).then_some(start)
}

/// The process whose death ends the watcher.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Parent {
    /// Our parent at startup; gone when `getppid` changes (re-parented).
    Ppid(libc::pid_t),
    /// `CLAUDE_CU_PARENT_PID`; gone when it is no longer running with the
    /// same start time (dead, zombie, or pid reused).
    Pid { pid: u32, start: u64 },
    /// `CLAUDE_CU_PARENT_PID` named a process that was not running.
    AlreadyGone,
}

impl Parent {
    /// Record who to watch, then ask the kernel for SIGTERM when our direct
    /// parent dies (kept in both modes). With `CLAUDE_CU_PARENT_PID` set to a
    /// valid pid (> 1) that pid is watched instead of the `getppid` snapshot,
    /// which also covers a parent that died before we started and wrapper
    /// launchers between Claude and the bridge. An unparsable value is
    /// ignored.
    pub(crate) fn watch() -> Self {
        // SAFETY: getppid and prctl(PR_SET_PDEATHSIG) have no memory effects.
        let ppid = unsafe { libc::getppid() };
        unsafe {
            libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGTERM as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            );
        }
        let named = std::env::var(PARENT_PID_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|&p| p > 1);
        match named {
            Some(pid) => match proc_start_time(pid) {
                Some(start) => Parent::Pid { pid, start },
                None => Parent::AlreadyGone,
            },
            None => Parent::Ppid(ppid),
        }
    }

    pub(crate) fn gone(&self) -> bool {
        match *self {
            // SAFETY: getppid has no memory effects.
            Parent::Ppid(p) => unsafe { libc::getppid() != p },
            Parent::Pid { pid, start } => proc_start_time(pid) != Some(start),
            Parent::AlreadyGone => true,
        }
    }
}

/// Readiness of the two polled descriptors.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Ready {
    /// stdin is readable, at EOF, or invalid.
    pub(crate) input: bool,
    pub(crate) wake: bool,
}

pub(crate) fn poll2(input: RawFd, wake: RawFd, timeout: Duration) -> Ready {
    let mut fds = [
        libc::pollfd {
            fd: input,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    // SAFETY: fds is a valid array of two pollfd.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
    if n <= 0 {
        // Timeout or EINTR (a signal; its byte is in the wake pipe now and
        // the next poll returns at once).
        return Ready::default();
    }
    Ready {
        input: fds[0].revents != 0,
        wake: fds[1].revents != 0,
    }
}

/// What a raw read of stdin returned.
pub(crate) enum ReadResult {
    Data(usize),
    Eof,
    /// EAGAIN / EINTR: nothing this time.
    Again,
}

pub(crate) fn read_fd(fd: RawFd, buf: &mut [u8]) -> ReadResult {
    // SAFETY: reading into the caller's buffer of the given length.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n > 0 {
        return ReadResult::Data(n as usize);
    }
    if n == 0 {
        return ReadResult::Eof;
    }
    match io::Error::last_os_error().kind() {
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => ReadResult::Again,
        // EBADF / EIO and friends: stdin is unusable, which ends the watcher
        // like a closed pipe would.
        _ => ReadResult::Eof,
    }
}

/// Write all of `data` to `fd`, blocking (also when someone made the fd
/// non-blocking). Bypasses `std::io::stdout()` and its process-wide lock, so
/// a writer stuck on a full pipe never blocks std's exit-time flush.
pub(crate) fn write_all_fd(fd: RawFd, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        // SAFETY: writing from a valid slice of the given length.
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if n > 0 {
            data = &data[n as usize..];
            continue;
        }
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        let e = io::Error::last_os_error();
        match e.kind() {
            io::ErrorKind::Interrupted => {}
            io::ErrorKind::WouldBlock => {
                let mut p = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                // SAFETY: one valid pollfd.
                unsafe { libc::poll(&mut p, 1, -1) };
            }
            _ => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat_with_odd_comm() {
        let stat = "1234 (a) b (c) S 1 1234 1234 0 -1 4194560 100 0 0 0 0 0 0 0 20 0 1 0 987654 0";
        assert_eq!(parse_stat(stat), Some(('S', 987654)));
        let zombie = "77 (x) Z 1 77 77 0 -1 4194560 100 0 0 0 0 0 0 0 20 0 1 0 55 0";
        assert_eq!(parse_stat(zombie), Some(('Z', 55)));
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn own_process_is_running() {
        assert!(proc_start_time(std::process::id()).is_some());
    }
}
