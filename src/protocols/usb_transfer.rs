//! A minimal seam over `rusb` USB endpoint transfers.
//!
//! `rusb::DeviceHandle` exposes its transfers as inherent methods on a concrete
//! type, so code that calls them directly cannot be tested without hardware.
//! This trait mirrors exactly the endpoint operations the Helios and LaserCube
//! USB backends need, with a blanket implementation for the real handle. Streams
//! and comms are made generic over it (`Stream<H: UsbEndpoints>`), so tests can
//! inject a fake device that records writes and scripts reads — the same pattern
//! `DatagramSocket` provides for the network transports.

use std::time::Duration;

/// USB endpoint transfer operations, mirroring the subset of
/// [`rusb::DeviceHandle`] used by the USB DAC backends.
///
/// All methods take `&self`, matching `rusb`'s interior-mutable handle, so
/// making a struct generic over this trait requires no `&mut` changes. It is
/// public because it appears as a bound on the public `Stream` type, but
/// callers never name it directly — they use the concrete stream from `open()`.
pub trait UsbEndpoints {
    fn write_bulk(&self, endpoint: u8, buf: &[u8], timeout: Duration) -> rusb::Result<usize>;
    fn read_bulk(&self, endpoint: u8, buf: &mut [u8], timeout: Duration) -> rusb::Result<usize>;
    fn write_interrupt(&self, endpoint: u8, buf: &[u8], timeout: Duration) -> rusb::Result<usize>;
    fn read_interrupt(
        &self,
        endpoint: u8,
        buf: &mut [u8],
        timeout: Duration,
    ) -> rusb::Result<usize>;
    fn clear_halt(&self, endpoint: u8) -> rusb::Result<()>;
    fn release_interface(&self, iface: u8) -> rusb::Result<()>;
}

impl<T: rusb::UsbContext> UsbEndpoints for rusb::DeviceHandle<T> {
    fn write_bulk(&self, endpoint: u8, buf: &[u8], timeout: Duration) -> rusb::Result<usize> {
        rusb::DeviceHandle::write_bulk(self, endpoint, buf, timeout)
    }

    fn read_bulk(&self, endpoint: u8, buf: &mut [u8], timeout: Duration) -> rusb::Result<usize> {
        rusb::DeviceHandle::read_bulk(self, endpoint, buf, timeout)
    }

    fn write_interrupt(&self, endpoint: u8, buf: &[u8], timeout: Duration) -> rusb::Result<usize> {
        rusb::DeviceHandle::write_interrupt(self, endpoint, buf, timeout)
    }

    fn read_interrupt(
        &self,
        endpoint: u8,
        buf: &mut [u8],
        timeout: Duration,
    ) -> rusb::Result<usize> {
        rusb::DeviceHandle::read_interrupt(self, endpoint, buf, timeout)
    }

    fn clear_halt(&self, endpoint: u8) -> rusb::Result<()> {
        rusb::DeviceHandle::clear_halt(self, endpoint)
    }

    fn release_interface(&self, iface: u8) -> rusb::Result<()> {
        rusb::DeviceHandle::release_interface(self, iface)
    }
}
