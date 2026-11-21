//! Video for Moonlight: each encoded frame cut into equal shards, each shard an RTP packet
//! with Moonlight's NV video header, protected by Reed-Solomon parity per FEC block.
//!
//! A shard is `packet_size + 16` bytes (`packet_size` is what the client asked for in the
//! ANNOUNCE):
//!
//! | bytes | what |
//! |---|---|
//! | 0..12 | RTP: `0x90` (version 2, extension), payload type 0, sequence (BE), 90 kHz timestamp (BE), SSRC 0 |
//! | 12..16 | the RTP extension, zero |
//! | 16..20 | stream packet index << 8 (LE): counts every packet; continuous within a block |
//! | 20..24 | frame index (LE), from 1 |
//! | 24 | flags: contains picture data (1), end of block (2), start of block (4) |
//! | 25 | extra flags (0) |
//! | 26 | multi-FEC flags, always 0x10 |
//! | 27 | multi-FEC blocks: this block << 4, last block << 6 |
//! | 28..32 | FEC info (LE): shard index << 12, data shards << 22, FEC percentage << 4 |
//! | 32.. | payload |
//!
//! The frame's payload is an 8-byte frame header (`0x01`, host latency in 0.1 ms, the frame
//! type, the length of the last shard's payload) followed by the bitstream, cut into
//! `packet_size - 16` byte pieces; the last piece is zero-padded. A frame of more than
//! about 212 shards (at 20% FEC) is split into up to four FEC blocks. Parity is computed
//! over whole shards before the RTP header and FEC info are filled in; Moonlight rewrites
//! those fields in a shard it recovers, so they needn't be covered.
//!
//! With video encryption each shard is AES-GCM sealed and prefixed with its IV, the frame
//! index and the tag (32 bytes), and the client's `packet_size` already allows for that.

use anyhow::Result;

use crate::crypto::{Key, gcm_seal};
use crate::fec::{MAX_SHARDS, ParityMatrix};

pub const RTP_HEADER: usize = 16;
pub const NV_HEADER: usize = 16;
pub const FRAME_HEADER: usize = 8;
const MAX_FEC_BLOCKS: usize = 4;

pub const FLAG_CONTAINS_PIC_DATA: u8 = 0x1;
pub const FLAG_EOF: u8 = 0x2;
pub const FLAG_SOF: u8 = 0x4;

/// What the frame header says the frame is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    P = 1,
    Idr = 2,
    IntraRefresh = 4,
    /// The first frame after a reference frame invalidation.
    AfterInvalidation = 5,
}

/// Cuts frames into datagrams.
pub struct VideoPacketizer {
    packet_size: usize,
    fec_percent: usize,
    min_parity: usize,
    /// Counts every packet sent: the RTP sequence (low 16 bits) and the stream packet index
    /// (low 24 bits).
    packets: u32,
    frame_index: u32,
    cipher: Option<(Key, u64)>,
}

impl VideoPacketizer {
    /// `packet_size` as the client announced it, at least 64 and a multiple of 16 when
    /// encrypting; `fec_percent` of parity per block (0 for none); `min_parity` parity shards
    /// at least per block (the client's `fec.minRequiredFecPackets`).
    pub fn new(packet_size: usize, fec_percent: u8, min_parity: usize, cipher: Option<Key>) -> Self {
        Self {
            packet_size: packet_size.max(64),
            fec_percent: usize::from(fec_percent.min(255)),
            min_parity,
            packets: 0,
            frame_index: 0,
            cipher: cipher.map(|k| (k, 0)),
        }
    }

    /// The index the next frame gets.
    pub fn next_frame_index(&self) -> u32 {
        self.frame_index.wrapping_add(1)
    }

    fn shard_size(&self) -> usize {
        self.packet_size + RTP_HEADER
    }

    fn piece(&self) -> usize {
        self.packet_size - NV_HEADER
    }

    /// The datagrams for one frame, in send order. `latency_100us` is capture-to-send in
    /// tenths of a millisecond (shown in Moonlight's stats); `rtp_ts` is the frame's 90 kHz
    /// timestamp.
    pub fn packetize(&mut self, bitstream: &[u8], frame_type: FrameType, latency_100us: u16, rtp_ts: u32) -> Result<Vec<Vec<u8>>> {
        self.frame_index = self.frame_index.wrapping_add(1);
        let frame_index = self.frame_index;
        let piece = self.piece();
        let shard_size = self.shard_size();

        let mut payload = Vec::with_capacity(FRAME_HEADER + bitstream.len());
        let total = FRAME_HEADER + bitstream.len();
        let mut last_len = total % piece;
        if last_len == 0 {
            last_len = piece;
        }
        payload.push(0x01);
        payload.extend_from_slice(&latency_100us.to_le_bytes());
        payload.push(frame_type as u8);
        payload.extend_from_slice(&(last_len as u16).to_le_bytes());
        payload.extend_from_slice(&[0, 0]);
        payload.extend_from_slice(bitstream);
        let n_data = total.div_ceil(piece);

        let mut fec_percent = self.fec_percent;
        let max_data = (MAX_SHARDS * 100) / (100 + fec_percent);
        let mut n_blocks = n_data.div_ceil(max_data);
        if n_blocks > MAX_FEC_BLOCKS {
            fec_percent = 0;
            n_blocks = MAX_FEC_BLOCKS;
        }
        let per_block = n_data.div_ceil(n_blocks);
        if per_block >= 1024 {
            anyhow::bail!("a frame of {} bytes needs {per_block} packets per FEC block, more than Moonlight can index", bitstream.len());
        }

        let mut out = Vec::new();
        let mut chunks = payload.chunks(piece);
        for block in 0..n_blocks {
            let first = block * per_block;
            let d = per_block.min(n_data - first);
            let multi_fec = ((block as u8) << 4) | (((n_blocks - 1) as u8) << 6);
            let base = self.packets;

            let mut shards: Vec<Vec<u8>> = Vec::with_capacity(d);
            for x in 0..d {
                let mut s = vec![0u8; shard_size];
                let spi = (base.wrapping_add(x as u32) & 0xFF_FFFF) << 8;
                s[16..20].copy_from_slice(&spi.to_le_bytes());
                s[20..24].copy_from_slice(&frame_index.to_le_bytes());
                let mut flags = FLAG_CONTAINS_PIC_DATA;
                if x == 0 {
                    flags |= FLAG_SOF;
                }
                if x == d - 1 {
                    flags |= FLAG_EOF;
                }
                s[24] = flags;
                s[26] = 0x10;
                s[27] = multi_fec;
                let c = chunks.next().expect("enough pieces");
                s[32..32 + c.len()].copy_from_slice(c);
                shards.push(s);
            }

            let (parity_n, block_percent) = parity_for(d, fec_percent, self.min_parity);
            if parity_n > 0 {
                let matrix = ParityMatrix::cauchy(d, parity_n)?;
                let mut parity = vec![vec![0u8; shard_size]; parity_n];
                {
                    let data: Vec<&[u8]> = shards.iter().map(Vec::as_slice).collect();
                    let mut outs: Vec<&mut [u8]> = parity.iter_mut().map(Vec::as_mut_slice).collect();
                    matrix.encode(&data, &mut outs);
                }
                for p in &mut parity {
                    p[20..24].copy_from_slice(&frame_index.to_le_bytes());
                    p[27] = multi_fec;
                }
                shards.extend(parity);
            }

            for (x, s) in shards.iter_mut().enumerate() {
                let seq = base.wrapping_add(x as u32) as u16;
                s[0] = 0x90;
                s[1] = 0;
                s[2..4].copy_from_slice(&seq.to_be_bytes());
                s[4..8].copy_from_slice(&rtp_ts.to_be_bytes());
                let fec_info = ((x as u32) << 12) | ((d as u32) << 22) | ((block_percent as u32) << 4);
                s[28..32].copy_from_slice(&fec_info.to_le_bytes());
            }
            self.packets = base.wrapping_add(shards.len() as u32);

            for s in shards {
                out.push(match &mut self.cipher {
                    None => s,
                    Some((key, counter)) => {
                        let mut iv = [0u8; 12];
                        iv[..8].copy_from_slice(&counter.to_le_bytes());
                        iv[11] = b'V';
                        *counter += 1;
                        let mut body = s;
                        let tag = gcm_seal(key, &iv, &mut body);
                        let mut pkt = Vec::with_capacity(32 + body.len());
                        pkt.extend_from_slice(&iv);
                        pkt.extend_from_slice(&frame_index.to_le_bytes());
                        pkt.extend_from_slice(&tag);
                        pkt.extend_from_slice(&body);
                        pkt
                    }
                });
            }
        }
        Ok(out)
    }
}

/// Parity shards for a block of `data` shards, and the percentage to announce so that the
/// client's `ceil(data × percent / 100)` comes out the same.
fn parity_for(data: usize, percent: usize, min_parity: usize) -> (usize, usize) {
    if percent == 0 || data >= MAX_SHARDS {
        return (0, 0);
    }
    let natural = (data * percent).div_ceil(100);
    let want = natural.max(min_parity).min(MAX_SHARDS - data);
    if want == natural && percent <= 255 {
        return (want, percent);
    }
    // The client recomputes the count from the announced (8-bit) percentage, so pick the
    // most parity, up to `want`, that some percentage reproduces exactly.
    for p in (1..=want).rev() {
        let pct = 100 * p / data;
        if pct <= 255 && (data * pct).div_ceil(100) == p {
            return (p, pct);
        }
    }
    (0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::gcm_open;

    fn le32(b: &[u8]) -> u32 {
        u32::from_le_bytes(b[..4].try_into().unwrap())
    }

    /// What Moonlight does with a frame's packets: per block, rebuild lost data shards from
    /// parity, then strip headers and join the payloads.
    fn depacketize(packets: &[Option<Vec<u8>>], packet_size: usize, n_data_expected: Option<usize>) -> Vec<u8> {
        let mut frame = Vec::new();
        let mut i = 0;
        while i < packets.len() {
            let (j, p) = (i..packets.len()).find_map(|j| packets[j].as_ref().map(|p| (j, p))).expect("a packet of the block");
            let fec_info = le32(&p[28..]);
            let idx = ((fec_info >> 12) & 0x3FF) as usize;
            let data = (fec_info >> 22) as usize;
            let pct = ((fec_info >> 4) & 0xFF) as usize;
            let parity = (data * pct).div_ceil(100);
            assert_eq!(j - idx, i, "blocks are contiguous");
            let mut shards = packets[i..i + data + parity].to_vec();
            if shards[..data].iter().any(Option::is_none) {
                ParityMatrix::cauchy(data, parity).unwrap().reconstruct(&mut shards).unwrap();
            }
            for (x, s) in shards[..data].iter().enumerate() {
                let s = s.as_ref().unwrap();
                let flags = s[24];
                assert_eq!(flags & FLAG_CONTAINS_PIC_DATA, FLAG_CONTAINS_PIC_DATA);
                assert_eq!(flags & FLAG_SOF != 0, x == 0, "SOF on shard {x}");
                assert_eq!(flags & FLAG_EOF != 0, x == data - 1, "EOF on shard {x}");
                assert_eq!(s.len(), packet_size + 16);
                frame.extend_from_slice(&s[32..]);
            }
            i += data + parity;
        }
        if let Some(n) = n_data_expected {
            assert_eq!(frame.len() / (packet_size - 16), n);
        }
        frame
    }

    fn frame_bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 13 + i / 251) as u8).collect()
    }

    #[test]
    fn small_frame_layout() {
        let mut v = VideoPacketizer::new(1024, 20, 2, None);
        let bits = frame_bytes(3000);
        let pkts = v.packetize(&bits, FrameType::Idr, 37, 9000).unwrap();
        // 3008 bytes over 1008-byte pieces: 3 data shards, and ceil(3 × 20%) = 1 < 2 parity,
        // so 2 parity at 66%.
        assert_eq!(pkts.len(), 5);
        let p0 = &pkts[0];
        assert_eq!(p0.len(), 1040);
        assert_eq!(&p0[..4], &[0x90, 0, 0, 0]);
        assert_eq!(&p0[4..8], &9000u32.to_be_bytes());
        assert_eq!(le32(&p0[16..]), 0);
        assert_eq!(le32(&p0[20..]), 1, "frame index starts at 1");
        assert_eq!(p0[24], FLAG_CONTAINS_PIC_DATA | FLAG_SOF);
        assert_eq!((p0[26], p0[27]), (0x10, 0));
        assert_eq!(le32(&p0[28..]), (3 << 22) | (66 << 4));
        assert_eq!(le32(&pkts[4][28..]), (4 << 12) | (3 << 22) | (66 << 4));
        // The frame header: type, latency, last payload length (3008 % 1008 = 992).
        assert_eq!(&p0[32..40], &[0x01, 37, 0, 2, 0xE0, 0x03, 0, 0]);
        assert_eq!(le32(&pkts[1][16..]), 1 << 8);
        assert_eq!(pkts[2][24], FLAG_CONTAINS_PIC_DATA | FLAG_EOF);
        let got = depacketize(&pkts.iter().cloned().map(Some).collect::<Vec<_>>(), 1024, Some(3));
        assert_eq!(&got[8..8 + 3000], &bits[..]);
        assert!(got[8 + 3000..].iter().all(|&b| b == 0), "zero padding");

        // The next frame continues the counters.
        let pkts2 = v.packetize(&frame_bytes(10), FrameType::P, 0, 10500).unwrap();
        assert_eq!(u16::from_be_bytes([pkts2[0][2], pkts2[0][3]]), 5);
        assert_eq!(le32(&pkts2[0][16..]), 5 << 8);
        assert_eq!(le32(&pkts2[0][20..]), 2);
        assert_eq!(pkts2[0][24], FLAG_CONTAINS_PIC_DATA | FLAG_SOF | FLAG_EOF);
        assert_eq!(pkts2[0][35], FrameType::P as u8);
    }

    #[test]
    fn fec_recovers_lost_shards() {
        let mut v = VideoPacketizer::new(1392, 20, 2, None);
        let bits = frame_bytes(100_000);
        let pkts = v.packetize(&bits, FrameType::P, 0, 0).unwrap();
        let n_data = (100_008usize).div_ceil(1376);
        let parity = (n_data * 20).div_ceil(100);
        assert_eq!(pkts.len(), n_data + parity);
        // Lose `parity` data shards spread through the frame.
        let mut rx: Vec<Option<Vec<u8>>> = pkts.into_iter().map(Some).collect();
        for k in 0..parity {
            rx[k * (n_data / parity)] = None;
        }
        let got = depacketize(&rx, 1392, Some(n_data));
        assert_eq!(&got[8..8 + bits.len()], &bits[..]);
    }

    #[test]
    fn big_frames_split_into_blocks() {
        let mut v = VideoPacketizer::new(1024, 20, 2, None);
        // 212 data shards per block at 20%; 500 KB needs 497 data shards, so 3 blocks.
        let bits = frame_bytes(500_000);
        let pkts = v.packetize(&bits, FrameType::Idr, 0, 0).unwrap();
        let blocks: Vec<u8> = pkts.iter().map(|p| p[27]).collect();
        assert_eq!(blocks.first(), Some(&(2 << 6)));
        assert_eq!(blocks.last(), Some(&((2 << 6) | (2 << 4))));
        // Each block starts with SOF and has a contiguous stream packet index.
        let sof: Vec<usize> = pkts.iter().enumerate().filter(|(_, p)| p[24] & FLAG_SOF != 0 && le32(&p[28..]) >> 12 & 0x3FF == 0).map(|(i, _)| i).collect();
        assert_eq!(sof.len(), 3);
        let mut rx: Vec<Option<Vec<u8>>> = pkts.into_iter().map(Some).collect();
        rx[3] = None;
        rx[300] = None;
        let got = depacketize(&rx, 1024, None);
        assert_eq!(&got[8..8 + bits.len()], &bits[..]);
    }

    #[test]
    fn no_fec_when_percent_is_zero() {
        let mut v = VideoPacketizer::new(1024, 0, 2, None);
        let pkts = v.packetize(&frame_bytes(5000), FrameType::P, 0, 0).unwrap();
        assert_eq!(pkts.len(), 5);
        assert!(pkts.iter().all(|p| le32(&p[28..]) >> 4 & 0xFF == 0));
    }

    #[test]
    fn parity_counts_match_the_client() {
        for pct in [0usize, 5, 10, 20, 50] {
            for data in 1..=(MAX_SHARDS * 100) / (100 + pct) {
                for min in [0usize, 2, 4] {
                    let (p, announced) = parity_for(data, pct, min);
                    assert!(announced <= 255);
                    assert_eq!((data * announced).div_ceil(100), p, "data {data} pct {pct} min {min}");
                    assert!(data + p <= MAX_SHARDS);
                    if pct > 0 {
                        assert!(p >= (data * pct).div_ceil(100).min(MAX_SHARDS - data), "never less than asked");
                        if data <= 100 && 100 * min / data <= 255 {
                            assert!(p >= min.min(MAX_SHARDS - data), "the client's minimum");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn encrypted_shards() {
        let key = [0x42; 16];
        let mut v = VideoPacketizer::new(1024 - 32, 20, 2, Some(key));
        let pkts = v.packetize(&frame_bytes(2000), FrameType::Idr, 0, 0).unwrap();
        for (i, p) in pkts.iter().enumerate() {
            assert_eq!(p.len(), 32 + 1024 - 32 + 16);
            let iv: [u8; 12] = p[..12].try_into().unwrap();
            assert_eq!(u64::from_le_bytes(iv[..8].try_into().unwrap()), i as u64);
            assert_eq!(iv[11], b'V');
            assert_eq!(le32(&p[12..]), 1);
            let tag: [u8; 16] = p[16..32].try_into().unwrap();
            let mut body = p[32..].to_vec();
            gcm_open(&key, &iv, &mut body, &tag).unwrap();
            assert_eq!(body[0], 0x90);
        }
    }
}
