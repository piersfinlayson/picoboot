// Copyright (C) 2025 Piers Finlayson <piers@piers.rocks>
//
// MIT License

use deku::DekuContainerWrite;
#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
use std::sync::Arc;
use std::time::Duration;

use crate::cmd::{PicobootCmd, PicobootCmdId, PicobootStatusCmd, PicobootXCmd, otp_row_count};
use crate::transport::{Timeouts, Transaction, Transport};
use crate::{Access, Direction, Error as PicobootError, Target};

// see https://github.com/raspberrypi/picotool/blob/master/main.cpp#L4173
// for loading firmware over a connection

type Error = PicobootError;
type Result<T> = ::std::result::Result<T, Error>;

pub(crate) const REBOOT_TYPE_NORMAL: u32 = 0x0000;

/// The most one FLASH_ERASE command erases, and the boundaries it ends on: the
/// size of the boot ROM's flash erase block.  A command is limited by one
/// transport timeout.
const ERASE_CHUNK: u64 = 0x1_0000;

/// The size of a WRITE where the transport's limit allows it.
const WRITE_CHUNK: usize = crate::SECTOR_SIZE as usize;

/// Active connection to a PICOBOOT device
///
/// This object is used to send commands to a connected PICOBOOT capable
/// device.  Its copies share one open of the transport, which is closed when
/// the last copy is dropped.
///
/// ## Example
///
/// ```rust,no_run
/// # use picoboot::{Picoboot, Error, Transport};
/// # async fn example(transport: impl Transport + 'static) -> Result<(), Error> {
///     let mut picoboot = Picoboot::new(transport)?;
///     let conn = picoboot.connect().await?;
///     conn.flash_erase_start(4096).await?;
///     conn.flash_write_start(&[0u8; 4096]).await?;
///     let data = conn.flash_read_start(4096).await?;
/// #    Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Connection {
    target: Target,
    open: Arc<Open>,
    pub(crate) timeouts: Timeouts,
    cmd_token: u32,
}

/// One open of a transport, closed when dropped.
#[derive(Debug)]
pub(crate) struct Open {
    transport: Arc<dyn Transport>,
}

impl Open {
    /// Opens `transport`.
    pub(crate) async fn new(
        transport: Arc<dyn Transport>,
        target: &Target,
        timeouts: &Timeouts,
    ) -> Result<Self> {
        transport.open(target, timeouts).await?;
        Ok(Self { transport })
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        self.transport.close();
    }
}

impl Connection {
    /// Resets the PICOBOOT interface.
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
        self.open.transport.reset(&self.timeouts).await
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

        let out = cmd.is_data_transfer() && cmd.direction() == Direction::Out;
        if out && buf.is_none() {
            debug!("No buffer provided for OUT data transfer command");
            return Err(Error::PicobootCmdDataMissing(self.target.clone(), cmd.id()));
        }

        trace!("Sending command {cmd:?}");
        let out = if out { buf.unwrap_or_default() } else { &[] };
        self.transact(&cmd_bytes, cmd.direction(), cmd.get_transfer_len(), out)
            .await
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
    /// The erase is sent as one command for each 64 KB block it touches, so a
    /// failure shows within one command's timeout wherever it happens.  The
    /// first and last commands are shorter where the range doesn't start or
    /// end on a 64 KB boundary.
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

        let end = u64::from(addr) + u64::from(size);
        let mut start = u64::from(addr);
        loop {
            let next = ((start / ERASE_CHUNK + 1) * ERASE_CHUNK).min(end);
            let chunk = PicobootCmd::flash_erase(start as u32, (next - start) as u32);
            let _ = self.send_cmd(chunk, None).await?;
            if next >= end {
                return Ok(());
            }
            start = next;
        }
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
        let chunk_size = self.write_chunk();
        let mut offset = 0;

        while offset < buf.len() {
            let chunk = &buf[offset..std::cmp::min(offset + chunk_size, buf.len())];
            let chunk_addr = addr + offset as u32;
            let _ = self
                .send_cmd(
                    PicobootCmd::flash_write(chunk_addr, chunk.len() as u32),
                    Some(chunk),
                )
                .await?;
            offset += chunk_size;
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
    ///
    /// A read larger than the transport's largest transfer is sent as several
    /// READ commands.
    pub async fn read(&mut self, addr: u32, size: u32) -> Result<Vec<u8>> {
        debug!("Picoboot: Reading {size} bytes from address 0x{addr:08X}");
        let chunk = self.read_chunk(size);
        if chunk >= size {
            return self
                .send_cmd(PicobootCmd::flash_read(addr, size), None)
                .await;
        }

        let mut data = Vec::with_capacity(size as usize);
        let mut offset = 0;
        while offset < size {
            let len = chunk.min(size - offset);
            let part = self
                .send_cmd(PicobootCmd::flash_read(addr + offset, len), None)
                .await?;
            data.extend_from_slice(&part);
            offset += len;
        }
        Ok(data)
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
    /// RP2040 requires it before a flash erase or write.  The RP2350
    /// datasheet documents it as a no-op, but the RP2350 A2 bootrom requires
    /// it too.  See [`Picoboot::flash_erase`](crate::Picoboot::flash_erase).
    ///
    /// Returns:
    /// - `Ok(())` - If exit XIP command is successfully sent.
    pub async fn exit_xip(&mut self) -> Result<()> {
        let _ = self.send_cmd(PicobootCmd::exit_xip(), None).await?;

        Ok(())
    }

    /// Reads OTP rows with ECC.  RP2350 only.
    ///
    /// - `row` - First row to read.
    /// - `count` - Number of rows to read.
    ///
    /// An ECC read never reports bad data.  The hardware corrects one incorrect
    /// bit in a row.  A row with more damage returns an incorrect value.  A row
    /// written raw returns a meaningless value.  [`Self::otp_read_raw()`]
    /// returns the stored bits.
    ///
    /// Returns:
    /// - `Ok(Vec<u16>)` - One value per row.
    /// - `Err(Error::PicobootOtpInvalidRange)` - A row is past the end of OTP.
    ///   Nothing is sent.
    pub async fn otp_read_ecc(&mut self, row: u16, count: u16) -> Result<Vec<u16>> {
        let data = self.otp_read(row, count, true).await?;
        let (rows, _) = data.as_chunks::<2>();
        Ok(rows.iter().map(|b| u16::from_le_bytes(*b)).collect())
    }

    /// Reads raw OTP rows.  RP2350 only.
    ///
    /// - `row` - First row to read.
    /// - `count` - Number of rows to read.
    ///
    /// Returns:
    /// - `Ok(Vec<u32>)` - One 24-bit value per row.
    /// - `Err(Error::PicobootOtpInvalidRange)` - A row is past the end of OTP.
    ///   Nothing is sent.
    pub async fn otp_read_raw(&mut self, row: u16, count: u16) -> Result<Vec<u32>> {
        let data = self.otp_read(row, count, false).await?;
        let (rows, _) = data.as_chunks::<4>();
        Ok(rows.iter().map(|b| u32::from_le_bytes(*b)).collect())
    }

    /// Writes OTP rows with ECC.  RP2350 only.
    ///
    /// - `row` - First row to write.
    /// - `values` - One value per row.
    ///
    /// The device refuses a locked row, and any value it can't store over the
    /// bits already set.  After a refusal:
    /// - the rows before the refused one stay written
    /// - the call returns a USB error
    /// - [`Self::get_command_status()`] gives the reason until the interface is
    ///   reset
    ///
    /// A timeout can leave any of the rows written.
    ///
    /// A written row is not always refused.  Where its bits allow, the device
    /// stores the new value inverted and the row then reads as the new value.
    /// Writing 0 to a writable row always succeeds.  Only a raw read shows
    /// whether a row is unwritten.
    ///
    /// The device does not check what it wrote.  To verify a row, read it raw
    /// and compare it with the value's ECC encoding.
    ///
    /// Returns:
    /// - `Err(Error::PicobootOtpInvalidRange)` - A row is past the end of OTP.
    ///   Nothing is sent.
    pub async fn otp_write_ecc(&mut self, row: u16, values: &[u16]) -> Result<()> {
        let data: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.otp_write(row, values.len(), true, &data).await
    }

    /// Writes raw OTP rows.  RP2350 only.
    ///
    /// - `row` - First row to write.
    /// - `values` - One 24-bit value per row.  Bits 31–24 are ignored.
    ///
    /// A value must keep every bit already set in its row.  To set bits, read
    /// the row and write it back with the bits added.  Refusals and timeouts
    /// are as for [`Self::otp_write_ecc()`].  The device does not check what
    /// it wrote.  Read the rows back to verify them.
    ///
    /// Returns:
    /// - `Err(Error::PicobootOtpInvalidRange)` - A row is past the end of OTP.
    ///   Nothing is sent.
    pub async fn otp_write_raw(&mut self, row: u16, values: &[u32]) -> Result<()> {
        let data: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.otp_write(row, values.len(), false, &data).await
    }

    /// Returns the target type of the connected PICOBOOT device
    pub fn target(&mut self) -> &Target {
        &self.target
    }

    /// Returns the status of the last command.
    pub async fn get_command_status(&mut self) -> Result<PicobootStatusCmd> {
        self.open.transport.command_status(&self.timeouts).await
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

        let out = cmd.is_data_transfer() && cmd.direction() == Direction::Out;
        if out && buf.is_none() {
            debug!("No buffer provided for OUT data transfer picobootx command");
            return Err(Error::PicobootXCmdDataMissing(
                self.target.clone(),
                cmd.get_cmd_id(),
            ));
        }

        trace!("Sending picobootx command {cmd:?}");
        let out = if out { buf.unwrap_or_default() } else { &[] };
        self.transact(&cmd_bytes, cmd.direction(), cmd.get_transfer_len(), out)
            .await
    }
}

// Internal methods
impl Connection {
    pub(crate) fn new(target: Target, open: Arc<Open>, timeouts: Timeouts) -> Self {
        Self {
            target,
            open,
            timeouts,
            cmd_token: 1,
        }
    }

    /// Runs one transaction.  IN data that isn't `transfer_len` bytes is an
    /// error.
    async fn transact(
        &mut self,
        packet: &[u8],
        direction: Direction,
        transfer_len: usize,
        out: &[u8],
    ) -> Result<Vec<u8>> {
        let packet: &[u8; 32] = packet
            .try_into()
            .expect("PicobootCmd and PicobootXCmd serialize to 32 bytes");
        let transaction = Transaction {
            packet,
            direction,
            transfer_len,
            out,
        };
        let data = self
            .open
            .transport
            .command(transaction, &self.timeouts)
            .await?;
        let expected = match direction {
            Direction::In => transfer_len,
            Direction::Out => 0,
        };
        if data.len() != expected {
            debug!("IN data is {} bytes, expected {expected}", data.len());
            return Err(Error::PicobootInDataLength(
                self.target.clone(),
                expected,
                data.len(),
            ));
        }
        Ok(data)
    }

    /// The size of each WRITE: a flash sector, or the transport's limit in
    /// whole pages where that is smaller.
    fn write_chunk(&self) -> usize {
        let page = self.target.flash_page_size() as usize;
        match self.open.transport.max_transfer() {
            Some(max) if max < WRITE_CHUNK => (max - max % page).max(page),
            _ => WRITE_CHUNK,
        }
    }

    /// The largest READ for the transport, a whole number of flash pages where
    /// the transport allows one.  A transport limit of zero leaves `size`
    /// unchanged so the transport reports the failure.
    fn read_chunk(&self, size: u32) -> u32 {
        let Some(max) = self.open.transport.max_transfer() else {
            return size;
        };
        let max = u32::try_from(max).unwrap_or(u32::MAX);
        let page = self.target.flash_page_size();
        let chunk = if max >= page { max - max % page } else { max };
        if chunk == 0 { size } else { chunk }
    }

    /// Sends an OTP_READ and returns its data.  A short read is an error rather
    /// than fewer rows, from [`Self::transact()`].
    async fn otp_read(&mut self, row: u16, count: u16, ecc: bool) -> Result<Vec<u8>> {
        self.refuse_rp2040(PicobootCmdId::OtpRead)?;
        self.check_otp_range(row, usize::from(count))?;
        debug!("Picoboot: Reading {count} OTP rows from row {row:#05x}, ecc={ecc}");

        self.send_cmd(PicobootCmd::otp_read(row, count, ecc), None)
            .await
    }

    /// Sends an OTP_WRITE of `rows` rows carrying `data`.
    async fn otp_write(&mut self, row: u16, rows: usize, ecc: bool, data: &[u8]) -> Result<()> {
        self.refuse_rp2040(PicobootCmdId::OtpWrite)?;
        let count = self.check_otp_range(row, rows)?;
        debug!("Picoboot: Writing {count} OTP rows from row {row:#05x}, ecc={ecc}");

        let _ = self
            .send_cmd(PicobootCmd::otp_write(row, count, ecc), Some(data))
            .await?;
        Ok(())
    }

    /// Checks the range and returns its row count.
    fn check_otp_range(&self, row: u16, rows: usize) -> Result<u16> {
        otp_row_count(row, rows)
            .ok_or_else(|| Error::PicobootOtpInvalidRange(self.target.clone(), row, rows))
    }

    /// Refuses a command an RP2040 does not have.
    fn refuse_rp2040(&self, cmd: PicobootCmdId) -> Result<()> {
        if self.target == Target::Rp2040 {
            return Err(Error::PicobootCmdNotAllowedForTarget(
                self.target.clone(),
                cmd,
            ));
        }
        Ok(())
    }

    fn cmd_token(&mut self) -> u32 {
        let token = self.cmd_token;
        self.cmd_token += 1;
        token
    }
}
