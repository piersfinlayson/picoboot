// Copyright (C) 2025 Piers Finlayson <piers@piers.rocks>
//
// MIT License

#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
use std::sync::Arc;
use std::time::Duration;

use crate::cmd::{PicobootCmdId, PicobootStatusCmd, PicobootXCmd};
use crate::connection::{Connection, Open, REBOOT_TYPE_NORMAL};
use crate::transport::{Timeouts, Transport};
use crate::{Error as PicobootError, RebootType, Target};

type Error = PicobootError;
type Result<T> = ::std::result::Result<T, Error>;

const REBOOT_TYPE_BOOTSEL: u32 = 0x0002;
const REBOOT_TYPE_RAM_IMAGE: u32 = 0x0003;
const DISABLE_MSD_INTERFACE: u32 = 0x01;
const DISABLE_PICOBOOT_INTERFACE: u32 = 0x02;

/// A PICOBOOT device, connected using a [`Transport`].
///
/// ## Example
///
/// ```rust,no_run
/// use picoboot::{Picoboot, Error};
/// use picoboot_nusb::UsbDevice;
///
/// #[tokio::main]
/// async fn main() -> Result<(), Error> {
///     let mut picoboot = Picoboot::new(UsbDevice::first(None).await?)?;
///     let conn = picoboot.connect().await?;
///     Ok(())
/// }
/// ```
#[derive(Debug, Clone)]
pub struct Picoboot {
    transport: Arc<dyn Transport>,
    connection: Option<Connection>,
    target: Target,
    timeouts: Timeouts,
    last_status: Option<PicobootStatusCmd>,
}

impl Picoboot {
    /// Creates a `Picoboot` for `transport`'s device, not yet connected.
    ///
    /// Fails with [`Error::PicobootTargetUnknown`] when the transport can't
    /// tell its target.  Use [`Self::with_target()`] for that transport.
    pub fn new(transport: impl Transport + 'static) -> Result<Self> {
        let Some(target) = transport.target() else {
            return Err(Error::PicobootTargetUnknown(transport.to_string()));
        };
        Ok(Self::with_target(transport, target))
    }

    /// Creates a `Picoboot` for `transport`'s device with `target` in place of
    /// the transport's own.
    pub fn with_target(transport: impl Transport + 'static, target: Target) -> Self {
        Self {
            transport: Arc::new(transport),
            connection: None,
            target,
            timeouts: Timeouts::default(),
            last_status: None,
        }
    }

    /// Opens the transport and returns the connection.  A connected
    /// `Picoboot` returns its existing connection.
    pub async fn connect(&mut self) -> Result<&mut Connection> {
        if self.connection.is_none() {
            let open = Open::new(self.transport.clone(), &self.target, &self.timeouts).await?;
            self.connection = Some(Connection::new(
                self.target.clone(),
                Arc::new(open),
                self.timeouts.clone(),
            ));
        }

        Ok(self
            .connection
            .as_mut()
            .expect("connection was just established"))
    }

    /// Drops this copy's connection.  The transport closes when no copy of
    /// the connection is left.
    pub fn disconnect(&mut self) {
        self.connection = None;
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

    /// The device's command status for the last command that failed.  `None`
    /// before a command fails, or where the status couldn't be read.
    pub fn last_status(&self) -> Option<&PicobootStatusCmd> {
        self.last_status.as_ref()
    }

    /// The transport's description of the device, for example `2E8A:000F`
    /// for a USB device.
    pub fn info(&self) -> String {
        self.transport.to_string()
    }

    async fn internal_read(&mut self, addr: u32, size: u32, flash: bool) -> Result<Vec<u8>> {
        let was_connected = self.is_connected();
        if !was_connected {
            self.connect().await?;
        }

        // The RP2350 A2 bootrom returns zeros for a flash read until an
        // EXIT_XIP, as it does nothing for an erase or write (see
        // flash_erase()).  Whether an RP2040 flash read works after EXIT_XIP
        // hasn't been tested, so it's sent on RP2350 only.  PicobootReader
        // does the same.
        let exit_xip = flash && self.target == Target::Rp2350;

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

        if exit_xip {
            trace!("Exit XIP");
            if let Err(e) = conn.exit_xip().await {
                self.last_status = conn.get_command_status().await.ok();
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
                self.last_status = conn.get_command_status().await.ok();
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

        // See flash_erase() for why EXIT_XIP comes first.
        if flash {
            trace!("Exit XIP");
            if let Err(e) = conn.exit_xip().await {
                self.last_status = conn.get_command_status().await.ok();
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
                self.last_status = conn.get_command_status().await.ok();
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

        // The RP2350 A2 bootrom requires an EXIT_XIP before a flash erase or
        // write, otherwise the command returns success having done nothing.
        // See:
        // https://github.com/raspberrypi/pico-sdk/issues/2878
        //
        // EXIT_XIP is always required on RP2040 silicon.
        trace!("Exit XIP");
        match conn.exit_xip().await {
            Ok(()) => {}
            Err(e) => {
                self.last_status = conn.get_command_status().await.ok();
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
                self.last_status = conn.get_command_status().await.ok();
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

        // See flash_erase() for why EXIT_XIP comes first.
        trace!("Exit XIP");
        if let Err(e) = conn.exit_xip().await {
            self.last_status = conn.get_command_status().await.ok();
            if !was_connected {
                self.disconnect();
            }
            return Err(e);
        }

        trace!("Erasing flash memory");
        match conn.flash_erase(addr, flash_erase_size).await {
            Ok(()) => {}
            Err(e) => {
                // Best effort reset
                self.last_status = conn.get_command_status().await.ok();
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
                self.last_status = conn.get_command_status().await.ok();
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
    /// Note: [`RebootType::Bootsel`] and [`RebootType::RamImage`] are only
    /// supported on RP2350. Returns
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
            RebootType::RamImage { start, size } => match &target {
                Target::Rp2040 => Err(Error::PicobootCmdNotAllowedForTarget(
                    target.clone(),
                    PicobootCmdId::Reboot2,
                )),
                _ => {
                    debug!("Rebooting RP2350 into the RAM image at 0x{start:08X}, {size} bytes");
                    conn.reboot_rp2350(REBOOT_TYPE_RAM_IMAGE, start, size, delay)
                        .await
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
                self.last_status = conn.get_command_status().await.ok();
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
                self.last_status = conn.get_command_status().await.ok();
                conn.reset_interface().await.ok();
                if !was_connected {
                    self.disconnect();
                }
                Err(e)
            }
        }
    }
}
