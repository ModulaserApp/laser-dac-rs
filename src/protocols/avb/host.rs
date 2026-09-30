//! Audio host seam for the AVB backend.
//!
//! Every AVB call into the platform audio stack goes through a single
//! process-wide [`HostAccess`]. It enforces three rules that ASIO (the Windows
//! AVB host) needs and cpal does not provide:
//!
//! 1. **One host instance per process.** asio-sys tracks the loaded driver per
//!    `Asio` instance, but the Steinberg SDK underneath holds a single global
//!    driver slot. A second instance loading a driver silently unloads the
//!    driver a live stream is using, so discovery and connect must share one.
//! 2. **Serialized access.** The SDK is not thread-safe. Enumeration, stream
//!    open, and stream teardown hold [`HostAccess::session`] so a discovery
//!    scan never interleaves with a connect or disconnect.
//! 3. **A COM apartment on the calling thread.** See
//!    [`super::apartment`]: a session enters one for its duration.
//!
//! The [`AudioHost`] / [`OutputDevice`] traits exist so tests can drive the
//! real enumeration → resolve → worker-open path against a model of the ASIO
//! SDK instead of a stubbed engine.

use std::ops::Deref;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;

use super::apartment::{self, AudioThreadScope};
use super::backend::{
    fill_output_buffer_converted, OutputConfigRange, RuntimeState, SelectedStreamConfig,
};
use crate::error::{Error, Result};
use crate::protocols::audio_sink::{CpalStreamHandle, RunningAudioStream};

pub(super) trait OutputDevice {
    fn name(&self) -> Option<String>;
    fn output_config_ranges(&self) -> Vec<OutputConfigRange>;
    /// Default output `(channels, sample_rate)`, if the device reports one.
    fn default_output(&self) -> Option<(u16, u32)>;
    fn start_output(
        &self,
        config: SelectedStreamConfig,
        runtime: &Arc<RuntimeState>,
    ) -> Result<Box<dyn RunningAudioStream>>;
}

pub(super) trait AudioHost: Send + Sync + 'static {
    type Device: OutputDevice;
    /// Lazily yields output devices. On ASIO each `next()` loads a driver, and
    /// a driver that fails to load is skipped rather than reported.
    fn output_devices(&self) -> Result<Box<dyn Iterator<Item = Self::Device> + '_>>;
    /// Whether the host can run only one output stream at a time (ASIO).
    fn single_stream(&self) -> bool;
}

/// How long a caller waits for the host before giving up. A driver call that
/// never returns keeps the host busy; everyone else gets an error instead of
/// blocking with it.
const SESSION_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct HostAccess<H> {
    host: H,
    state: Mutex<HostState>,
    released: Condvar,
    session_timeout: Duration,
}

#[derive(Default)]
struct HostState {
    busy: bool,
    live_streams: usize,
}

impl<H: AudioHost> HostAccess<H> {
    pub(super) fn new(host: H) -> Self {
        Self::with_session_timeout(host, SESSION_TIMEOUT)
    }

    pub(super) fn with_session_timeout(host: H, session_timeout: Duration) -> Self {
        Self {
            host,
            state: Mutex::new(HostState::default()),
            released: Condvar::new(),
            session_timeout,
        }
    }

    /// Exclusive, apartment-initialized use of the host. Anything that loads,
    /// queries, or releases a driver must happen while a session is alive, and
    /// device handles obtained through it must be dropped before it ends.
    pub(super) fn session(&self) -> Result<HostSession<'_, H>> {
        let deadline = Instant::now() + self.session_timeout;
        let mut state = self.lock_state();
        while state.busy {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::disconnected(format!(
                    "AVB audio host busy for over {:?} (a driver call may be hung)",
                    self.session_timeout
                )));
            }
            state = self
                .released
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        state.busy = true;
        Ok(HostSession {
            access: self,
            _thread: apartment::enter(),
        })
    }

    fn lock_state(&self) -> MutexGuard<'_, HostState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub(super) struct HostSession<'a, H: AudioHost> {
    access: &'a HostAccess<H>,
    _thread: AudioThreadScope,
}

impl<H: AudioHost> HostSession<'_, H> {
    /// ASIO runs one stream per driver: a second open through the loaded
    /// driver disposes the buffers the live stream's callback is using.
    pub(super) fn ensure_stream_slot_free(&self) -> Result<()> {
        if self.access.host.single_stream() && self.access.lock_state().live_streams > 0 {
            return Err(Error::disconnected(
                "an AVB device is already streaming on this ASIO host; \
                 ASIO drives one device at a time",
            ));
        }
        Ok(())
    }
}

impl<H: AudioHost> Deref for HostSession<'_, H> {
    type Target = H;

    fn deref(&self) -> &H {
        &self.access.host
    }
}

impl<H: AudioHost> Drop for HostSession<'_, H> {
    // Releases the host before `_thread` leaves the apartment.
    fn drop(&mut self) {
        self.access.lock_state().busy = false;
        self.access.released.notify_one();
    }
}

/// A running stream that counts against the host's stream limit and whose
/// teardown (which releases the ASIO driver) is serialized with every other
/// host operation.
pub(super) struct SerializedStream<H: AudioHost> {
    stream: Option<Box<dyn RunningAudioStream>>,
    access: Arc<HostAccess<H>>,
}

impl<H: AudioHost> SerializedStream<H> {
    /// Must be called inside the session that opened `stream`.
    pub(super) fn boxed(
        stream: Box<dyn RunningAudioStream>,
        access: Arc<HostAccess<H>>,
    ) -> Box<dyn RunningAudioStream> {
        access.lock_state().live_streams += 1;
        Box::new(Self {
            stream: Some(stream),
            access,
        })
    }
}

impl<H: AudioHost> RunningAudioStream for SerializedStream<H> {}

impl<H: AudioHost> Drop for SerializedStream<H> {
    fn drop(&mut self) {
        // Stopping output beats waiting on a hung host: tear down regardless.
        let session = self.access.session();
        if let Err(err) = &session {
            log::error!(
                "AVB: stopping stream without exclusive host access: {}",
                err
            );
        }
        self.stream.take();
        self.access.lock_state().live_streams -= 1;
        drop(session);
    }
}

/// The process-wide cpal host used for AVB output.
pub(super) fn cpal_host_access() -> Result<Arc<HostAccess<cpal::Host>>> {
    static SHARED: OnceLock<Arc<HostAccess<cpal::Host>>> = OnceLock::new();
    if let Some(access) = SHARED.get() {
        return Ok(Arc::clone(access));
    }
    let host = get_audio_host()?;
    Ok(Arc::clone(
        SHARED.get_or_init(|| Arc::new(HostAccess::new(host))),
    ))
}

/// Returns the cpal audio host to use for AVB output.
///
/// - Windows with the `asio` default feature: the ASIO host (recommended
///   for reliable multichannel output on pro audio interfaces).
/// - Otherwise: the cpal default host — CoreAudio on macOS, WASAPI on
///   Windows (when `asio` is disabled), ALSA on Linux.
fn get_audio_host() -> Result<cpal::Host> {
    #[cfg(all(target_os = "windows", feature = "asio"))]
    {
        cpal::host_from_id(cpal::HostId::Asio).map_err(|e| {
            Error::invalid_config(format!(
                "ASIO host not available (is the ASIO SDK installed?): {}",
                e
            ))
        })
    }
    #[cfg(not(all(target_os = "windows", feature = "asio")))]
    {
        Ok(cpal::default_host())
    }
}

impl AudioHost for cpal::Host {
    type Device = cpal::Device;

    fn output_devices(&self) -> Result<Box<dyn Iterator<Item = cpal::Device> + '_>> {
        let devices = HostTrait::output_devices(self).map_err(Error::backend)?;
        Ok(Box::new(devices))
    }

    fn single_stream(&self) -> bool {
        #[cfg(all(target_os = "windows", feature = "asio"))]
        {
            self.id() == cpal::HostId::Asio
        }
        #[cfg(not(all(target_os = "windows", feature = "asio")))]
        {
            false
        }
    }
}

impl OutputDevice for cpal::Device {
    fn name(&self) -> Option<String> {
        DeviceTrait::name(self).ok()
    }

    fn output_config_ranges(&self) -> Vec<OutputConfigRange> {
        self.supported_output_configs()
            .map(|configs| {
                configs
                    .map(|cfg| OutputConfigRange {
                        channels: cfg.channels(),
                        min_sample_rate: cfg.min_sample_rate().0,
                        max_sample_rate: cfg.max_sample_rate().0,
                        sample_format: cfg.sample_format(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn default_output(&self) -> Option<(u16, u32)> {
        self.default_output_config()
            .ok()
            .map(|cfg| (cfg.channels(), cfg.sample_rate().0))
    }

    fn start_output(
        &self,
        config: SelectedStreamConfig,
        runtime: &Arc<RuntimeState>,
    ) -> Result<Box<dyn RunningAudioStream>> {
        let output_channels = config.channels as usize;
        let sample_format = config.sample_format;
        let stream = build_output_stream_for_format(
            self,
            &build_cpal_stream_config(config),
            output_channels,
            sample_format,
            runtime,
        )?;
        stream.play().map_err(Error::backend)?;
        Ok(CpalStreamHandle::boxed(stream))
    }
}

pub(super) fn build_cpal_stream_config(stream_config: SelectedStreamConfig) -> cpal::StreamConfig {
    // Always let the host/driver pick the buffer size. Requesting a fixed
    // size fails on drivers that don't support it: ASIO drivers (e.g. RME)
    // only accept the buffer size configured in their own control panel, and
    // WASAPI shared mode can reject buffer durations that don't match the
    // engine period. The queue provides the jitter cushion, so the device
    // buffer size only affects callback granularity, not correctness.
    cpal::StreamConfig {
        channels: stream_config.channels,
        sample_rate: cpal::SampleRate(stream_config.sample_rate),
        buffer_size: cpal::BufferSize::Default,
    }
}

/// Build an output stream for the given sample format, converting f32 samples
/// to the device's native format inside the callback.
fn build_output_stream_for_format(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    output_channels: usize,
    sample_format: SampleFormat,
    runtime: &Arc<RuntimeState>,
) -> Result<cpal::Stream> {
    let callback_state = Arc::clone(runtime);
    let err_state = Arc::clone(runtime);
    let err_fn = move |err: cpal::StreamError| {
        log::error!("AVB output stream error: {}", err);
        if matches!(err, cpal::StreamError::DeviceNotAvailable) {
            err_state.mark_stream_failed();
        }
    };

    let built = match sample_format {
        SampleFormat::F32 => device.build_output_stream(
            config,
            move |data: &mut [f32], _| {
                fill_output_buffer_converted(data, output_channels, &callback_state)
            },
            err_fn,
            None,
        ),
        SampleFormat::I16 => device.build_output_stream(
            config,
            move |data: &mut [i16], _| {
                fill_output_buffer_converted(data, output_channels, &callback_state)
            },
            err_fn,
            None,
        ),
        SampleFormat::I32 => device.build_output_stream(
            config,
            move |data: &mut [i32], _| {
                fill_output_buffer_converted(data, output_channels, &callback_state)
            },
            err_fn,
            None,
        ),
        _ => return Err(Error::backend(super::error::Error::UnsupportedOutputConfig)),
    };

    built.map_err(Error::backend)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_cpal_stream_config_uses_default_buffer_size() {
        let config = build_cpal_stream_config(SelectedStreamConfig {
            channels: 6,
            sample_rate: 48_000,
            sample_format: SampleFormat::F32,
        });
        assert_eq!(config.buffer_size, cpal::BufferSize::Default);
        assert_eq!(config.channels, 6);
        assert_eq!(config.sample_rate, cpal::SampleRate(48_000));
    }

    #[test]
    fn cpal_host_access_is_shared_process_wide() {
        // A second host instance would get its own asio-sys driver bookkeeping
        // and unload the driver under a live stream on its next scan.
        let first = cpal_host_access().unwrap();
        let second = cpal_host_access().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }
}
