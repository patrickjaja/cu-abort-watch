//! A fake bridge for the integration tests (and a wiring example).
//!
//! Environment:
//! - `FAKE_LOG`: file that gets one line per backend call (`arm`, `suspend`,
//!   `resume`, `disarm`, `bind`, `unbind`).
//! - `FAKE_TRIGGER`: file the backend polls; each byte appended is one event:
//!   `e` = user Escape, `E` = 3000 Escapes 100 us apart (flood), `p` = panic
//!   on the backend thread, `f` = backend failure.
//! - `FAKE_ARM_FAIL=1`: arming fails. `FAKE_ARM_PANIC=1`: arming panics.
//! - `FAKE_SUSPEND_FAIL=1` / `FAKE_SUSPEND_PANIC=1`: suspend fails / panics
//!   (on the control thread). `FAKE_RESUME_FAIL=1`: resume fails.
//! - `FAKE_SYNTHETIC_SAFE=0`: report `syntheticSafe: false`.
//! - `FAKE_SHARED=1`: join the `sway` holder set under `$XDG_RUNTIME_DIR`
//!   (logs `bind` when first, `unbind` when last).
//! - `FAKE_RUNTIME=tokio`: the backend lives on a tokio runtime (tasks +
//!   `Handle::block_on`), `run` is called from `spawn_blocking`.
//! - `FAKE_ROLE=parent`: spawn the watcher as a child that inherits stdio,
//!   then sleep (tests kill this process to prove parent-death cleanup).

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use cu_abort_watch::{AbortSink, Backend, BoxError, Mechanism, Ready, SharedBinding, run};

fn log(what: &str) {
    if let Some(p) = std::env::var_os("FAKE_LOG") {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .unwrap();
        writeln!(f, "{what}").unwrap();
    }
}

fn flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1")
}

/// Bytes appended to the trigger file since the last call.
struct Trigger {
    path: Option<PathBuf>,
    pos: u64,
}

impl Trigger {
    fn new() -> Self {
        Self {
            path: std::env::var_os("FAKE_TRIGGER").map(PathBuf::from),
            pos: 0,
        }
    }
    fn poll(&mut self) -> Vec<u8> {
        let Some(p) = &self.path else { return vec![] };
        let Ok(mut f) = std::fs::File::open(p) else {
            return vec![];
        };
        let _ = f.seek(SeekFrom::Start(self.pos));
        let mut buf = Vec::new();
        let _ = f.read_to_end(&mut buf);
        self.pos += buf.len() as u64;
        buf
    }
}

fn dispatch(sink: &AbortSink, bytes: Vec<u8>) {
    for b in bytes {
        match b {
            b'e' => sink.abort(),
            b'E' => {
                for _ in 0..3000 {
                    sink.abort();
                    std::thread::sleep(Duration::from_micros(100));
                }
            }
            b'p' => panic!("fake backend thread panic"),
            b'f' => sink.fail("fake backend failure"),
            _ => {}
        }
    }
}

struct Fake {
    shared: Option<SharedBinding>,
    rt: Option<tokio::runtime::Handle>,
}

impl Backend for Fake {
    fn arm(&mut self, sink: AbortSink) -> Result<Ready, BoxError> {
        log("arm");
        if flag("FAKE_ARM_FAIL") {
            return Err("fake arm failure".into());
        }
        if flag("FAKE_ARM_PANIC") {
            panic!("fake arm panic");
        }
        if flag("FAKE_SHARED") {
            let set = SharedBinding::for_mechanism(Mechanism::Sway)?;
            set.acquire(|| {
                log("bind");
                Ok(())
            })?;
            self.shared = Some(set);
        }
        let mut trigger = Trigger::new();
        match &self.rt {
            Some(h) => {
                h.spawn(async move {
                    loop {
                        dispatch(&sink, trigger.poll());
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                });
            }
            None => {
                std::thread::spawn(move || {
                    loop {
                        dispatch(&sink, trigger.poll());
                        std::thread::sleep(Duration::from_millis(10));
                    }
                });
            }
        }
        Ok(Ready {
            mechanism: Mechanism::Sway,
            synthetic_safe: std::env::var("FAKE_SYNTHETIC_SAFE").map_or(true, |v| v != "0"),
        })
    }

    fn suspend(&mut self) -> Result<(), BoxError> {
        if let Some(h) = &self.rt {
            h.block_on(tokio::time::sleep(Duration::from_millis(1)));
        }
        log("suspend");
        if flag("FAKE_SUSPEND_PANIC") {
            panic!("fake suspend panic");
        }
        if flag("FAKE_SUSPEND_FAIL") {
            return Err("fake suspend failure".into());
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<(), BoxError> {
        log("resume");
        if flag("FAKE_RESUME_FAIL") {
            return Err("fake resume failure".into());
        }
        Ok(())
    }

    fn disarm(&mut self) {
        log("disarm");
        if let Some(set) = &self.shared {
            let _ = set.release(|| log("unbind"));
        }
    }
}

fn main() -> ExitCode {
    if std::env::var("FAKE_ROLE").is_ok_and(|r| r == "parent") {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .env_remove("FAKE_ROLE")
            .spawn()
            .unwrap();
        eprintln!("fake parent: watcher pid {}", child.id());
        std::thread::sleep(Duration::from_secs(60));
        let _ = child.kill();
        let _ = child.wait();
        return ExitCode::SUCCESS;
    }

    if std::env::var("FAKE_RUNTIME").is_ok_and(|r| r == "tokio") {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_time()
            .build()
            .unwrap();
        let handle = rt.handle().clone();
        // The usual shape of a #[tokio::main] bridge: run() on a blocking
        // thread, the backend drives async work through the Handle.
        return rt.block_on(async move {
            tokio::task::spawn_blocking(move || {
                run(Fake {
                    shared: None,
                    rt: Some(handle),
                })
            })
            .await
            .unwrap()
        });
    }

    run(Fake {
        shared: None,
        rt: None,
    })
}
