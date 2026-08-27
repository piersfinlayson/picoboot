// Copyright (C) 2025 Piers Finlayson <piers@piers.rocks>
//
// MIT License

use deku::{DekuContainerRead, DekuContainerWrite};
#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
#[cfg(target_os = "linux")]
use nusb::ErrorKind as NusbErrorKind;
use nusb::descriptors::TransferType;
use nusb::transfer::ControlType::{Standard, Vendor};
use nusb::transfer::{
    Buffer, Bulk, ControlIn, ControlOut, Direction as NusbDirection, In, Out, Recipient,
};
use nusb::{Device, DeviceInfo, Endpoint, Interface};
use std::time::Duration;

use crate::cmd::{PicobootCmd, PicobootCmdId, PicobootStatusCmd, PicobootXCmd};
use crate::cmd::{REQUEST_GET_COMMAND_STATUS, REQUEST_RESET, RESPONSE_GET_COMMAND_STATUS_SIZE};
use crate::{Access, Direction, Error as PicobootError, RebootType, Speed, Target};

// see https://github.com/raspberrypi/picotool/blob/master/main.cpp#L4173
// for loading firmware over a connection

type Error = PicobootError;
type Result<T> = ::std::result::Result<T, Error>;

// Standard USB GET_STATUS, and the bit an endpoint's answer sets when it is
// halted.  See USB 2.0 sections 9.4.5 and 9.4.6.
const REQUEST_GET_STATUS: u8 = 0x00;
const ENDPOINT_STATUS_HALT: u8 = 0x01;
const ENDPOINT_STATUS_LEN: u16 = 2;

// USB class/subclass for PICOBOOT
const PICOBOOT_USB_CLASS: u8 = 0xFF;
const PICOBOOT_USB_SUBCLASS: u8 = 0x00;

// Timeout defaults
const DEFAULT_ENDPOINT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_COMMAND_STATUS_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_RESET_TIMEOUT: Duration = Duration::from_secs(1);

const REBOOT_TYPE_NORMAL: u32 = 0x0000;
const REBOOT_TYPE_BOOTSEL: u32 = 0x0002;
const DISABLE_MSD_INTERFACE: u32 = 0x01;
const DISABLE_PICOBOOT_INTERFACE: u32 = 0x02;

/// USB timeouts for PICOBOOT operations
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeouts {
    /// Endpoint timeout duration
    pub endpoint: Duration,

    /// Command status timeout duration
    pub command_status: Duration,

    /// Reset timeout duration
    pub reset: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT_TIMEOUT,
            command_status: DEFAULT_COMMAND_STATUS_TIMEOUT,
            reset: DEFAULT_RESET_TIMEOUT,
        }
    }
}

/// Active connection to a PICOBOOT device
///
/// This object is used to send commands to a connected PICOBOOT capable
/// device.
///
/// ## Example
///
/// ```rust,no_run
/// # use picoboot::{Picoboot, Error};
/// # #[tokio::main]
/// # async fn main() -> Result<(), Error> {
///     let mut picoboot = Picoboot::from_first(None).await?;
///     let conn = picoboot.connect().await?;
///     conn.flash_erase_start(4096).await?;
///     conn.flash_write_start(&[0u8; 4096]).await?;
///     let data = conn.flash_read_start(4096).await?;
/// #    Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Connection {
    // Device is only used on linux
    #[allow(dead_code)]
    device: Device,
    target: Target,
    interface: Interface,
    out_ep: u8,
    in_ep: u8,
    in_ep_max_packet_size: usize,
    kernel_driver_detached: bool,
    timeouts: Timeouts,
    cmd_token: u32,
}

impl Drop for Connection {
    fn drop(&mut self) {
        if self.kernel_driver_detached {
            // Consume error
            let _ = self.reattach_kernel_driver();
        }
    }
}

impl Connection {
    /// Resets PICOBOOT USB interface.
    ///
    /// This may be called after opening a brand new connection to ensure a
    /// consistent starting state, or to reset a stalled connection.
    ///
    /// Note that this resets any exclusive access, re-enabling the USB mass
    /// storage device if it was ejects due to exclusivity.
    ///
    /// Returns:
    /// - `Ok(())` - If interface is successfully reset.
    pub async fn reset_interface(&mut self) -> Result<()> {
        // Get command status
        trace!("Getting command status before reset");
        match self.get_command_status().await {
            Ok(status) => {
                if status.is_ok() {
                    trace!("Device status OK {status:?}");
                } else {
                    debug!("Device status not OK {status:?}");
                }
            }
            Err(e) => {
                info!(
                    "Failed to get device status before reset - sending reset control request: {e}"
                );
            }
        }

        // Only a halt the device reports is cleared.
        //
        // CLEAR_FEATURE(ENDPOINT_HALT) also resets that endpoint's data toggle,
        // so clearing one that was never halted puts the two ends out of step
        // and the transfer after it is discarded as a repeat.  The device
        // answers GET_STATUS on the control endpoint whatever state the bulk
        // pair is in, so there is no need to guess from which transfers failed.
        if self.endpoint_halted(self.in_ep).await {
            info!("Unstall IN endpoint");
            let mut in_ep: Endpoint<Bulk, In> = self
                .interface
                .endpoint(self.in_ep)
                .inspect_err(|e| debug!("Failed to get bulk IN endpoint: {e}"))
                .map_err(|e| PicobootError::UsbInEndpointClaimFailure(self.target.clone(), e))?;
            in_ep
                .clear_halt()
                .await
                .inspect_err(|e| debug!("Failed to clear halt on bulk IN endpoint: {e}"))
                .map_err(|e| {
                    PicobootError::UsbClearInEndpointHaltFailure(self.target.clone(), e)
                })?;
        }

        if self.endpoint_halted(self.out_ep).await {
            info!("Unstall OUT endpoint");
            let mut out_ep: Endpoint<Bulk, Out> = self
                .interface
                .endpoint(self.out_ep)
                .inspect_err(|e| debug!("Failed to get bulk OUT endpoint: {e}"))
                .map_err(|e| PicobootError::UsbOutEndpointClaimFailure(self.target.clone(), e))?;
            out_ep
                .clear_halt()
                .await
                .inspect_err(|e| debug!("Failed to clear halt on bulk OUT endpoint: {e}"))
                .map_err(|e| {
                    PicobootError::UsbClearOutEndpointHaltFailure(self.target.clone(), e)
                })?;
        }

        // Send reset control request
        debug!("Sending reset control request");
        let timeout = self.timeouts.reset;
        let control_out = ControlOut {
            control_type: Vendor,
            recipient: Recipient::Interface,
            request: REQUEST_RESET,
            value: 0,
            index: self.interface.interface_number() as u16,
            data: &[0u8; 0],
        };
        {
            #[cfg(target_os = "windows")]
            {
                self.interface.control_out(control_out, timeout).await
            }
            #[cfg(not(target_os = "windows"))]
            {
                self.device.control_out(control_out, timeout).await
            }
        }
        .inspect_err(|e| debug!("Failed to reset PICOBOOT interface: {e}"))
        .map_err(|e| Error::PicobootResetInterfaceFailure(self.target.clone(), e))
    }

    /// Sends a low-level command to the PICOBOOT device.
    ///
    /// Depending on the command, the buffer argument may be used to send data
    /// to the device.
    ///
    /// Depending on the command, the returned Vec may contain data from the
    /// device.
    ///
    /// Returns:
    /// - `Ok(Vec<u8>)` - Data returned from the device, if any.
    pub async fn send_cmd(&mut self, cmd: PicobootCmd, buf: Option<&[u8]>) -> Result<Vec<u8>> {
        // Set token
        let cmd = cmd.set_token(self.cmd_token());

        // Construct the write command
        let cmd_bytes = cmd
            .to_bytes()
            .inspect_err(|e| debug!("Failed to serialize command: {e}"))
            .map_err(|e| Error::PicobootCmdSerializeFailure(self.target.clone(), e))?;

        // Write the command
        trace!("Sending command {cmd:?}");
        self.bulk_write(cmd_bytes.as_slice(), true)?;

        // Do the appropriate read/write if this is a data transfer command
        let mut res = vec![];
        if cmd.is_data_transfer() {
            match cmd.direction() {
                Direction::In => {
                    res = self.bulk_read(cmd.get_transfer_len(), true)?;
                }
                Direction::Out => {
                    if buf.is_none() {
                        debug!("No buffer provided for OUT data transfer command");
                        return Err(Error::PicobootCmdDataMissing(self.target.clone(), cmd.id()));
                    }
                    let buf = buf.unwrap();
                    let _written = self.bulk_write(buf, true)?;
                }
            }
        }

        // Handle acknowledgement
        match cmd.direction() {
            Direction::In => {
                self.bulk_write(&[0u8; 1], false)?;
            }
            Direction::Out => {
                self.bulk_read(1, false)?;
            }
        }

        Ok(res)
    }

    /// Requests an exclusive access mode with the device.
    ///
    /// Returns:
    /// - `Ok(())` - If the exclusive access mode is successfully set.
    pub async fn set_exclusive_access(&mut self, access: Access) -> Result<()> {
        let cmd = PicobootCmd::exclusive_access(access.into());

        let _ = self.send_cmd(cmd, None).await?;

        Ok(())
    }

    /// Performs a simple reboot of the device.
    ///
    /// Args:
    /// - `delay` - Time to start the device after.  Must fit into a u32
    ///   milliseconds.
    ///
    /// For a target [`Target::Custom`] this command is not supported - use
    /// [`Self::reboot_rp2040`] or [`Self::reboot_rp2350`] instead.
    ///
    /// Returns:
    /// - `Ok(())` - If reboot command is successfully sent.
    pub async fn reboot(&mut self, delay: Duration) -> Result<()> {
        match self.target {
            Target::Rp2040 => {
                self.reboot_rp2040(0, self.target.default_stack_pointer().unwrap(), delay)
                    .await
            }
            Target::Rp2350 => self.reboot_rp2350(REBOOT_TYPE_NORMAL, 0, 0, delay).await,
            Target::Custom { .. } => Err(Error::PicobootCmdNotAllowedForTarget(
                self.target.clone(),
                PicobootCmdId::Reboot,
            )),
        }
    }

    /// Reboots the device with a specified program counter, stack pointer, and
    /// delay.
    ///
    /// Note, reboot is not supported on the RP2350.
    ///
    /// - `program_counter` - Program counter to start the device with. Use `0` for a standard flash boot, or a RAM address to start executing at.
    /// - `stack_pointer` - Stack pointer to start the device with. Unused if `program_counter` is `0`.
    /// - `delay` - Time to start the device after.  Must fit into a u32
    ///   milliseconds.
    ///
    /// Returns:
    /// - `Ok(())` - If reboot command is successfully sent.
    pub async fn reboot_rp2040(
        &mut self,
        program_counter: u32,
        stack_pointer: u32,
        delay: Duration,
    ) -> Result<()> {
        let delay_ms: u32 = delay
            .as_millis()
            .try_into()
            .map_err(|_| Error::PicobootInvalidDuration(self.target.clone(), delay))?;

        if self.target == Target::Rp2350 {
            return Err(Error::PicobootCmdNotAllowedForTarget(
                self.target.clone(),
                PicobootCmdId::Reboot,
            ));
        }

        let _ = self
            .send_cmd(
                PicobootCmd::reboot(program_counter, stack_pointer, delay_ms),
                None,
            )
            .await?;

        Ok(())
    }

    /// Reboots the device out of BOOTSEL mode into normal operation
    ///
    /// Note, not supported on the RP2040.
    ///
    /// Args:
    /// - `flags` - Reboot flags to use.
    /// - `p0` - Reboot parameter 0.
    /// - `p1` - Reboot parameter 1.
    /// - `delay` - Time to start the device after.  Must fit into a u32
    ///   milliseconds.
    ///
    /// See the RP2350 datasheet, section 5.4.8.24. reboot for details on the
    /// flags and parameters.  For a standard boot, set them all to 0.
    ///
    /// Returns:
    /// - `Ok(())` - If reboot command is successfully sent.
    pub async fn reboot_rp2350(
        &mut self,
        flags: u32,
        p0: u32,
        p1: u32,
        delay: Duration,
    ) -> Result<()> {
        let delay_ms: u32 = delay
            .as_millis()
            .try_into()
            .map_err(|_| Error::PicobootInvalidDuration(self.target.clone(), delay))?;

        if self.target == Target::Rp2040 {
            return Err(Error::PicobootCmdNotAllowedForTarget(
                self.target.clone(),
                PicobootCmdId::Reboot2,
            ));
        }

        trace!(
            "Rebooting RP2350 with flags=0x{flags:08X}, p0=0x{p0:08X}, p1=0x{p1:08X}, delay={delay:?}"
        );
        let _ = self
            .send_cmd(PicobootCmd::reboot2(flags, p0, p1, delay_ms), None)
            .await?;

        Ok(())
    }

    /// Erases the flash memory of the device from the start
    ///
    /// Args:
    /// - `size` - Number of bytes to erase. Will be rounded up to the nearest
    ///   multiple of [`Target::flash_sector_size`].
    ///
    /// Returns:
    /// - `Ok(())` - If erase command is successfully sent.
    pub async fn flash_erase_start(&mut self, size: usize) -> Result<()> {
        let size = u32::try_from(size)
            .map_err(|_| Error::PicobootEraseInvalidSize(self.target.clone(), size as u32))?;

        let sector_size = self.target.flash_sector_size();
        let size = size.div_ceil(sector_size) * sector_size;

        trace!("Erasing flash size rounded to sector size: {size:#X} bytes");
        self.flash_erase(self.target.flash_start(), size).await
    }

    /// Erases the flash memory of the device.
    ///
    /// - `addr` - Address to start the erase. Must be on a multiple of [`Target::flash_sector_size`].
    /// - `size` - Number of bytes to erase. Must be a multiple of [`Target::flash_sector_size`].
    ///
    /// Returns:
    /// - `Ok(())` - If erase command is successfully sent.
    pub async fn flash_erase(&mut self, addr: u32, size: u32) -> Result<()> {
        if !addr.is_multiple_of(self.target.flash_sector_size()) {
            return Err(Error::PicobootEraseInvalidAddr(self.target.clone(), addr));
        }
        if !size.is_multiple_of(self.target.flash_sector_size()) {
            return Err(Error::PicobootEraseInvalidSize(self.target.clone(), size));
        }

        let _ = self
            .send_cmd(PicobootCmd::flash_erase(addr, size), None)
            .await?;

        Ok(())
    }

    /// Writes a buffer to the start of the flash memory of the device.
    ///
    /// Args:
    /// - `buf` - Buffer of data to write to flash. Should be a multiple of
    ///   [`Target::flash_page_size`]. If not, the remainder of the final page is zero-filled.
    ///
    /// Returns:
    /// - `Ok(())` - If write command is successfully sent.
    pub async fn flash_write_start(&mut self, buf: &[u8]) -> Result<()> {
        self.flash_write(self.target.flash_start(), buf).await
    }

    /// Writes a buffer to the flash memory of the device.
    ///
    /// - `addr` - Address to start the write. Must be on a multiple of
    ///   [`Target::flash_page_size`].
    /// - `buf` - Buffer of data to write to memory. Should be a multiple of
    ///   [`Target::flash_page_size`]. If not, the remainder of the final page
    ///   is zero-filled.
    ///
    /// Returns:
    /// - `Ok(())` - If write command is successfully sent.
    pub async fn flash_write(&mut self, addr: u32, buf: &[u8]) -> Result<()> {
        let page_size = self.target.flash_page_size();

        if !addr.is_multiple_of(page_size) {
            return Err(Error::PicobootWriteInvalidAddr(self.target.clone(), addr));
        }

        self.write(addr, buf).await
    }

    /// Writes a buffer to the (RAM) memory of the device.
    ///
    /// For flash, use flash_write() as that polices valid flash write
    /// addresses and sizes.
    ///
    /// - `addr` - Address to start the write.
    /// - `buf` - Buffer of data to write to memory.
    ///
    ///
    pub async fn write(&mut self, addr: u32, buf: &[u8]) -> Result<()> {
        debug!(
            "Picoboot: Writing {} bytes to address 0x{addr:08X}",
            buf.len()
        );
        // We send in 256 byte chunks, as it's safest, even for RAM writes.
        let page_size = self.target.flash_page_size();

        let page_size = page_size as usize;
        let mut offset = 0;

        while offset < buf.len() {
            let chunk = &buf[offset..std::cmp::min(offset + page_size, buf.len())];
            let chunk_addr = addr + offset as u32;
            let _ = self
                .send_cmd(
                    PicobootCmd::flash_write(chunk_addr, chunk.len() as u32),
                    Some(chunk),
                )
                .await?;
            offset += page_size;
        }

        Ok(())
    }

    /// Reads from the start of flash memory of the device.
    ///
    /// - `addr` - Address to start the read.
    /// - `size` - Number of bytes to read.
    ///
    /// Returns:
    /// - `Ok(Vec<u8>)` - Buffer of data read from flash.
    pub async fn flash_read_start(&mut self, size: u32) -> Result<Vec<u8>> {
        self.read(self.target.flash_start(), size).await
    }

    /// Reads from the flash memory of the device.
    ///
    /// - `addr` - Address to start the write. Must be on a multiple of
    ///   [`Target::flash_page_size`].
    /// - `size` - Amount of read.  Should be a multiple of
    ///   [`Target::flash_page_size`]. If not, it will be rejected.
    pub async fn flash_read(&mut self, addr: u32, size: u32) -> Result<Vec<u8>> {
        if !addr.is_multiple_of(self.target.flash_page_size()) {
            return Err(Error::PicobootReadInvalidAddr(self.target.clone(), addr));
        }
        if !size.is_multiple_of(self.target.flash_page_size()) {
            return Err(Error::PicobootReadInvalidSize(self.target.clone(), size));
        }

        self.read(addr, size).await
    }

    /// Reads from memory of the device.
    ///
    /// For flash reads, use flash_read() or flash_read_start() as that
    /// polices valid flash read addresses and sizes.
    pub async fn read(&mut self, addr: u32, size: u32) -> Result<Vec<u8>> {
        debug!("Picoboot: Reading {size} bytes from address 0x{addr:08X}");
        self.send_cmd(PicobootCmd::flash_read(addr, size), None)
            .await
    }

    /// Enter Flash XIP (execute-in-place) mode.
    ///
    /// Only functional on RP2040 devices - is a no-op on RP2350.
    ///
    /// Returns:
    /// - `Ok(())` - If enter XIP command is successfully sent.
    pub async fn enter_xip(&mut self) -> Result<()> {
        let _ = self.send_cmd(PicobootCmd::enter_xip(), None).await?;

        Ok(())
    }

    /// Exits Flash XIP (execute-in-place) mode.
    ///
    /// Only functional on RP2040 devices - is a no-op on RP2350.
    ///
    /// Returns:
    /// - `Ok(())` - If exit XIP command is successfully sent.
    pub async fn exit_xip(&mut self) -> Result<()> {
        let _ = self.send_cmd(PicobootCmd::exit_xip(), None).await?;

        Ok(())
    }

    /// Returns the target type of the connected PICOBOOT device
    pub fn target(&mut self) -> &Target {
        &self.target
    }

    /// Issues a GET_COMMAND_STATUS control request to the device and returns
    /// the result.
    pub async fn get_command_status(&mut self) -> Result<PicobootStatusCmd> {
        let timeout = self.timeouts.command_status;

        // Build control request
        let if_num = self.interface.interface_number() as u16;
        let control = ControlIn {
            control_type: Vendor,
            recipient: Recipient::Interface,
            request: REQUEST_GET_COMMAND_STATUS,
            value: 0,
            index: if_num,
            length: RESPONSE_GET_COMMAND_STATUS_SIZE as u16,
        };

        trace!("Issuing GET_COMMAND_STATUS control for interface {if_num}");

        // Send control request
        let buf = {
            #[cfg(target_os = "windows")]
            {
                self.interface.control_in(control, timeout).await
            }
            #[cfg(not(target_os = "windows"))]
            {
                self.device.control_in(control, timeout).await
            }
        }
        .inspect_err(|e| debug!("Failed to get command status: {e}"))
        .map_err(|e| Error::PicobootGetCommandStatusFailure(self.target.clone(), e))?;

        // Deserialize response
        let (_, cmd) = PicobootStatusCmd::from_bytes((&buf, 0))
            .inspect_err(|e| debug!("Failed to deserialize command status: {e}"))
            .map_err(|e| Error::PicobootCmdDeserializeFailure(self.target.clone(), e))?;

        Ok(cmd)
    }

    /// Sends a custom picobootx protocol extension command to the device.
    ///
    /// Follows identical protocol mechanics to send_cmd but accepts an arbitrary
    /// magic value. The caller is responsible for constructing valid args for
    /// their extension protocol using the [`PicobootXCmd`] struct.
    pub async fn send_picobootx_cmd(
        &mut self,
        cmd: PicobootXCmd,
        buf: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        let cmd = cmd.set_token(self.cmd_token());

        let cmd_bytes = cmd
            .to_bytes()
            .inspect_err(|e| debug!("Failed to serialize picobootx command: {e}"))
            .map_err(|e| Error::PicobootCmdSerializeFailure(self.target.clone(), e))?;

        trace!("Sending picobootx command {cmd:?}");
        self.bulk_write(cmd_bytes.as_slice(), true)?;

        let mut res = vec![];
        if cmd.is_data_transfer() {
            match cmd.direction() {
                Direction::In => {
                    res = self.bulk_read(cmd.get_transfer_len(), true)?;
                }
                Direction::Out => {
                    if buf.is_none() {
                        debug!("No buffer provided for OUT data transfer picobootx command");
                        return Err(Error::PicobootXCmdDataMissing(
                            self.target.clone(),
                            cmd.get_cmd_id(),
                        ));
                    }
                    self.bulk_write(buf.unwrap(), true)?;
                }
            }
        }

        match cmd.direction() {
            Direction::In => {
                self.bulk_write(&[0u8; 1], false)?;
            }
            Direction::Out => {
                self.bulk_read(1, false)?;
            }
        }

        Ok(res)
    }
}

// Internal methods
impl Connection {
    /// Asks the device whether one of its bulk endpoints is halted.
    ///
    /// A device that cannot answer is treated as having nothing halted, and
    /// says so in the log.  Clearing a halt resets the endpoint's data toggle,
    /// so clearing one on a guess costs a lost transfer, and GET_STATUS goes to
    /// the control endpoint, which only stops answering when the device has
    /// gone.
    ///
    /// Args:
    /// - `addr` - Endpoint address, direction bit included.
    ///
    /// Returns:
    /// - `true` - The device reports this endpoint halted.
    async fn endpoint_halted(&self, addr: u8) -> bool {
        let control_in = ControlIn {
            control_type: Standard,
            recipient: Recipient::Endpoint,
            request: REQUEST_GET_STATUS,
            value: 0,
            index: u16::from(addr),
            length: ENDPOINT_STATUS_LEN,
        };
        let timeout = self.timeouts.reset;
        let status = {
            #[cfg(target_os = "windows")]
            {
                self.interface.control_in(control_in, timeout).await
            }
            #[cfg(not(target_os = "windows"))]
            {
                self.device.control_in(control_in, timeout).await
            }
        };

        match status.as_deref() {
            Ok([first, ..]) => first & ENDPOINT_STATUS_HALT != 0,
            Ok(_) => {
                info!(
                    "Endpoint {addr:#04x} answered GET_STATUS with nothing - assuming not halted"
                );
                false
            }
            Err(e) => {
                info!("Failed to get status of endpoint {addr:#04x} - assuming not halted: {e}");
                false
            }
        }
    }

    async fn new(
        device_info: &DeviceInfo,
        target: Target,
        if_num: u8,
        out_ep: u8,
        in_ep: u8,
        in_ep_max_packet_size: usize,
        timeouts: Timeouts,
    ) -> Result<Self> {
        let mut device = device_info
            .open()
            .await
            .map_err(|e| PicobootError::UsbOpenError(target.clone(), e))?;

        // Detach kernel driver BEFORE claiming interface
        let kernel_driver_detached = Self::detach_kernel_driver(&mut device, &target, if_num)?;

        // Claim the interface
        let interface = device
            .claim_interface(if_num)
            .await
            .map_err(|e| PicobootError::UsbClaimInterfaceFailure(target.clone(), e))?;

        Ok(Self {
            device,
            target,
            interface,
            out_ep,
            in_ep,
            in_ep_max_packet_size,
            kernel_driver_detached,
            timeouts,
            cmd_token: 1,
        })
    }

    fn detach_kernel_driver(device: &mut Device, target: &Target, if_num: u8) -> Result<bool> {
        #[cfg(target_os = "linux")]
        {
            let kernel_driver_detached = match device.detach_kernel_driver(if_num) {
                Ok(()) => {
                    trace!("Detached kernel driver for interface {if_num}");
                    true
                }
                Err(e) => match &e.kind() {
                    NusbErrorKind::Other
                        if e.os_error()
                            == Some(rustix::io::Errno::NODATA.raw_os_error() as u32) =>
                    {
                        trace!("Kernel driver not active for interface {if_num}, not detaching");
                        false
                    }
                    _ => {
                        return Err(PicobootError::UsbDetachKernelDriverFailure(
                            target.clone(),
                            e,
                        ));
                    }
                },
            };
            Ok(kernel_driver_detached)
        }
        #[cfg(not(target_os = "linux"))]
        {
            // On non-Linux platforms, there is no kernel driver to detach
            let _ = device;
            let _ = target;
            let _ = if_num;
            Ok(false)
        }
    }

    fn reattach_kernel_driver(&mut self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            let if_num = self.interface.interface_number();
            match self.device.attach_kernel_driver(if_num) {
                Ok(()) => {
                    trace!("Reattached kernel driver for interface {if_num}");
                    Ok(())
                }
                Err(e) => Err(PicobootError::UsbReattachKernelDriverFailure(
                    self.target.clone(),
                    e,
                )),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            // On non-Linux platforms, assume no kernel driver to re-attach
            unreachable!(
                "Internal error - kernel driver was not detached in the first place - please report this as a bug"
            )
        }
    }

    fn bulk_read(&mut self, buf_size: usize, check: bool) -> Result<Vec<u8>> {
        // nusb requires IN transfer sizes to be multiples of max packet size
        // Round up to the nearest multiple
        let max_packet_size = self.in_ep_max_packet_size;
        let transfer_size = buf_size.div_ceil(max_packet_size) * max_packet_size;

        let buf = Buffer::new(transfer_size);
        let timeout = self.timeouts.endpoint;

        // Get the endpoint
        let mut ep: Endpoint<Bulk, In> = self
            .interface
            .endpoint(self.in_ep)
            .inspect_err(|e| debug!("Failed to get bulk IN endpoint: {e}"))
            .map_err(|e| PicobootError::UsbInEndpointClaimFailure(self.target.clone(), e))?;

        // Perform the transfer
        let completion = ep.transfer_blocking(buf, timeout);
        let actual_len = completion.actual_len;
        let mut data = completion
            .into_result()
            .map(|buffer| buffer.into_vec())
            .inspect_err(|e| debug!("Failed to read bulk data: {e}"))
            .map_err(|e| PicobootError::UsbReadBulkFailure(self.target.clone(), e))?;

        if check && buf_size < actual_len {
            debug!("Bulk read size mismatch: expected {buf_size}, got {actual_len}");
            return Err(PicobootError::UsbReadBulkMismatch(
                self.target.clone(),
                buf_size,
                actual_len,
            ));
        }

        data.truncate(buf_size);

        Ok(data)
    }

    fn bulk_write(&mut self, data: &[u8], check: bool) -> Result<usize> {
        let buf = Buffer::from(data.to_vec());
        let timeout = self.timeouts.endpoint;

        // Get the endpoint
        let mut ep: Endpoint<Bulk, Out> = self
            .interface
            .endpoint(self.out_ep)
            .inspect_err(|e| debug!("Failed to get bulk OUT endpoint: {e}"))
            .map_err(|e| PicobootError::UsbOutEndpointClaimFailure(self.target.clone(), e))?;

        // Perform the transfer
        let completion = ep.transfer_blocking(buf, timeout);
        let actual_len = completion.actual_len;
        completion
            .into_result()
            .inspect_err(|e| debug!("Failed to write bulk data: {e}"))
            .map_err(|e| PicobootError::UsbWriteBulkFailure(self.target.clone(), e))?;

        if check && actual_len != data.len() {
            debug!(
                "Bulk write size mismatch: expected {}, wrote {actual_len}",
                data.len()
            );
            return Err(PicobootError::UsbWriteBulkMismatch(
                self.target.clone(),
                data.len(),
                actual_len,
            ));
        }

        Ok(actual_len)
    }

    fn cmd_token(&mut self) -> u32 {
        let token = self.cmd_token;
        self.cmd_token += 1;
        token
    }
}

/// Represents an attached PICOBOOT capable device
///
/// This object is used to discover and manage connections to PICOBOOT
/// devices over USB.
///
/// ## Example
///
/// Create a new Picoboot instance and connect to the first discovered device:
///
/// ```rust,no_run
/// use picoboot::{Picoboot, Error};
///
/// #[tokio::main]
/// async fn main() -> Result<(), Error> {
///     let mut picoboot = Picoboot::from_first(None).await?;
///     let conn = picoboot.connect().await?;
///     Ok(())
/// }
/// ```
#[derive(Debug, Clone)]
pub struct Picoboot {
    // Information about the USB device
    device_info: DeviceInfo,

    // Active connection to device
    connection: Option<Connection>,

    // Interface number of PICOBOOT interface
    if_num: u8,

    // Bulk IN endpoint address.  Dynamically determined in new().
    in_ep: u8,

    // Bulk OUT endpoint address..  Dynamically determined in new().
    out_ep: u8,

    // IN endpoint max packet size
    in_ep_max_packet_size: usize,

    // Type of target device
    target: Target,

    // Endpoint timeout duration
    timeouts: Timeouts,
}

/// Two `Picoboot` instances are equal if they refer to the same USB device
/// connection with the same VID/PID, same serial number and same timeout
/// configuration.
///
/// This ignores:
/// - Connection state (open/closed)
/// - Derived configuration (endpoints, packet sizes)
/// - Timeout settings
impl PartialEq for Picoboot {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target
            && self.device_info.vendor_id() == other.device_info.vendor_id()
            && self.device_info.product_id() == other.device_info.product_id()
            && self.device_info.bus_id() == other.device_info.bus_id()
            && self.device_info.device_address() == other.device_info.device_address()
            && self.device_info.serial_number() == other.device_info.serial_number()
            && self.timeouts == other.timeouts
    }
}

impl Eq for Picoboot {}

impl Picoboot {
    /// Creates a new PICOBOOT object from the specified `nusb` device info.
    ///
    /// Use this function if you have already discovered a suitable device
    /// using `nusb` directly.
    ///
    /// Otherwise, see:
    /// - [`Self::from_first`] for automatic device discovery
    /// - [`Self::list_devices`] to list available devices
    ///
    /// Args:
    /// - `device_info` - An `nusb::DeviceInfo` object representing the
    ///   PICOBOOT capable device.
    ///
    /// Returns:
    /// - `Ok(Picoboot)` - A new Picoboot instance if the device is compatible
    ///   and successfully queried.
    pub async fn new(device_info: DeviceInfo) -> Result<Self> {
        let target = Target::from(&device_info);

        // First, open the device
        let device = device_info
            .open()
            .await
            .map_err(|e| PicobootError::UsbOpenError(target.clone(), e))?;

        // Find the PICOBOOT interface by class/subclass and verify it has bulk endpoints
        let mut if_num = None;
        let mut in_ep = None;
        let mut in_ep_max_packet_size = 0;
        let mut out_ep = None;

        // Check the device for an interface that matches the PICOBOOT
        // class/subclass _and_ has bulk IN and OUT endpoints.  That matches
        // an RP2040/RP2350 in BOOTSEL mode, but also custom targets like One
        // ROM.
        'outer: for config in device.configurations() {
            for iface in config.interfaces() {
                for interface_info in iface.alt_settings() {
                    trace!(
                        "Checking {target} interface {} for PICOBOOT class/subclass",
                        interface_info.interface_number()
                    );
                    if interface_info.class() != PICOBOOT_USB_CLASS
                        || interface_info.subclass() != PICOBOOT_USB_SUBCLASS
                    {
                        trace!(
                            "Interface {} class/subclass mismatch, skipping",
                            interface_info.interface_number()
                        );
                        continue;
                    }
                    let num = interface_info.interface_number();
                    trace!("Found PICOBOOT interface {num} on {target}, checking endpoints");

                    let mut found_in = None;
                    let mut found_in_mps = 0;
                    let mut found_out = None;
                    for endpoint in interface_info.endpoints() {
                        match (endpoint.transfer_type(), endpoint.direction()) {
                            (TransferType::Bulk, NusbDirection::In) => {
                                found_in = Some(endpoint.address());
                                found_in_mps = endpoint.max_packet_size();
                            }
                            (TransferType::Bulk, NusbDirection::Out) => {
                                found_out = Some(endpoint.address());
                            }
                            _ => {}
                        }
                    }

                    if found_in.is_some() && found_out.is_some() {
                        if_num = Some(num);
                        in_ep = found_in;
                        in_ep_max_packet_size = found_in_mps;
                        out_ep = found_out;
                        debug!(
                            "Found PICOBOOT interface {num} on {target} with bulk IN/OUT endpoints"
                        );
                        break 'outer;
                    }
                    trace!(
                        "PICOBOOT interface {num} on {target} has no bulk IN/OUT endpoints, continuing"
                    );
                }
            }
        }

        if if_num.is_none() {
            debug!("No PICOBOOT interface with bulk endpoints found on {target}");
            return Err(PicobootError::PicobootInterfaceNotFound(target.clone()));
        }

        Ok(Self {
            device_info,
            connection: None,
            if_num: if_num.unwrap(),
            in_ep: in_ep.unwrap(),
            in_ep_max_packet_size,
            out_ep: out_ep.unwrap(),
            target,
            timeouts: Timeouts::default(),
        })
    }

    /// Creates a new PICOBOOT object, ready for use, but not yet connected.
    ///
    /// Function returns the first device found matching any of the provided
    /// types, or the first discovered RP2040/RP2350, if no targets are listed.
    ///
    /// Args:
    /// - `targets` - An optional slice of [`Target`] values to specify which
    ///   device types to search for. If `None`, standard RP2040 and RP2350
    ///   types will be searched.
    ///
    /// Returns:
    /// - `Ok(Picoboot)` - A new Picoboot instance if a compatible device is
    ///   found and successfully queried.
    pub async fn from_first(targets: Option<&[Target]>) -> Result<Self> {
        let mut devices = Self::list_devices(targets).await?;

        if !devices.is_empty() {
            trace!("Found {} PICOBOOT devices", devices.len());
            Self::new(devices.remove(0)).await
        } else {
            debug!("No PICOBOOT devices found");
            Err(PicobootError::PicobootNoDevicesFound)
        }
    }

    /// Returns a list of available PICOBOOT devices detected by `nusb`.
    ///
    /// Either searches for all connected RP2040/RP2350 devices with stock
    /// VID/PID, or, if `targets` is provided, only those types.
    ///
    /// Args:
    /// - `targets` - An optional slice of [`Target`] values to specify which
    ///
    /// Returns:
    /// - `Ok(Vec<nusb::DeviceInfo>)` - A vector of detected nusb devices.
    pub async fn list_devices(targets: Option<&[Target]>) -> Result<Vec<DeviceInfo>> {
        // Determine which targets to search for
        let targets = match targets {
            Some(t) => t,
            None => &[Target::Rp2040, Target::Rp2350],
        };
        trace!(
            "Searching for PICOBOOT devices with VID/PID matches: {:?}",
            targets
        );

        // Get attached USB devices and filter to PICOBOOT devices
        let devices = nusb::list_devices()
            .await
            .inspect_err(|e| debug!("Failed to list USB devices: {e}"))
            .map_err(PicobootError::UsbEnumerationError)?
            .filter(|dev| {
                targets
                    .iter()
                    .any(|t| dev.vendor_id() == t.vid() && dev.product_id() == t.pid())
            })
            .collect();

        Ok(devices)
    }

    /// Establish PICOBOOT connection
    ///
    /// Called to open a connection to the PICOBOOT device, enabling PICOBOOT
    /// commands to be sent.
    ///
    /// Returns:
    /// - `Ok(())` - If connection is successfully established.
    pub async fn connect(&mut self) -> Result<&mut Connection> {
        if self.connection.is_none() {
            self.connection = Some(
                Connection::new(
                    &self.device_info,
                    self.target.clone(),
                    self.if_num,
                    self.out_ep,
                    self.in_ep,
                    self.in_ep_max_packet_size,
                    self.timeouts.clone(),
                )
                .await?,
            );
        }

        Ok(self
            .connection
            .as_mut()
            .expect("connection was just established"))
    }

    /// Disconnects the PICOBOOT connection
    pub fn disconnect(&mut self) {
        self.connection = None; // Drop handles disconnection
    }

    /// Sets timeouts for the PICOBOOT connection, otherwise
    /// [`Timeouts::default`] is used
    pub fn set_timeouts(&mut self, timeouts: Timeouts) {
        self.timeouts = timeouts;
        if let Some(conn) = &mut self.connection {
            conn.timeouts = self.timeouts.clone();
        }
    }

    /// Returns whether the PICOBOOT device is currently connected
    pub fn is_connected(&self) -> bool {
        self.connection.is_some()
    }

    /// Returns a mutable reference to the active PICOBOOT connection, if any
    pub fn connection(&mut self) -> Option<&mut Connection> {
        self.connection.as_mut()
    }

    /// Returns the target type of the connected PICOBOOT device
    pub fn target(&self) -> &Target {
        &self.target
    }

    /// Returns VID/PID of the connected PICOBOOT device
    pub fn info(&self) -> String {
        format!(
            "{:04X}:{:04X}",
            self.device_info.vendor_id(),
            self.device_info.product_id()
        )
    }

    async fn internal_read(&mut self, addr: u32, size: u32, flash: bool) -> Result<Vec<u8>> {
        let was_connected = self.is_connected();
        if !was_connected {
            self.connect().await?;
        }

        trace!("Connected to PICOBOOT device for flash read");
        let conn = self.connection.as_mut().unwrap();

        trace!("Reset PICOBOOT interface");
        match conn.reset_interface().await {
            Ok(()) => {}
            Err(e) => {
                if !was_connected {
                    self.disconnect();
                }
                return Err(e);
            }
        }

        let read_fn = if flash {
            trace!("Reading flash");
            conn.flash_read(addr, size).await
        } else {
            trace!("Reading memory");
            conn.read(addr, size).await
        };
        match read_fn {
            Ok(data) => Ok(data),
            Err(e) => {
                // Perform best effort reset
                conn.reset_interface().await.ok();
                if !was_connected {
                    self.disconnect();
                }
                Err(e)
            }
        }
    }

    /// Convience function to avoid the need to explicitly connect before
    /// performing a flash read.
    ///
    /// Useful when performing a single operation.
    ///
    /// Args:
    /// - `addr` - Address to start the read. Must be on a multiple of
    ///   [`Target::flash_page_size`].
    /// - `size` - Number of bytes to read. Should be a multiple of
    ///   [`Target::flash_page_size`]. If not, it will be rejected.
    ///
    /// Returns:
    /// - `Ok(Vec<u8>)` - Buffer of data read from flash.
    pub async fn flash_read(&mut self, addr: u32, size: u32) -> Result<Vec<u8>> {
        self.internal_read(addr, size, true).await
    }

    /// Convience function to avoid the need to explicitly connect before
    /// performing a read.
    ///
    /// Useful when performing a single operation.
    ///
    /// Args:
    /// - `addr` - Address to start the read.
    /// - `size` - Number of bytes to read.
    ///
    /// Returns:
    /// - `Ok(Vec<u8>)` - Buffer of data read from flash.
    pub async fn read(&mut self, addr: u32, size: u32) -> Result<Vec<u8>> {
        self.internal_read(addr, size, false).await
    }

    async fn internal_write(&mut self, addr: u32, buf: &[u8], flash: bool) -> Result<()> {
        let was_connected = self.is_connected();
        if !was_connected {
            self.connect().await?;
        }

        trace!("Connected to PICOBOOT device for flash write");
        let conn = self.connection.as_mut().unwrap();

        trace!("Reset PICOBOOT interface");
        match conn.reset_interface().await {
            Ok(()) => {}
            Err(e) => {
                if !was_connected {
                    self.disconnect();
                }
                return Err(e);
            }
        }

        trace!("Writing memory");
        let write_fn = if flash {
            trace!("Writing flash");
            conn.flash_write(addr, buf).await
        } else {
            trace!("Writing memory");
            conn.write(addr, buf).await
        };
        match write_fn {
            Ok(()) => Ok(()),
            Err(e) => {
                // Best effort reset
                conn.reset_interface().await.ok();
                if !was_connected {
                    self.disconnect();
                }
                Err(e)
            }
        }
    }

    /// Convience function to perform a combined flash erase and write, and to
    /// avoid the need to explicitly connect before performing the operation.
    ///
    /// Useful when performing a single operation.
    ///
    /// Args:
    /// - `addr` - Address to start the write. Must be on a multiple of
    ///   [`Target::flash_page_size`].
    /// - `buf` - Buffer of data to write to memory. Should be a multiple of
    ///   [`Target::flash_page_size`]. If not, the remainder of the final page
    ///   is zero-filled.
    ///
    /// Returns:
    /// - `Ok(())` - If write command is successfully sent.
    ///
    /// Note: This does not perform a flash erase, so the caller must ensure that
    /// the flash region being written to is already erased, otherwise the write
    /// may fail.
    pub async fn flash_write(&mut self, addr: u32, buf: &[u8]) -> Result<()> {
        self.internal_write(addr, buf, true).await
    }

    /// Convience function to avoid the need to explicitly connect before
    /// performing a write.
    ///
    /// Useful when performing a single operation.
    ///
    /// Args:
    /// - `addr` - Address to start the write.
    /// - `buf` - Buffer of data to write to memory.
    ///
    /// Returns:
    /// - `Ok(())` - If write command is successfully sent.
    pub async fn write(&mut self, addr: u32, buf: &[u8]) -> Result<()> {
        self.internal_write(addr, buf, false).await
    }

    /// Convience function to avoid the need to explicitly connect before
    /// performing a flash erase.
    ///
    /// Useful when performing a single operation.
    ///
    /// Args:
    /// - `addr` - Address to start the erase. Must be on a multiple of
    ///   [`Target::flash_sector_size`].
    /// - `size` - Number of bytes to erase. Must be a multiple of
    ///   [`Target::flash_sector_size`].
    ///
    /// Returns:
    /// - `Ok(())` - If erase command is successfully sent.
    pub async fn flash_erase(&mut self, addr: u32, size: u32) -> Result<()> {
        let was_connected = self.is_connected();
        if !was_connected {
            self.connect().await?;
        }

        trace!("Connected to PICOBOOT device for flash erase");
        let conn = self.connection.as_mut().unwrap();

        trace!("Reset PICOBOOT interface");
        match conn.reset_interface().await {
            Ok(()) => {}
            Err(e) => {
                if !was_connected {
                    self.disconnect();
                }
                return Err(e);
            }
        }

        // There appears to be a bug in the RP2350 bootrom stepping where an
        // EXIT_XIP is required before a flash erase, otherwise the erase
        // fails silently (returns success, immediately).  See:
        // https://github.com/raspberrypi/pico-sdk/issues/2878
        //
        // EXIT_XIP is always required on RP2040 silicon.
        trace!("Exit XIP");
        match conn.exit_xip().await {
            Ok(()) => {}
            Err(e) => {
                if !was_connected {
                    self.disconnect();
                }
                return Err(e);
            }
        }

        trace!("Erasing flash memory");
        match conn.flash_erase(addr, size).await {
            Ok(()) => {
                // Do  not ENTER_XIP again, as other commands are likely to
                // be performed.  And the device is either in BOOTSEL mode (so
                // who cares if left in EXIT_XIP mode) or running an
                // application PICOBOOT stack like picobootx, which handles
                // XIP itself.
                Ok(())
            }
            Err(e) => {
                // Best effort reset
                conn.reset_interface().await.ok();
                if !was_connected {
                    self.disconnect();
                }
                Err(e)
            }
        }
    }

    /// Convience function to perform a combined flash erase and write, and to
    /// avoid the need to explicitly connect before performing the operation.
    ///
    /// Useful when performing a single operation.
    ///
    /// Args:
    /// - `addr` - Address to start the erase and write. Must be on a multiple of
    ///   [`Target::flash_sector_size`] and [`Target::flash_page_size`].
    /// - `buf` - Buffer of data to write to flash. Should be a multiple of
    ///   [`Target::flash_page_size`]. If not, the remainder of the final
    ///   page is zero-filled.
    ///
    /// Returns:
    /// - `Ok(())` - If erase and write commands are successfully sent.
    pub async fn flash_erase_and_write(&mut self, addr: u32, buf: &[u8]) -> Result<()> {
        if !addr.is_multiple_of(self.target.flash_sector_size())
            || !addr.is_multiple_of(self.target.flash_page_size())
        {
            return Err(Error::PicobootEraseInvalidAddr(self.target.clone(), addr));
        }

        // Round up flash erase size
        let flash_erase_size = u32::try_from(buf.len())
            .map_err(|_| Error::PicobootEraseInvalidSize(self.target.clone(), buf.len() as u32))?;
        let sector_size = self.target.flash_sector_size();
        let flash_erase_size = flash_erase_size.div_ceil(sector_size) * sector_size;

        let was_connected = self.is_connected();
        if !was_connected {
            self.connect().await?;
        }

        trace!("Connected to PICOBOOT device for flash erase and write");
        let conn = self.connection.as_mut().unwrap();

        trace!("Reset PICOBOOT interface");
        match conn.reset_interface().await {
            Ok(()) => {}
            Err(e) => {
                if !was_connected {
                    self.disconnect();
                }
                return Err(e);
            }
        }

        trace!("Erasing flash memory");
        match conn.flash_erase(addr, flash_erase_size).await {
            Ok(()) => {}
            Err(e) => {
                // Best effort reset
                conn.reset_interface().await.ok();
                if !was_connected {
                    self.disconnect();
                }
                return Err(e);
            }
        }

        match conn.flash_write(addr, buf).await {
            Ok(()) => Ok(()),
            Err(e) => {
                // Best effort reset
                conn.reset_interface().await.ok();
                if !was_connected {
                    self.disconnect();
                }
                Err(e)
            }
        }
    }

    /// Convenience function to reboot the device without needing to explicitly
    /// connect first.
    ///
    /// Args:
    /// - `reboot_type` - The type of reboot to perform.
    /// - `delay` - Time to start the device after. Must fit into a u32
    ///   milliseconds.
    ///
    /// Note: [`RebootType::Bootsel`] is only supported on RP2350. Returns
    /// [`Error::PicobootCmdNotAllowedForTarget`] for RP2040.  For custom
    /// targets it is allowed (although care must be taken to ensure the
    /// target actually supports it).
    ///
    /// Returns:
    /// - `Ok(())` - If reboot command is successfully sent.
    pub async fn reboot(&mut self, reboot_type: RebootType, delay: Duration) -> Result<()> {
        let was_connected = self.is_connected();
        if !was_connected {
            self.connect().await?;
        }

        let conn = self.connection.as_mut().unwrap();
        let target = self.target.clone();
        let result = match reboot_type {
            RebootType::Normal => match &target {
                Target::Rp2040 => conn.reboot(delay).await,
                _ => conn.reboot_rp2350(REBOOT_TYPE_NORMAL, 0, 0, delay).await,
            },
            RebootType::Bootsel {
                disable_msd,
                disable_picoboot,
            } => match &target {
                Target::Rp2040 => Err(Error::PicobootCmdNotAllowedForTarget(
                    target.clone(),
                    PicobootCmdId::Reboot2,
                )),
                _ => {
                    let mut p0 = 0u32;
                    if disable_msd {
                        p0 |= DISABLE_MSD_INTERFACE;
                    }
                    if disable_picoboot {
                        p0 |= DISABLE_PICOBOOT_INTERFACE;
                    }
                    let p1 = 0;

                    // Datasheet has p0 and p1 transposed
                    debug!(
                        "Rebooting RP2350 to BOOTSEL with p0=0x{p0:08X}, p1=0x{p1:08X}, delay={delay:?}"
                    );
                    conn.reboot_rp2350(REBOOT_TYPE_BOOTSEL, p0, p1, delay).await
                }
            },
        };

        match result {
            Ok(()) => {
                self.disconnect();
                Ok(())
            }
            Err(e) => {
                // Best effort reset
                conn.reset_interface().await.ok();
                if !was_connected {
                    self.disconnect();
                }
                Err(e)
            }
        }
    }

    /// Sends a custom picobootx protocol extension command to the device.
    ///
    /// Handles connection lifecycle automatically — connects if needed and
    /// disconnects afterwards if the connection was established by this call.
    ///
    /// Args:
    /// - `cmd` - The picobootx command to send.
    /// - `buf` - Data buffer for OUT data transfer commands; None for action commands.
    ///
    /// Returns:
    /// - `Ok(Vec<u8>)` - Data returned from the device for IN transfer commands,
    ///   empty for action commands.
    pub async fn send_picobootx_cmd(
        &mut self,
        cmd: PicobootXCmd,
        buf: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        let was_connected = self.is_connected();
        if !was_connected {
            self.connect().await?;
        }

        let conn = self.connection.as_mut().unwrap();

        match conn.reset_interface().await {
            Ok(()) => {}
            Err(e) => {
                if !was_connected {
                    self.disconnect();
                }
                return Err(e);
            }
        }

        match conn.send_picobootx_cmd(cmd, buf).await {
            Ok(res) => {
                if !was_connected {
                    self.disconnect();
                }
                Ok(res)
            }
            Err(e) => {
                // Best effort reset
                conn.reset_interface().await.ok();
                if !was_connected {
                    self.disconnect();
                }
                Err(e)
            }
        }
    }

    /// Returns the serial number of the device, if available.
    pub fn serial_number(&self) -> Option<&str> {
        self.device_info.serial_number()
    }

    /// Returns the vendor ID of the device.
    pub fn vid(&self) -> u16 {
        self.device_info.vendor_id()
    }

    /// Returns the product ID of the device.
    pub fn pid(&self) -> u16 {
        self.device_info.product_id()
    }

    /// Returns the device.
    pub fn device_version(&self) -> u16 {
        self.device_info.device_version()
    }

    /// Returns the device's manufacturer string, if available.
    pub fn manufacturer_string(&self) -> Option<&str> {
        self.device_info.manufacturer_string()
    }

    /// Returns the device's product string, if available.
    pub fn product_string(&self) -> Option<&str> {
        self.device_info.product_string()
    }

    /// Returns the USB version supported by the device.
    pub fn usb_version(&self) -> u16 {
        self.device_info.usb_version()
    }

    /// Returns the device's speed, if available.
    pub fn speed(&self) -> Option<Speed> {
        self.device_info.speed()
    }
}
