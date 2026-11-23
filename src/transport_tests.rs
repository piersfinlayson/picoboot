// Copyright (C) 2026 Piers Finlayson <piers@piers.rocks>
//
// MIT License

//! `Picoboot` and `Connection` against a fake transport.

use std::fmt;
use std::sync::{Arc, Mutex};

use crate::cmd::{PicobootCmdId, PicobootStatusCmd};
use crate::{
    BoxFuture, Direction, Error, Picoboot, Target, Timeouts, Transaction, Transport,
    TransportError, TransportErrorKind,
};

/// One transaction as the fake saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    cmd_id: u8,
    transfer_len: usize,
    out_len: usize,
    /// The command's arguments.
    args: [u8; 16],
}

#[derive(Debug, Default)]
struct Log {
    opens: usize,
    closes: usize,
    open_targets: Vec<Target>,
    seen: Vec<Seen>,
}

#[derive(Debug, Clone)]
struct Fake {
    target: Option<Target>,
    max_transfer: Option<usize>,
    // IN data is this many bytes short.
    short_by: usize,
    fail_with: Option<TransportErrorKind>,
    log: Arc<Mutex<Log>>,
}

impl Fake {
    fn new() -> Self {
        Self {
            target: Some(Target::Rp2350),
            max_transfer: None,
            short_by: 0,
            fail_with: None,
            log: Arc::default(),
        }
    }

    fn log(&self) -> std::sync::MutexGuard<'_, Log> {
        self.log.lock().unwrap()
    }
}

#[derive(Debug)]
struct FakeError;

impl fmt::Display for FakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fake failure")
    }
}

impl std::error::Error for FakeError {}

impl fmt::Display for Fake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fake")
    }
}

impl Transport for Fake {
    fn target(&self) -> Option<Target> {
        self.target.clone()
    }

    fn open<'a>(
        &'a self,
        target: &'a Target,
        _timeouts: &'a Timeouts,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let mut log = self.log();
            log.opens += 1;
            log.open_targets.push(target.clone());
            Ok(())
        })
    }

    fn close(&self) {
        self.log().closes += 1;
    }

    fn command<'a>(
        &'a self,
        transaction: Transaction<'a>,
        _timeouts: &'a Timeouts,
    ) -> BoxFuture<'a, Result<Vec<u8>, Error>> {
        Box::pin(async move {
            if let Some(kind) = self.fail_with {
                return Err(Error::Transport(TransportError::new(kind, FakeError)));
            }
            let p = transaction.packet;
            self.log().seen.push(Seen {
                cmd_id: p[8],
                transfer_len: u32::from_le_bytes([p[12], p[13], p[14], p[15]]) as usize,
                out_len: transaction.out.len(),
                args: p[16..].try_into().unwrap(),
            });
            match transaction.direction {
                Direction::In => Ok(vec![0xA5; transaction.transfer_len - self.short_by]),
                Direction::Out => Ok(Vec::new()),
            }
        })
    }

    fn command_status<'a>(
        &'a self,
        _timeouts: &'a Timeouts,
    ) -> BoxFuture<'a, Result<PicobootStatusCmd, Error>> {
        Box::pin(async { Ok(PicobootStatusCmd::from([0u8; 16])) })
    }

    fn reset<'a>(&'a self, _timeouts: &'a Timeouts) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }

    fn max_transfer(&self) -> Option<usize> {
        self.max_transfer
    }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(f)
}

const WRITE: u8 = PicobootCmdId::Write as u8;
const READ: u8 = PicobootCmdId::Read as u8;

#[test]
fn new_fails_without_a_target() {
    let fake = Fake {
        target: None,
        ..Fake::new()
    };
    let e = Picoboot::new(fake).unwrap_err();
    assert!(
        matches!(&e, Error::PicobootTargetUnknown(s) if s == "fake"),
        "{e:?}"
    );
}

#[test]
fn with_target_replaces_the_transports() {
    let fake = Fake::new();
    let custom = Target::Custom {
        vid: 0x1234,
        pid: 0x5678,
    };
    let mut picoboot = Picoboot::with_target(fake.clone(), custom.clone());
    assert_eq!(picoboot.target(), &custom);
    block_on(picoboot.connect()).unwrap();
    assert_eq!(fake.log().open_targets, vec![custom]);
}

#[test]
fn info_is_the_transports_display() {
    assert_eq!(Picoboot::new(Fake::new()).unwrap().info(), "fake");
}

/// Copies of a connected `Picoboot` share one open, closed when the last copy
/// is dropped.
#[test]
fn copies_share_one_open() {
    let fake = Fake::new();
    let mut picoboot = Picoboot::new(fake.clone()).unwrap();
    block_on(picoboot.connect()).unwrap();
    let mut copy = picoboot.clone();
    let conn = picoboot.connection().unwrap().clone();
    assert_eq!(fake.log().opens, 1);

    picoboot.disconnect();
    assert_eq!(fake.log().closes, 0);
    copy.disconnect();
    assert_eq!(fake.log().closes, 0);
    drop(conn);
    assert_eq!(fake.log().closes, 1);
}

/// Two copies of a closed `Picoboot` that connect each open the transport,
/// which counts them.
#[test]
fn each_connect_of_a_closed_copy_opens() {
    let fake = Fake::new();
    let mut a = Picoboot::new(fake.clone()).unwrap();
    let mut b = a.clone();
    block_on(a.connect()).unwrap();
    block_on(b.connect()).unwrap();
    assert_eq!(fake.log().opens, 2);
    drop(a);
    drop(b);
    assert_eq!(fake.log().closes, 2);
}

/// A WRITE is a flash sector where the transport allows it.
#[test]
fn a_write_is_a_sector() {
    let fake = Fake::new();
    let mut picoboot = Picoboot::new(fake.clone()).unwrap();
    let conn = block_on(picoboot.connect()).unwrap();
    block_on(conn.flash_write_start(&[0u8; 8192 + 256])).unwrap();
    let writes: Vec<_> = fake
        .log()
        .seen
        .iter()
        .filter(|s| s.cmd_id == WRITE)
        .map(|s| (s.transfer_len, s.out_len))
        .collect();
    assert_eq!(writes, vec![(4096, 4096), (4096, 4096), (256, 256)]);
}

/// A transport limit below a sector makes each WRITE that limit in whole
/// pages.
#[test]
fn a_write_fits_the_transports_limit() {
    let fake = Fake {
        max_transfer: Some(1000),
        ..Fake::new()
    };
    let mut picoboot = Picoboot::new(fake.clone()).unwrap();
    let conn = block_on(picoboot.connect()).unwrap();
    block_on(conn.flash_write_start(&[0u8; 2048])).unwrap();
    let lens: Vec<_> = fake.log().seen.iter().map(|s| s.transfer_len).collect();
    assert_eq!(lens, vec![768, 768, 512]);
}

#[test]
fn a_read_is_split_to_fit_the_transports_limit() {
    let fake = Fake {
        max_transfer: Some(4096),
        ..Fake::new()
    };
    let mut picoboot = Picoboot::new(fake.clone()).unwrap();
    let conn = block_on(picoboot.connect()).unwrap();
    let data = block_on(conn.flash_read_start(10240)).unwrap();
    assert_eq!(data.len(), 10240);
    let reads: Vec<_> = fake
        .log()
        .seen
        .iter()
        .filter(|s| s.cmd_id == READ)
        .map(|s| s.transfer_len)
        .collect();
    assert_eq!(reads, vec![4096, 4096, 2048]);
}

#[test]
fn short_in_data_is_an_error() {
    let fake = Fake {
        short_by: 4,
        ..Fake::new()
    };
    let mut picoboot = Picoboot::new(fake).unwrap();
    let conn = block_on(picoboot.connect()).unwrap();
    let e = block_on(conn.flash_read_start(256)).unwrap_err();
    assert!(
        matches!(e, Error::PicobootInDataLength(Target::Rp2350, 256, 252)),
        "{e:?}"
    );
}

/// A transport's failure reaches the caller with its kind and source.
#[test]
fn a_transport_failure_keeps_its_kind() {
    let fake = Fake {
        fail_with: Some(TransportErrorKind::AfterSend),
        ..Fake::new()
    };
    let mut picoboot = Picoboot::new(fake).unwrap();
    let conn = block_on(picoboot.connect()).unwrap();
    let e = block_on(conn.exit_xip()).unwrap_err();
    let Error::Transport(t) = &e else {
        panic!("{e:?}")
    };
    assert_eq!(t.kind(), TransportErrorKind::AfterSend);
    assert_eq!(e.to_string(), "fake failure");
    let source = std::error::Error::source(t).unwrap();
    assert!(source.downcast_ref::<FakeError>().is_some());
}

/// An OUT command without its data fails before anything is sent.
#[test]
fn an_out_command_without_data_sends_nothing() {
    let fake = Fake::new();
    let mut picoboot = Picoboot::new(fake.clone()).unwrap();
    let conn = block_on(picoboot.connect()).unwrap();
    let cmd = crate::PicobootCmd::flash_write(crate::FLASH_START, 256);
    let e = block_on(conn.send_cmd(cmd, None)).unwrap_err();
    assert!(matches!(e, Error::PicobootCmdDataMissing(..)), "{e:?}");
    assert!(fake.log().seen.is_empty());
}

/// REBOOT2's dFlags, then dDelayMS, dParam0 and dParam1 (RP2350 datasheet
/// section 5.6.4): REBOOT_TYPE_RAM_IMAGE with the image's start and size.
#[test]
fn a_ram_image_reboot_passes_the_start_and_size() {
    use crate::RebootType;
    let fake = Fake::new();
    let mut picoboot = Picoboot::new(fake.clone()).unwrap();
    let ram_image = RebootType::RamImage {
        start: 0x2000_0000,
        size: 0x1_0000,
    };
    block_on(picoboot.reboot(ram_image.clone(), std::time::Duration::from_millis(10))).unwrap();
    let seen = fake.log().seen.clone();
    let reboot = seen.last().unwrap();
    assert_eq!(reboot.cmd_id, PicobootCmdId::Reboot2 as u8);
    let word = |i: usize| u32::from_le_bytes(reboot.args[i * 4..i * 4 + 4].try_into().unwrap());
    assert_eq!(
        [word(0), word(1), word(2), word(3)],
        [3, 10, 0x2000_0000, 0x1_0000]
    );

    let mut rp2040 = Picoboot::with_target(Fake::new(), Target::Rp2040);
    let e = block_on(rp2040.reboot(ram_image, std::time::Duration::ZERO)).unwrap_err();
    assert!(
        matches!(e, Error::PicobootCmdNotAllowedForTarget(..)),
        "{e:?}"
    );
}

const FLASH_ERASE: u8 = PicobootCmdId::FlashErase as u8;

/// The (address, size) of each FLASH_ERASE sent to a fresh fake, for an erase
/// of `size` bytes from `addr`.
fn erase(addr: u32, size: u32) -> Vec<(u32, u32)> {
    let fake = Fake::new();
    let mut picoboot = Picoboot::new(fake.clone()).unwrap();
    block_on(async {
        picoboot.connect().await.unwrap();
        picoboot.flash_erase(addr, size).await.unwrap();
    });
    let log = fake.log();
    log.seen
        .iter()
        .filter(|s| s.cmd_id == FLASH_ERASE)
        .map(|s| {
            let word = |i: usize| u32::from_le_bytes(s.args[i..i + 4].try_into().unwrap());
            (word(0), word(4))
        })
        .collect()
}

/// Each FLASH_ERASE ends on a 64 KB boundary, so a failure shows within one
/// command's timeout.
#[test]
fn an_erase_is_one_command_for_each_64_kb_block() {
    assert_eq!(erase(0x1000_0000, 0x1000), [(0x1000_0000, 0x1000)]);
    assert_eq!(erase(0x1000_0000, 0x1_0000), [(0x1000_0000, 0x1_0000)]);
    assert_eq!(
        erase(0x1000_0000, 0x2_0000),
        [(0x1000_0000, 0x1_0000), (0x1001_0000, 0x1_0000)]
    );
    assert_eq!(erase(0x1000_0000, 0x20_0000).len(), 32);
}

#[test]
fn the_last_block_of_an_erase_can_be_short() {
    assert_eq!(
        erase(0x1000_0000, 0x2_1000),
        [
            (0x1000_0000, 0x1_0000),
            (0x1001_0000, 0x1_0000),
            (0x1002_0000, 0x1000),
        ]
    );
}

#[test]
fn the_first_block_of_an_erase_can_start_inside_a_block() {
    assert_eq!(
        erase(0x1000_F000, 0x2_1000),
        [
            (0x1000_F000, 0x1000),
            (0x1001_0000, 0x1_0000),
            (0x1002_0000, 0x1_0000),
        ]
    );
    assert_eq!(erase(0x1000_3000, 0x2000), [(0x1000_3000, 0x2000)]);
}

#[test]
fn an_erase_of_no_bytes_is_one_command() {
    assert_eq!(erase(0x1000_0000, 0), [(0x1000_0000, 0)]);
}

/// A range that ends at 2^32 doesn't overflow.
#[test]
fn an_erase_to_the_top_of_the_address_space_sends_its_blocks() {
    assert_eq!(
        erase(0xFFFE_F000, 0x1_1000),
        [(0xFFFE_F000, 0x1000), (0xFFFF_0000, 0x1_0000)]
    );
}
