//! Drives examples/fake.rs over pipes: protocol, ordering, every exit path.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(5);

fn fake_exe() -> PathBuf {
    // target/<triple>/<profile>/deps/<test> -> target/<triple>/<profile>/examples/fake
    let exe = std::env::current_exe().unwrap();
    let p = exe
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples/fake");
    assert!(
        p.exists(),
        "{} missing (cargo test builds examples)",
        p.display()
    );
    p
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "cu-abort-watch-it-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn log(&self) -> PathBuf {
        self.0.join("log")
    }
    fn trigger(&self) -> PathBuf {
        self.0.join("trigger")
    }
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
    fn fire(&self, what: &str) {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.trigger())
            .unwrap();
        f.write_all(what.as_bytes()).unwrap();
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Spawning without a reader thread (stdout left to the test).
struct Raw;
impl Raw {
    fn command(s: &Scratch, env: &[(&str, &str)]) -> (Child, ChildStdout) {
        let mut cmd = Command::new(fake_exe());
        cmd.env("FAKE_LOG", s.log())
            .env("FAKE_TRIGGER", s.trigger())
            .env("XDG_RUNTIME_DIR", &s.0)
            .env_remove("CLAUDE_CU_PARENT_PID")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        (child, stdout)
    }
}

struct Watcher {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Watcher {
    fn spawn(s: &Scratch, env: &[(&str, &str)]) -> Self {
        let (mut child, stdout) = Raw::command(s, env);
        let stdin = child.stdin.take();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines: rx,
        }
    }

    fn next(&self) -> String {
        self.lines
            .recv_timeout(WAIT)
            .expect("watcher did not print a line in time")
    }

    /// No line within `d`.
    fn quiet(&self, d: Duration) {
        match self.lines.recv_timeout(d) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(l) => panic!("unexpected line: {l}"),
            Err(RecvTimeoutError::Disconnected) => panic!("watcher exited"),
        }
    }

    fn send(&mut self, line: &str) {
        let w = self.stdin.as_mut().unwrap();
        w.write_all(line.as_bytes()).unwrap();
        w.flush().unwrap();
    }

    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    fn signal(&self, sig: libc::c_int) {
        // SAFETY: signalling our own child.
        assert_eq!(
            unsafe { libc::kill(self.child.id() as libc::pid_t, sig) },
            0
        );
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return st;
            }
            assert!(Instant::now() < deadline, "watcher did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Remaining stdout lines after exit.
    fn rest(&self) -> Vec<String> {
        let mut v = Vec::new();
        while let Ok(l) = self.lines.recv_timeout(Duration::from_millis(500)) {
            v.push(l);
        }
        v
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const READY_SAFE: &str = r#"{"event":"ready","mechanism":"sway","syntheticSafe":true}"#;
const ABORT: &str = r#"{"event":"abort"}"#;
const ACK_SUSPEND: &str = r#"{"event":"ack","cmd":"suspend"}"#;
const ACK_RESUME: &str = r#"{"event":"ack","cmd":"resume"}"#;

#[test]
fn ready_abort_and_eof_disarms_once() {
    let s = Scratch::new();
    let mut w = Watcher::spawn(&s, &[]);
    assert_eq!(w.next(), READY_SAFE);
    s.fire("e");
    assert_eq!(w.next(), ABORT);
    s.fire("e");
    assert_eq!(w.next(), ABORT);
    w.close_stdin();
    assert!(w.wait().success());
    assert!(w.rest().is_empty());
    assert_eq!(s.calls(), ["arm", "disarm"]);
}

#[test]
fn ready_reports_synthetic_safe_false() {
    let s = Scratch::new();
    let w = Watcher::spawn(&s, &[("FAKE_SYNTHETIC_SAFE", "0")]);
    assert_eq!(
        w.next(),
        r#"{"event":"ready","mechanism":"sway","syntheticSafe":false}"#
    );
}

#[test]
fn suspend_mutes_aborts_and_acks_in_order() {
    let s = Scratch::new();
    let mut w = Watcher::spawn(&s, &[("FAKE_SYNTHETIC_SAFE", "0")]);
    w.next();
    w.send("suspend\n");
    assert_eq!(w.next(), ACK_SUSPEND);
    // An Escape while suspended is dropped.
    s.fire("e");
    w.quiet(Duration::from_millis(300));
    // A second suspend is acked without calling the backend again.
    w.send("suspend\n");
    assert_eq!(w.next(), ACK_SUSPEND);
    w.send("resume\n");
    assert_eq!(w.next(), ACK_RESUME);
    // Resume while not suspended: ack only.
    w.send("resume\n");
    assert_eq!(w.next(), ACK_RESUME);
    s.fire("e");
    assert_eq!(w.next(), ABORT);
    // Several commands in one write, CRLF and blank lines tolerated.
    w.send("suspend\r\n\n  \nresume\n");
    assert_eq!(w.next(), ACK_SUSPEND);
    assert_eq!(w.next(), ACK_RESUME);
    w.close_stdin();
    assert!(w.wait().success());
    assert_eq!(
        s.calls(),
        ["arm", "suspend", "resume", "suspend", "resume", "disarm"]
    );
}

#[test]
fn unknown_command_is_an_error_but_not_fatal() {
    let s = Scratch::new();
    let mut w = Watcher::spawn(&s, &[]);
    w.next();
    w.send("stop\n");
    assert_eq!(
        w.next(),
        r#"{"event":"error","message":"unknown command: stop"}"#
    );
    w.send(&format!("{}\n", "x".repeat(10_000)));
    assert_eq!(
        w.next(),
        r#"{"event":"error","message":"unknown command: <line too long>"}"#
    );
    w.send("suspend\n");
    assert_eq!(w.next(), ACK_SUSPEND);
    // A last command without newline still runs before EOF ends the watcher.
    w.send("resume");
    w.close_stdin();
    assert_eq!(w.next(), ACK_RESUME);
    assert!(w.wait().success());
    assert_eq!(s.calls(), ["arm", "suspend", "resume", "disarm"]);
}

#[test]
fn exit_signals_disarm() {
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        let s = Scratch::new();
        let mut w = Watcher::spawn(&s, &[]);
        assert_eq!(w.next(), READY_SAFE);
        w.signal(sig);
        let st = w.wait();
        assert!(st.success(), "signal {sig}: {st:?}");
        assert_eq!(s.calls(), ["arm", "disarm"], "signal {sig}");
    }
}

#[test]
fn arm_failure_reports_and_leaves_nothing() {
    let s = Scratch::new();
    let mut w = Watcher::spawn(&s, &[("FAKE_ARM_FAIL", "1")]);
    assert_eq!(
        w.next(),
        r#"{"event":"error","message":"fake arm failure"}"#
    );
    assert_eq!(w.wait().code(), Some(2));
    assert_eq!(s.calls(), ["arm"]);
}

#[test]
fn backend_failure_exits_with_cleanup() {
    let s = Scratch::new();
    let mut w = Watcher::spawn(&s, &[]);
    w.next();
    s.fire("f");
    assert_eq!(
        w.next(),
        r#"{"event":"error","message":"fake backend failure"}"#
    );
    assert_eq!(w.wait().code(), Some(1));
    assert_eq!(s.calls(), ["arm", "disarm"]);
}

#[test]
fn backend_thread_panic_exits_with_cleanup() {
    let s = Scratch::new();
    let mut w = Watcher::spawn(&s, &[]);
    w.next();
    s.fire("p");
    assert_eq!(
        w.next(),
        r#"{"event":"error","message":"abort watcher panicked"}"#
    );
    assert_eq!(w.wait().code(), Some(1));
    assert_eq!(s.calls(), ["arm", "disarm"]);
}

#[test]
fn resume_failure_is_an_error_without_ack() {
    let s = Scratch::new();
    let mut w = Watcher::spawn(&s, &[("FAKE_RESUME_FAIL", "1")]);
    w.next();
    w.send("suspend\n");
    assert_eq!(w.next(), ACK_SUSPEND);
    w.send("resume\n");
    assert_eq!(
        w.next(),
        r#"{"event":"error","message":"resume failed: fake resume failure"}"#
    );
    assert_eq!(w.wait().code(), Some(1));
    assert!(w.rest().is_empty());
    assert_eq!(s.calls(), ["arm", "suspend", "resume", "disarm"]);
}

#[test]
fn tokio_backend() {
    let s = Scratch::new();
    let mut w = Watcher::spawn(&s, &[("FAKE_RUNTIME", "tokio")]);
    assert_eq!(w.next(), READY_SAFE);
    s.fire("e");
    assert_eq!(w.next(), ABORT);
    w.send("suspend\nresume\n");
    assert_eq!(w.next(), ACK_SUSPEND);
    assert_eq!(w.next(), ACK_RESUME);
    w.signal(libc::SIGTERM);
    assert!(w.wait().success());
    assert_eq!(s.calls(), ["arm", "suspend", "resume", "disarm"]);
}

fn wait_for(cond: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !cond() {
        assert!(Instant::now() < deadline, "condition not reached");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn parent_death_disarms() {
    // test -> fake (parent role) -> fake (watcher). The watcher inherits OUR
    // stdin pipe, which we keep open: only the parent's death can end it.
    let s = Scratch::new();
    let mut parent = Watcher::spawn(&s, &[("FAKE_ROLE", "parent")]);
    assert_eq!(parent.next(), READY_SAFE);
    parent.child.kill().unwrap(); // SIGKILL: no cleanup in the parent itself
    parent.child.wait().unwrap();
    wait_for(|| s.calls() == ["arm", "disarm"]);
    // The watcher exited: its stdout (our pipe) reaches EOF.
    let deadline = Instant::now() + WAIT;
    loop {
        match parent.lines.recv_timeout(Duration::from_millis(100)) {
            Err(RecvTimeoutError::Disconnected) => break,
            Ok(l) => panic!("unexpected line: {l}"),
            Err(RecvTimeoutError::Timeout) => {
                assert!(Instant::now() < deadline, "watcher still running")
            }
        }
    }
    assert!(parent.stdin.is_some());
}

fn holders(dir: &Path) -> usize {
    fs::read_dir(dir)
        .map(|d| {
            d.filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_str()
                    .is_some_and(|n| n.parse::<u32>().is_ok())
            })
            .count()
        })
        .unwrap_or(0)
}

#[test]
fn shared_binding_across_processes() {
    let s = Scratch::new();
    let dir = s.0.join("claude-cu-esc/sway");
    // A crashed earlier holder.
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("999999999"), "1\n").unwrap();

    let mut a = Watcher::spawn(&s, &[("FAKE_SHARED", "1")]);
    assert_eq!(a.next(), READY_SAFE);
    let mut b = Watcher::spawn(&s, &[("FAKE_SHARED", "1")]);
    assert_eq!(b.next(), READY_SAFE);
    assert_eq!(s.calls(), ["arm", "bind", "arm"]);
    assert_eq!(holders(&dir), 2);

    // One Escape reaches both.
    s.fire("e");
    assert_eq!(a.next(), ABORT);
    assert_eq!(b.next(), ABORT);

    a.close_stdin();
    assert!(a.wait().success());
    assert_eq!(s.calls(), ["arm", "bind", "arm", "disarm"]);
    b.signal(libc::SIGTERM);
    assert!(b.wait().success());
    assert_eq!(
        s.calls(),
        ["arm", "bind", "arm", "disarm", "disarm", "unbind"]
    );
    assert_eq!(holders(&dir), 0);
}

#[test]
fn suspend_failure_is_reported_and_acked() {
    let s = Scratch::new();
    let mut w = Watcher::spawn(&s, &[("FAKE_SUSPEND_FAIL", "1")]);
    w.next();
    w.send("suspend\n");
    assert_eq!(
        w.next(),
        r#"{"event":"error","message":"suspend failed: fake suspend failure"}"#
    );
    assert_eq!(w.next(), ACK_SUSPEND);
    // Still muted, still running.
    s.fire("e");
    w.quiet(Duration::from_millis(300));
    w.send("resume\n");
    assert_eq!(w.next(), ACK_RESUME);
    w.close_stdin();
    assert!(w.wait().success());
    assert_eq!(s.calls(), ["arm", "suspend", "resume", "disarm"]);
}

#[test]
fn arm_panic_reports_and_cleans_up() {
    let s = Scratch::new();
    let mut w = Watcher::spawn(&s, &[("FAKE_ARM_PANIC", "1")]);
    assert_eq!(w.next(), r#"{"event":"error","message":"arming panicked"}"#);
    assert_eq!(w.wait().code(), Some(2));
    assert_eq!(s.calls(), ["arm", "disarm"]);
}

#[test]
fn control_thread_panic_exits_with_cleanup() {
    let s = Scratch::new();
    let mut w = Watcher::spawn(&s, &[("FAKE_SUSPEND_PANIC", "1")]);
    w.next();
    w.send("suspend\n");
    assert_eq!(
        w.next(),
        r#"{"event":"error","message":"abort watcher panicked"}"#
    );
    assert_eq!(w.wait().code(), Some(1));
    assert!(w.rest().is_empty());
    assert_eq!(s.calls(), ["arm", "suspend", "disarm"]);
}

#[test]
fn stdout_closed_exits_with_cleanup() {
    let s = Scratch::new();
    let (mut child, stdout) = Raw::command(&s, &[]);
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line.trim_end(), READY_SAFE);
    drop(reader); // close the read end
    s.fire("e");
    let deadline = Instant::now() + WAIT;
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        assert!(Instant::now() < deadline, "watcher did not exit");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(1));
    assert_eq!(s.calls(), ["arm", "disarm"]);
}

/// The caller stops reading stdout while aborts keep coming: the pipe fills
/// up, yet suspend is still processed and SIGTERM / EOF still reach disarm.
#[test]
fn full_stdout_pipe_never_blocks_cleanup() {
    for by_signal in [true, false] {
        let s = Scratch::new();
        let (mut child, stdout) = Raw::command(&s, &[]);
        // Smallest pipe (one page), so a few hundred aborts fill it.
        // SAFETY: fcntl on our own pipe descriptor.
        unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETPIPE_SZ, 4096) };
        s.fire("E");
        std::thread::sleep(Duration::from_millis(800));
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"suspend\n").unwrap();
        stdin.flush().unwrap();
        wait_for(|| s.calls() == ["arm", "suspend"]);
        if by_signal {
            // SAFETY: signalling our own child.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        } else {
            drop(stdin);
        }
        wait_for(|| s.calls() == ["arm", "suspend", "disarm"]);
        let deadline = Instant::now() + WAIT;
        let status = loop {
            if let Some(st) = child.try_wait().unwrap() {
                break st;
            }
            assert!(Instant::now() < deadline, "watcher did not exit");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success(), "{status:?}");
        // What did reach the pipe: ready first, never an abort after the
        // suspend ack.
        let lines: Vec<String> = BufReader::new(stdout).lines().map(Result::unwrap).collect();
        assert_eq!(lines[0], READY_SAFE);
        assert!(lines.len() > 100, "pipe was not filled: {}", lines.len());
        if let Some(i) = lines.iter().position(|l| l == ACK_SUSPEND) {
            assert!(!lines[i..].iter().any(|l| l == ABORT));
        }
    }
}

#[test]
fn named_parent_pid_death_disarms() {
    let s = Scratch::new();
    let mut parent = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = parent.id().to_string();
    let mut w = Watcher::spawn(&s, &[("CLAUDE_CU_PARENT_PID", &pid)]);
    assert_eq!(w.next(), READY_SAFE);
    parent.kill().unwrap();
    parent.wait().unwrap();
    assert!(w.wait().success());
    assert_eq!(s.calls(), ["arm", "disarm"]);
    assert!(w.stdin.is_some());
}

#[test]
fn named_parent_already_dead_never_arms() {
    let s = Scratch::new();
    let mut dead = Command::new("true").spawn().unwrap();
    let pid = dead.id().to_string();
    dead.wait().unwrap();
    let mut w = Watcher::spawn(&s, &[("CLAUDE_CU_PARENT_PID", &pid)]);
    assert!(w.wait().success());
    assert!(w.rest().is_empty());
    assert!(s.calls().is_empty());
}

#[test]
fn invalid_parent_pid_is_ignored() {
    let s = Scratch::new();
    let mut w = Watcher::spawn(&s, &[("CLAUDE_CU_PARENT_PID", "nope")]);
    assert_eq!(w.next(), READY_SAFE);
    w.close_stdin();
    assert!(w.wait().success());
    assert_eq!(s.calls(), ["arm", "disarm"]);
}

#[test]
fn overlong_last_line_at_eof_gets_the_same_reply() {
    let s = Scratch::new();
    let mut w = Watcher::spawn(&s, &[]);
    w.next();
    w.send(&"x".repeat(10_000));
    w.close_stdin();
    assert_eq!(
        w.next(),
        r#"{"event":"error","message":"unknown command: <line too long>"}"#
    );
    assert!(w.wait().success());
}
