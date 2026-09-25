// Copyright (C) 2026 Piers Finlayson <piers@piers.rocks>
//
// MIT License

//! Reads and writes RP2350 OTP rows through PICOBOOT.  An OTP write cannot be
//! undone.
//!
//! ```text
//! otp list
//! otp SERIAL read-ecc ROW COUNT
//! otp SERIAL read-raw ROW COUNT
//! otp SERIAL write-ecc ROW VALUE...
//! otp SERIAL write-raw ROW VALUE...
//! otp SERIAL test-writes ROW
//! otp SERIAL lock PAGE
//! otp SERIAL test-lock PAGE
//! ```
//!
//! - `list` shows every USB device, including a board whose VID and PID were
//!   changed in OTP.
//! - `read-ecc` and `read-raw` read `COUNT` rows starting at `ROW`.
//! - `write-ecc` and `write-raw` write each `VALUE` to its own row, starting at
//!   `ROW`.
//! - `test-writes` checks the chip's write rules on 7 blank rows starting at
//!   `ROW`.
//! - `lock` makes `PAGE` read-only from the board's next reset.
//! - `test-lock` checks that `PAGE` and its lock word refuse writes.  Reboot the
//!   board into the bootloader between `lock` and `test-lock`.
//!
//! Every command except `list`:
//! - takes the board's USB serial number, so it never reaches another board
//! - refuses a device that doesn't have a PICOBOOT interface
//! - prints the time it took
//! - prints the device's reason for any refusal
//!
//! The test commands are for a scratch board.  They only use pages 3 to 60,
//! which are free for users.  `lock` and `test-lock` need an RP2350 A3 or
//! later.  On an A2, erratum RP2350-E15 protects each lock word with page 62's
//! or page 63's lock instead of its own page's.
//!
//! Numbers are decimal, or hex with a `0x` prefix.  Raw values are 24 bits.

use picoboot::cmd::PicobootStatus;
use picoboot::{Connection, Error, Picoboot};
use std::ops::RangeInclusive;
use std::time::Instant;

/// OTP pages free for users.
const USER_PAGES: RangeInclusive<u16> = 3..=60;
const ROWS_PER_PAGE: u16 = 64;

/// Rows `test-writes` uses.
const TEST_WRITES_ROWS: u16 = 7;

/// Row of page 0's second lock word.  Page n's is `2 * n` rows on.
const PAGE0_LOCK1_ROW: u16 = 0xf81;

/// A second lock word making its page read-only for Secure, Non-secure and
/// bootloader access, in all three of its copies.
const LOCK1_READ_ONLY: u32 = 0x15_1515;

/// ECC values for the overwrite tests.  The chip can't store `CLASH` over
/// `FIRST` as it is or inverted.  `FIRST` has a bit that `CLASH` lacks, and
/// they share a set bit.
const FIRST: u16 = 0x1234;
const CLASH: u16 = 0x5678;

enum Request<'a> {
    List,
    Otp(&'a str, Command),
}

enum Command {
    Op(Op),
    TestWrites { row: u16 },
    Lock { page: u16 },
    TestLock { page: u16 },
}

enum Op {
    ReadEcc { row: u16, count: u16 },
    ReadRaw { row: u16, count: u16 },
    WriteEcc { row: u16, values: Vec<u16> },
    WriteRaw { row: u16, values: Vec<u32> },
}

/// The result a test expects from a write.
#[derive(Debug, Clone, Copy)]
enum Expect {
    Accepted,
    Refused(PicobootStatus),
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().collect();
    let request = match parse(&args[1..]) {
        Ok(request) => request,
        Err(e) => {
            eprintln!("{e}");
            eprintln!("Usage: {} list", args[0]);
            eprintln!("       {} SERIAL read-ecc|read-raw ROW COUNT", args[0]);
            eprintln!("       {} SERIAL write-ecc|write-raw ROW VALUE...", args[0]);
            eprintln!("       {} SERIAL test-writes ROW", args[0]);
            eprintln!("       {} SERIAL lock|test-lock PAGE", args[0]);
            std::process::exit(1);
        }
    };

    let result = match request {
        Request::List => list().await,
        Request::Otp(serial, command) => run(serial, command).await,
    };
    if let Err(e) = result {
        eprintln!("Hit error: {e}");
        std::process::exit(1);
    }
}

fn parse(args: &[String]) -> Result<Request<'_>, String> {
    if let [list] = args
        && list == "list"
    {
        return Ok(Request::List);
    }
    let [serial, command, arg, rest @ ..] = args else {
        return Err("Too few arguments".to_string());
    };

    let command = match command.as_str() {
        "read-ecc" | "read-raw" => {
            let row = parse_row(arg)?;
            let [count] = rest else {
                return Err("A read takes a row and a count".to_string());
            };
            let count = number(count, 0xffff).ok_or(format!("Bad count: {count}"))? as u16;
            if command == "read-ecc" {
                Command::Op(Op::ReadEcc { row, count })
            } else {
                Command::Op(Op::ReadRaw { row, count })
            }
        }
        "write-ecc" | "write-raw" => {
            let row = parse_row(arg)?;
            if rest.is_empty() {
                return Err("A write takes a row and at least one value".to_string());
            }
            let max = if command == "write-ecc" {
                0xffff
            } else {
                0xff_ffff
            };
            let values = rest
                .iter()
                .map(|v| number(v, max).ok_or(format!("Bad value: {v}")))
                .collect::<Result<Vec<u32>, String>>()?;
            if command == "write-ecc" {
                let values = values.into_iter().map(|v| v as u16).collect();
                Command::Op(Op::WriteEcc { row, values })
            } else {
                Command::Op(Op::WriteRaw { row, values })
            }
        }
        "test-writes" => {
            no_more(rest)?;
            let row = parse_row(arg)?;
            let first = USER_PAGES.start() * ROWS_PER_PAGE;
            let last = (USER_PAGES.end() + 1) * ROWS_PER_PAGE - 1;
            if row < first || row > last - (TEST_WRITES_ROWS - 1) {
                return Err(format!(
                    "test-writes needs {TEST_WRITES_ROWS} rows between {first:#05x} and {last:#05x}"
                ));
            }
            Command::TestWrites { row }
        }
        "lock" | "test-lock" => {
            no_more(rest)?;
            let page = number(arg, 0xffff)
                .map(|p| p as u16)
                .filter(|p| USER_PAGES.contains(p))
                .ok_or(format!(
                    "Page must be {} to {}: {arg}",
                    USER_PAGES.start(),
                    USER_PAGES.end()
                ))?;
            if command == "lock" {
                Command::Lock { page }
            } else {
                Command::TestLock { page }
            }
        }
        _ => return Err(format!("Unknown command: {command}")),
    };

    Ok(Request::Otp(serial, command))
}

fn parse_row(s: &str) -> Result<u16, String> {
    number(s, 0xffff)
        .map(|n| n as u16)
        .ok_or(format!("Bad row: {s}"))
}

fn no_more(rest: &[String]) -> Result<(), String> {
    if rest.is_empty() {
        Ok(())
    } else {
        Err("Too many arguments".to_string())
    }
}

/// Parses a decimal or `0x` hex number no larger than `max`.
fn number(s: &str, max: u32) -> Option<u32> {
    let n = match s.strip_prefix("0x") {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => s.parse().ok()?,
    };
    (n <= max).then_some(n)
}

/// Every USB device, whatever its VID and PID.
async fn usb_devices() -> Result<Vec<nusb::DeviceInfo>, String> {
    Ok(nusb::list_devices()
        .await
        .map_err(|e| format!("Listing USB devices: {e}"))?
        .collect())
}

async fn list() -> Result<(), String> {
    let devices = usb_devices().await?;
    if devices.is_empty() {
        println!("Didn't find any USB devices");
    }
    for d in devices {
        println!(
            "{:04x}:{:04x}  {}  {}",
            d.vendor_id(),
            d.product_id(),
            d.serial_number().unwrap_or("-"),
            d.product_string().unwrap_or("-")
        );
    }
    Ok(())
}

async fn run(serial: &str, command: Command) -> Result<(), String> {
    let Some(device) = usb_devices()
        .await?
        .into_iter()
        .find(|d| d.serial_number() == Some(serial))
    else {
        return Err(format!(
            "Didn't find a USB device with serial number {serial}"
        ));
    };

    let mut picoboot = Picoboot::new(device).await.map_err(|e| e.to_string())?;
    let conn = picoboot.connect().await.map_err(|e| e.to_string())?;
    conn.reset_interface().await.map_err(|e| e.to_string())?;
    println!("Connected to {} {serial}", conn.target());

    match command {
        Command::Op(op) => run_op(conn, op).await,
        Command::TestWrites { row } => test_writes(conn, row).await,
        Command::Lock { page } => lock(conn, page).await,
        Command::TestLock { page } => test_lock(conn, page).await,
    }
}

async fn run_op(conn: &mut Connection, op: Op) -> Result<(), String> {
    let start = Instant::now();
    let result = match &op {
        Op::ReadEcc { row, count } => conn
            .otp_read_ecc(*row, *count)
            .await
            .map(|values| rows(*row, values.iter().map(|v| format!("{v:#06x}")))),
        Op::ReadRaw { row, count } => conn
            .otp_read_raw(*row, *count)
            .await
            .map(|values| rows(*row, values.iter().map(|v| format!("{v:#08x}")))),
        Op::WriteEcc { row, values } => conn
            .otp_write_ecc(*row, values)
            .await
            .map(|()| vec![format!("Wrote {} rows from row {row:#05x}", values.len())]),
        Op::WriteRaw { row, values } => conn
            .otp_write_raw(*row, values)
            .await
            .map(|()| vec![format!("Wrote {} rows from row {row:#05x}", values.len())]),
    };
    let took = format!("Took {:.1} ms", start.elapsed().as_secs_f64() * 1000.0);

    match result {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
            println!("{took}");
            Ok(())
        }
        Err(e) => {
            println!("{took}");
            // A refusal arrives as a stalled transfer.  The reason is in the
            // command status, which the next interface reset clears.
            if matches!(
                e,
                Error::UsbReadBulkFailure(..) | Error::UsbWriteBulkFailure(..)
            ) {
                match conn.get_command_status().await {
                    Ok(status) => eprintln!(
                        "Device status: {} ({})",
                        status
                            .try_status_code()
                            .map_or("unknown".to_string(), |s| format!("{s:?}")),
                        status.raw_status_code()
                    ),
                    Err(se) => eprintln!("Could not read the device status: {se}"),
                }
            }
            Err(e.to_string())
        }
    }
}

async fn test_writes(conn: &mut Connection, row: u16) -> Result<(), String> {
    let blank = read_raw(conn, row, TEST_WRITES_ROWS).await?;
    if let Some(i) = blank.iter().position(|&v| v != 0) {
        return Err(format!(
            "Row {:#05x} isn't blank: {:#08x}",
            row + i as u16,
            blank[i]
        ));
    }
    println!(
        "Rows {row:#05x} to {:#05x} are blank",
        row + TEST_WRITES_ROWS - 1
    );

    // An ECC write, an overwrite the chip can't store, then a write of 0.
    let a = row;
    ecc_write(conn, a, &[FIRST], Expect::Accepted).await?;
    expect_ecc(conn, a, &[FIRST], &[ecc_row(FIRST)]).await?;
    let refused = Expect::Refused(PicobootStatus::UnsupportedModification);
    ecc_write(conn, a, &[CLASH], refused).await?;
    expect_ecc(conn, a, &[FIRST], &[ecc_row(FIRST)]).await?;
    ecc_write(conn, a, &[0], Expect::Accepted).await?;
    expect_ecc(conn, a, &[0], &[0xff_ffff]).await?;

    // An ECC row written raw with all ones.
    let b = row + 1;
    ecc_write(conn, b, &[0xabcd], Expect::Accepted).await?;
    expect_ecc(conn, b, &[0xabcd], &[ecc_row(0xabcd)]).await?;
    raw_write(conn, b, &[0xff_ffff], Expect::Accepted).await?;
    expect_ecc(conn, b, &[0], &[0xff_ffff]).await?;

    // Raw bits added one write at a time, then a refused write that would clear
    // one.
    let c = row + 2;
    raw_write(conn, c, &[0x1], Expect::Accepted).await?;
    expect_raw(conn, c, &[0x1]).await?;
    raw_write(conn, c, &[0x3], Expect::Accepted).await?;
    expect_raw(conn, c, &[0x3]).await?;
    raw_write(conn, c, &[0x2], refused).await?;
    expect_raw(conn, c, &[0x3]).await?;

    // A 4-row ECC write refused at its third row, which already holds FIRST.
    let d = row + 3;
    ecc_write(conn, d + 2, &[FIRST], Expect::Accepted).await?;
    expect_ecc(conn, d + 2, &[FIRST], &[ecc_row(FIRST)]).await?;
    ecc_write(conn, d, &[1, 2, CLASH, 4], refused).await?;
    expect_ecc(
        conn,
        d,
        &[1, 2, FIRST, 0],
        &[ecc_row(1), ecc_row(2), ecc_row(FIRST), 0],
    )
    .await?;

    println!("Write tests passed");
    Ok(())
}

async fn lock(conn: &mut Connection, page: u16) -> Result<(), String> {
    let lock_row = PAGE0_LOCK1_ROW + 2 * page;
    match read_raw(conn, lock_row, 1).await?[0] {
        0 => {}
        LOCK1_READ_ONLY => {
            println!("Page {page}'s lock word, row {lock_row:#05x}, already makes it read-only");
            return Ok(());
        }
        other => {
            return Err(format!(
                "Page {page}'s lock word, row {lock_row:#05x}, already holds {other:#08x}"
            ));
        }
    }

    raw_write(conn, lock_row, &[LOCK1_READ_ONLY], Expect::Accepted).await?;
    expect_raw(conn, lock_row, &[LOCK1_READ_ONLY]).await?;
    println!(
        "Page {page} is read-only from the next reset.  Reboot the board into the \
         bootloader, then run test-lock {page}."
    );
    Ok(())
}

async fn test_lock(conn: &mut Connection, page: u16) -> Result<(), String> {
    let lock_row = PAGE0_LOCK1_ROW + 2 * page;
    let lock_word = read_raw(conn, lock_row, 1).await?[0];
    if lock_word != LOCK1_READ_ONLY {
        return Err(format!(
            "Page {page}'s lock word, row {lock_row:#05x}, holds {lock_word:#08x}.  Run lock {page} first."
        ));
    }

    let first = page * ROWS_PER_PAGE;
    let page_rows = read_raw(conn, first, ROWS_PER_PAGE).await?;
    println!("Page {page} reads");

    // A write of 0 to a blank row doesn't change it, even if it isn't refused.
    let blank = page_rows.iter().position(|&v| v == 0).ok_or(format!(
        "Page {page} doesn't have a blank row to test a write on"
    ))?;
    let refused = Expect::Refused(PicobootStatus::NotPermitted);
    ecc_write(conn, first + blank as u16, &[0], refused)
        .await
        .map_err(|e| format!("{e}.  Has the board been reset since lock?"))?;

    // Writing the lock word's own value doesn't change a bit, so the write is
    // harmless if the lock fails.
    raw_write(conn, lock_row, &[LOCK1_READ_ONLY], refused).await?;

    println!("Lock tests passed");
    Ok(())
}

/// Writes ECC rows and checks the result against `expect`.
async fn ecc_write(
    conn: &mut Connection,
    row: u16,
    values: &[u16],
    expect: Expect,
) -> Result<(), String> {
    let what = format!("ECC write of {} to {row:#05x}", hex16(values));
    let start = Instant::now();
    let result = conn.otp_write_ecc(row, values).await;
    check_write(conn, &what, start, result, expect).await
}

/// Writes raw rows and checks the result against `expect`.
async fn raw_write(
    conn: &mut Connection,
    row: u16,
    values: &[u32],
    expect: Expect,
) -> Result<(), String> {
    let what = format!("Raw write of {} to {row:#05x}", hex24(values));
    let start = Instant::now();
    let result = conn.otp_write_raw(row, values).await;
    check_write(conn, &what, start, result, expect).await
}

async fn check_write(
    conn: &mut Connection,
    what: &str,
    start: Instant,
    result: Result<(), Error>,
    expect: Expect,
) -> Result<(), String> {
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    let refusal = match result {
        Ok(()) => None,
        // A refusal arrives as a stalled transfer.  The reason is in the
        // command status, which the interface reset then clears.
        Err(Error::UsbReadBulkFailure(..) | Error::UsbWriteBulkFailure(..)) => {
            let status = conn
                .get_command_status()
                .await
                .map_err(|e| format!("{what}: {e}"))?;
            conn.reset_interface()
                .await
                .map_err(|e| format!("{what}: {e}"))?;
            Some((status.try_status_code(), status.raw_status_code()))
        }
        Err(e) => return Err(format!("{what}: {e}")),
    };

    match (refusal, expect) {
        (None, Expect::Accepted) => println!("{what}: accepted in {ms:.1} ms"),
        (Some((Some(status), _)), Expect::Refused(want)) if status == want => {
            println!("{what}: refused with {status:?} in {ms:.1} ms")
        }
        (None, Expect::Refused(want)) => {
            return Err(format!(
                "{what}: accepted, but should have been refused with {want:?}"
            ));
        }
        (Some((status, raw)), expect) => {
            let status = status.map_or("an unknown status".to_string(), |s| format!("{s:?}"));
            let should = match expect {
                Expect::Accepted => "accepted".to_string(),
                Expect::Refused(want) => format!("refused with {want:?}"),
            };
            return Err(format!(
                "{what}: refused with {status} ({raw}), but should have been {should}"
            ));
        }
    }
    Ok(())
}

/// Reads rows with ECC and raw, and checks both.
async fn expect_ecc(
    conn: &mut Connection,
    row: u16,
    ecc: &[u16],
    raw: &[u32],
) -> Result<(), String> {
    let count = ecc.len() as u16;
    let read_ecc = conn
        .otp_read_ecc(row, count)
        .await
        .map_err(|e| format!("ECC read of {row:#05x}: {e}"))?;
    let read = read_raw(conn, row, count).await?;
    if read_ecc != ecc || read != raw {
        return Err(format!(
            "Rows from {row:#05x}: expected ECC {} raw {}, read ECC {} raw {}",
            hex16(ecc),
            hex24(raw),
            hex16(&read_ecc),
            hex24(&read)
        ));
    }
    println!(
        "Rows from {row:#05x} read ECC {} raw {}",
        hex16(ecc),
        hex24(raw)
    );
    Ok(())
}

/// Reads raw rows and checks them.
async fn expect_raw(conn: &mut Connection, row: u16, raw: &[u32]) -> Result<(), String> {
    let read = read_raw(conn, row, raw.len() as u16).await?;
    if read != raw {
        return Err(format!(
            "Rows from {row:#05x}: expected raw {}, read raw {}",
            hex24(raw),
            hex24(&read)
        ));
    }
    println!("Rows from {row:#05x} read raw {}", hex24(raw));
    Ok(())
}

async fn read_raw(conn: &mut Connection, row: u16, count: u16) -> Result<Vec<u32>, String> {
    conn.otp_read_raw(row, count)
        .await
        .map_err(|e| format!("Raw read of {row:#05x}: {e}"))
}

/// The 24-bit row the bootrom writes for an ECC value, before any inversion.
/// The parity masks are the bootrom's `otp_ecc_parity_table`.
fn ecc_row(value: u16) -> u32 {
    const PARITY: [u32; 6] = [
        0b0000001010110101011011,
        0b0000000011011001101101,
        0b0000001100011110001110,
        0b0000000000011111110000,
        0b0000001111100000000000,
        0b0111111111111111111111,
    ];
    let mut row = u32::from(value);
    for (i, mask) in PARITY.iter().enumerate() {
        row |= ((row & mask).count_ones() & 1) << (16 + i);
    }
    row
}

fn hex16(values: &[u16]) -> String {
    values
        .iter()
        .map(|v| format!("{v:#06x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn hex24(values: &[u32]) -> String {
    values
        .iter()
        .map(|v| format!("{v:#08x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// One line per row, starting at `first`.
fn rows(first: u16, values: impl Iterator<Item = String>) -> Vec<String> {
    values
        .enumerate()
        .map(|(i, v)| format!("{:#05x}: {v}", usize::from(first) + i))
        .collect()
}
