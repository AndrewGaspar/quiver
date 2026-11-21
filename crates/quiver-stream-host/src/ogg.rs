//! A minimal Ogg Opus writer (RFC 7845), so `--audio-out` makes a file ffplay plays: the two
//! header packets, then one Opus packet per page.

use std::io::Write;

fn crc32(data: &[u8]) -> u32 {
    // Ogg's CRC: polynomial 0x04c11db7, no reflection, initial 0.
    let mut crc = 0u32;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04c1_1db7 } else { crc << 1 };
        }
    }
    crc
}

pub struct OggOpus<W: Write> {
    out: W,
    serial: u32,
    page: u32,
    granule: u64,
    frame: u64,
}

impl<W: Write> OggOpus<W> {
    /// Write the headers for `channels` channels, with libopus's surround layout
    /// ([`crate::opus::Encoder`]'s streams, coupled count and mapping).
    pub fn new(out: W, channels: u8, streams: u8, coupled: u8, mapping: &[u8], frame_samples: u64) -> std::io::Result<Self> {
        let mut head = b"OpusHead".to_vec();
        head.push(1);
        head.push(channels);
        head.extend_from_slice(&312u16.to_le_bytes()); // pre-skip
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes());
        if channels <= 2 {
            head.push(0);
        } else {
            head.push(1);
            head.push(streams);
            head.push(coupled);
            head.extend_from_slice(mapping);
        }
        let mut tags = b"OpusTags".to_vec();
        let vendor = b"quiver-stream-host";
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor);
        tags.extend_from_slice(&0u32.to_le_bytes());
        let mut w = OggOpus { out, serial: 0x5155_4956, page: 0, granule: 0, frame: frame_samples };
        w.page(&head, 0x02, 0)?;
        w.page(&tags, 0x00, 0)?;
        Ok(w)
    }

    fn page(&mut self, packet: &[u8], flags: u8, granule: u64) -> std::io::Result<()> {
        let mut segs = Vec::new();
        let mut left = packet.len();
        loop {
            let n = left.min(255);
            segs.push(n as u8);
            left -= n;
            if n < 255 {
                break;
            }
        }
        let mut p = b"OggS".to_vec();
        p.push(0);
        p.push(flags);
        p.extend_from_slice(&granule.to_le_bytes());
        p.extend_from_slice(&self.serial.to_le_bytes());
        p.extend_from_slice(&self.page.to_le_bytes());
        p.extend_from_slice(&0u32.to_le_bytes());
        p.push(segs.len() as u8);
        p.extend_from_slice(&segs);
        p.extend_from_slice(packet);
        let crc = crc32(&p);
        p[22..26].copy_from_slice(&crc.to_le_bytes());
        self.page += 1;
        self.out.write_all(&p)
    }

    /// One Opus packet.
    pub fn packet(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.granule += self.frame;
        let g = self.granule;
        self.page(data, 0, g)
    }

    pub fn finish(mut self) -> std::io::Result<W> {
        self.out.flush()?;
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ogg_crc_reference() {
        // The CRC of "OggS" with the rest of a header zeroed is stable; check the polynomial on
        // a known value: crc32 (Ogg variant) of b"123456789" is 0x89a1897f.
        assert_eq!(crc32(b"123456789"), 0x89a1_897f);
    }

    #[test]
    fn headers_then_pages() {
        let mut w = OggOpus::new(Vec::new(), 2, 1, 1, &[0, 1], 240).unwrap();
        w.packet(&[0xe8, 1, 2, 3]).unwrap();
        let b = w.finish().unwrap();
        assert_eq!(&b[..4], b"OggS");
        assert_eq!(b.windows(8).filter(|w| *w == b"OggS\0\x02\0\0").count(), 1, "one BOS page");
        assert!(b.windows(8).any(|w| w == b"OpusHead"));
        assert!(b.windows(8).any(|w| w == b"OpusTags"));
        assert_eq!(b.windows(4).filter(|w| *w == b"OggS").count(), 3);
    }
}
