//! Ether Dream DAC streaming backend implementation.

use crate::backend::{StreamBackend, WriteOutcome};
use crate::error::{Error, Result};
use crate::protocols::ether_dream::dac::{stream, LightEngine, Playback, PlaybackFlags};
use crate::protocols::ether_dream::protocol::{DacBroadcast, DacPoint};
use crate::types::{caps_for_dac_type, Caps, DacType, LaserPoint};
use std::net::IpAddr;
use std::time::Duration;

/// Ether Dream DAC backend (network).
pub struct EtherDreamBackend {
    broadcast: DacBroadcast,
    ip_addr: IpAddr,
    stream: Option<stream::Stream>,
    caps: Caps,
}

impl EtherDreamBackend {
    pub fn new(broadcast: DacBroadcast, ip_addr: IpAddr) -> Self {
        Self {
            broadcast,
            ip_addr,
            stream: None,
            caps: caps_for_dac_type(&DacType::EtherDream),
        }
    }
}

impl StreamBackend for EtherDreamBackend {
    fn dac_type(&self) -> DacType {
        DacType::EtherDream
    }

    fn caps(&self) -> &Caps {
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

    fn try_write_chunk(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| Error::disconnected("Not connected"))?;

        let dac_points: Vec<DacPoint> = points.iter().map(|p| p.into()).collect();
        if dac_points.is_empty() {
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
            }
            LightEngine::Warmup | LightEngine::Cooldown => {
                return Ok(WriteOutcome::WouldBlock);
            }
            LightEngine::Ready => {}
        }

        let buffer_capacity = stream.dac().buffer_capacity;
        let buffer_fullness = stream.dac().status.buffer_fullness;
        let available = buffer_capacity as usize - buffer_fullness as usize - 1;

        if available == 0 {
            return Ok(WriteOutcome::WouldBlock);
        }

        let point_rate = if pps > 0 {
            pps
        } else {
            stream.dac().max_point_rate / 16
        };

        const MIN_POINTS_BEFORE_BEGIN: u16 = 500;
        let target_buffer_points = (point_rate / 20).max(MIN_POINTS_BEFORE_BEGIN as u32) as usize;
        let target_len = target_buffer_points
            .min(available)
            .max(dac_points.len().min(available));

        let mut points_to_send = dac_points;
        if points_to_send.len() > available {
            points_to_send.truncate(available);
        } else if points_to_send.len() < target_len {
            let seed = points_to_send.clone();
            points_to_send.extend(seed.iter().cycle().take(target_len - points_to_send.len()));
        }

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
                .data(points_to_send)
                .submit()
                .map_err(Error::backend)?;
        } else {
            let send_result = stream
                .queue_commands()
                .data(points_to_send.clone())
                .submit();

            if send_result.is_err() && stream.dac().status.playback == Playback::Idle {
                stream
                    .queue_commands()
                    .prepare_stream()
                    .submit()
                    .map_err(Error::backend)?;

                stream
                    .queue_commands()
                    .data(points_to_send)
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

        Ok(WriteOutcome::Written)
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

    fn queued_points(&self) -> Option<u64> {
        self.stream
            .as_ref()
            .map(|s| s.dac().status.buffer_fullness as u64)
    }
}
