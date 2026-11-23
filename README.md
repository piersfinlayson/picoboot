# picoboot-rs

A crate for connecting to and communicating with RP2040/RP2350 microcontrollers using the PICOBOOT interface.

For USB, use [picoboot-nusb](./picoboot-nusb).

picoboot-rs now supports the [picobootx](https://github.com/piersfinlayson/picobootx) picoboot extension library, which is a device side picoboot implementation that allows custom commands to be added to the picoboot protocol.

## Getting Started

1. Find a device, for example with `picoboot_nusb::UsbDevice`, and pass it to `Picoboot::new()`.

2. Use `Picoboot::connect()` to connect to the device and get a `Connection`

3. Use the `Connection` to interact with the device, such as reading/writing/erasing flash memory.

## Example

```rust
use picoboot::{Picoboot, Access, Error};
use picoboot_nusb::UsbDevice;

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Create connection to first RP2040/RP2350 found
    let mut picoboot = Picoboot::new(UsbDevice::first(None).await?)?;
    let conn = picoboot.connect().await?;

    // Claim exclusive access, ejecting the BOOTSEL mass storage device and
    // exit XIP mode (only necessary on RP2040)
    conn.set_exclusive_access(Access::ExclusiveAndEject).await?;
    conn.exit_xip().await?;

    // Erase first 4096 bytes of flash (1 sector)
    conn.flash_erase_start(4096).await?;

    // Write 256 bytes of data to start of flash (1 page)
    conn.flash_write_start(&[0u8; 256]).await?;

    // Retrieve 256 bytes of data from start of flash
    let data = conn.flash_read_start(256).await?;

    Ok(())
}
```

See the [examples](./examples) directory for more complete examples.

## Notes

When using picoboot-nusb it is possible that further configuration for USB devices on the host machine may be required.

- When running on Linux , you may need to add some additional udev rules to allow your user to access the PICOBOOT USB . These udev rules can be found [here](https://github.com/raspberrypi/picotool/blob/master/udev/60-picotool.rules) unless they have been moved!

- When running on Windows, you shouldn't need to do anything further, as the RP2040/RP2350 include WCID support.  However, if you hit problems, install WinUSB for the device using [Zadig](https://zadig.akeo.ie/). Plug in the Pico device while holding the BOOTSEL button, and install any of the listed drivers for the RP2 Boot device in Zadig.

- macOS tends to need no further configuration.

## License

`picoboot` is dual-licensed under the MIT OR Apache 2.0 Licenses.

You can choose either the MIT license or the Apache 2.0 license when you re-use this code.

See [`LICENSE-MIT`](./LICENSE-MIT) and [`LICENSE-APACHE`](./LICENSE-APACHE) for more information on each specific license.

The Apache 2.0 notices can be found in [`NOTICE`](./NOTICE).

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

## Acknowledgements

This crate was based on [`picoboot-rs`](https://crates.io/crates/picoboot-rs) by Hickok-Dickson
