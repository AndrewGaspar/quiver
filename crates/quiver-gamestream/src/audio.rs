//! Audio for Moonlight: Opus in RTP (payload type 97), with Reed-Solomon FEC (payload type
//! 127) and optional AES-CBC encryption.
//!
//! Every packet holds one Opus packet of the negotiated duration (5 ms usually). Packets go
//! in blocks of four, aligned on sequence numbers divisible by 4; after the fourth, two FEC
//! packets carry parity over the four payloads ([`ParityMatrix::audio`]). The parity needs
//! the four payloads to be the same length, so the host encodes at a constant bitrate; a
//! block whose packets differ just goes without FEC. The RTP timestamp counts milliseconds.
//!
//! Surround is Opus multistream. The layout ([`OpusLayout`]) is the server's to choose and
//! is announced in the RTSP DESCRIBE; the host's encoder must use exactly that layout, with
//! its input channels in Moonlight's order: FL FR FC LFE BL BR (5.1), then SL SR (7.1).

use crate::crypto::{Key, cbc_encrypt};
use crate::fec::ParityMatrix;

pub const PAYLOAD_TYPE_AUDIO: u8 = 97;
pub const PAYLOAD_TYPE_FEC: u8 = 127;
const DATA_SHARDS: usize = 4;
const RTP_HEADER: usize = 12;
const FEC_HEADER: usize = 12;

/// An Opus multistream layout, as `opus_multistream_encoder_create` takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusLayout {
    pub channels: u8,
    pub streams: u8,
    pub coupled_streams: u8,
    /// Input channel `i` (in Moonlight's order) is coded channel `mapping[i]`.
    pub mapping: Vec<u8>,
    /// The whole stream's bitrate, bits per second.
    pub bitrate_bps: u32,
    /// Moonlight's "high quality surround": one stream per channel, at a higher rate.
    pub high_quality: bool,
}

impl OpusLayout {
    /// The layout for `channels` (2, 6 or 8; anything else gets stereo).
    ///
    /// These are exactly what libopus's surround encoder (mapping family 1) makes when fed
    /// Vorbis channel order, which is how `quiver-stream-host` encodes: coupled streams for
    /// FL/FR, then the side and back pairs, then centre and LFE alone. The mapping here is
    /// that encoder's, re-indexed by Moonlight's channel order, so Moonlight's decoder puts
    /// each speaker back where it belongs. "High quality" keeps the layout and doubles the
    /// bitrate (the host has one encoder layout per channel count).
    pub fn for_channels(channels: u8, high_quality: bool) -> Self {
        let (channels, streams, coupled_streams, mapping, bps): (u8, u8, u8, Vec<u8>, u32) = match channels {
            // Vorbis 5.1: FL FC FR RL RR LFE, encoder mapping 0 4 1 2 3 5.
            6 => (6, 4, 2, vec![0, 1, 4, 5, 2, 3], 256_000),
            // Vorbis 7.1: FL FC FR SL SR RL RR LFE, encoder mapping 0 6 1 2 3 4 5 7.
            8 => (8, 5, 3, vec![0, 1, 6, 7, 4, 5, 2, 3], 450_000),
            _ => (2, 1, 1, vec![0, 1], 128_000),
        };
        let bitrate_bps = if high_quality { bps * 2 } else { bps };
        Self { channels, streams, coupled_streams, mapping, bitrate_bps, high_quality }
    }

    /// Moonlight's channel order (FL FR FC LFE BL BR SL SR) as indexes into Vorbis order
    /// (FL FC FR [SL SR] BL BR LFE), the order the host's encoder takes its input in.
    pub fn vorbis_index(channels: u8) -> &'static [usize] {
        match channels {
            6 => &[0, 2, 1, 5, 3, 4],
            8 => &[0, 2, 1, 7, 5, 6, 3, 4],
            _ => &[0, 1],
        }
    }

    /// The digits after `surround-params=`: channels, streams, coupled streams, mapping.
    ///
    /// Moonlight rotates the mapping of the first (normal quality) entry it reads for 5.1
    /// and 7.1: its channel 3 takes the last digit and channels 4.. shift up by one (a
    /// quirk of GeForce Experience's announcements). So a normal-quality entry is written
    /// pre-rotated the other way, and what Moonlight ends up with is [`Self::mapping`].
    pub fn sdp_params(&self) -> String {
        let n = self.mapping.len();
        let mut m = self.mapping.clone();
        if !self.high_quality && n > 4 {
            m[3..].rotate_left(1);
        }
        let mut s = format!("{}{}{}", self.channels, self.streams, self.coupled_streams);
        for d in m {
            s.push((b'0' + d) as char);
        }
        s
    }
}

/// Turns Opus packets into Moonlight's RTP datagrams.
pub struct AudioPacketizer {
    seq: u16,
    timestamp: u32,
    packet_ms: u32,
    /// The launch key and `rikeyid`, when audio is encrypted.
    cipher: Option<(Key, u32)>,
    matrix: ParityMatrix,
    block: Vec<Vec<u8>>,
    block_base_seq: u16,
    block_base_ts: u32,
}

impl AudioPacketizer {
    pub fn new(packet_ms: u32, cipher: Option<(Key, u32)>) -> Self {
        Self {
            seq: 0,
            timestamp: 0,
            packet_ms,
            cipher,
            matrix: ParityMatrix::audio(),
            block: Vec::with_capacity(DATA_SHARDS),
            block_base_seq: 0,
            block_base_ts: 0,
        }
    }

    /// The datagrams for one Opus packet: its RTP packet, then after every fourth packet
    /// the block's two FEC packets.
    pub fn push(&mut self, opus: &[u8]) -> Vec<Vec<u8>> {
        let seq = self.seq;
        let ts = self.timestamp;
        let payload = match &self.cipher {
            Some((key, key_id)) => {
                let mut iv = [0u8; 16];
                iv[..4].copy_from_slice(&key_id.wrapping_add(u32::from(seq)).to_be_bytes());
                cbc_encrypt(key, &iv, opus)
            }
            None => opus.to_vec(),
        };
        let mut out = Vec::with_capacity(3);
        let mut pkt = Vec::with_capacity(RTP_HEADER + payload.len());
        rtp_header(&mut pkt, PAYLOAD_TYPE_AUDIO, seq, ts);
        pkt.extend_from_slice(&payload);
        out.push(pkt);

        if seq as usize % DATA_SHARDS == 0 {
            self.block.clear();
            self.block_base_seq = seq;
            self.block_base_ts = ts;
        }
        // Only a block that started on its boundary gets FEC.
        if self.block.len() == seq as usize % DATA_SHARDS {
            self.block.push(payload);
        }
        if seq as usize % DATA_SHARDS == DATA_SHARDS - 1 && self.block.len() == DATA_SHARDS {
            let len = self.block[0].len();
            if self.block.iter().all(|p| p.len() == len) {
                let mut parity = vec![vec![0u8; len]; 2];
                {
                    let data: Vec<&[u8]> = self.block.iter().map(Vec::as_slice).collect();
                    let mut outs: Vec<&mut [u8]> = parity.iter_mut().map(Vec::as_mut_slice).collect();
                    self.matrix.encode(&data, &mut outs);
                }
                for (x, p) in parity.into_iter().enumerate() {
                    let mut fec = Vec::with_capacity(RTP_HEADER + FEC_HEADER + len);
                    rtp_header(&mut fec, PAYLOAD_TYPE_FEC, seq.wrapping_add(1 + x as u16), 0);
                    fec.push(x as u8);
                    fec.push(PAYLOAD_TYPE_AUDIO);
                    fec.extend_from_slice(&self.block_base_seq.to_be_bytes());
                    fec.extend_from_slice(&self.block_base_ts.to_be_bytes());
                    fec.extend_from_slice(&0u32.to_be_bytes());
                    fec.extend_from_slice(&p);
                    out.push(fec);
                }
            }
        }
        self.seq = seq.wrapping_add(1);
        self.timestamp = ts.wrapping_add(self.packet_ms);
        out
    }
}

fn rtp_header(out: &mut Vec<u8>, payload_type: u8, seq: u16, ts: u32) {
    out.push(0x80);
    out.push(payload_type);
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&ts.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::cbc_decrypt;

    #[test]
    fn layouts_and_sdp_params() {
        let s = OpusLayout::for_channels(2, false);
        assert_eq!(s.sdp_params(), "21101");
        let n51 = OpusLayout::for_channels(6, false);
        assert_eq!(n51.sdp_params(), "642014235");
        // What Moonlight does with a normal-quality entry: m[3] = p[last], m[4..] = p[3..].
        for l in [n51.clone(), OpusLayout::for_channels(8, false)] {
            let n = l.mapping.len();
            let p: Vec<u8> = l.sdp_params()[3..].bytes().map(|b| b - b'0').collect();
            let mut m = p.clone();
            m[3] = p[n - 1];
            m[4..n].copy_from_slice(&p[3..n - 1]);
            assert_eq!(m, l.mapping);
        }
        // High quality: same layout, not rotated, twice the rate.
        let hq = OpusLayout::for_channels(6, true);
        assert_eq!(hq.sdp_params(), "642014523");
        assert_eq!(hq.bitrate_bps, 2 * n51.bitrate_bps);
        for ch in [6u8, 8] {
            let l = OpusLayout::for_channels(ch, false);
            assert_eq!(usize::from(l.streams + l.coupled_streams), usize::from(l.channels), "{l:?}");
        }
    }

    #[test]
    fn mappings_follow_libopus_family_1() {
        // libopus's vorbis_mappings for 5.1 and 7.1 (src/opus_multistream_encoder.c), indexed
        // by Vorbis channel, re-indexed by Moonlight's.
        for (ch, family1) in [(6u8, vec![0u8, 4, 1, 2, 3, 5]), (8, vec![0, 6, 1, 2, 3, 4, 5, 7])] {
            let ours: Vec<u8> = OpusLayout::vorbis_index(ch).iter().map(|&v| family1[v]).collect();
            assert_eq!(ours, OpusLayout::for_channels(ch, false).mapping, "{ch} channels");
        }
    }

    #[test]
    fn rtp_headers_and_fec() {
        let mut a = AudioPacketizer::new(5, None);
        let packets: Vec<Vec<Vec<u8>>> = (0..8u8).map(|i| a.push(&[i; 40])).collect();
        assert_eq!(packets.iter().map(Vec::len).collect::<Vec<_>>(), [1, 1, 1, 3, 1, 1, 1, 3]);
        let p2 = &packets[2][0];
        assert_eq!(&p2[..12], &[0x80, 97, 0, 2, 0, 0, 0, 10, 0, 0, 0, 0]);
        assert_eq!(&p2[12..], &[2u8; 40]);
        // The FEC packets after seq 7: RTP seq 8 and 9, base seq 4, base timestamp 20.
        let f0 = &packets[7][1];
        assert_eq!(f0[1], PAYLOAD_TYPE_FEC);
        assert_eq!(u16::from_be_bytes([f0[2], f0[3]]), 8);
        assert_eq!(&f0[12..24], &[0, 97, 0, 4, 0, 0, 0, 20, 0, 0, 0, 0]);
        assert_eq!(u16::from_be_bytes([packets[7][2][2], packets[7][2][3]]), 9);
        // Lose two of the four; the parity puts them back.
        let mut shards: Vec<Option<Vec<u8>>> = vec![
            Some(packets[4][0][12..].to_vec()),
            None,
            Some(packets[6][0][12..].to_vec()),
            None,
            Some(packets[7][1][24..].to_vec()),
            Some(packets[7][2][24..].to_vec()),
        ];
        ParityMatrix::audio().reconstruct(&mut shards).unwrap();
        assert_eq!(shards[1].as_ref().unwrap(), &[5u8; 40]);
        assert_eq!(shards[3].as_ref().unwrap(), &[7u8; 40]);
    }

    #[test]
    fn uneven_block_goes_without_fec() {
        let mut a = AudioPacketizer::new(5, None);
        let n: Vec<usize> = [40usize, 41, 40, 40].iter().map(|&l| a.push(&vec![0; l]).len()).collect();
        assert_eq!(n, [1, 1, 1, 1]);
    }

    #[test]
    fn encrypted_payloads() {
        let key = [3u8; 16];
        let mut a = AudioPacketizer::new(10, Some((key, 0xFFFF_FFFE)));
        a.push(&[1; 20]);
        let p = a.push(&[9; 20]).remove(0);
        assert_eq!(p.len(), 12 + 32);
        // IV = big-endian(rikeyid + seq), wrapping: 0xFFFFFFFE + 1.
        let mut iv = [0u8; 16];
        iv[..4].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        assert_eq!(cbc_decrypt(&key, &iv, &p[12..]).unwrap(), vec![9u8; 20]);
        assert_eq!(u32::from_be_bytes([p[4], p[5], p[6], p[7]]), 10);
    }
}
