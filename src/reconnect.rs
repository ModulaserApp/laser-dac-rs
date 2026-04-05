//! Reconnection policy and helpers for automatic reconnection.
//!
//! Provides [`ReconnectTarget`] (device identity) and [`ReconnectPolicy`]
//! (composed behavior) used internally by [`Stream`] and [`FrameSession`]
//! when reconnection is configured via [`ReconnectConfig`].
//!
//! [`Stream`]: crate::stream::Stream
//! [`FrameSession`]: crate::presentation::FrameSession
//! [`ReconnectConfig`]: crate::types::ReconnectConfig

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::backend::{BackendKind, Error, Result};
use crate::discovery::DacDiscovery;
use crate::stream::Dac;
use crate::types::{DacInfo, ReconnectConfig, RunExit};

type DisconnectCallback = Box<dyn FnMut(&Error) + Send + 'static>;
type ReconnectCallback = Box<dyn FnMut(&DacInfo) + Send + 'static>;
pub(crate) type DiscoveryFactory = Box<dyn Fn() -> DacDiscovery + Send + 'static>;

// =============================================================================
// ReconnectTarget
// =============================================================================

/// How to reopen a device. Populated by `open_device()`, carried by `Dac`.
pub(crate) struct ReconnectTarget {
    pub device_id: String,
    pub discovery_factory: Option<DiscoveryFactory>,
}

impl ReconnectTarget {
    /// Open a device using the configured discovery method.
    pub fn open_device(&self) -> Result<Dac> {
        if let Some(factory) = &self.discovery_factory {
            let mut discovery = factory();
            discovery.open_by_id(&self.device_id)
        } else {
            crate::open_device(&self.device_id)
        }
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
        let mut remaining = duration;
        while remaining > Duration::ZERO {
            if is_stopped() {
                return true;
            }
            let slice = remaining.min(SLICE);
            std::thread::sleep(slice);
            remaining = remaining.saturating_sub(slice);
            on_progress();
        }
        is_stopped()
    }
}

/// Re-open, validate, and connect a backend using the policy retry loop.
///
/// This is the shared reconnect scaffolding used by stream and frame schedulers.
/// Scheduler-specific state reset and callback timing remain local to the caller.
pub(crate) fn reconnect_backend_with_retry<FStop, FValidate, FProgress>(
    policy: &ReconnectPolicy,
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

        if ReconnectPolicy::sleep_with_stop(policy.backoff, &is_stopped, &mut on_progress) {
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
