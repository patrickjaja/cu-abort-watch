# cu-abort-watch

Shared Rust library behind the `abort-watch` subcommand of the [claude-desktop-extra](https://github.com/patrickjaja/claude-desktop-extra) Computer Use bridges (`x11-bridge`, `wlroots-bridge`, `kwin-portal-bridge`). It gives Linux the "press Escape to stop Computer Use" signal.

Each bridge supplies the compositor-specific watcher (a `Backend`): XInput2 raw keys on X11, per-device binds over sway or Hyprland IPC, a kglobalaccel shortcut on KDE. This crate owns everything else: the wire protocol, `suspend` / `resume`, signals, parent-death detection, exactly-once cleanup, reference counting of compositor-global binds across Claude profiles, and the `abortWatch` key of `doctor`.

Pure Rust (`libc`, `serde`, `serde_json`), no async runtime, no C dependencies: builds static for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`.

## Contract (optional part of the bridge CLI contract)

Claude Desktop spawns `<bridge> abort-watch` while Computer Use holds the lock and talks to it over pipes.

stdout, one JSON object per line. Lines are written by a dedicated writer thread, so a caller that stops reading never blocks the watcher: signals, EOF and parent death still reach cleanup with a full pipe.

| Line | When |
|---|---|
| `{"event":"ready","mechanism":"xi2\|sway\|hyprland\|kglobalaccel","syntheticSafe":<bool>}` | once, after the watcher is armed |
| `{"event":"abort"}` | the user pressed Escape (never while suspended, never after an `error` that ends the watcher). Presses while an `abort` is still unwritten are coalesced into it |
| `{"event":"ack","cmd":"suspend"}` / `{"event":"ack","cmd":"resume"}` | after a control line took effect (not after a failed `resume`, see below) |
| `{"event":"error","message":"..."}` | arming impossible (then exit 2, nothing left behind); a runtime failure (then cleanup, exit 1); an unknown command or a failed `suspend` (no exit) |

`syntheticSafe: true` means an Escape injected by the model (XTEST, virtual keyboard, portal) can never produce an `abort`, so the caller does not need `suspend`.

stdin, one command per line (surrounding whitespace and `\r` ignored, blank lines ignored without reply):

- `suspend`: stop reporting and consuming Escape. Used around an Escape the model injects when the mechanism is not `syntheticSafe`. Reporting is muted before the backend is asked to release the key, so the `ack` means both are done, and no `abort` follows `ack suspend` until the next `resume`.
- `resume`: consume and report again. The backend re-arms first and reporting is unmuted after, so a late-delivered injected Escape is still dropped. A real user press may be reported between the two, so an `abort` can legitimately arrive BEFORE `ack resume`. `resume` while not suspended is acked without effect.
- Not nested: a second `suspend` is acked without effect and one `resume` ends any number of them. The caller serializes its suspend windows (one injected Escape at a time).
- Anything else: `{"event":"error","message":"unknown command: <first 64 chars>"}`, the watcher keeps running. A line over 4096 bytes is reported as `unknown command: <line too long>`.

A `suspend` the backend fails to apply is reported with an `error` event, then acked (reporting stays muted either way). A `resume` the backend fails to apply gets NO ack: an `error` event (`resume failed: ...`), cleanup, exit 1. A caller waiting for `ack resume` must also treat an `error` line or the process exit as the end of that wait, and restart the watcher.

Parent: the watcher ends when its parent dies. When the environment variable `CLAUDE_CU_PARENT_PID` holds a pid (> 1), that process is watched instead of the direct parent (by pid and start time, so a zombie or a reused pid counts as dead); a value that does not parse is ignored. Claude Desktop should set it to its own pid (`process.pid`) in the spawn environment; this covers wrapper launchers between Claude and the bridge and a parent that died before the watcher started (then the watcher exits 0 without arming or printing anything). `PR_SET_PDEATHSIG` on the direct parent stays active in both modes.

Exit, always after the backend's `disarm` (called exactly once):

| Cause | Exit code |
|---|---|
| stdin EOF (a last line without newline still runs) | 0 |
| SIGTERM, SIGINT, SIGHUP | 0 |
| parent death (`PR_SET_PDEATHSIG` = SIGTERM, plus a check every 500 ms of `CLAUDE_CU_PARENT_PID` or of `getppid`) | 0 |
| backend failure (`AbortSink::fail`) | 1, after an `error` event |
| panic on the calling thread or any backend thread | 1, after `{"event":"error","message":"abort watcher panicked"}` |
| stdout closed (EPIPE; SIGPIPE is ignored) | 1 |
| arming failed | 2, after an `error` event; `disarm` is NOT called |

Before returning, `run` waits up to 1 s for queued lines to reach stdout. Not covered: SIGKILL and a build with `panic = "abort"` leave the binding in place. Shared binds are healed by the next first holder (see below).

## Use from a bridge

```toml
[dependencies]
# local development
cu-abort-watch = { path = "../cu-abort-watch" }
# at ship time
# cu-abort-watch = { git = "https://github.com/patrickjaja/cu-abort-watch", rev = "<pinned rev>" }
```

```rust
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

#[non_exhaustive]
pub enum Mechanism { Xi2, Sway, Hyprland, Kglobalaccel }   // serde: "xi2", ...
impl Mechanism { pub fn as_str(self) -> &'static str }

pub struct Ready { pub mechanism: Mechanism, pub synthetic_safe: bool }

pub trait Backend {
    fn arm(&mut self, sink: AbortSink) -> Result<Ready, BoxError>;
    fn suspend(&mut self) -> Result<(), BoxError> { Ok(()) }
    fn resume(&mut self) -> Result<(), BoxError> { Ok(()) }
    fn disarm(&mut self);
}

#[derive(Clone)] // Send + Sync
pub struct AbortSink;
impl AbortSink {
    pub fn abort(&self);                         // the user pressed Escape
    pub fn fail(&self, message: impl Into<String>); // the watcher broke for good
    pub fn is_suspended(&self) -> bool;
}

pub fn run<B: Backend>(backend: B) -> std::process::ExitCode;
pub const PARENT_PID_ENV: &str = "CLAUDE_CU_PARENT_PID";
```

```rust
// main.rs
Command::AbortWatch => return cu_abort_watch::run(MyBackend::new()),
```

Backend rules:

- `arm` starts the watcher (an event-loop thread, or a task on a runtime the backend owns) and keeps the `AbortSink`. It must leave nothing behind when it returns `Err`, and should be bounded (signals are acted on after it returns).
- Call `sink.abort()` once per physical press: filter key repeat and injected events in the backend. The sink drops presses while suspended.
- Call `sink.fail(msg)` when the watcher stops working (compositor IPC socket closed, a sway `reload` dropped the bind, D-Bus connection lost).
- All trait methods run on the thread that called `run`. `disarm` runs once, also after a panic, and should not block for long.
- Signals, EOF and parent death are acted on only BETWEEN trait calls: every IPC call in `arm`, `suspend`, `resume` and `disarm` (compositor socket, D-Bus, a spawned `swaymsg` / `hyprctl`) must carry a timeout of a few seconds at most.
- `AbortSink::abort` and `fail` never block (they queue), so they are safe on any event-loop thread.
- Call `run` once per process and return its exit code from `main` right away: afterwards SIGTERM/SIGINT/SIGHUP are back to their default action (SIGPIPE stays ignored).
- Nothing else may write to stdout while `run` is active; log to stderr.

### Sync and async bridges

`run` blocks and needs no runtime.

- Sync bridges (x11rb, wlroots IPC): call it from `main`, run the event loop on a thread spawned in `arm`.
- tokio bridges (`#[tokio::main]`, kwin-portal-bridge): call it off the async context and drive async work through a `Handle`:

```rust
let handle = tokio::runtime::Handle::current();
let code = tokio::task::spawn_blocking(move || cu_abort_watch::run(KdeBackend { rt: handle, .. }))
    .await?;
// in the backend: self.rt.spawn(listener(sink)) in arm, self.rt.block_on(call) in suspend/resume/disarm
```

`run` installs process-wide handlers for SIGTERM, SIGINT and SIGHUP, so do not also use `tokio::signal` for them in this subcommand. It also adds a panic hook (chained to the previous one) that ends the watcher on a panic in any thread.

## Shared compositor binds (`SharedBinding`)

sway, Hyprland and KDE binds are user-global, and several Claude profiles can arm at once. `SharedBinding` decides which process binds and unbinds:

- Directory `$XDG_RUNTIME_DIR/claude-cu-esc/<mechanism>/`: both levels must be real directories (not symlinks) owned by the user with no group/other access (a looser mode on our own directory is tightened to 0700). One file per holder, named by its pid and holding the process start time, plus `.lock` (opened with `O_NOFOLLOW`).
- Every lock prunes holders whose `/proc/<pid>/stat` is gone, is a zombie (`Z`/`X`, killed but not reaped) or has another start time (crashed watchers, reused pids), and leftover `.<pid>.tmp` files. An entry that cannot be read or removed is skipped, never counted as a holder.
- The bind and unbind calls run inside the lock, so a joining process never races the last holder's unbind.
- `acquire` registers first, then binds (and unregisters if the bind fails); `release` unbinds first, then unregisters. A crash inside either window leaves a stale holder file, so the next first holder sees `pruned() > 0`.
- First holder binds, last holder unbinds. Every armed process still receives its own abort events from its own IPC subscription or signal match, so one Escape stops Computer Use in every armed profile.
- `suspend` / `resume` are not counted: a backend whose suspend touches the shared bind affects every holder while it lasts.

```rust
impl SharedBinding {
    pub fn for_mechanism(m: Mechanism) -> io::Result<Self>;          // needs XDG_RUNTIME_DIR
    pub fn in_dir(dir: impl Into<PathBuf>) -> io::Result<Self>;
    pub fn with_holder(dir: impl Into<PathBuf>, pid: u32) -> io::Result<Self>;
    pub fn lock(&self) -> io::Result<HolderLock<'_>>;                // waits up to 3 s
    pub fn acquire(&self, bind: impl FnOnce() -> Result<(), BoxError>) -> Result<bool, BoxError>; // true = first
    pub fn release(&self, unbind: impl FnOnce()) -> io::Result<bool>;                             // true = last; waits up to 10 s
}
impl HolderLock<'_> {
    pub fn others(&self) -> usize;     // live holders except us
    pub fn is_first(&self) -> bool;    // others() == 0
    pub fn pruned(&self) -> usize;     // stale holders removed: a stale bind may still be live
    pub fn is_registered(&self) -> bool;
    pub fn register(&mut self) -> io::Result<()>;
    pub fn unregister(&mut self) -> io::Result<()>;
}
```

Typical use: `acquire` in `arm` (a failed bind is not registered; re-joining never binds twice), `release` in `disarm` (idempotent).

`release` results: `Ok(last)` means `unbind` ran exactly when `last` is true. `Err(e)` means the lock or the directory scan failed; `unbind` has then ALREADY run (best effort) unless another live holder was visible without the lock. On `Err` the backend only logs and finishes the rest of its disarm; it must not call `unbind` again. Use `lock()` directly when the first holder must also clear a stale bind (`pruned() > 0`) or check for a user bind before binding. When the first holder binds over a stale bind from a crashed holder, re-binding must overwrite or remove it first.

XInput2 is per-client and does not use it.

## `doctor`

```rust
pub struct AbortWatchDoctor { pub mechanism: Option<Mechanism>, pub available: bool, pub reason: Option<String> }
AbortWatchDoctor::available(Mechanism::Sway).fragment()
// {"abortWatch":{"available":true,"mechanism":"sway","reason":null}}  (a serde_json::Value: keys sorted)
AbortWatchDoctor::unavailable(None, "no global Escape on GNOME").fragment()
pub fn doctor_fragment(mechanism: Option<Mechanism>, available: bool, reason: Option<&str>) -> serde_json::Value;
```

`AbortWatchDoctor` also serializes as a typed field (`#[serde(rename = "abortWatch")]`).

## Tests

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release --target x86_64-unknown-linux-musl
```

`tests/fake_bridge.rs` drives `examples/fake.rs` (a fake bridge, also a wiring example, in a thread and in a tokio variant) over pipes: protocol lines, ack ordering, muting while suspended, every exit path with exactly one `disarm`, parent death (the parent is SIGKILLed while the watcher's stdin stays open, and `CLAUDE_CU_PARENT_PID`), a full stdout pipe that is never read (SIGTERM and EOF still disarm), stdout closed, panics in `arm` / on the control thread / on a backend thread, and the shared binding across two processes with a stale holder. Unit tests cover the holder set: zombie holders, register-before-bind, the crash windows, the give-up path of `release` and the ownership checks.

## License

MIT, see [LICENSE](LICENSE).
