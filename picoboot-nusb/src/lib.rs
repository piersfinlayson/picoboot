// Copyright (C) 2026 Piers Finlayson <piers@piers.rocks>
//
// MIT License

//! # picoboot-nusb
//!
//! The USB [`Transport`] for [`picoboot`], using `nusb`.
//!
//! ```rust,no_run
//! use picoboot::{Error, Picoboot};
//! use picoboot_nusb::UsbDevice;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Error> {
//!     let mut picoboot = Picoboot::new(UsbDevice::first(None).await?)?;
//!     let data = picoboot.flash_read(picoboot::FLASH_START, 256).await?;
//!     Ok(())
//! }
//! ```
//!
//! ## Feature Flags
//!
//! `tokio` (the default) and `smol` select nusb's async runtime.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
#[cfg(target_os = "linux")]
use nusb::ErrorKind as NusbErrorKind;
use nusb::descriptors::TransferType;
use nusb::transfer::ControlType::{Standard, Vendor};
use nusb::transfer::{
    Buffer, Bulk, ControlIn, ControlOut, Direction as NusbDirection, In, Out, Recipient,
    TransferError,
};
use nusb::{Device, DeviceInfo, Endpoint, Interface, MaybeFuture};
use picoboot::cmd::PicobootStatusCmd;
use picoboot::{
    BoxFuture, Direction, Error, PICOBOOT_PID_RP2040, PICOBOOT_PID_RP2350, PICOBOOT_VID, Target,
    Timeouts, Transaction, Transport, TransportError, TransportErrorKind,
};

type Result<T> = std::result::Result<T, Error>;

// PICOBOOT's vendor control requests.  See the RP2350 datasheet.
const REQUEST_RESET: u8 = 0x41;
const REQUEST_GET_COMMAND_STATUS: u8 = 0x42;
const COMMAND_STATUS_LEN: u16 = 16;

// Standard USB GET_STATUS, and the bit an endpoint's answer sets when it is
// halted.  See USB 2.0 sections 9.4.5 and 9.4.6.
const REQUEST_GET_STATUS: u8 = 0x00;
const ENDPOINT_STATUS_HALT: u8 = 0x01;
const ENDPOINT_STATUS_LEN: u16 = 2;

// USB class/subclass for PICOBOOT
const PICOBOOT_USB_CLASS: u8 = 0xFF;
const PICOBOOT_USB_SUBCLASS: u8 = 0x00;

/// A USB device with a PICOBOOT interface.
///
/// Copies share one open count.  The first open claims the interface and the
/// last close releases it.
#[derive(Debug, Clone)]
pub struct UsbDevice {
    info: DeviceInfo,
    if_num: u8,
    in_ep: u8,
    out_ep: u8,
    in_ep_max_packet_size: usize,
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct State {
    opens: usize,
    claimed: Option<Claimed>,
}

/// The claimed PICOBOOT interface.
#[derive(Debug, Clone)]
struct Claimed {
    // Device is used for control transfers off Windows, and on Linux for the
    // kernel driver.
    #[allow(dead_code)]
    device: Device,
    interface: Interface,
    target: Target,
    kernel_driver_detached: bool,
}

/// A USB failure, the source of picoboot's [`TransportError`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UsbError {
    #[error("Error enumerating USB devices: {0}")]
    Enumerate(nusb::Error),

    #[error("Failed to open target {0}: {1}")]
    Open(Target, nusb::Error),

    #[error("Failed to detach USB kernel driver for {0}: {1}")]
    DetachKernelDriver(Target, nusb::Error),

    #[error("Failed to claim USB interface on {0}: {1}")]
    ClaimInterface(Target, nusb::Error),

    #[error("PICOBOOT interface not found on {0}")]
    InterfaceNotFound(Target),

    #[error("{0} is closed")]
    Closed(Target),

    #[error("Failed to claim USB bulk IN endpoint on {0}: {1}")]
    InEndpoint(Target, nusb::Error),

    #[error("Failed to claim USB bulk OUT endpoint on {0}: {1}")]
    OutEndpoint(Target, nusb::Error),

    #[error("Failed to clear IN endpoint halt on {0}: {1}")]
    ClearInHalt(Target, nusb::Error),

    #[error("Failed to clear OUT endpoint halt on {0}: {1}")]
    ClearOutHalt(Target, nusb::Error),

    #[error("Bulk read failed on {0}: {1}")]
    ReadBulk(Target, TransferError),

    #[error("Bulk read longer than requested on {0}: {1} < {2}")]
    ReadBulkLength(Target, usize, usize),

    #[error("Bulk write failed on {0}: {1}")]
    WriteBulk(Target, TransferError),

    #[error("Bulk write did not match expected size on {0}: {1} != {2}")]
    WriteBulkLength(Target, usize, usize),

    #[error("PICOBOOT get command status from {0} failed: {1}")]
    CommandStatus(Target, TransferError),

    #[error("PICOBOOT command status from {0} is {1} bytes")]
    CommandStatusLength(Target, usize),

    #[error("Failed to reset PICOBOOT interface on {0}: {1}")]
    Reset(Target, TransferError),
}

fn failure(kind: TransportErrorKind, e: UsbError) -> Error {
    Error::Transport(TransportError::new(kind, e))
}

/// The target for a USB VID and PID.
fn target_of(vid: u16, pid: u16) -> Target {
    match (vid, pid) {
        (PICOBOOT_VID, PICOBOOT_PID_RP2040) => Target::Rp2040,
        (PICOBOOT_VID, PICOBOOT_PID_RP2350) => Target::Rp2350,
        (vid, pid) => Target::Custom { vid, pid },
    }
}

impl UsbDevice {
    /// The device `info` describes.  The device is opened to find its
    /// PICOBOOT interface and left closed.
    pub async fn new(info: DeviceInfo) -> Result<Self> {
        let target = target_of(info.vendor_id(), info.product_id());

        let device = info.open().await.map_err(|e| {
            failure(
                TransportErrorKind::Access,
                UsbError::Open(target.clone(), e),
            )
        })?;

        // The PICOBOOT interface has the PICOBOOT class and subclass and a
        // pair of bulk endpoints.  That matches an RP2040/RP2350 in BOOTSEL
        // mode and custom targets.
        for config in device.configurations() {
            for iface in config.interfaces() {
                for alt in iface.alt_settings() {
                    let num = alt.interface_number();
                    if alt.class() != PICOBOOT_USB_CLASS || alt.subclass() != PICOBOOT_USB_SUBCLASS
                    {
                        trace!("Interface {num} on {target} isn't PICOBOOT");
                        continue;
                    }

                    let mut found_in = None;
                    let mut found_out = None;
                    for endpoint in alt.endpoints() {
                        match (endpoint.transfer_type(), endpoint.direction()) {
                            (TransferType::Bulk, NusbDirection::In) => {
                                found_in = Some((endpoint.address(), endpoint.max_packet_size()));
                            }
                            (TransferType::Bulk, NusbDirection::Out) => {
                                found_out = Some(endpoint.address());
                            }
                            _ => {}
                        }
                    }

                    if let (Some((in_ep, in_ep_max_packet_size)), Some(out_ep)) =
                        (found_in, found_out)
                    {
                        debug!("Found PICOBOOT interface {num} on {target}");
                        return Ok(Self {
                            info,
                            if_num: num,
                            in_ep,
                            out_ep,
                            in_ep_max_packet_size,
                            state: Arc::default(),
                        });
                    }
                    trace!("PICOBOOT interface {num} on {target} has no bulk IN/OUT endpoints");
                }
            }
        }

        debug!("No PICOBOOT interface with bulk endpoints found on {target}");
        Err(failure(
            TransportErrorKind::Other,
            UsbError::InterfaceNotFound(target),
        ))
    }

    /// The first device found matching any of `targets`, or the first
    /// RP2040 or RP2350 when `targets` is `None`.
    ///
    /// Fails with [`Error::PicobootNoDevicesFound`] when there isn't one.
    pub async fn first(targets: Option<&[Target]>) -> Result<Self> {
        let mut devices = Self::list(targets).await?;
        if devices.is_empty() {
            debug!("No PICOBOOT devices found");
            return Err(Error::PicobootNoDevicesFound);
        }
        Self::new(devices.remove(0)).await
    }

    /// The connected devices matching any of `targets`, or every RP2040 and
    /// RP2350 when `targets` is `None`.
    pub async fn list(targets: Option<&[Target]>) -> Result<Vec<DeviceInfo>> {
        let targets = targets.unwrap_or(&[Target::Rp2040, Target::Rp2350]);
        trace!("Searching for PICOBOOT devices with VID/PID matches: {targets:?}");

        let devices = nusb::list_devices()
            .await
            .inspect_err(|e| debug!("Failed to list USB devices: {e}"))
            .map_err(|e| failure(TransportErrorKind::Other, UsbError::Enumerate(e)))?
            .filter(|dev| {
                targets
                    .iter()
                    .any(|t| dev.vendor_id() == t.vid() && dev.product_id() == t.pid())
            })
            .collect();
        Ok(devices)
    }

    /// The device's USB descriptor information.
    pub fn device_info(&self) -> &DeviceInfo {
        &self.info
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A panic while the lock is held leaves the count as it was.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A copy of the claimed interface, or `Closed`.
    fn claimed(&self) -> Result<Claimed> {
        self.state().claimed.clone().ok_or_else(|| self.closed())
    }

    fn closed(&self) -> Error {
        let target = target_of(self.info.vendor_id(), self.info.product_id());
        failure(TransportErrorKind::Closed, UsbError::Closed(target))
    }

    /// Opens the device and claims the PICOBOOT interface.  This blocks so the
    /// count's lock covers it, which stops two first opens claiming at once.
    fn claim(&self, target: &Target) -> Result<Claimed> {
        let access = |e| failure(TransportErrorKind::Access, e);
        let mut device = self
            .info
            .open()
            .wait()
            .map_err(|e| access(UsbError::Open(target.clone(), e)))?;
        let kernel_driver_detached = detach_kernel_driver(&mut device, self.if_num)
            .map_err(|e| access(UsbError::DetachKernelDriver(target.clone(), e)))?;
        let interface = device
            .claim_interface(self.if_num)
            .wait()
            .map_err(|e| access(UsbError::ClaimInterface(target.clone(), e)))?;
        Ok(Claimed {
            device,
            interface,
            target: target.clone(),
            kernel_driver_detached,
        })
    }

    /// One transaction.  Bulk transfers block, and the count's lock is held
    /// throughout so copies' transactions don't interleave.
    fn transact(&self, transaction: &Transaction<'_>, timeouts: &Timeouts) -> Result<Vec<u8>> {
        let state = self.state();
        let Some(c) = &state.claimed else {
            return Err(self.closed());
        };

        let before = |e| failure(TransportErrorKind::Other, e);
        self.bulk_write(c, timeouts, transaction.packet)
            .map_err(before)?;

        let after = |e| failure(TransportErrorKind::AfterSend, e);
        let mut data = vec![];
        if transaction.transfer_len != 0 {
            match transaction.direction {
                Direction::In => {
                    data = self
                        .bulk_read(c, timeouts, transaction.transfer_len, true)
                        .map_err(after)?;
                }
                Direction::Out => {
                    self.bulk_write(c, timeouts, transaction.out)
                        .map_err(after)?;
                }
            }
        }

        // The acknowledgement is a transfer in the other direction.  Its
        // length isn't checked.
        match transaction.direction {
            Direction::In => self.bulk_write_any(c, timeouts, &[0u8; 1]),
            Direction::Out => self.bulk_read(c, timeouts, 1, false).map(|_| ()),
        }
        .map_err(after)?;

        Ok(data)
    }

    fn bulk_read(
        &self,
        c: &Claimed,
        timeouts: &Timeouts,
        len: usize,
        check: bool,
    ) -> std::result::Result<Vec<u8>, UsbError> {
        // nusb requires an IN transfer to be a whole number of packets.
        let mps = self.in_ep_max_packet_size;
        let transfer_size = len.div_ceil(mps) * mps;

        let mut ep: Endpoint<Bulk, In> = c
            .interface
            .endpoint(self.in_ep)
            .map_err(|e| UsbError::InEndpoint(c.target.clone(), e))?;
        let completion = ep.transfer_blocking(Buffer::new(transfer_size), timeouts.endpoint);
        let actual_len = completion.actual_len;
        let mut data = completion
            .into_result()
            .map(Buffer::into_vec)
            .map_err(|e| UsbError::ReadBulk(c.target.clone(), e))?;

        if check && len < actual_len {
            debug!("Bulk read size mismatch: expected {len}, got {actual_len}");
            return Err(UsbError::ReadBulkLength(c.target.clone(), len, actual_len));
        }
        data.truncate(len);
        Ok(data)
    }

    /// Writes all of `data`.
    fn bulk_write(
        &self,
        c: &Claimed,
        timeouts: &Timeouts,
        data: &[u8],
    ) -> std::result::Result<(), UsbError> {
        let written = self.bulk_write_n(c, timeouts, data)?;
        if written != data.len() {
            debug!(
                "Bulk write size mismatch: expected {}, wrote {written}",
                data.len()
            );
            return Err(UsbError::WriteBulkLength(
                c.target.clone(),
                data.len(),
                written,
            ));
        }
        Ok(())
    }

    /// Writes `data`, whatever length the device accepts.
    fn bulk_write_any(
        &self,
        c: &Claimed,
        timeouts: &Timeouts,
        data: &[u8],
    ) -> std::result::Result<(), UsbError> {
        self.bulk_write_n(c, timeouts, data).map(|_| ())
    }

    fn bulk_write_n(
        &self,
        c: &Claimed,
        timeouts: &Timeouts,
        data: &[u8],
    ) -> std::result::Result<usize, UsbError> {
        let mut ep: Endpoint<Bulk, Out> = c
            .interface
            .endpoint(self.out_ep)
            .map_err(|e| UsbError::OutEndpoint(c.target.clone(), e))?;
        let completion = ep.transfer_blocking(Buffer::from(data.to_vec()), timeouts.endpoint);
        let actual_len = completion.actual_len;
        completion
            .into_result()
            .map_err(|e| UsbError::WriteBulk(c.target.clone(), e))?;
        Ok(actual_len)
    }

    async fn status(&self, c: &Claimed, timeouts: &Timeouts) -> Result<PicobootStatusCmd> {
        let other = |e| failure(TransportErrorKind::Other, e);
        let control = ControlIn {
            control_type: Vendor,
            recipient: Recipient::Interface,
            request: REQUEST_GET_COMMAND_STATUS,
            value: 0,
            index: u16::from(self.if_num),
            length: COMMAND_STATUS_LEN,
        };
        trace!("Issuing GET_COMMAND_STATUS for interface {}", self.if_num);
        let buf = c
            .control_in(control, timeouts.command_status)
            .await
            .map_err(|e| other(UsbError::CommandStatus(c.target.clone(), e)))?;
        let bytes: [u8; 16] = buf
            .as_slice()
            .try_into()
            .map_err(|_| other(UsbError::CommandStatusLength(c.target.clone(), buf.len())))?;
        Ok(PicobootStatusCmd::from(bytes))
    }

    async fn reset_interface(&self, c: &Claimed, timeouts: &Timeouts) -> Result<()> {
        let other = |e| failure(TransportErrorKind::Other, e);

        match self.status(c, timeouts).await {
            Ok(status) if status.is_ok() => trace!("Device status OK {status:?}"),
            Ok(status) => debug!("Device status not OK {status:?}"),
            Err(e) => info!("Failed to get device status before reset - resetting anyway: {e}"),
        }

        // Only a halt the device reports is cleared.
        //
        // CLEAR_FEATURE(ENDPOINT_HALT) also resets that endpoint's data toggle,
        // so clearing one that was never halted puts the two ends out of step
        // and the transfer after it is discarded as a repeat.  The device
        // answers GET_STATUS on the control endpoint whatever state the bulk
        // pair is in, so there is no need to guess from which transfers failed.
        if endpoint_halted(c, timeouts, self.in_ep).await {
            info!("Unstall IN endpoint");
            let mut ep: Endpoint<Bulk, In> = c
                .interface
                .endpoint(self.in_ep)
                .map_err(|e| other(UsbError::InEndpoint(c.target.clone(), e)))?;
            ep.clear_halt()
                .await
                .map_err(|e| other(UsbError::ClearInHalt(c.target.clone(), e)))?;
        }
        if endpoint_halted(c, timeouts, self.out_ep).await {
            info!("Unstall OUT endpoint");
            let mut ep: Endpoint<Bulk, Out> = c
                .interface
                .endpoint(self.out_ep)
                .map_err(|e| other(UsbError::OutEndpoint(c.target.clone(), e)))?;
            ep.clear_halt()
                .await
                .map_err(|e| other(UsbError::ClearOutHalt(c.target.clone(), e)))?;
        }

        debug!("Sending reset control request");
        let control = ControlOut {
            control_type: Vendor,
            recipient: Recipient::Interface,
            request: REQUEST_RESET,
            value: 0,
            index: u16::from(self.if_num),
            data: &[],
        };
        c.control_out(control, timeouts.reset)
            .await
            .map_err(|e| other(UsbError::Reset(c.target.clone(), e)))
    }
}

impl Claimed {
    async fn control_in(
        &self,
        control: ControlIn,
        timeout: std::time::Duration,
    ) -> std::result::Result<Vec<u8>, TransferError> {
        #[cfg(target_os = "windows")]
        {
            self.interface.control_in(control, timeout).await
        }
        #[cfg(not(target_os = "windows"))]
        {
            self.device.control_in(control, timeout).await
        }
    }

    async fn control_out(
        &self,
        control: ControlOut<'_>,
        timeout: std::time::Duration,
    ) -> std::result::Result<(), TransferError> {
        #[cfg(target_os = "windows")]
        {
            self.interface.control_out(control, timeout).await
        }
        #[cfg(not(target_os = "windows"))]
        {
            self.device.control_out(control, timeout).await
        }
    }

    /// Reattaches the Linux kernel driver if the claim detached it.  Runs
    /// after the interface is released.
    fn release(self) {
        let Claimed {
            device,
            interface,
            target,
            kernel_driver_detached,
        } = self;
        let if_num = interface.interface_number();
        drop(interface);
        if kernel_driver_detached {
            reattach_kernel_driver(&device, &target, if_num);
        }
    }
}

/// Whether the device reports bulk endpoint `addr` halted.
///
/// A device that can't answer is treated as having nothing halted, and the
/// log records it.  Clearing a halt resets the endpoint's data toggle, so
/// clearing one on a guess costs a lost transfer.  GET_STATUS goes to the
/// control endpoint, which only stops answering when the device has gone.
async fn endpoint_halted(c: &Claimed, timeouts: &Timeouts, addr: u8) -> bool {
    let control = ControlIn {
        control_type: Standard,
        recipient: Recipient::Endpoint,
        request: REQUEST_GET_STATUS,
        value: 0,
        index: u16::from(addr),
        length: ENDPOINT_STATUS_LEN,
    };
    match c.control_in(control, timeouts.reset).await.as_deref() {
        Ok([first, ..]) => first & ENDPOINT_STATUS_HALT != 0,
        Ok(_) => {
            info!("Endpoint {addr:#04x} answered GET_STATUS with nothing - assuming not halted");
            false
        }
        Err(e) => {
            info!("Failed to get status of endpoint {addr:#04x} - assuming not halted: {e}");
            false
        }
    }
}

/// Detaches the Linux kernel driver from `if_num`.  Returns whether one was
/// attached.
#[cfg(target_os = "linux")]
fn detach_kernel_driver(device: &mut Device, if_num: u8) -> std::result::Result<bool, nusb::Error> {
    match device.detach_kernel_driver(if_num) {
        Ok(()) => {
            trace!("Detached kernel driver for interface {if_num}");
            Ok(true)
        }
        Err(e)
            if e.kind() == NusbErrorKind::Other
                && e.os_error() == Some(rustix::io::Errno::NODATA.raw_os_error() as u32) =>
        {
            trace!("Kernel driver not active for interface {if_num}");
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

#[cfg(not(target_os = "linux"))]
fn detach_kernel_driver(
    _device: &mut Device,
    _if_num: u8,
) -> std::result::Result<bool, nusb::Error> {
    Ok(false)
}

#[cfg(target_os = "linux")]
fn reattach_kernel_driver(device: &Device, target: &Target, if_num: u8) {
    match device.attach_kernel_driver(if_num) {
        Ok(()) => trace!("Reattached kernel driver for interface {if_num}"),
        Err(e) => warn!("Failed to reattach USB kernel driver for {target}: {e}"),
    }
}

#[cfg(not(target_os = "linux"))]
fn reattach_kernel_driver(_device: &Device, _target: &Target, _if_num: u8) {}

impl Transport for UsbDevice {
    fn target(&self) -> Option<Target> {
        Some(target_of(self.info.vendor_id(), self.info.product_id()))
    }

    fn open<'a>(
        &'a self,
        target: &'a Target,
        _timeouts: &'a Timeouts,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut state = self.state();
            if state.opens == 0 {
                state.claimed = Some(self.claim(target)?);
            }
            state.opens += 1;
            Ok(())
        })
    }

    fn close(&self) {
        let released = {
            let mut state = self.state();
            match state.opens {
                0 => {
                    warn!("{self} closed more times than it was opened");
                    None
                }
                1 => {
                    state.opens = 0;
                    state.claimed.take()
                }
                _ => {
                    state.opens -= 1;
                    None
                }
            }
        };
        if let Some(c) = released {
            c.release();
        }
    }

    fn command<'a>(
        &'a self,
        transaction: Transaction<'a>,
        timeouts: &'a Timeouts,
    ) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move { self.transact(&transaction, timeouts) })
    }

    fn command_status<'a>(
        &'a self,
        timeouts: &'a Timeouts,
    ) -> BoxFuture<'a, Result<PicobootStatusCmd>> {
        Box::pin(async move { self.status(&self.claimed()?, timeouts).await })
    }

    fn reset<'a>(&'a self, timeouts: &'a Timeouts) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.reset_interface(&self.claimed()?, timeouts).await })
    }
}

/// `VID:PID` in upper-case hex, for example `2E8A:000F`.
impl fmt::Display for UsbDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04X}:{:04X}",
            self.info.vendor_id(),
            self.info.product_id()
        )
    }
}

/// Two `UsbDevice`s are equal when their VID, PID, bus, address and serial
/// number match.
impl PartialEq for UsbDevice {
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (&self.info, &other.info);
        a.vendor_id() == b.vendor_id()
            && a.product_id() == b.product_id()
            && a.bus_id() == b.bus_id()
            && a.device_address() == b.device_address()
            && a.serial_number() == b.serial_number()
    }
}

impl Eq for UsbDevice {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_ids_are_known_targets() {
        assert_eq!(target_of(PICOBOOT_VID, PICOBOOT_PID_RP2040), Target::Rp2040);
        assert_eq!(target_of(PICOBOOT_VID, PICOBOOT_PID_RP2350), Target::Rp2350);
        assert_eq!(
            target_of(0x1234, 0x5678),
            Target::Custom {
                vid: 0x1234,
                pid: 0x5678
            }
        );
    }

    /// A transport's failure displays as the USB error, which contains the
    /// target.
    #[test]
    fn a_failure_displays_the_usb_error() {
        let e = failure(TransportErrorKind::Closed, UsbError::Closed(Target::Rp2350));
        assert_eq!(e.to_string(), "RP2350 is closed");
        let Error::Transport(t) = e else {
            panic!("not a transport error")
        };
        assert_eq!(t.kind(), TransportErrorKind::Closed);
        let source = std::error::Error::source(&t).expect("a source");
        assert!(source.downcast_ref::<UsbError>().is_some());
    }
}
