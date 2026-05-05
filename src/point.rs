//! The DAC-agnostic laser point type and coordinate/colour conversion helpers.
//!
//! [`LaserPoint`] is the neutral point representation used throughout the
//! crate. Each protocol converts `LaserPoint` to its own wire format inside
//! its own module — `LaserPoint` itself does not know about wire formats.
//! The `coord_*` and `color_*` helpers are crate-private utilities shared by
//! protocol backends with the same target encoding.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// A DAC-agnostic laser point with full-precision f32 coordinates.
///
/// Coordinates are normalized:
/// - x: -1.0 (left) to 1.0 (right)
/// - y: -1.0 (bottom) to 1.0 (top)
///
/// Colors are 16-bit (0-65535) to support high-resolution DACs.
/// DACs with lower resolution (8-bit) will downscale automatically.
///
/// This allows each DAC to convert to its native format:
/// - Helios: 12-bit unsigned (0-4095), inverted
/// - EtherDream: 16-bit signed (-32768 to 32767)
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct LaserPoint {
    /// X coordinate, -1.0 to 1.0
    pub x: f32,
    /// Y coordinate, -1.0 to 1.0
    pub y: f32,
    /// Red channel (0-65535)
    pub r: u16,
    /// Green channel (0-65535)
    pub g: u16,
    /// Blue channel (0-65535)
    pub b: u16,
    /// Intensity (0-65535)
    pub intensity: u16,
}

impl LaserPoint {
    /// Creates a new laser point.
    pub fn new(x: f32, y: f32, r: u16, g: u16, b: u16, intensity: u16) -> Self {
        Self {
            x,
            y,
            r,
            g,
            b,
            intensity,
        }
    }

    /// Creates a blanked point (laser off) at the given position.
    pub fn blanked(x: f32, y: f32) -> Self {
        Self {
            x,
            y,
            ..Default::default()
        }
    }

    // =========================================================================
    // Coordinate conversion helpers (shared across protocol backends)
    // =========================================================================

    /// Convert a coordinate from [-1.0, 1.0] to 12-bit unsigned (0-4095) with axis inversion.
    ///
    /// Used by Helios and LaserCube WiFi backends.
    #[inline]
    pub(crate) fn coord_to_u12_inverted(v: f32) -> u16 {
        ((1.0 - (v + 1.0) / 2.0).clamp(0.0, 1.0) * 4095.0).round() as u16
    }

    /// Convert a coordinate from [-1.0, 1.0] to 12-bit unsigned (0-4095).
    ///
    /// Used by LaserCube USB backend.
    #[inline]
    pub(crate) fn coord_to_u12(v: f32) -> u16 {
        (((v.clamp(-1.0, 1.0) + 1.0) / 2.0) * 4095.0).round() as u16
    }

    /// Convert a coordinate from [-1.0, 1.0] to signed 16-bit (-32767 to 32767) with inversion.
    ///
    /// Used by Ether Dream and IDN backends.
    #[inline]
    pub(crate) fn coord_to_i16_inverted(v: f32) -> i16 {
        (v.clamp(-1.0, 1.0) * -32767.0).round() as i16
    }

    /// Downscale a u16 color channel (0-65535) to u8 (0-255).
    #[inline]
    pub(crate) fn color_to_u8(v: u16) -> u8 {
        (v >> 8) as u8
    }

    /// Downscale a u16 color channel (0-65535) to 12-bit (0-4095).
    #[inline]
    pub(crate) fn color_to_u12(v: u16) -> u16 {
        v >> 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_laser_point_blanked_sets_all_colors_to_zero() {
        // blanked() should set all color channels to 0 while preserving position
        let point = LaserPoint::blanked(0.25, 0.75);
        assert_eq!(point.x, 0.25);
        assert_eq!(point.y, 0.75);
        assert_eq!(point.r, 0);
        assert_eq!(point.g, 0);
        assert_eq!(point.b, 0);
        assert_eq!(point.intensity, 0);
    }
}
