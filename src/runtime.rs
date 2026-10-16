//! The calloop event loop that drives the service.
//!
//! Two event sources share one [`XwaylandService`]:
//!
//! * a [`Signals`] source turning `SIGINT`/`SIGTERM` into a clean shutdown, and
//! * a [`Timer`] polling the state machine for readiness and child exit.
//!
//! There is deliberately no hand-rolled thread or polling loop (root
//! `AGENTS.md` §4); calloop owns the readiness and the blocking shutdown waits.

use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use calloop::{
    EventLoop, LoopHandle, LoopSignal,
    signals::{Signal, Signals},
    timer::{TimeoutAction, Timer},
};
use tracing::{debug, error, info, warn};

use crate::{
    config::ServiceConfig,
    service::{Progress, ServiceError, XwaylandService},
};

/// How often the service is polled for readiness and child exit.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Terminal outcome of the service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceOutcome {
    /// A termination signal caused a clean shutdown.
    Signalled,
    /// The satellite/Xwayland exited on its own.
    ChildExited(std::process::ExitStatus),
}

/// State shared between the event sources and the caller.
struct LoopState {
    service: XwaylandService,
    outcome: Option<ServiceOutcome>,
    error: Option<ServiceError>,
    ready_logged: bool,
}

/// Drive the service on a calloop event loop until it terminates.
///
/// # Errors
///
/// Returns a [`ServiceError`] when the satellite cannot be spawned, when
/// Xwayland never becomes ready, when the readiness file cannot be written, or
/// when the event loop itself fails.
pub fn run(config: ServiceConfig) -> Result<ServiceOutcome, ServiceError> {
    let service = XwaylandService::start(config)?;

    let mut event_loop: EventLoop<'static, ()> = EventLoop::try_new().map_err(loop_error)?;
    let loop_handle = event_loop.handle();
    let loop_signal = event_loop.get_signal();

    let state = Rc::new(RefCell::new(LoopState {
        service,
        outcome: None,
        error: None,
        ready_logged: false,
    }));

    register_signals(&loop_handle, Rc::clone(&state), loop_signal.clone())?;
    register_poll_timer(&loop_handle, Rc::clone(&state), loop_signal)?;

    let mut data = ();
    event_loop
        .run(None, &mut data, |_| {})
        .map_err(loop_error)?;

    let mut state = state.borrow_mut();

    if let Some(error) = state.error.take() {
        return Err(error);
    }

    // A loop that stopped without recording anything means "signalled": this is
    // the defensive branch the design asks for.
    Ok(state.outcome.take().unwrap_or(ServiceOutcome::Signalled))
}

/// Turn a calloop failure into a [`ServiceError`].
fn loop_error(error: calloop::Error) -> ServiceError {
    ServiceError::EventLoop {
        reason: error.to_string(),
    }
}

/// Turn `SIGINT`/`SIGTERM` into a clean shutdown and ignore `SIGHUP`.
fn register_signals(
    loop_handle: &LoopHandle<'static, ()>,
    state: Rc<RefCell<LoopState>>,
    loop_signal: LoopSignal,
) -> Result<(), ServiceError> {
    let signals =
        Signals::new(&[Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP]).map_err(loop_error)?;

    loop_handle
        .insert_source(signals, move |event, _, _meta| match event.signal() {
            Signal::SIGHUP => {
                warn!("SIGHUP received; configuration reload is not supported yet");
            }
            signal @ (Signal::SIGINT | Signal::SIGTERM) => {
                info!(?signal, "termination signal received, shutting down");

                let mut state = state.borrow_mut();
                match state.service.shutdown() {
                    Ok(status) => {
                        debug!(?status, "satellite terminated");
                        state.outcome = Some(ServiceOutcome::Signalled);
                    }
                    Err(error) => state.error = Some(error),
                }

                drop(state);
                loop_signal.stop();
            }
            other => debug!(?other, "ignoring signal"),
        })
        .map(drop)
        .map_err(|error| loop_error(error.into()))
}

/// Poll the service until it becomes ready, exits or fails.
fn register_poll_timer(
    loop_handle: &LoopHandle<'static, ()>,
    state: Rc<RefCell<LoopState>>,
    loop_signal: LoopSignal,
) -> Result<(), ServiceError> {
    loop_handle
        .insert_source(Timer::from_duration(POLL_INTERVAL), move |_, _, _meta| {
            let mut state = state.borrow_mut();

            match state.service.poll(Instant::now()) {
                Ok(Progress::Pending) => {}
                Ok(Progress::Ready(ready_display)) => {
                    if !state.ready_logged {
                        state.ready_logged = true;
                        info!("XWayland ready on {}", ready_display);
                    }
                }
                Ok(Progress::Exited(status)) => {
                    info!(?status, "the satellite exited on its own");
                    state.outcome = Some(ServiceOutcome::ChildExited(status));

                    drop(state);
                    loop_signal.stop();

                    return TimeoutAction::Drop;
                }
                Err(error) => {
                    error!(%error, "the service failed, shutting down");
                    if let Err(cleanup) = state.service.shutdown() {
                        warn!(%cleanup, "failed to terminate the satellite");
                    }

                    state.error = Some(error);

                    drop(state);
                    loop_signal.stop();

                    return TimeoutAction::Drop;
                }
            }

            TimeoutAction::ToDuration(POLL_INTERVAL)
        })
        .map(drop)
        .map_err(|error| loop_error(error.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calloop_failures_become_event_loop_errors() {
        let error = loop_error(calloop::Error::InvalidToken);

        assert!(matches!(error, ServiceError::EventLoop { .. }));
        assert!(error.to_string().contains("event loop"));
    }
}
