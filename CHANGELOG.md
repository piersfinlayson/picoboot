# Changelog

## [0.2.4] - 2026-03-26

Pull in airfrog_rpc 0.2.2.
Up-rev various other crates.

## [0.2.3] - 2026-03-25

Work around RP2350A2 stepping bootrom bug wheere EXIT_XIP is required before a flash erase in BOOTSEL mode: https://github.com/raspberrypi/pico-sdk/issues/2878

## [0.2.2] - 2026-03-24

Modify Picoboot::reboot to use Connection::reboot2 for a custom target.

## [0.2.1] - 2026-03-22

Add some informational APIs to Picoboot object - vid(), pid(), etc.

## [0.2.0]

- Added picobootx support which extends picoboot support with additional command using a different (non RP2040/RP2350) magic.  Works with [picobootx](https://github.com/piersfinlayson/picobootx).
- More sophisticated picoboot interface matching, supporting new One ROM Fire USB stack/picobootx.
- Add an airfrog-rpc::io::Reader implementation for live reading flash and RAM via Picoboot.
- Add Picotboot::reboot convenience function to reboot the device.
- Added read/write functions to support RAM.  The old flash_read and flash_write supported RAM, but added unnecessary alignment and size checks for RAM.
- Allow reboot2 on custom targets.

## [0.1.1]

Added:
- `Picoboot::info` to output VID/PID of device
- Additional `Picoboot` error variants for better error handling

Fixed:
- USB control request types from Class to Vendor

## [0.1.0] - 2025-11-03

Initial release