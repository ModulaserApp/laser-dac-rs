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

    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use crate::backend::{BackendKind, DacBackend, FifoBackend, WriteOutcome};
    use crate::buffer_estimate::{BufferEstimator, SoftwareDecayEstimator};
    use crate::device::{DacCapabilities, DacType, EnabledDacTypes, OutputModel};
    use crate::discovery::{DacDiscovery, DiscoveredDevice, DiscoveredDeviceInfo, Discoverer};
    use crate::point::LaserPoint;

    const MOCK_ID: &str = "reconmock:1";

    // -------------------------------------------------------------------------
    // ReconnectPolicy::is_retriable
    // -------------------------------------------------------------------------

    #[test]
    fn is_retriable_true_for_transient_errors() {
        assert!(ReconnectPolicy::is_retriable(&Error::disconnected("gone")));
        assert!(ReconnectPolicy::is_retriable(&Error::backend(
            std::io::Error::new(std::io::ErrorKind::TimedOut, "usb timeout")
        )));
    }

    #[test]
    fn is_retriable_false_for_config_and_stop() {
        assert!(!ReconnectPolicy::is_retriable(&Error::invalid_config(
            "bad pps"
        )));
        assert!(!ReconnectPolicy::is_retriable(&Error::Stopped));
    }

    // -------------------------------------------------------------------------
    // ReconnectPolicy::sleep_with_stop
    // -------------------------------------------------------------------------

    #[test]
    fn sleep_with_stop_completes_and_ticks_progress() {
        let ticks = Cell::new(0u32);
        let mut on_progress = || ticks.set(ticks.get() + 1);

        let start = std::time::Instant::now();
        let stopped =
            ReconnectPolicy::sleep_with_stop(Duration::from_millis(10), || false, &mut on_progress);

        assert!(!stopped, "not stopped when the stop closure stays false");
        assert!(
            ticks.get() >= 1,
            "progress callback should tick at least once"
        );
        assert!(start.elapsed() >= Duration::from_millis(10));
    }

    #[test]
    fn sleep_with_stop_returns_early_when_stopped() {
        let mut on_progress = || {};
        let start = std::time::Instant::now();
        let stopped =
            ReconnectPolicy::sleep_with_stop(Duration::from_secs(10), || true, &mut on_progress);

        assert!(stopped);
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "should not actually sleep the full duration"
        );
    }

    // -------------------------------------------------------------------------
    // Mock backend + discoverer for reconnect_backend_with_retry
    // -------------------------------------------------------------------------

    struct MockFifo {
        connected: bool,
        estimator: SoftwareDecayEstimator,
    }

    impl MockFifo {
        fn new() -> Self {
            Self {
                connected: false,
                estimator: SoftwareDecayEstimator::new(),
            }
        }
    }

    impl DacBackend for MockFifo {
        fn dac_type(&self) -> DacType {
            DacType::Custom("ReconMock".into())
        }
        fn caps(&self) -> &DacCapabilities {
            static CAPS: DacCapabilities = DacCapabilities {
                pps_min: 1000,
                pps_max: 100_000,
                max_points_per_chunk: 1000,
                output_model: OutputModel::NetworkFifo,
            };
            &CAPS
        }
        fn connect(&mut self) -> Result<()> {
            self.connected = true;
            Ok(())
        }
        fn disconnect(&mut self) -> Result<()> {
            self.connected = false;
            Ok(())
        }
        fn is_connected(&self) -> bool {
            self.connected
        }
        fn stop(&mut self) -> Result<()> {
            Ok(())
        }
        fn set_shutter(&mut self, _open: bool) -> Result<()> {
            Ok(())
        }
    }

    impl FifoBackend for MockFifo {
        fn try_write_points(&mut self, _pps: u32, _points: &[LaserPoint]) -> Result<WriteOutcome> {
            Ok(WriteOutcome::Written)
        }
        fn estimator(&self) -> &dyn BufferEstimator {
            &self.estimator
        }
    }

    #[derive(Clone, Copy)]
    enum ConnectBehavior {
        Ok,
        InvalidConfig,
    }

    struct ReconMockDiscoverer {
        return_device: bool,
        connect: ConnectBehavior,
        connect_count: Arc<AtomicUsize>,
    }

    impl Discoverer for ReconMockDiscoverer {
        fn dac_type(&self) -> DacType {
            DacType::Custom("ReconMock".into())
        }
        fn prefix(&self) -> &str {
            "reconmock"
        }
        fn scan(&mut self) -> Vec<DiscoveredDevice> {
            if !self.return_device {
                return vec![];
            }
            let info = DiscoveredDeviceInfo::new(
                DacType::Custom("ReconMock".into()),
                MOCK_ID,
                "Recon Mock Device",
            );
            vec![DiscoveredDevice::new(info, Box::new(()))]
        }
        fn connect(&mut self, _opaque: Box<dyn std::any::Any + Send>) -> Result<BackendKind> {
            self.connect_count.fetch_add(1, Ordering::SeqCst);
            match self.connect {
                ConnectBehavior::Ok => Ok(BackendKind::Fifo(Box::new(MockFifo::new()))),
                ConnectBehavior::InvalidConfig => Err(Error::invalid_config("mock connect reject")),
            }
        }
    }

    /// Build a policy that re-opens `MOCK_ID` via a fresh `ReconMockDiscoverer`.
    fn make_policy(
        config: ReconnectConfig,
        return_device: bool,
        connect_behavior: ConnectBehavior,
        connect_count: Arc<AtomicUsize>,
    ) -> ReconnectPolicy {
        let factory = move || {
            let mut d = DacDiscovery::new(EnabledDacTypes::none());
            d.register(Box::new(ReconMockDiscoverer {
                return_device,
                connect: connect_behavior,
                connect_count: connect_count.clone(),
            }));
            d
        };
        let target = ReconnectTarget {
            device_id: MOCK_ID.to_string(),
            discovery_factory: Some(Box::new(factory)),
        };
        ReconnectPolicy::new(config, target)
    }

    fn accept(_info: &DacInfo, _backend: &BackendKind) -> std::result::Result<(), RunExit> {
        Ok(())
    }

    // -------------------------------------------------------------------------
    // reconnect_backend_with_retry edge cases
    // -------------------------------------------------------------------------

    #[test]
    fn retry_succeeds_and_fires_on_disconnect() {
        let disconnected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let disconnected_cb = disconnected.clone();
        let config = ReconnectConfig::new()
            .backoff(Duration::from_millis(1))
            .on_disconnect(move |_err| disconnected_cb.store(true, Ordering::SeqCst));
        let connect_count = Arc::new(AtomicUsize::new(0));
        let policy = make_policy(config, true, ConnectBehavior::Ok, connect_count.clone());

        let result = reconnect_backend_with_retry(&policy, None, || false, accept, || {});

        let (info, backend) = result.expect("should reconnect");
        assert_eq!(info.id, MOCK_ID);
        assert!(
            backend.is_connected(),
            "backend should be connected on return"
        );
        assert!(
            disconnected.load(Ordering::SeqCst),
            "on_disconnect callback should fire once at the start"
        );
        assert_eq!(connect_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn retry_exhausts_max_retries_to_disconnected() {
        // Discoverer returns no matching device → open_by_id fails (retriable).
        let config = ReconnectConfig::new()
            .max_retries(2)
            .backoff(Duration::from_millis(1));
        let connect_count = Arc::new(AtomicUsize::new(0));
        let policy = make_policy(config, false, ConnectBehavior::Ok, connect_count.clone());

        let result = reconnect_backend_with_retry(&policy, None, || false, accept, || {});
        assert_eq!(result.err(), Some(RunExit::Disconnected));
        assert_eq!(
            connect_count.load(Ordering::SeqCst),
            0,
            "connect is never reached when the device is not discoverable"
        );
    }

    #[test]
    fn retry_stops_during_backoff() {
        // The first attempt is normally immediate, so drive the backoff via the
        // flapping path: a recent `last_reconnect` forces a backoff *before* the
        // first attempt. is_stopped is false on the first (top-of-loop) check and
        // true afterwards — i.e. inside the backoff sleep — so we must bail with
        // Stopped before opening.
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_stop = calls.clone();
        let is_stopped = move || calls_stop.fetch_add(1, Ordering::SeqCst) >= 1;

        let config = ReconnectConfig::new().backoff(Duration::from_millis(20));
        let connect_count = Arc::new(AtomicUsize::new(0));
        let policy = make_policy(config, true, ConnectBehavior::Ok, connect_count.clone());

        let result =
            reconnect_backend_with_retry(&policy, Some(Instant::now()), is_stopped, accept, || {});
        assert_eq!(result.err(), Some(RunExit::Stopped));
        assert_eq!(
            connect_count.load(Ordering::SeqCst),
            0,
            "open should not run after a stop during backoff"
        );
    }

    #[test]
    fn retry_non_retriable_open_error_to_disconnected() {
        // Device is discoverable but connect() returns a non-retriable config error.
        let config = ReconnectConfig::new().backoff(Duration::from_millis(1));
        let connect_count = Arc::new(AtomicUsize::new(0));
        let policy = make_policy(
            config,
            true,
            ConnectBehavior::InvalidConfig,
            connect_count.clone(),
        );

        let result = reconnect_backend_with_retry(&policy, None, || false, accept, || {});
        assert_eq!(result.err(), Some(RunExit::Disconnected));
        assert_eq!(
            connect_count.load(Ordering::SeqCst),
            1,
            "connect is attempted exactly once before the non-retriable bail-out"
        );
    }

    #[test]
    fn retry_validate_rejection_propagates() {
        let config = ReconnectConfig::new().backoff(Duration::from_millis(1));
        let connect_count = Arc::new(AtomicUsize::new(0));
        let policy = make_policy(config, true, ConnectBehavior::Ok, connect_count.clone());

        let validated = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let validated_cb = validated.clone();
        let reject = move |_info: &DacInfo, _backend: &BackendKind| {
            validated_cb.store(true, Ordering::SeqCst);
            Err(RunExit::Disconnected)
        };

        let result = reconnect_backend_with_retry(&policy, None, || false, reject, || {});
        assert_eq!(result.err(), Some(RunExit::Disconnected));
        assert!(
            validated.load(Ordering::SeqCst),
            "validate closure should have run on the opened device"
        );
    }

    // -------------------------------------------------------------------------
    // is_flapping (first-attempt backoff floor)
    // -------------------------------------------------------------------------

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
