//! Reconnection policy and helpers for automatic reconnection.
//!
//! Provides [`ReconnectTarget`] (device identity) and [`ReconnectPolicy`]
//! (composed behavior) used internally by [`Stream`] and [`FrameSession`]
//! when reconnection is configured via [`ReconnectConfig`].
//!
//! [`Stream`]: crate::stream::Stream
//! [`FrameSession`]: crate::presentation::FrameSession
//! [`ReconnectConfig`]: crate::config::ReconnectConfig

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::backend::{BackendKind, Error, Result};
use crate::config::ReconnectConfig;
use crate::device::{DacInfo, EnabledDacTypes};
use crate::discovery::DacDiscovery;
use crate::stream::{Dac, RunExit};

type DisconnectCallback = Box<dyn FnMut(&Error) + Send + 'static>;
type ReconnectCallback = Box<dyn FnMut(&DacInfo) + Send + 'static>;
pub(crate) type DiscoveryFactory = Box<dyn Fn() -> DacDiscovery + Send + 'static>;

// =============================================================================
// ReconnectTarget
// =============================================================================

/// How to reopen a device. Populated by [`DacDiscovery::open_discovered`]
/// (and [`Dac::with_discovery_factory`]), carried by `Dac`.
pub(crate) struct ReconnectTarget {
    pub device_id: String,
    pub discovery_factory: Option<DiscoveryFactory>,
}

impl ReconnectTarget {
    /// Open a device using the configured discovery method.
    pub fn open_device(&self) -> Result<Dac> {
        let mut discovery = if let Some(factory) = &self.discovery_factory {
            factory()
        } else {
            DacDiscovery::new(EnabledDacTypes::all())
        };
        discovery.open_by_id(&self.device_id)
    }
}

// =============================================================================
// ReconnectPolicy
// =============================================================================

/// Composed policy: target (identity) + config (behavior).
///
/// Lives on the Stream/scheduler thread. Created from a [`ReconnectConfig`]
/// and a [`ReconnectTarget`].
pub(crate) struct ReconnectPolicy {
    pub target: ReconnectTarget,
    pub max_retries: Option<u32>,
    pub backoff: Duration,
    pub on_disconnect: Arc<Mutex<Option<DisconnectCallback>>>,
    pub on_reconnect: Arc<Mutex<Option<ReconnectCallback>>>,
}

impl ReconnectPolicy {
    /// Compose a reconnect policy from user config and device target.
    pub fn new(config: ReconnectConfig, target: ReconnectTarget) -> Self {
        Self {
            target,
            max_retries: config.max_retries,
            backoff: config.backoff,
            on_disconnect: Arc::new(Mutex::new(config.on_disconnect)),
            on_reconnect: Arc::new(Mutex::new(config.on_reconnect)),
        }
    }

    /// Check if an error is retriable (not a config or stop error).
    pub fn is_retriable(err: &Error) -> bool {
        !matches!(err, Error::InvalidConfig(_) | Error::Stopped)
    }

    /// Sleep for a duration, checking for stop requests periodically.
    ///
    /// Returns `true` if stop was requested during sleep.
    pub fn sleep_with_stop<FStop, FProgress>(
        duration: Duration,
        is_stopped: FStop,
        on_progress: &mut FProgress,
    ) -> bool
    where
        FStop: Fn() -> bool,
        FProgress: FnMut(),
    {
        const SLICE: Duration = Duration::from_millis(50);
        // Absolute deadline rather than decrementing by the nominal slice, so a
        // coarse OS timer can't stretch the total backoff far beyond `duration`.
        let deadline = Instant::now() + duration;
        loop {
            if is_stopped() {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            std::thread::sleep(remaining.min(SLICE));
            on_progress();
        }
        is_stopped()
    }
}

/// Re-open, validate, and connect a backend using the policy retry loop.
///
/// This is the shared reconnect scaffolding used by stream and frame schedulers.
/// Scheduler-specific state reset and callback timing remain local to the caller.
///
/// `last_reconnect` is the instant the *previous* reconnect cycle completed
/// successfully (or `None` on the first ever call). It gates the immediate
/// first attempt: if the last reconnect was less than `backoff` ago the device
/// is flapping (accepts the connection, then dies immediately), so we back off
/// before even the first attempt instead of spinning disconnect→reopen→disconnect
/// with zero delay. A genuine one-off disconnect (old or absent `last_reconnect`)
/// still recovers instantly.
pub(crate) fn reconnect_backend_with_retry<FStop, FValidate, FProgress>(
    policy: &ReconnectPolicy,
    last_reconnect: Option<Instant>,
    is_stopped: FStop,
    validate: FValidate,
    mut on_progress: FProgress,
) -> std::result::Result<(DacInfo, BackendKind), RunExit>
where
    FStop: Fn() -> bool,
    FValidate: Fn(&DacInfo, &BackendKind) -> std::result::Result<(), RunExit>,
    FProgress: FnMut(),
{
    if let Some(cb) = policy.on_disconnect.lock().unwrap().as_mut() {
        cb(&Error::disconnected("backend disconnected"));
    }

    // Flapping guard: back off before the first attempt when the previous
    // reconnect completed less than a full backoff ago.
    let flapping = is_flapping(last_reconnect, policy.backoff);

    let mut retries = 0u32;
    loop {
        on_progress();
        if is_stopped() {
            return Err(RunExit::Stopped);
        }

        if let Some(max) = policy.max_retries {
            if retries >= max {
                return Err(RunExit::Disconnected);
            }
        }

        // Attempt immediately on the first try; back off only *between* attempts
        // so an instantly-recoverable disconnect doesn't blank output for a full
        // backoff period before we even try to reopen. The exception is a
        // flapping device (see `flapping` above): there we back off even before
        // the first attempt so rapid connect/die cycles can't busy-loop.
        if (retries > 0 || flapping)
            && ReconnectPolicy::sleep_with_stop(policy.backoff, &is_stopped, &mut on_progress)
        {
            return Err(RunExit::Stopped);
        }

        on_progress();
        log::info!(
            "'{}' reconnect attempt {} ...",
            policy.target.device_id,
            retries + 1
        );

        let device = match policy.target.open_device() {
            Ok(d) => d,
            Err(err) => {
                if !ReconnectPolicy::is_retriable(&err) {
                    log::error!("'{}' non-retriable error: {}", policy.target.device_id, err);
                    return Err(RunExit::Disconnected);
                }
                log::warn!("'{}' open_device failed: {}", policy.target.device_id, err);
                retries = retries.saturating_add(1);
                continue;
            }
        };

        let info = device.info().clone();
        let mut backend = match device.into_backend() {
            Some(b) => b,
            None => {
                retries = retries.saturating_add(1);
                continue;
            }
        };

        validate(&info, &backend)?;

        if !backend.is_connected() {
            match backend.connect() {
                Ok(()) => {}
                Err(err) => {
                    if !ReconnectPolicy::is_retriable(&err) {
                        log::error!("'{}' non-retriable error: {}", policy.target.device_id, err);
                        return Err(RunExit::Disconnected);
                    }
                    log::warn!("'{}' connect failed: {}", policy.target.device_id, err);
                    retries = retries.saturating_add(1);
                    continue;
                }
            }
        }

        log::info!("'{}' reconnected successfully", policy.target.device_id);
        return Ok((info, backend));
    }
}

/// Whether the first reconnect attempt should back off instead of firing
/// immediately: true when the previous reconnect completed less than `backoff`
/// ago (a flapping device that accepts the connection then dies immediately).
/// `None` (first ever reconnect) never delays.
fn is_flapping(last_reconnect: Option<Instant>, backoff: Duration) -> bool {
    last_reconnect
        .map(|t| t.elapsed() < backoff)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_ever_reconnect_does_not_delay() {
        assert!(!is_flapping(None, Duration::from_millis(100)));
    }

    #[test]
    fn recent_reconnect_flaps_and_delays() {
        // Previous reconnect just completed → within backoff → must back off.
        assert!(is_flapping(
            Some(Instant::now()),
            Duration::from_millis(100)
        ));
    }

    #[test]
    fn old_reconnect_does_not_delay() {
        // A genuine one-off disconnect long after the last reconnect recovers
        // instantly.
        let long_ago = Instant::now() - Duration::from_secs(5);
        assert!(!is_flapping(Some(long_ago), Duration::from_millis(100)));
    }
}
