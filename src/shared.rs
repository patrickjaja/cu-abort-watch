//! Reference counting for compositor-global bindings (sway, Hyprland, KDE).
//!
//! Several Claude profiles can run `abort-watch` at the same time, but a
//! compositor binding is user-global: two `bindsym Escape` calls overwrite
//! each other and the first `unbindsym` removes it for everyone. The holder
//! set coordinates them:
//!
//! - Directory: `$XDG_RUNTIME_DIR/claude-cu-esc/<mechanism>/`: both levels
//!   must be real directories (not symlinks) owned by us with no group/other
//!   access (a looser mode on our own directory is tightened to 0700). One
//!   file per holder named by its pid, holding the process start time
//!   (field 22 of `/proc/<pid>/stat`), plus `.lock` (opened `O_NOFOLLOW`).
//! - Every read-modify-write runs under an exclusive `flock` on `.lock`, and
//!   the bind/unbind call runs INSIDE that lock, so a joining process never
//!   races the last holder's unbind.
//! - A holder is alive only when `/proc/<pid>/stat` exists, is not a zombie
//!   (`Z`/`X`: killed but not reaped) and has the same start time; dead or
//!   reused pids and leftover `.<pid>.tmp` files are pruned on every lock.
//! - Joining registers BEFORE binding and leaving unbinds BEFORE
//!   unregistering. A crash inside either window leaves a stale holder file,
//!   so the next first holder sees [`HolderLock::pruned`] > 0 and knows a
//!   bind from a dead process may still be live.
//! - **First** = no other live holder: bind. **Last** = no other live holder
//!   when leaving: unbind.
//! - The holder set only decides who binds. Every armed process still gets
//!   its own abort events from its own IPC subscription / signal match, so one
//!   Escape stops Computer Use in every armed profile.
//!
//! Suspend/resume are not reference counted: a backend whose suspend has to
//! touch the shared binding affects the other holders while it lasts.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::protocol::Mechanism;
use crate::run::BoxError;
use crate::sys::proc_start_time;

const LOCK_FILE: &str = ".lock";
/// Lock wait for [`SharedBinding::lock`] / [`SharedBinding::acquire`].
const LOCK_WAIT: Duration = Duration::from_secs(3);
/// Lock wait on the exit path ([`SharedBinding::release`]): longer, since
/// giving up there may leave a bind behind.
const RELEASE_LOCK_WAIT: Duration = Duration::from_secs(10);
/// Directory under `$XDG_RUNTIME_DIR`.
pub const RUNTIME_SUBDIR: &str = "claude-cu-esc";

/// `<runtime_dir>/claude-cu-esc/<mechanism>`.
pub fn binding_dir(runtime_dir: &Path, mechanism: Mechanism) -> PathBuf {
    runtime_dir.join(RUNTIME_SUBDIR).join(mechanism.as_str())
}

fn denied(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{}: {why}", path.display()),
    )
}

/// `path` must be a real directory owned by us without group/other access.
/// A looser mode on our own directory is tightened to 0700.
fn check_private_dir(path: &Path) -> io::Result<()> {
    let md = fs::symlink_metadata(path)?;
    if md.file_type().is_symlink() {
        return Err(denied(path, "is a symlink"));
    }
    if !md.is_dir() {
        return Err(denied(path, "is not a directory"));
    }
    // SAFETY: getuid has no memory effects and cannot fail.
    if md.uid() != unsafe { libc::getuid() } {
        return Err(denied(path, "is owned by another user"));
    }
    if md.mode() & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        let md = fs::symlink_metadata(path)?;
        if md.file_type().is_symlink() || md.mode() & 0o077 != 0 {
            return Err(denied(path, "is accessible to other users"));
        }
    }
    Ok(())
}

/// The holder set of one shared binding, seen from one holder process.
#[derive(Debug, Clone)]
pub struct SharedBinding {
    dir: PathBuf,
    pid: u32,
    start: u64,
}

impl SharedBinding {
    /// The set for `mechanism` under `$XDG_RUNTIME_DIR`. Fails when the
    /// variable is unset, empty or relative (no safe place to coordinate),
    /// or when `claude-cu-esc` or `<mechanism>` fails the ownership checks.
    pub fn for_mechanism(mechanism: Mechanism) -> io::Result<Self> {
        let rt = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
        let dir = binding_dir(&rt, mechanism);
        DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        check_private_dir(&rt.join(RUNTIME_SUBDIR))?;
        Self::with_holder(dir, std::process::id())
    }

    /// A set in an explicit directory, held by this process.
    pub fn in_dir(dir: impl Into<PathBuf>) -> io::Result<Self> {
        Self::with_holder(dir, std::process::id())
    }

    /// A set held by another live process `pid` (tests, or a helper acting
    /// for its parent). `dir` is created (0700) and must pass the ownership
    /// checks.
    pub fn with_holder(dir: impl Into<PathBuf>, pid: u32) -> io::Result<Self> {
        let dir = dir.into();
        DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        check_private_dir(&dir)?;
        let start = proc_start_time(pid).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("process {pid} is not running"),
            )
        })?;
        Ok(Self { dir, pid, start })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn holder_path(&self) -> PathBuf {
        self.dir.join(self.pid.to_string())
    }

    /// Take the lock (waits up to 3 s) and prune dead holders.
    pub fn lock(&self) -> io::Result<HolderLock<'_>> {
        self.lock_wait(LOCK_WAIT)
    }

    fn lock_wait(&self, wait: Duration) -> io::Result<HolderLock<'_>> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.dir.join(LOCK_FILE))?;
        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(fs::TryLockError::WouldBlock) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("{} is locked", self.dir.display()),
                    ));
                }
                Err(fs::TryLockError::Error(e)) => return Err(e),
            }
        }
        let mut lock = HolderLock {
            set: self,
            _file: file,
            others: 0,
            pruned: 0,
            registered: false,
        };
        lock.scan()?;
        Ok(lock)
    }

    /// Live holders other than us, read WITHOUT the lock (0 when the
    /// directory cannot be read). Only for the give-up path of `release`.
    fn others_unlocked(&self) -> usize {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return 0;
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let pid = e.file_name().to_str()?.parse::<u32>().ok()?;
                let recorded = fs::read_to_string(e.path())
                    .ok()?
                    .trim()
                    .parse::<u64>()
                    .ok()?;
                (pid != self.pid && proc_start_time(pid) == Some(recorded)).then_some(())
            })
            .count()
    }

    /// Join: under the lock, register, then call `bind` when no other live
    /// holder exists. Returns whether we were first. When `bind` fails we
    /// unregister again and return its error. Re-joining while registered
    /// never binds again.
    pub fn acquire(&self, bind: impl FnOnce() -> Result<(), BoxError>) -> Result<bool, BoxError> {
        let mut lock = self.lock()?;
        let first = lock.is_first() && !lock.is_registered();
        lock.register()?;
        if first && let Err(e) = bind() {
            let _ = lock.unregister();
            return Err(e);
        }
        Ok(first)
    }

    /// Leave: under the lock (waits up to 10 s), call `unbind` when no other
    /// live holder is left, then unregister. Idempotent: leaving without
    /// having joined still unbinds when nobody else holds it.
    ///
    /// - `Ok(last)`: `unbind` ran exactly when `last` is true.
    /// - `Err(e)`: the lock or the directory scan failed. `unbind` has
    ///   ALREADY run (best effort) unless another live holder could be seen
    ///   without the lock. The backend only logs the error and carries on
    ///   with the rest of its disarm; it must not unbind again.
    ///
    /// A failure to remove our own holder file is ignored (it is pruned as
    /// stale once we exit).
    pub fn release(&self, unbind: impl FnOnce()) -> io::Result<bool> {
        match self.lock_wait(RELEASE_LOCK_WAIT) {
            Ok(mut lock) => {
                let last = lock.others() == 0;
                if last {
                    unbind();
                }
                let _ = lock.unregister();
                Ok(last)
            }
            Err(e) => {
                if self.others_unlocked() == 0 {
                    unbind();
                }
                let _ = fs::remove_file(self.holder_path());
                Err(e)
            }
        }
    }
}

/// The locked holder set. Dropping it releases the lock.
#[derive(Debug)]
pub struct HolderLock<'a> {
    set: &'a SharedBinding,
    _file: File,
    others: usize,
    pruned: usize,
    registered: bool,
}

impl HolderLock<'_> {
    /// Classify every entry. Tolerant: an entry it cannot read or remove is
    /// skipped (and never counted as another holder); only an unreadable
    /// directory is an error.
    fn scan(&mut self) -> io::Result<()> {
        for entry in fs::read_dir(&self.set.dir)? {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') && name.ends_with(".tmp") {
                // A register that crashed between write and rename; under
                // the lock nobody else is mid-register.
                let _ = fs::remove_file(entry.path());
                continue;
            }
            let Ok(pid) = name.parse::<u32>() else {
                continue;
            };
            let recorded = fs::read_to_string(entry.path())
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok());
            let alive = recorded.is_some() && recorded == proc_start_time(pid);
            if !alive {
                if fs::remove_file(entry.path()).is_ok() {
                    self.pruned += 1;
                }
            } else if pid == self.set.pid && recorded == Some(self.set.start) {
                self.registered = true;
            } else {
                self.others += 1;
            }
        }
        Ok(())
    }

    /// Live holders other than this process.
    pub fn others(&self) -> usize {
        self.others
    }

    /// No other live holder: the caller is (or would be) the first.
    pub fn is_first(&self) -> bool {
        self.others == 0
    }

    /// Stale holder files removed by this lock (crashed watchers: their
    /// binding may still be in the compositor).
    pub fn pruned(&self) -> usize {
        self.pruned
    }

    /// Whether this process is in the set.
    pub fn is_registered(&self) -> bool {
        self.registered
    }

    /// Add this process (idempotent).
    pub fn register(&mut self) -> io::Result<()> {
        if self.registered {
            return Ok(());
        }
        let tmp = self.set.dir.join(format!(".{}.tmp", self.set.pid));
        let res = fs::write(&tmp, format!("{}\n", self.set.start))
            .and_then(|()| fs::rename(&tmp, self.set.holder_path()));
        if res.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        res?;
        self.registered = true;
        Ok(())
    }

    /// Remove this process (idempotent).
    pub fn unregister(&mut self) -> io::Result<()> {
        match fs::remove_file(self.set.holder_path()) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        self.registered = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "cu-abort-watch-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Sleeper(Child);
    impl Sleeper {
        fn new() -> Self {
            Self(Command::new("sleep").arg("30").spawn().unwrap())
        }
        fn pid(&self) -> u32 {
            self.0.id()
        }
        /// SIGKILL without reaping: the process stays a zombie.
        fn kill_unreaped(&self) {
            // SAFETY: signalling our own child.
            unsafe { libc::kill(self.pid() as libc::pid_t, libc::SIGKILL) };
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let stat = fs::read_to_string(format!("/proc/{}/stat", self.pid())).unwrap();
                if crate::sys::parse_stat(&stat).unwrap().0 == 'Z' {
                    return;
                }
                assert!(Instant::now() < deadline, "child did not become a zombie");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn dead_pid() -> u32 {
        let mut c = Command::new("true").spawn().unwrap();
        let pid = c.id();
        c.wait().unwrap();
        pid
    }

    fn mode(p: &Path) -> u32 {
        fs::symlink_metadata(p).unwrap().mode() & 0o777
    }

    #[test]
    fn binding_dir_layout() {
        assert_eq!(
            binding_dir(Path::new("/run/user/1000"), Mechanism::Sway),
            PathBuf::from("/run/user/1000/claude-cu-esc/sway")
        );
    }

    #[test]
    fn first_binds_last_unbinds() {
        let tmp = TempDir::new();
        let dir = binding_dir(&tmp.0, Mechanism::Hyprland);
        let (pa, pb) = (Sleeper::new(), Sleeper::new());
        let a = SharedBinding::with_holder(&dir, pa.pid()).unwrap();
        let b = SharedBinding::with_holder(&dir, pb.pid()).unwrap();
        let mut binds = 0;
        let mut unbinds = 0;

        assert!(
            a.acquire(|| {
                binds += 1;
                Ok(())
            })
            .unwrap()
        );
        assert!(
            !b.acquire(|| {
                binds += 1;
                Ok(())
            })
            .unwrap()
        );
        assert_eq!(binds, 1);

        // Re-joining never binds again.
        assert!(!a.acquire(|| panic!("bound twice")).unwrap());

        assert!(!a.release(|| unbinds += 1).unwrap());
        assert_eq!(unbinds, 0);
        assert!(b.release(|| unbinds += 1).unwrap());
        assert_eq!(unbinds, 1);

        // Nothing left behind but the lock file.
        let left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(left, vec![LOCK_FILE.to_owned()]);
    }

    #[test]
    fn rejoin_while_alone_does_not_rebind() {
        let tmp = TempDir::new();
        let me = Sleeper::new();
        let set =
            SharedBinding::with_holder(binding_dir(&tmp.0, Mechanism::Sway), me.pid()).unwrap();
        assert!(set.acquire(|| Ok(())).unwrap());
        assert!(!set.acquire(|| panic!("bound twice")).unwrap());
    }

    #[test]
    fn stale_holders_are_pruned() {
        let tmp = TempDir::new();
        let dir = binding_dir(&tmp.0, Mechanism::Sway);
        let me = Sleeper::new();
        let other = Sleeper::new();
        let set = SharedBinding::with_holder(&dir, me.pid()).unwrap();
        // A dead pid, a live pid with the wrong start time (pid reuse), and
        // a garbage file: all stale. A leftover tmp file is removed without
        // counting. A live pid with the right start: alive.
        fs::write(dir.join(dead_pid().to_string()), "123\n").unwrap();
        fs::write(dir.join(other.pid().to_string()), "1\n").unwrap();
        fs::write(dir.join("999999999"), "not a number").unwrap();
        fs::write(dir.join("not-a-holder"), "").unwrap();
        fs::write(dir.join(".4242.tmp"), "1\n").unwrap();
        {
            let lock = set.lock().unwrap();
            assert_eq!(lock.pruned(), 3);
            assert!(lock.is_first());
            assert!(!lock.is_registered());
        }
        assert!(!dir.join(".4242.tmp").exists());
        let live = SharedBinding::with_holder(&dir, other.pid()).unwrap();
        live.lock().unwrap().register().unwrap();
        let lock = set.lock().unwrap();
        assert_eq!(lock.pruned(), 0);
        assert_eq!(lock.others(), 1);
        drop(lock);

        // The live holder dies without releasing: we become first again.
        drop(other);
        let lock = set.lock().unwrap();
        assert_eq!(lock.pruned(), 1);
        assert!(lock.is_first());
        assert!(dir.join("not-a-holder").exists());
    }

    #[test]
    fn zombie_holder_does_not_block_the_last_unbind() {
        let tmp = TempDir::new();
        let dir = binding_dir(&tmp.0, Mechanism::Sway);
        let (pa, pb) = (Sleeper::new(), Sleeper::new());
        let a = SharedBinding::with_holder(&dir, pa.pid()).unwrap();
        let b = SharedBinding::with_holder(&dir, pb.pid()).unwrap();
        assert!(a.acquire(|| Ok(())).unwrap());
        assert!(!b.acquire(|| Ok(())).unwrap());
        // `a` is SIGKILLed and nobody reaps it: /proc/<pid>/stat says Z.
        pa.kill_unreaped();
        let mut unbinds = 0;
        assert!(b.release(|| unbinds += 1).unwrap());
        assert_eq!(unbinds, 1);
    }

    #[test]
    fn failed_bind_does_not_register() {
        let tmp = TempDir::new();
        let dir = binding_dir(&tmp.0, Mechanism::Kglobalaccel);
        let me = Sleeper::new();
        let set = SharedBinding::with_holder(&dir, me.pid()).unwrap();
        assert!(set.acquire(|| Err("no".into())).is_err());
        assert!(!set.lock().unwrap().is_registered());
        assert_eq!(set.lock().unwrap().pruned(), 0);
    }

    #[test]
    fn failed_register_does_not_bind() {
        // SAFETY: geteuid has no memory effects.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: root ignores directory permissions");
            return;
        }
        let tmp = TempDir::new();
        let dir = binding_dir(&tmp.0, Mechanism::Sway);
        let me = Sleeper::new();
        let set = SharedBinding::with_holder(&dir, me.pid()).unwrap();
        drop(set.lock().unwrap()); // create .lock while writable
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let mut binds = 0;
        let res = set.acquire(|| {
            binds += 1;
            Ok(())
        });
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(res.is_err());
        assert_eq!(binds, 0, "bound without being registered");
        assert!(!set.lock().unwrap().is_registered());
    }

    #[test]
    fn crash_inside_join_or_leave_window_is_visible() {
        // Join: registered, crashed before (or while) binding. Leave: unbound,
        // crashed before unregistering. Both leave a holder file of a dead
        // process, which the next joiner sees as pruned() > 0.
        let tmp = TempDir::new();
        let dir = binding_dir(&tmp.0, Mechanism::Hyprland);
        let crashed = Sleeper::new();
        SharedBinding::with_holder(&dir, crashed.pid())
            .unwrap()
            .lock()
            .unwrap()
            .register()
            .unwrap();
        drop(crashed);
        let me = Sleeper::new();
        let set = SharedBinding::with_holder(&dir, me.pid()).unwrap();
        let lock = set.lock().unwrap();
        assert_eq!(lock.pruned(), 1);
        assert!(lock.is_first());
    }

    #[test]
    fn release_unbinds_when_the_lock_cannot_be_taken() {
        let tmp = TempDir::new();
        let dir = binding_dir(&tmp.0, Mechanism::Sway);
        let me = Sleeper::new();
        let set = SharedBinding::with_holder(&dir, me.pid()).unwrap();
        assert!(set.acquire(|| Ok(())).unwrap());
        // Replace .lock with a symlink: O_NOFOLLOW refuses it, so release
        // cannot coordinate and must still unbind (nobody else is visible).
        fs::remove_file(dir.join(LOCK_FILE)).unwrap();
        std::os::unix::fs::symlink(tmp.0.join("elsewhere"), dir.join(LOCK_FILE)).unwrap();
        let mut unbinds = 0;
        assert!(set.release(|| unbinds += 1).is_err());
        assert_eq!(unbinds, 1);
        assert!(!dir.join(me.pid().to_string()).exists());
        assert!(!tmp.0.join("elsewhere").exists());

        // With another live holder visible, the give-up path does not unbind.
        let other = Sleeper::new();
        fs::write(
            dir.join(other.pid().to_string()),
            format!("{}\n", proc_start_time(other.pid()).unwrap()),
        )
        .unwrap();
        let mut unbinds = 0;
        assert!(set.release(|| unbinds += 1).is_err());
        assert_eq!(unbinds, 0);
    }

    #[test]
    fn ownership_checks() {
        let tmp = TempDir::new();
        // A loose mode on our own directory is tightened.
        let loose = tmp.0.join("loose");
        fs::create_dir(&loose).unwrap();
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o755)).unwrap();
        SharedBinding::in_dir(&loose).unwrap();
        assert_eq!(mode(&loose), 0o700);
        // A symlinked directory is refused.
        let real = tmp.0.join("real");
        fs::create_dir(&real).unwrap();
        let link = tmp.0.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = SharedBinding::in_dir(&link).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        // A file is refused.
        let file = tmp.0.join("file");
        fs::write(&file, "").unwrap();
        assert!(SharedBinding::in_dir(&file).is_err());
        // Created directories are 0700.
        let dir = binding_dir(&tmp.0, Mechanism::Sway);
        SharedBinding::in_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(dir.parent().unwrap()), 0o700);
    }

    #[test]
    fn lock_is_exclusive_across_handles() {
        let tmp = TempDir::new();
        let dir = binding_dir(&tmp.0, Mechanism::Sway);
        let set = SharedBinding::in_dir(&dir).unwrap();
        let held = set.lock().unwrap();
        // A second open file description cannot take it while held.
        let f = File::open(dir.join(LOCK_FILE)).unwrap();
        assert!(matches!(f.try_lock(), Err(fs::TryLockError::WouldBlock)));
        drop(held);
        // Retry briefly: a test on another thread forking at that moment
        // holds a copy of the descriptor (and so the flock) until its exec.
        let deadline = Instant::now() + Duration::from_secs(2);
        while f.try_lock().is_err() {
            assert!(Instant::now() < deadline, "lock not released");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn dead_holder_rejected() {
        let tmp = TempDir::new();
        assert!(SharedBinding::with_holder(tmp.0.join("x"), dead_pid()).is_err());
    }
}
