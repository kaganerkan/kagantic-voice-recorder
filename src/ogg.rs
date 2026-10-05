//! Minimal RFC 3533 / RFC 7845 Ogg container writer.
//!
//! We hand-roll the Ogg framing because the published `ogg` crate's
//! `PacketWriter` resets them every time it is constructed, which makes it
//! unusable for our "one writer across the whole recording" pattern.
//!
//! Stream layout:
//! 1. `OggS` BOS page carrying the OpusHead identification header.
//! 2. `OggS` page carrying the OpusTags comment packet.
//! 3. `OggS` pages carrying audio packets (one packet per page).
//! 4. `OggS` EOS page carrying the final granule position.
//!
//! All pages are checksummed with CRC-32 (polynomial 0x04C11DB7) per the
//! Ogg spec, with multi-byte lacing tables when packets exceed 255 bytes.

use std::fs::File;
use std::io::Write;

/// Ogg CRC-32 lookup table.
///
/// Per RFC 3533 §6.1 and libogg's `_ogg_crc_init`, the Ogg CRC uses
/// polynomial 0x04C11DB7 with an **unreflected** shift-left algorithm,
/// init = 0, no final XOR. (This is *not* the reflected IEEE 802.3 / zlib
/// variant — the polynomial is the same but the algorithm differs.)
const CRC_TABLE: [u32; 256] = generate_crc_table();

const fn generate_crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        // crc = i << 24; for j in 0..8: crc = (crc << 1) ^ ((crc & 0x8000_0000 != 0) ? POLY : 0)
        let mut r = (i as u32) << 24;
        let mut j = 0;
        while j < 8 {
            let high = (r & 0x8000_0000) != 0;
            r <<= 1;
            if high {
                r ^= 0x04C1_1DB7;
            }
            j += 1;
        }
        t[i] = r;
        i += 1;
    }
    t
}

/// Compute the Ogg CRC-32 over `data`, matching libogg's algorithm.
fn ogg_crc(data: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &b in data {
        let idx = ((crc >> 24) ^ b as u32) as usize;
        crc = (crc << 8) ^ CRC_TABLE[idx & 0xFF];
    }
    crc
}

bitflags::bitflags! {
    /// Page-header flag bits (RFC 3533 §5.2).
    struct PageFlags: u8 {
        /// Beginning of stream — set on the first page of the logical stream.
        const BOS = 0x02;
        /// End of stream — set on the final page.
        const EOS = 0x04;
    }
}

/// Long-lived Ogg writer that owns the underlying `File` and tracks a
/// monotonically increasing page sequence number across every packet.
pub struct OggWriter {
    file: File,
    page_seq: u32,
}

impl OggWriter {
    pub fn new(file: File) -> Self {
        Self { file, page_seq: 0 }
    }

    /// Emit a "beginning of stream" page containing `header_packet`.
    pub fn write_bos(&mut self, header_packet: &[u8]) -> std::io::Result<()> {
        self.write_page(header_packet, 0, PageFlags::BOS)
    }

    /// Emit one regular audio page carrying `packet`.
    pub fn write_audio(&mut self, packet: &[u8], granule: u64) -> std::io::Result<()> {
        self.write_page(packet, granule, PageFlags::empty())
    }

    /// Emit the final EOS page carrying `packet` and the last granule.
    pub fn write_eos(&mut self, packet: &[u8], granule: u64) -> std::io::Result<()> {
        self.write_page(packet, granule, PageFlags::EOS)
    }

    /// Flush and recover the inner `File`.
    pub fn finish(mut self) -> std::io::Result<File> {
        self.file.flush()?;
        Ok(self.file)
    }

    fn write_page(&mut self, packet: &[u8], granule: u64, flags: PageFlags) -> std::io::Result<()> {
        // Build the segment lacing table per RFC 3533 §5.1: each entry is a
        // single packet's contribution to this page, split into 255-byte
        // chunks followed by a remainder byte.
        let mut seg_table: Vec<u8> = Vec::new();
        let mut remaining = packet.len();
        while remaining >= 255 {
            seg_table.push(255);
            remaining -= 255;
        }
        seg_table.push(remaining as u8);
        assert!(seg_table.len() <= 255, "packet exceeds Ogg page size limit");

        // Page header layout per RFC 3533 §5.5 (27 bytes):
        //   0..4   capture_pattern       "OggS"
        //   4      stream_structure_ver  0
        //   5      header_type_flag      flags
        //   6..14  granule_position      granule (u64 LE)
        //  14..18  bitstream_serial_no   serial (u32 LE; arbitrary per stream)
        //  18..22  page_sequence_no      page_seq (u32 LE; monotonic)
        //  22..26  CRC                   filled in after computing
        //  26      number_page_segments  (u8)
        let mut header = [0u8; 27];
        header[0..4].copy_from_slice(b"OggS");
        header[4] = 0; // stream structure version
        header[5] = flags.bits();
        header[6..14].copy_from_slice(&granule.to_le_bytes());
        // Stream serial: 1 (RFC 3533 §5.3 just requires uniqueness per logical stream).
        header[14..18].copy_from_slice(&1u32.to_le_bytes());
        header[18..22].copy_from_slice(&self.page_seq.to_le_bytes());
        header[26] = seg_table.len() as u8;

        // CRC is computed over header (with CRC field zeroed) + segment_table + data.
        let mut crc_buf = Vec::with_capacity(27 + seg_table.len() + packet.len());
        crc_buf.extend_from_slice(&header);
        crc_buf.extend_from_slice(&seg_table);
        crc_buf.extend_from_slice(packet);
        let crc = ogg_crc(&crc_buf);
        header[22..26].copy_from_slice(&crc.to_le_bytes());

        self.file.write_all(&header)?;
        self.file.write_all(&seg_table)?;
        self.file.write_all(packet)?;

        self.page_seq = self.page_seq.wrapping_add(1);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_page_crc_is_zero() {
        // An all-zero input produces CRC 0 with init=0, no final XOR.
        assert_eq!(ogg_crc(&[0u8; 27]), 0);
    }

    #[test]
    fn bos_page_crc_matches_expected() {
        // Reproduce the BOS page emitted by `OggWriter::write_bos` for a
        // single-Ogg-stream Opus file and check the CRC matches the value
        // our writer produces.
        let mut header = [0u8; 27];
        header[0..4].copy_from_slice(b"OggS");
        header[4] = 0; // stream structure version
        header[5] = 0x02; // BOS flag
                          // bytes 6..14 = granule = 0
        header[14..18].copy_from_slice(&1u32.to_le_bytes()); // serial = 1
                                                             // bytes 18..22 = page_seq = 0
        header[26] = 1; // nseg = 1
        let seg = [0x13u8];
        let payload = b"OpusHead\x01\x01\x38\x01\x80\xbb\x00\x00\x00\x00\x00";
        let mut buf = Vec::new();
        buf.extend_from_slice(&header);
        buf.extend_from_slice(&seg);
        buf.extend_from_slice(payload);
        assert_eq!(ogg_crc(&buf), 0xD9F5_9F1C);
    }
}
