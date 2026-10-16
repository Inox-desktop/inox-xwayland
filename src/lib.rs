//! XWayland integration service for the Inox desktop.
//!
//! `inox-xwayland` is an **L2** service that integrates *rootless* XWayland
//! into an Inox session by **supervising the external `xwayland-satellite`
//! process**. The satellite is a runtime system dependency, exactly like
//! `Xwayland` itself: the crate never links it, it only spawns the binary and
//! forwards its lifecycle to the compositor session.
//!
//! # M1 scope
//!
//! * spawn `xwayland-satellite` for a chosen display (default `:1`) and let it
//!   start `Xwayland`;
//! * detect readiness by probing the X11 UNIX socket and report it through the
//!   log and an optional readiness file;
//! * supervise the child: notice an unexpected exit and forward termination
//!   signals (`SIGTERM` with a `SIGKILL` escalation);
//! * exit with status `0` on `SIGINT`/`SIGTERM`.
//!
//! Clipboard/DnD, compositor `-listenfd` activation, restart supervision and
//! DRM/KMS input are explicitly out of scope for M1.
//!
//! # Layout
//!
//! * [`config`] — command line parsing, free of process and X11 side effects.
//! * [`x11`] — display numbers and their sockets.
//! * [`process`] — spawning and terminating the satellite child.
//! * [`service`] — the supervision state machine.
//! * [`runtime`] — the calloop event loop tying it all together.

pub mod config;
pub mod process;
pub mod runtime;
pub mod service;
pub mod x11;

pub use config::{ConfigError, HELP, Invocation, ServiceConfig};
pub use process::ProcessError;
pub use runtime::{ServiceOutcome, run};
pub use service::{Phase, Progress, ServiceError, XwaylandService};
pub use x11::{Display, DisplayError};

/// The crate version as reported by Cargo.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
