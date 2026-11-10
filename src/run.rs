//! The runner: arms a [`Backend`], owns stdin/stdout/signals/parent death,
//! and disarms exactly once on every exit path it can see.
//!
//! stdout is written by a dedicated writer thread from a queue, so neither
//! the control loop nor a backend thread ever blocks on a full pipe: SIGTERM,
//! EOF and parent death always reach `disarm`.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitCode;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Once};
use std::time::{Duration, Instant};

use crate::protocol::{Command, Event, Mechanism, parse_command};
use crate::sys::{self, Parent, ReadResult, WAKE_FAIL, WAKE_PANIC, WAKE_STDOUT, WakePipe};

/// Error type of the backend hooks. `?` works on `io::Error`, `String`,
/// `&str`, `anyhow::Error` and any `std::error::Error`.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// What a successful [`Backend::arm`] reports; becomes the `ready` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ready {
    pub mechanism: Mechanism,
    /// True when an Escape injected by the model (XTEST, virtual keyboard,
    /// portal) can never trigger an `abort`, so the JS side does not need
    /// to send `suspend` around it.
    pub synthetic_safe: bool,
}

/// The compositor-specific half of `abort-watch`.
///
/// All methods run on the thread that called [`run`]. A backend that needs
/// an event loop starts it in [`Backend::arm`] (a thread, or a task on a
/// runtime it owns) and reports user presses through the [`AbortSink`].
///
/// Signals, stdin EOF and parent death are only acted on between these
/// calls. Every IPC call made in `arm`, `suspend`, `resume` and `disarm`
/// (compositor socket, D-Bus, a spawned `swaymsg` / `hyprctl`) must carry a
/// timeout (a few seconds at most), or a wedged compositor wedges the
/// watcher and its cleanup.
pub trait Backend {
    /// Start watching. On `Ok` the watcher must be fully armed. On `Err`
    /// the backend must have undone everything it did: [`run`] prints the
    /// `error` event and exits 2 WITHOUT calling [`Backend::disarm`]. A
    /// panic here is followed by a best-effort `disarm`.
    fn arm(&mut self, sink: AbortSink) -> Result<Ready, BoxError>;

    /// Stop consuming Escape (the model is about to inject one). Reporting
    /// is already muted by [`run`] before this is called, so a
    /// `synthetic_safe` backend can keep the default no-op. An `Err` is
    /// reported as an `error` event, the `ack` follows and the watcher keeps
    /// running (still muted).
    fn suspend(&mut self) -> Result<(), BoxError> {
        Ok(())
    }

    /// Undo [`Backend::suspend`]. An `Err` is fatal: no `ack`, an `error`
    /// event, [`Backend::disarm`] and exit 1.
    fn resume(&mut self) -> Result<(), BoxError> {
        Ok(())
    }

    /// Remove everything [`Backend::arm`] set up. Called at most once, also
    /// after a panic. Must not block for long; errors are the backend's to
    /// log (stderr) since the process is exiting anyway.
    fn disarm(&mut self);
}

/// Handle a backend uses to report the user's Escape. Cheap to clone,
/// `Send + Sync`, usable from any thread or async task. Never blocks on
/// stdout.
#[derive(Clone)]
pub struct AbortSink {
    shared: Arc<Shared>,
}

/// Output state, all under one lock: an `abort` is queued only while not
/// suspended and not muted, so it is either ahead of the `suspend` ack in
/// the queue or dropped.
#[derive(Default)]
struct Out {
    suspended: bool,
    /// No `abort` is queued: before `ready`, and from the moment the control
    /// loop decided to exit.
    muted: bool,
    queue: VecDeque<Event>,
    /// The writer is in the middle of a line.
    writing: bool,
    /// stdout failed; nothing more is queued.
    broken: bool,
}

/// Queue bound: lines beyond it are dropped (only reachable when the caller
/// floods stdin with commands and never reads stdout).
const QUEUE_MAX: usize = 1024;
/// How long [`run`] waits for queued lines to reach stdout before returning.
const FLUSH_WAIT: Duration = Duration::from_secs(1);

struct Shared {
    out: Mutex<Out>,
    changed: Condvar,
    failure: Mutex<Option<String>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Shared {
    fn new() -> Self {
        Self {
            out: Mutex::new(Out {
                muted: true,
                ..Out::default()
            }),
            changed: Condvar::new(),
            failure: Mutex::new(None),
        }
    }

    fn push(&self, out: &mut Out, ev: Event) {
        if out.broken || out.queue.len() >= QUEUE_MAX {
            return;
        }
        out.queue.push_back(ev);
        self.changed.notify_all();
    }

    fn enqueue(&self, ev: Event) {
        let mut out = lock(&self.out);
        self.push(&mut out, ev);
    }

    fn mute(&self) {
        lock(&self.out).muted = true;
    }

    /// Wait until the queue is written (or stdout broke), at most `wait`.
    fn flush(&self, wait: Duration) {
        let deadline = Instant::now() + wait;
        let mut out = lock(&self.out);
        while !out.broken && (out.writing || !out.queue.is_empty()) {
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            out = self
                .changed
                .wait_timeout(out, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    fn writer(self: Arc<Self>) {
        loop {
            let ev = {
                let mut out = lock(&self.out);
                loop {
                    if let Some(ev) = out.queue.pop_front() {
                        out.writing = true;
                        break ev;
                    }
                    out = self.changed.wait(out).unwrap_or_else(|p| p.into_inner());
                }
            };
            let res = sys::write_all_fd(libc::STDOUT_FILENO, ev.line().as_bytes());
            let mut out = lock(&self.out);
            out.writing = false;
            if res.is_err() {
                out.broken = true;
                out.queue.clear();
            }
            drop(out);
            self.changed.notify_all();
            if res.is_err() {
                sys::wake(WAKE_STDOUT);
                return;
            }
        }
    }
}

impl AbortSink {
    /// The user pressed Escape: queue `{"event":"abort"}`, unless the watcher
    /// is suspended or exiting (then the press is dropped). Presses while an
    /// `abort` is still queued are coalesced into it. Never blocks.
    pub fn abort(&self) {
        let mut out = lock(&self.shared.out);
        if out.suspended || out.muted || out.queue.contains(&Event::Abort) {
            return;
        }
        self.shared.push(&mut out, Event::Abort);
    }

    /// The watcher broke for good (compositor IPC closed, D-Bus connection
    /// lost, ...): [`run`] writes `{"event":"error","message":...}`, calls
    /// [`Backend::disarm`] and exits 1. Only the first message is kept.
    pub fn fail(&self, message: impl Into<String>) {
        {
            let mut f = lock(&self.shared.failure);
            if f.is_none() {
                *f = Some(message.into());
            }
        }
        sys::wake(WAKE_FAIL);
    }

    /// Whether the watcher is currently suspended.
    pub fn is_suspended(&self) -> bool {
        lock(&self.shared.out).suspended
    }
}

/// Why the control loop ended.
#[derive(Debug)]
enum Exit {
    Eof,
    Signal,
    ParentDied,
    Failed(String),
    Panicked,
    StdoutClosed,
}

/// Exit code when arming is impossible.
const EXIT_ARM_FAILED: u8 = 2;
/// Exit code after a runtime failure (backend failure, panic, stdout gone).
const EXIT_FAILED: u8 = 1;

/// How often the parent is re-checked when nothing else happens.
const PARENT_POLL: Duration = Duration::from_millis(500);
/// A stdin line longer than this is discarded (with an error event).
const MAX_LINE: usize = 4096;

/// Owns one armed backend and disarms it at most once, also when unwinding.
struct Armed<B: Backend> {
    backend: B,
    armed: bool,
}

impl<B: Backend> Armed<B> {
    fn disarm_once(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        if catch_unwind(AssertUnwindSafe(|| self.backend.disarm())).is_err() {
            eprintln!("cu-abort-watch: backend disarm panicked");
        }
    }
}

impl<B: Backend> Drop for Armed<B> {
    fn drop(&mut self) {
        self.disarm_once();
    }
}

fn install_panic_hook() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            prev(info);
            // A panic on a backend thread would otherwise leave a watcher
            // that silently stopped reporting; wake the loop to clean up.
            sys::wake(WAKE_PANIC);
        }));
    });
}

fn error(message: impl Into<String>) -> Event {
    Event::Error {
        message: message.into(),
    }
}

/// Run `abort-watch` with `backend` until stdin EOF, SIGTERM/SIGINT/SIGHUP,
/// parent death, a backend failure or a panic. Returns the process exit
/// code: 0 after a requested stop, 1 after a runtime failure, 2 when arming
/// failed.
///
/// Call it once per process, from a thread that is not inside an async
/// runtime context (plain `main`, `std::thread::spawn`, or
/// `tokio::task::spawn_blocking`): it blocks. Nothing else may write to
/// stdout while it runs. Return its code from `main` right away: on return
/// SIGTERM/SIGINT/SIGHUP are back to their default action (SIGPIPE stays
/// ignored) and lines still queued after a 1 s flush wait are lost.
pub fn run<B: Backend>(backend: B) -> ExitCode {
    let shared = Arc::new(Shared::new());
    let writer = {
        let s = shared.clone();
        std::thread::Builder::new()
            .name("abort-watch-stdout".into())
            .spawn(move || s.writer())
    };
    if let Err(e) = writer {
        eprintln!("cu-abort-watch: cannot start the stdout writer: {e}");
        return ExitCode::from(EXIT_ARM_FAILED);
    }
    let finish = |code: u8| {
        shared.flush(FLUSH_WAIT);
        sys::restore_default_signals();
        ExitCode::from(code)
    };

    let wake = match WakePipe::install() {
        Ok(w) => w,
        Err(e) => {
            shared.enqueue(error(format!("signal setup failed: {e}")));
            return finish(EXIT_ARM_FAILED);
        }
    };
    let parent = Parent::watch();
    if parent.gone() {
        // The parent died before we could arm: nothing to clean up.
        return finish(0);
    }
    install_panic_hook();

    let sink = AbortSink {
        shared: shared.clone(),
    };
    let mut armed = Armed {
        backend,
        armed: false,
    };

    let ready = match catch_unwind(AssertUnwindSafe(|| armed.backend.arm(sink))) {
        Ok(Ok(ready)) => ready,
        Ok(Err(e)) => {
            shared.enqueue(error(e.to_string()));
            return finish(EXIT_ARM_FAILED);
        }
        Err(_) => {
            // Unknown partial state: best-effort cleanup.
            shared.enqueue(error("arming panicked"));
            armed.armed = true;
            armed.disarm_once();
            return finish(EXIT_ARM_FAILED);
        }
    };
    armed.armed = true;
    {
        // `ready` goes first; aborts are accepted from here on.
        let mut out = lock(&shared.out);
        shared.push(
            &mut out,
            Event::Ready {
                mechanism: ready.mechanism,
                synthetic_safe: ready.synthetic_safe,
            },
        );
        out.muted = false;
    }

    let exit = catch_unwind(AssertUnwindSafe(|| {
        control_loop(&mut armed.backend, &shared, &wake, parent)
    }))
    .unwrap_or(Exit::Panicked);

    // No abort after this point: none follows an error line or races disarm.
    shared.mute();
    let code = match &exit {
        Exit::Eof | Exit::Signal | Exit::ParentDied => 0,
        Exit::StdoutClosed => EXIT_FAILED,
        Exit::Failed(msg) => {
            shared.enqueue(error(msg.clone()));
            EXIT_FAILED
        }
        Exit::Panicked => {
            shared.enqueue(error("abort watcher panicked"));
            EXIT_FAILED
        }
    };
    armed.disarm_once();
    finish(code)
}

fn control_loop<B: Backend>(
    backend: &mut B,
    shared: &Shared,
    wake: &WakePipe,
    parent: Parent,
) -> Exit {
    let mut line: Vec<u8> = Vec::new();
    let mut overlong = false;
    let mut buf = [0u8; 1024];
    let command = |line: &[u8], overlong: bool| {
        if overlong {
            Command::Unknown("<line too long>".to_owned())
        } else {
            parse_command(line)
        }
    };
    loop {
        let ready = sys::poll2(libc::STDIN_FILENO, wake.fd(), PARENT_POLL);

        if ready.wake {
            for code in wake.drain() {
                match code {
                    WAKE_FAIL => {
                        let msg = lock(&shared.failure)
                            .take()
                            .unwrap_or_else(|| "abort watcher failed".to_owned());
                        return Exit::Failed(msg);
                    }
                    WAKE_PANIC => return Exit::Panicked,
                    WAKE_STDOUT => return Exit::StdoutClosed,
                    c if sys::EXIT_SIGNALS.contains(&libc::c_int::from(c)) => {
                        return Exit::Signal;
                    }
                    _ => {}
                }
            }
        }
        if parent.gone() {
            return Exit::ParentDied;
        }
        if !ready.input {
            continue;
        }

        let n = match sys::read_fd(libc::STDIN_FILENO, &mut buf) {
            ReadResult::Data(n) => n,
            ReadResult::Again => continue,
            ReadResult::Eof => {
                // A last unterminated line is still a command.
                if (!line.is_empty() || overlong)
                    && let Some(exit) = handle(backend, shared, command(&line, overlong))
                {
                    return exit;
                }
                return Exit::Eof;
            }
        };
        for &b in &buf[..n] {
            if b != b'\n' {
                if line.len() < MAX_LINE {
                    line.push(b);
                } else {
                    overlong = true;
                }
                continue;
            }
            let cmd = command(&line, overlong);
            line.clear();
            overlong = false;
            if let Some(exit) = handle(backend, shared, cmd) {
                return exit;
            }
        }
    }
}

/// Apply one command. `Some` ends the loop. Never blocks on stdout.
fn handle<B: Backend>(backend: &mut B, shared: &Shared, cmd: Command) -> Option<Exit> {
    match cmd {
        Command::Empty => None,
        Command::Unknown(s) => {
            shared.enqueue(error(format!("unknown command: {s}")));
            None
        }
        Command::Suspend => {
            let newly = !std::mem::replace(&mut lock(&shared.out).suspended, true);
            if newly && let Err(e) = backend.suspend() {
                shared.enqueue(error(format!("suspend failed: {e}")));
            }
            shared.enqueue(Event::Ack { cmd: "suspend" });
            None
        }
        Command::Resume => {
            if lock(&shared.out).suspended {
                // Re-arm first, unmute after: a late-delivered injected
                // Escape is still dropped.
                if let Err(e) = backend.resume() {
                    return Some(Exit::Failed(format!("resume failed: {e}")));
                }
                lock(&shared.out).suspended = false;
            }
            shared.enqueue(Event::Ack { cmd: "resume" });
            None
        }
    }
}
