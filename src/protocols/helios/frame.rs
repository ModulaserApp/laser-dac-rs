//! Helios frame and point types.

use bitflags::bitflags;

use crate::types::LaserPoint;

/// A frame to be sent to the Helios DAC.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    /// Rate of output in points per second
    pub pps: u32,
    /// Frame flags (default is empty)
    pub flags: WriteFrameFlags,
    /// Points in this frame
    pub points: Vec<Point>,
}

impl Frame {
    /// Create a new frame with the given point rate and points.
    ///
    /// Defaults to `SINGLE_MODE` (play once, don't repeat), matching the
    /// official Helios SDK's `HELIOS_FLAGS_DEFAULT`. This prevents the DAC
    /// from repeating the last frame indefinitely if the host stops sending.
    pub fn new(pps: u32, points: Vec<Point>) -> Self {
        Frame {
            pps,
            points,
            flags: WriteFrameFlags::SINGLE_MODE,
        }
    }

    /// Create a new frame with specific flags.
    pub fn new_with_flags(pps: u32, points: Vec<Point>, flags: WriteFrameFlags) -> Self {
        Frame { pps, points, flags }
    }
}

/// A single laser point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    /// X/Y coordinate
    pub coordinate: Coordinate,
    /// RGB color
    pub color: Color,
    /// Intensity (0-255)
    pub intensity: u8,
}

/// Coordinates (x, y) for Helios DAC.
///
/// 12 bit (from 0 to 0xFFF)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coordinate {
    pub x: u16,
    pub y: u16,
}

impl From<(u16, u16)> for Coordinate {
    fn from((x, y): (u16, u16)) -> Self {
        Coordinate { x, y }
    }
}

/// RGB color for a laser point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    /// Red channel (0-255)
    pub r: u8,
    /// Green channel (0-255)
    pub g: u8,
    /// Blue channel (0-255)
    pub b: u8,
}

impl Color {
    /// Create a new color.
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Color { r, g, b }
    }
}

bitflags! {
    /// Flags for WriteFrame operation.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct WriteFrameFlags: u8 {
        /// Bit 0 (LSB) = if 1, start output immediately, instead of waiting for current frame (if there is one) to finish playing
        const START_IMMEDIATELY = 0b0000_0001;
        /// Bit 1 = if 1, play frame only once, instead of repeating until another frame is written
        const SINGLE_MODE = 0b0000_0010;
        /// Bit 2 = if 1, don't let WriteFrame() block execution while waiting for the transfer to finish
        const DONT_BLOCK = 0b0000_0100;
    }
}

impl From<&LaserPoint> for Point {
    /// Convert a [`LaserPoint`] to a Helios [`Point`].
    ///
    /// [`LaserPoint`] uses f32 coordinates (-1.0 to 1.0) and u16 colors (0-65535).
    /// Helios uses u16 12-bit coordinates (0-4095) with inverted axes and u8 colors.
    fn from(p: &LaserPoint) -> Self {
        let dac_x = ((1.0 - (p.x + 1.0) / 2.0).clamp(0.0, 1.0) * 4095.0) as u16;
        let dac_y = ((1.0 - (p.y + 1.0) / 2.0).clamp(0.0, 1.0) * 4095.0) as u16;

        Point {
            coordinate: Coordinate { x: dac_x, y: dac_y },
            color: Color::new((p.r >> 8) as u8, (p.g >> 8) as u8, (p.b >> 8) as u8),
            intensity: (p.intensity >> 8) as u8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==========================================================================
    // LaserPoint to Helios Point Conversion Tests
    // These test the From<&LaserPoint> implementation which handles:
    // - Coordinate inversion (Helios has inverted axes)
    // - 12-bit conversion (f32 -1..1 to u16 0..4095)
    // - Out-of-range clamping
    // ==========================================================================

    #[test]
    fn test_helios_conversion_center() {
        // Center point (0, 0) should map to (2047, 2047) due to inversion
        // Colors: u16 values that downscale to expected u8 values (128, 64, 32, 200)
        let laser_point = LaserPoint::new(0.0, 0.0, 128 * 257, 64 * 257, 32 * 257, 200 * 257);
        let helios_point: Point = (&laser_point).into();

        // (1.0 - (0.0 + 1.0) / 2.0) * 4095 = (1.0 - 0.5) * 4095 = 2047.5 -> 2047
        assert_eq!(helios_point.coordinate.x, 2047);
        assert_eq!(helios_point.coordinate.y, 2047);
        // Colors should downscale from u16 to u8 (>> 8)
        assert_eq!(helios_point.color.r, 128);
        assert_eq!(helios_point.color.g, 64);
        assert_eq!(helios_point.color.b, 32);
        assert_eq!(helios_point.intensity, 200);
    }

    #[test]
    fn test_helios_conversion_boundaries() {
        // Min point (-1, -1) should map to (4095, 4095) due to inversion
        let min = LaserPoint::new(-1.0, -1.0, 0, 0, 0, 0);
        let min_helios: Point = (&min).into();
        assert_eq!(min_helios.coordinate.x, 4095);
        assert_eq!(min_helios.coordinate.y, 4095);

        // Max point (1, 1) should map to (0, 0) due to inversion
        let max = LaserPoint::new(1.0, 1.0, 0, 0, 0, 0);
        let max_helios: Point = (&max).into();
        assert_eq!(max_helios.coordinate.x, 0);
        assert_eq!(max_helios.coordinate.y, 0);
    }

    #[test]
    fn test_helios_conversion_asymmetric() {
        // Test that x and y convert independently with different values
        let laser_point = LaserPoint::new(-0.5, 0.5, 0, 0, 0, 0);
        let helios_point: Point = (&laser_point).into();

        // x: (1.0 - (-0.5 + 1.0) / 2.0) * 4095 = (1.0 - 0.25) * 4095 = 3071
        // y: (1.0 - (0.5 + 1.0) / 2.0) * 4095 = (1.0 - 0.75) * 4095 = 1023
        assert_eq!(helios_point.coordinate.x, 3071);
        assert_eq!(helios_point.coordinate.y, 1023);
    }

    #[test]
    fn test_helios_conversion_clamps_out_of_range() {
        // Out of range positive values should clamp to 0 (due to inversion)
        let positive = LaserPoint::new(2.0, 3.0, 0, 0, 0, 0);
        let positive_helios: Point = (&positive).into();
        assert_eq!(positive_helios.coordinate.x, 0);
        assert_eq!(positive_helios.coordinate.y, 0);

        // Out of range negative values should clamp to 4095 (due to inversion)
        let negative = LaserPoint::new(-2.0, -3.0, 0, 0, 0, 0);
        let negative_helios: Point = (&negative).into();
        assert_eq!(negative_helios.coordinate.x, 4095);
        assert_eq!(negative_helios.coordinate.y, 4095);
    }

    #[test]
    fn test_helios_inversion_symmetry() {
        // Verify that x and -x produce symmetric results around center
        // This validates the inversion formula is mathematically correct
        let p1 = LaserPoint::new(0.5, 0.0, 0, 0, 0, 0);
        let p2 = LaserPoint::new(-0.5, 0.0, 0, 0, 0, 0);
        let h1: Point = (&p1).into();
        let h2: Point = (&p2).into();

        // Due to 12-bit resolution, h1.x + h2.x should equal ~4095
        let sum = h1.coordinate.x as i32 + h2.coordinate.x as i32;
        assert!((sum - 4095).abs() <= 1, "Sum was {}, expected ~4095", sum);
    }

    #[test]
    fn test_helios_conversion_infinity_clamps() {
        let laser_point = LaserPoint::new(f32::INFINITY, f32::NEG_INFINITY, 0, 0, 0, 0);
        let helios_point: Point = (&laser_point).into();

        // Infinity clamps like out-of-range values
        assert_eq!(helios_point.coordinate.x, 0);
        assert_eq!(helios_point.coordinate.y, 4095);
    }

    #[test]
    fn test_helios_conversion_nan_does_not_panic() {
        // NaN should produce some valid output without panicking
        let laser_point = LaserPoint::new(
            f32::NAN,
            f32::NAN,
            100 * 257,
            100 * 257,
            100 * 257,
            100 * 257,
        );
        let helios_point: Point = (&laser_point).into();

        // Just verify it's within valid range
        assert!(helios_point.coordinate.x <= 4095);
        assert!(helios_point.coordinate.y <= 4095);
    }
}
