//! Ether Dream DAC streaming backend implementation.

use crate::backend::{DacBackend, FifoBackend, WriteOutcome};
use crate::error::{Error, Result};
use crate::protocols::ether_dream::dac::{stream, LightEngine, Playback, PlaybackFlags};
use crate::protocols::ether_dream::protocol::{DacBroadcast, DacPoint};
use crate::types::{DacCapabilities, DacType, LaserPoint};
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// Ether Dream DAC backend (network).
pub struct EtherDreamBackend {
    broadcast: DacBroadcast,
    ip_addr: IpAddr,
    stream: Option<stream::Stream>,
    caps: DacCapabilities,
    /// When we last received a fresh status from the DAC.
    last_status_time: Option<Instant>,
    /// The point rate from the last write (for decay calculation).
    last_point_rate: u32,
    /// Pre-allocated conversion buffer (avoids per-write heap allocation).
    point_buffer: Vec<DacPoint>,
}

impl EtherDreamBackend {
    pub fn new(broadcast: DacBroadcast, ip_addr: IpAddr) -> Self {
        Self {
            broadcast,
            ip_addr,
            stream: None,
            caps: super::default_capabilities(),
            last_status_time: None,
            last_point_rate: 0,
            point_buffer: Vec::new(),
        }
    }

    /// Decay a raw buffer fullness value based on elapsed time since last status.
    fn decay_fullness(
        raw: u16,
        capacity: u16,
        last_status_time: Option<Instant>,
        point_rate: u32,
    ) -> u16 {
        if let Some(last_time) = last_status_time {
            let elapsed_secs = last_time.elapsed().as_secs_f64();
            let consumed = (elapsed_secs * point_rate as f64) as u16;
            raw.saturating_sub(consumed).min(capacity)
        } else {
            raw
        }
    }
}

impl DacBackend for EtherDreamBackend {
    fn dac_type(&self) -> DacType {
        DacType::EtherDream
    }

    fn caps(&self) -> &DacCapabilities {
        &self.caps
    }

    fn connect(&mut self) -> Result<()> {
        let stream = stream::connect_timeout(&self.broadcast, self.ip_addr, Duration::from_secs(5))
            .map_err(Error::backend)?;

        self.stream = Some(stream);
        Ok(())
    }

    fn disconnect(&mut self) -> Result<()> {
        if let Some(stream) = &mut self.stream {
            let _ = stream.queue_commands().stop().submit();
        }
        self.stream = None;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(stream) = &mut self.stream {
            stream
                .queue_commands()
                .stop()
                .submit()
                .map_err(Error::backend)?;
        }
        Ok(())
    }

    fn set_shutter(&mut self, _open: bool) -> Result<()> {
        Ok(())
    }
}

impl FifoBackend for EtherDreamBackend {
    fn try_write_points(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| Error::disconnected("Not connected"))?;

        if points.is_empty() {
            return Ok(WriteOutcome::WouldBlock);
        }

        match stream.dac().status.light_engine {
            LightEngine::EmergencyStop => {
                stream
                    .queue_commands()
                    .clear_emergency_stop()
                    .submit()
                    .map_err(Error::backend)?;

                stream
                    .queue_commands()
                    .ping()
                    .submit()
                    .map_err(Error::backend)?;

                if stream.dac().status.light_engine == LightEngine::EmergencyStop {
                    return Err(Error::disconnected(
                        "DAC stuck in emergency stop - check hardware interlock",
                    ));
                }
                // Status is now fresh from the ping response.
                self.last_status_time = Some(Instant::now());
            }
            LightEngine::Warmup | LightEngine::Cooldown => {
                return Ok(WriteOutcome::WouldBlock);
            }
            LightEngine::Ready => {}
        }

        let buffer_capacity = stream.dac().buffer_capacity;
        let raw_fullness = stream.dac().status.buffer_fullness;
        let fullness = Self::decay_fullness(
            raw_fullness,
            buffer_capacity,
            self.last_status_time,
            self.last_point_rate,
        );

        let available = buffer_capacity.saturating_sub(fullness).saturating_sub(1) as usize;

        if available == 0 {
            return Ok(WriteOutcome::WouldBlock);
        }

        let point_rate = if pps > 0 {
            pps
        } else {
            stream.dac().max_point_rate / 16
        };

        const MIN_POINTS_BEFORE_BEGIN: u16 = 256;

        // Convert points into pre-allocated buffer (avoids per-write allocation).
        self.point_buffer.clear();
        let count = points.len().min(available);
        self.point_buffer
            .extend(points[..count].iter().map(DacPoint::from));

        let playback_flags = stream.dac().status.playback_flags;
        let playback = stream.dac().status.playback;
        let current_point_rate = stream.dac().status.point_rate;

        let needs_prepare =
            playback_flags.contains(PlaybackFlags::UNDERFLOWED) || playback == Playback::Idle;

        if needs_prepare {
            stream
                .queue_commands()
                .prepare_stream()
                .submit()
                .map_err(Error::backend)?;
        }

        if playback == Playback::Playing && current_point_rate != point_rate {
            stream
                .queue_commands()
                .update(0, point_rate)
                .data(self.point_buffer.iter().copied())
                .submit()
                .map_err(Error::backend)?;
        } else {
            let send_result = stream
                .queue_commands()
                .data(self.point_buffer.iter().copied())
                .submit();

            if send_result.is_err() && stream.dac().status.playback == Playback::Idle {
                stream
                    .queue_commands()
                    .prepare_stream()
                    .submit()
                    .map_err(Error::backend)?;

                stream
                    .queue_commands()
                    .data(self.point_buffer.iter().copied())
                    .submit()
                    .map_err(Error::backend)?;
            } else {
                send_result.map_err(Error::backend)?;
            }
        }

        let buffer_fullness = stream.dac().status.buffer_fullness;
        let needs_begin =
            playback != Playback::Playing && buffer_fullness >= MIN_POINTS_BEFORE_BEGIN;

        if needs_begin {
            stream
                .queue_commands()
                .begin(0, point_rate)
                .submit()
                .map_err(Error::backend)?;
        }

        self.last_status_time = Some(Instant::now());
        self.last_point_rate = point_rate;
        Ok(WriteOutcome::Written)
    }

    fn queued_points(&self) -> Option<u64> {
        self.stream.as_ref().map(|s| {
            let raw = s.dac().status.buffer_fullness;
            let cap = s.dac().buffer_capacity;
            Self::decay_fullness(raw, cap, self.last_status_time, self.last_point_rate) as u64
        })
    }
}
