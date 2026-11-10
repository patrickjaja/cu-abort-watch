//! Shared half of the `abort-watch` subcommand of the claude-desktop-extra
//! Computer Use bridges: "the user pressed Escape, stop Computer Use".
//!
//! A bridge implements [`Backend`] for its compositor (XInput2 on X11, sway or
//! Hyprland IPC binds, KDE kglobalaccel) and calls [`run`]. The crate owns the
//! protocol (JSON lines on stdout, `suspend` / `resume` on stdin), the signals,
//! parent-death detection and the exactly-once cleanup. [`SharedBinding`]
//! reference-counts compositor-global bindings across processes;
//! [`AbortWatchDoctor`] is the `abortWatch` key of `doctor`.
//!
//! See README.md for the wire contract.

mod doctor;
mod protocol;
mod run;
mod shared;
mod sys;

pub use doctor::{AbortWatchDoctor, doctor_fragment};
pub use protocol::Mechanism;
pub use run::{AbortSink, Backend, BoxError, Ready, run};
pub use shared::{HolderLock, RUNTIME_SUBDIR, SharedBinding, binding_dir};
pub use sys::PARENT_PID_ENV;
