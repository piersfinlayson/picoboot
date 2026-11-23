// Copyright (C) 2025 Piers Finlayson <piers@piers.rocks>
//
// MIT License

//! Discovers and lists PICOBOOT-compatible devices connected to the system.

use picoboot::Error;
use picoboot_nusb::UsbDevice;

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    match run().await {
        Ok(_) => (),
        Err(e) => eprintln!("Hit error: {}", e),
    }
}

async fn run() -> Result<(), Error> {
    // List all connected PICOBOOT devices
    println!("Discovering devices...");
    let devices = UsbDevice::list(None).await?;

    if devices.is_empty() {
        println!("No PICOBOOT devices found");
        return Ok(());
    } else if devices.len() == 1 {
        println!("Found 1 PICOBOOT device:");
    } else {
        println!("Found {} PICOBOOT device(s):", devices.len());
    }

    for device_info in devices.iter() {
        let product = device_info.product_string().unwrap_or("unknown");
        let serial = device_info.serial_number().unwrap_or("unknown");
        let vid = device_info.vendor_id();
        let pid = device_info.product_id();
        let bus = device_info.bus_id();
        let addr = device_info.device_address();
        println!(
            "- Product '{product}' Serial '{serial}' VID:PID '{vid:04x}:{pid:04x}' Bus:Address '{bus}:{addr}'"
        );
    }

    Ok(())
}
