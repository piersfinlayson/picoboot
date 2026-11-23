// Copyright (C) 2025 Piers Finlayson <piers@piers.rocks>
//
// MIT License

//! Flashes a binary file (not a UF2 file) to a Raspberry Pi Pico/RP2040/RP2350
//! using PICOBOOT.
//!
//! Based on an original example from `picoboot-rs`.

use picoboot::{Access, Error, Picoboot};
use picoboot_nusb::UsbDevice;
use std::path::Path;

// Delay to use when rebooting the device
const REBOOT_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Get a single argument - the binary file to flash
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: {} <firmware binary file>", args[0]);
        std::process::exit(1);
    }
    let firmware_file = &args[1];

    // Check file exists
    let firmware_path = Path::new(firmware_file);
    if !firmware_path.exists() {
        eprintln!("Firmware file '{}' does not exist", firmware_file);
        std::process::exit(1);
    }

    match run(firmware_path).await {
        Ok(_) => println!("Completed successfully"),
        Err(e) => eprintln!("Hit error: {}", e),
    }
}

async fn run(firmware_path: &Path) -> Result<(), Error> {
    // Create Picoboot object - this will find the first connected device with
    // an RP2040 or RP2350 stock VID/PID in BOOTSEL mode
    println!("Discovering device...");
    let mut picoboot = Picoboot::new(UsbDevice::first(None).await?)?;

    // Connect to the device
    println!("Connecting to device...");
    let conn = picoboot.connect().await?;
    println!("Connected to: {}", conn.target());

    // Reset the interface
    //println!("Resetting interface...");
    //conn.reset_interface().await?;

    // Claim exclusive access, ejecting the BOOTSEL mass storage device
    println!("Setting exclusive access...");
    conn.set_exclusive_access(Access::ExclusiveAndEject).await?;

    // Exit from XIP mode, before flashing (not required on RP2350)
    println!("Exiting XIP mode...");
    conn.exit_xip().await?;

    // Read in the firmware binary
    println!("Reading firmware binary...");
    let fw = std::fs::read(firmware_path).expect("Failed to read firmware file");

    // Erase sufficient space in flash for the firmware.  We could erase a
    // sector at a time, for example, if we wanted to show progress.  Note
    // that this function rounds up to erase whole sectors automatically.
    println!("Erasing {} bytes of flash...", fw.len());
    conn.flash_erase_start(fw.len()).await?;

    // We could flash a page at a time, for example, if we wanted to show
    // progress.  This will pad up to the nearest flash page size with 0x0.
    println!("Flashing firmware...");
    conn.flash_write_start(&fw).await?;

    // Reboot the device to start firmware
    conn.reboot(REBOOT_DELAY).await
}
