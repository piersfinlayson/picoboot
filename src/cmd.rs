// Copyright (C) 2025 Piers Finlayson <piers@piers.rocks>
//
// MIT License

use deku::{DekuContainerWrite, DekuRead, DekuWrite};

use crate::{Direction, PICOBOOT_MAGIC};

pub(crate) const REQUEST_RESET: u8 = 0x41;
pub(crate) const REQUEST_GET_COMMAND_STATUS: u8 = 0x42;
pub(crate) const RESPONSE_GET_COMMAND_STATUS_SIZE: usize = 16;

// see https://datasheets.raspberrypi.com/rp2040/rp2040-datasheet.pdf
// section 2.8.5 for details on PICOBOOT interface

/// Command ID of commands for PICOBOOT interface.
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum PicobootCmdId {
    /// Request exclusive access to the device, ejecting mass storage if
    /// desired.
    ExclusiveAccess = 0x1,
    /// Reboot an RP2040
    Reboot = 0x2,
    /// Reboot an RP2350
    Reboot2 = 0xA,
    /// Erase a region of FLASH.  Must be aligned to sector boundaries and
    /// be a multiple of sector size.
    FlashErase = 0x3,
    /// Write to RAM or FLASH.  Does not erase first, and FLASH writes must be
    /// aligned to page boundaries.  Writes of less than a page multiple will
    /// be padded with zeros.
    Write = 0x5,
    /// Exit XIP mode - supported but a no-op on RP2350
    ExitXip = 0x6,
    /// Enter XIP mode - supported but a no-op on RP2350
    EnterXip = 0x7,
    /// Execute code in RAM or FLASH - only supported on RP2040
    Exec = 0x8,
    /// Requests that the vector table of flash access functions used
    /// internally by the Mass Storage and PICOBOOT interfaces be copied into
    /// RAM - only supported on RP2040
    VectorizeFlash = 0x9,
    /// Write to OTP - only supported on RP2350
    OtpWrite = 0xD,
    /// Get device information - only supported on RP2350
    GetInfo = 0x8B,
    /// Read from OTP - only supported on RP2350
    OtpRead = 0x8C,
    /// Read from RAM or FLASH
    Read = 0x84,
}

impl std::fmt::Display for PicobootCmdId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            PicobootCmdId::ExclusiveAccess => "EXCLUSIVE_ACCESS",
            PicobootCmdId::Reboot => "REBOOT",
            PicobootCmdId::Reboot2 => "REBOOT2",
            PicobootCmdId::FlashErase => "FLASH_ERASE",
            PicobootCmdId::Write => "WRITE",
            PicobootCmdId::ExitXip => "EXIT_XIP",
            PicobootCmdId::EnterXip => "ENTER_XIP",
            PicobootCmdId::Exec => "EXEC",
            PicobootCmdId::VectorizeFlash => "VECTORIZE_FLASH",
            PicobootCmdId::GetInfo => "GET_INFO",
            PicobootCmdId::OtpRead => "OTP_READ",
            PicobootCmdId::OtpWrite => "OTP_WRITE",
            PicobootCmdId::Read => "READ",
        };
        write!(f, "{}", name)
    }
}

impl TryFrom<u8> for PicobootCmdId {
    type Error = ();

    fn try_from(x: u8) -> Result<Self, Self::Error> {
        match x {
            x if x == Self::ExclusiveAccess as u8 => Ok(Self::ExclusiveAccess),
            x if x == Self::Reboot as u8 => Ok(Self::Reboot),
            x if x == Self::FlashErase as u8 => Ok(Self::FlashErase),
            x if x == Self::Read as u8 => Ok(Self::Read),
            x if x == Self::Write as u8 => Ok(Self::Write),
            x if x == Self::ExitXip as u8 => Ok(Self::ExitXip),
            x if x == Self::EnterXip as u8 => Ok(Self::EnterXip),
            x if x == Self::Exec as u8 => Ok(Self::Exec),
            x if x == Self::VectorizeFlash as u8 => Ok(Self::VectorizeFlash),
            x if x == Self::Reboot2 as u8 => Ok(Self::Reboot2),
            x if x == Self::GetInfo as u8 => Ok(Self::GetInfo),
            x if x == Self::OtpRead as u8 => Ok(Self::OtpRead),
            x if x == Self::OtpWrite as u8 => Ok(Self::OtpWrite),
            _ => Err(()),
        }
    }
}

impl PicobootCmdId {
    pub fn direction(&self) -> Direction {
        if *self as u8 & 0x80 != 0 {
            Direction::In
        } else {
            Direction::Out
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum PicobootStatus {
    Ok = 0,
    UnknownCmd = 1,
    InvalidCmdLength = 2,
    InvalidTransferLength = 3,
    InvalidAddress = 4,
    BadAlignment = 5,
    InterleavedWrite = 6,
    Rebooting = 7,
    UnknownError = 8,
    InvalidState = 9,
    NotPermitted = 10,
    InvalidArg = 11,
    BufferTooSmall = 12,
    PreconditionNotMet = 13,
    ModifiedData = 14,
    InvalidData = 15,
    NotFound = 16,
    UnsupportedModification = 17,
}
impl TryFrom<u32> for PicobootStatus {
    type Error = ();

    fn try_from(x: u32) -> Result<Self, Self::Error> {
        match x {
            x if x == Self::Ok as u32 => Ok(Self::Ok),
            x if x == Self::UnknownCmd as u32 => Ok(Self::UnknownCmd),
            x if x == Self::InvalidCmdLength as u32 => Ok(Self::InvalidCmdLength),
            x if x == Self::InvalidTransferLength as u32 => Ok(Self::InvalidTransferLength),
            x if x == Self::InvalidAddress as u32 => Ok(Self::InvalidAddress),
            x if x == Self::BadAlignment as u32 => Ok(Self::BadAlignment),
            x if x == Self::InterleavedWrite as u32 => Ok(Self::InterleavedWrite),
            x if x == Self::Rebooting as u32 => Ok(Self::Rebooting),
            x if x == Self::UnknownError as u32 => Ok(Self::UnknownError),
            x if x == Self::InvalidState as u32 => Ok(Self::InvalidState),
            x if x == Self::NotPermitted as u32 => Ok(Self::NotPermitted),
            x if x == Self::InvalidArg as u32 => Ok(Self::InvalidArg),
            x if x == Self::BufferTooSmall as u32 => Ok(Self::BufferTooSmall),
            x if x == Self::PreconditionNotMet as u32 => Ok(Self::PreconditionNotMet),
            x if x == Self::ModifiedData as u32 => Ok(Self::ModifiedData),
            x if x == Self::InvalidData as u32 => Ok(Self::InvalidData),
            x if x == Self::NotFound as u32 => Ok(Self::NotFound),
            x if x == Self::UnsupportedModification as u32 => Ok(Self::UnsupportedModification),
            _ => Err(()),
        }
    }
}

impl PicobootStatus {
    pub fn is_ok(&self) -> bool {
        matches!(self, PicobootStatus::Ok)
    }
}

#[derive(DekuRead, DekuWrite, Debug, Clone)]
#[deku(endian = "little")]
struct PicobootRangeCmd {
    addr: u32,
    size: u32,
    _unused: u64,
}
impl PicobootRangeCmd {
    pub fn ser(addr: u32, size: u32) -> [u8; 16] {
        let c = PicobootRangeCmd {
            addr,
            size,
            _unused: 0,
        };
        c.to_bytes()
            .unwrap()
            .try_into()
            .unwrap_or_else(|v: Vec<u8>| {
                panic!("Expected a Vec of length {} but it was {}", 16, v.len())
            })
    }
}

#[derive(DekuRead, DekuWrite, Debug, Clone)]
#[deku(endian = "little")]
struct PicobootRebootCmd {
    pc: u32,
    sp: u32,
    delay: u32,
    _unused: u32,
}
impl PicobootRebootCmd {
    pub fn ser(pc: u32, sp: u32, delay: u32) -> [u8; 16] {
        let c = PicobootRebootCmd {
            pc,
            sp,
            delay,
            _unused: 0,
        };
        c.to_bytes()
            .unwrap()
            .try_into()
            .unwrap_or_else(|v: Vec<u8>| {
                panic!("Expected a Vec of length {} but it was {}", 16, v.len())
            })
    }
}

#[derive(DekuRead, DekuWrite, Debug, Clone)]
#[deku(endian = "little")]
struct PicobootReboot2Cmd {
    flags: u32,
    delay: u32,
    p0: u32,
    p1: u32,
}
impl PicobootReboot2Cmd {
    pub fn ser(flags: u32, delay: u32, p0: u32, p1: u32) -> [u8; 16] {
        let c = PicobootReboot2Cmd {
            flags,
            delay,
            p0,
            p1,
        };
        c.to_bytes()
            .unwrap()
            .try_into()
            .unwrap_or_else(|v: Vec<u8>| {
                panic!("Expected a Vec of length {} but it was {}", 16, v.len())
            })
    }
}

#[derive(DekuRead, DekuWrite, Debug, Clone)]
#[deku(endian = "little")]
pub struct PicobootStatusCmd {
    token: u32,
    status_code: u32,
    cmd_id: u8,
    in_progress: u8,
    _unused: [u8; 6],
}
impl PicobootStatusCmd {
    pub fn get_token(&self) -> u32 {
        self.token
    }

    /// The status the device reported, as a known [`PicobootStatus`].
    ///
    /// A code this crate does not know is reported as
    /// [`PicobootStatus::UnknownError`] rather than panicking: the value comes
    /// off the wire from a device, so it is not something a host can assume is
    /// well formed. Use [`Self::raw_status_code`] for the exact value, or
    /// [`Self::try_status_code`] to tell a genuine `UnknownError` apart from a
    /// code this crate has no name for.
    pub fn get_status_code(&self) -> PicobootStatus {
        self.try_status_code()
            .unwrap_or(PicobootStatus::UnknownError)
    }

    /// The status the device reported, or `None` if this crate has no name for
    /// the code.
    pub fn try_status_code(&self) -> Option<PicobootStatus> {
        self.status_code.try_into().ok()
    }

    /// The status exactly as the device sent it, known to this crate or not.
    pub fn raw_status_code(&self) -> u32 {
        self.status_code
    }

    pub fn get_cmd_id(&self) -> u8 {
        self.cmd_id
    }

    pub fn get_in_progress(&self) -> u8 {
        self.in_progress
    }

    /// Whether the device reported success.
    ///
    /// Compares the raw code, so an unrecognised status is correctly *not*
    /// success rather than being mapped through a name first.
    pub fn is_ok(&self) -> bool {
        self.status_code == PicobootStatus::Ok as u32
    }
}

/// Command structure for PICOBOOT interface.
///
/// This structure contains shorthands for creating commands but does not do any
/// sort of runtime checks to ensure safe use of these commands.
#[derive(DekuRead, DekuWrite, Debug, Clone)]
#[deku(endian = "little")]
pub struct PicobootCmd {
    /// Magic number ([`PICOBOOT_MAGIC`]) to identify the command for the PICOBOOT interface.
    magic: u32,
    /// Token number to uniquely identify commands and their responses.
    token: u32,
    /// Command ID ([`PicobootCmdId`]) to tell what command the data is to be used for. The top bit (0x80) indicates data transfer direction.
    cmd_id: u8,
    /// Command size, number of bytes to read from the `args` field.
    cmd_size: u8,
    /// Reserved space
    _unused: u16,
    /// Transfer length, the number of bytes expected to send or recieve over the bulk endpoint(s).
    transfer_len: u32,
    /// Command specific args, padded with zeros.
    args: [u8; 16],
}

impl PicobootCmd {
    /// Creates a new PicobootCmd
    pub fn new(cmd_id: PicobootCmdId, cmd_size: u8, transfer_len: u32, args: [u8; 16]) -> Self {
        PicobootCmd {
            magic: PICOBOOT_MAGIC,
            token: 0,
            cmd_id: cmd_id as u8,
            cmd_size,
            _unused: 0,
            transfer_len,
            args,
        }
    }

    pub fn set_token(mut self, token: u32) -> Self {
        self.token = token;
        self
    }

    pub fn get_transfer_len(&self) -> usize {
        self.transfer_len as usize
    }

    pub fn id(&self) -> PicobootCmdId {
        self.cmd_id.try_into().unwrap()
    }

    pub fn direction(&self) -> Direction {
        self.id().direction()
    }

    pub fn is_data_transfer(&self) -> bool {
        self.transfer_len != 0
    }

    /// Creates an EXCLUSIVE_ACCESS command
    pub fn exclusive_access(exclusive: u8) -> Self {
        let mut args = [0; 16];
        args[0] = exclusive;
        PicobootCmd::new(PicobootCmdId::ExclusiveAccess, 1, 0, args)
    }

    /// Creates a REBOOT command
    pub fn reboot(pc: u32, sp: u32, delay: u32) -> Self {
        let args = PicobootRebootCmd::ser(pc, sp, delay);
        PicobootCmd::new(PicobootCmdId::Reboot, 12, 0, args)
    }

    /// Creates a REBOOT2 command (normal boot)
    pub fn reboot2(flags: u32, p0: u32, p1: u32, delay: u32) -> Self {
        let args = PicobootReboot2Cmd::ser(flags, delay, p0, p1);
        PicobootCmd::new(PicobootCmdId::Reboot2, 0x10, 0, args)
    }

    /// Creates a FLASH_ERASE command
    pub fn flash_erase(addr: u32, size: u32) -> Self {
        let args = PicobootRangeCmd::ser(addr, size);
        PicobootCmd::new(PicobootCmdId::FlashErase, 8, 0, args)
    }

    /// Creates a WRITE command
    pub fn flash_write(addr: u32, size: u32) -> Self {
        let args = PicobootRangeCmd::ser(addr, size);
        PicobootCmd::new(PicobootCmdId::Write, 8, size, args)
    }

    /// Creates a READ command
    pub fn flash_read(addr: u32, size: u32) -> Self {
        let args = PicobootRangeCmd::ser(addr, size);
        PicobootCmd::new(PicobootCmdId::Read, 8, size, args)
    }

    /// Creates an ENTER_XIP command
    pub fn enter_xip() -> Self {
        PicobootCmd::new(PicobootCmdId::EnterXip, 0, 0, [0; 16])
    }

    /// Creates an EXIT_XIP command
    pub fn exit_xip() -> Self {
        PicobootCmd::new(PicobootCmdId::ExitXip, 0, 0, [0; 16])
    }
}

/// Command structure for picobootx extended protocol.
///
/// Identical wire format to PicobootCmd but accepts an arbitrary magic value,
/// enabling extension protocols over the same bulk endpoints.
///
/// For commands that require Data IN (that is data to be transferred from the
/// device to the host), the cmd_id should have the MSB (0x80) set, and for
/// Data OUT commands it should be clear.
///
/// The caller is responsible for constructing valid args for their extension
/// protocol using the [`PicobootXCmd`] struct.
#[derive(DekuRead, DekuWrite, Debug, Clone)]
#[deku(endian = "little")]
pub struct PicobootXCmd {
    magic: u32,
    token: u32,
    cmd_id: u8,
    cmd_size: u8,
    _unused: u16,
    transfer_len: u32,
    args: [u8; 16],
}

impl PicobootXCmd {
    /// Create a new PicobootXCmd with the given magic, cmd_id, cmd_size,
    /// transfer_len and args.
    ///
    /// These fields should be set according to th specification of the
    /// extension.  For more guidance on these fields, see the RP2040/RP2350
    /// datasheets which define the underlying picoboot protocl.
    ///
    /// Any magic can be used so long as it's not [`PICOBOOT_MAGIC`].
    pub fn new(magic: u32, cmd_id: u8, cmd_size: u8, transfer_len: u32, args: [u8; 16]) -> Self {
        assert!(
            magic != PICOBOOT_MAGIC,
            "magic value cannot be the same as PICOBOOT_MAGIC"
        );
        PicobootXCmd {
            magic,
            token: 0,
            cmd_id,
            cmd_size,
            _unused: 0,
            transfer_len,
            args,
        }
    }

    pub(crate) fn set_token(mut self, token: u32) -> Self {
        self.token = token;
        self
    }

    /// Returns the magic value of this command.
    pub fn get_magic(&self) -> u32 {
        self.magic
    }

    // Returns the token of this command.
    pub fn get_cmd_id(&self) -> u8 {
        self.cmd_id
    }

    /// Returns the command size of this command.
    pub fn get_cmd_size(&self) -> u8 {
        self.cmd_size
    }

    /// Returns the transfer length of this command.
    pub fn get_transfer_len(&self) -> usize {
        self.transfer_len as usize
    }

    /// Returns the args of this command.
    pub fn get_args(&self) -> &[u8] {
        &self.args[..self.cmd_size as usize]
    }

    /// Returns the direction of this command.
    pub fn direction(&self) -> Direction {
        if self.cmd_id & 0x80 != 0 {
            Direction::In
        } else {
            Direction::Out
        }
    }

    /// Returns true if this command is a data transfer command (that is, it
    /// expects data to be transferred over the bulk endpoints), false
    /// otherwise.
    pub fn is_data_transfer(&self) -> bool {
        self.transfer_len != 0
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use deku::DekuContainerRead;

    /// Build a status packet carrying `code`, as the device would send it.
    fn status_with(code: u32) -> PicobootStatusCmd {
        let mut bytes = [0u8; 16];
        bytes[4..8].copy_from_slice(&code.to_le_bytes());
        PicobootStatusCmd::from_bytes((&bytes, 0)).unwrap().1
    }

    /// A status code this crate has no name for must not panic. It arrives from
    /// a device over the wire, so it cannot be assumed well formed.
    #[test]
    fn unknown_status_code_does_not_panic() {
        for code in [18u32, 99, 0xFFFF_FFFF] {
            let status = status_with(code);
            assert_eq!(status.get_status_code(), PicobootStatus::UnknownError);
            assert_eq!(status.try_status_code(), None);
            assert_eq!(status.raw_status_code(), code);
            assert!(!status.is_ok());
        }
    }

    #[test]
    fn known_status_codes_round_trip() {
        let ok = status_with(PicobootStatus::Ok as u32);
        assert!(ok.is_ok());
        assert_eq!(ok.try_status_code(), Some(PicobootStatus::Ok));

        let refused = status_with(PicobootStatus::NotPermitted as u32);
        assert!(!refused.is_ok());
        assert_eq!(refused.get_status_code(), PicobootStatus::NotPermitted);
        assert_eq!(refused.raw_status_code(), 10);
    }
}
