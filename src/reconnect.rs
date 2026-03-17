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

use crate::backend::{Error, Result};
use crate::discovery::DacDiscovery;
use crate::stream::Dac;
use crate::types::{DacInfo, ReconnectConfig};

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
    pub fn sleep_with_stop<F>(duration: Duration, is_stopped: F) -> bool
    where
        F: Fn() -> bool,
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
        }
        is_stopped()
    }
}
