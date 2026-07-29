// Copyright (C) 2026 Piers Finlayson <piers@piers.rocks>
//
// MIT License

#![cfg(feature = "reader")]

use crate::Picoboot;
use airfrog_rpc::io::Reader;
use log::debug;

/// A reader that reads from a live device via the PICOBOOT protocol.
///
/// Passes absolute addresses directly to PICOBOOT, so no base address
/// offset arithmetic is required.
pub struct PicobootReader {
    picoboot: Picoboot,
}

impl PicobootReader {
    /// Create a new PicobootReader, connecting to the device and resetting
    /// the interface ready for use.
    pub async fn new(mut picoboot: Picoboot) -> Result<Self, String> {
        let conn = picoboot.connect().await.map_err(|e| e.to_string())?;
        conn.reset_interface().await.map_err(|e| e.to_string())?;
        match conn.get_command_status().await {
            Ok(status) => debug!("PicobootReader: command status after reset: {:?}", status),
            Err(e) => debug!("PicobootReader: failed to get command status after reset: {e}"),
        }
        Ok(Self { picoboot })
    }
}

impl Reader for PicobootReader {
    type Error = String;

    async fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        debug!("Reading {} bytes from address 0x{addr:08x}", buf.len());
        let conn = self
            .picoboot
            .connection()
            .ok_or_else(|| "Not connected".to_string())?;

        // For flash addresses, align reads to 256-byte page boundaries
        let (read_addr, read_size, copy_offset) = if (0x1000_0000..=0x1FFF_FFFF).contains(&addr) {
            let page_start = addr & !0xFF;
            let offset_in_page = (addr - page_start) as usize;
            let total_needed = offset_in_page + buf.len();
            let aligned_size = (total_needed + 255) & !255;
            (page_start, aligned_size as u32, offset_in_page)
        } else {
            (addr, buf.len() as u32, 0)
        };

        match conn.read(read_addr, read_size).await {
            Ok(data) => {
                buf.copy_from_slice(&data[copy_offset..copy_offset + buf.len()]);
                Ok(())
            }
            Err(e) => {
                debug!("Error reading from device: {e}");
                conn.reset_interface().await.map_err(|e| e.to_string())?;
                Err(e.to_string())
            }
        }
    }

    fn update_base_address(&mut self, _new_base: u32) {
        // no-op: PICOBOOT reads from absolute addresses directly
    }
}
