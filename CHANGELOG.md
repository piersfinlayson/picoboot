# Changelog

## [0.2.6] - 2026-??-??

`Timeouts::command_status` is now honoured.  `get_command_status()` hard-coded
a 1s timeout and ignored the field, so a caller raising it had no effect.  The
default is unchanged at 1s.

Dropped the note on `Timeouts` saying Windows ignores these timeouts.  nusb
0.2.4 implemented control transfer timeouts on Windows, and this crate has
required `nusb 0.2.4` since picoboot 0.2.4.

Fixed clippy warning on linux hosts.

## [0.2.5] - 2026-07-29

`PicobootStatusCmd::get_status_code()` no longer panics on a status code this crate has no name for.  The code arrives from a device over the wire, so a host cannot assume it is well formed; an unrecognised one is now reported as `PicobootStatus::UnknownError`.  `is_ok()` went through the same `unwrap()` and so could panic too; it now compares the raw code.

Adds `PicobootStatusCmd::try_status_code()`, returning `Option<PicobootStatus>` so a caller can tell a genuine `UnknownError` from a code with no name, and `raw_status_code()`, returning the value exactly as the device sent it.  `PicobootStatus` gains `PartialEq`/`Eq`.  All additive; no existing signature changed.

CI now runs `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings`, and the crate was made clean under both.  The clippy fixes are internal only, bar some doc comment indentation that was causing list continuations to render as separate paragraphs.

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