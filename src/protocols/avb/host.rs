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
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

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
}

pub(super) struct HostAccess<H> {
    host: H,
    lock: Mutex<()>,
}

impl<H> HostAccess<H> {
    pub(super) fn new(host: H) -> Self {
        Self {
            host,
            lock: Mutex::new(()),
        }
    }

    /// Exclusive, apartment-initialized use of the host. Anything that loads,
    /// queries, or releases a driver must happen while a session is alive, and
    /// device handles obtained through it must be dropped before it ends.
    pub(super) fn session(&self) -> HostSession<'_, H> {
        HostSession {
            host: &self.host,
            _lock: self.lock.lock().unwrap_or_else(PoisonError::into_inner),
            _thread: apartment::enter(),
        }
    }
}

pub(super) struct HostSession<'a, H> {
    host: &'a H,
    // Field order is drop order: release the lock before leaving the apartment.
    _lock: MutexGuard<'a, ()>,
    _thread: AudioThreadScope,
}

impl<H> Deref for HostSession<'_, H> {
    type Target = H;

    fn deref(&self) -> &H {
        self.host
    }
}

/// A running stream whose teardown (which releases the ASIO driver) is
/// serialized with every other host operation.
pub(super) struct SerializedStream<H: AudioHost> {
    stream: Option<Box<dyn RunningAudioStream>>,
    access: Arc<HostAccess<H>>,
}

impl<H: AudioHost> SerializedStream<H> {
    pub(super) fn boxed(
        stream: Box<dyn RunningAudioStream>,
        access: Arc<HostAccess<H>>,
    ) -> Box<dyn RunningAudioStream> {
        Box::new(Self {
            stream: Some(stream),
            access,
        })
    }
}

impl<H: AudioHost> RunningAudioStream for SerializedStream<H> {}

impl<H: AudioHost> Drop for SerializedStream<H> {
    fn drop(&mut self) {
        let _session = self.access.session();
        self.stream.take();
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
