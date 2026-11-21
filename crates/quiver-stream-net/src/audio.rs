//! The audio lane: each packet goes out at once, unpaced, with the previous
//! [`AudioSender::redundancy`] packets piggybacked. At 5 ms Opus frames that costs a few
//! hundred bytes a datagram and covers any loss of up to that many datagrams in a row, with
//! no added delay. The receiver hands each packet up once, in arrival order; the client's
//! jitter buffer orders them and conceals what is still missing.
//!
//! Chunk body: `count: u8`, then the packets newest first ([`codec::put_audio`]).

use std::collections::VecDeque;

use quiver_stream_proto::AudioPacket;

use crate::codec;
use crate::wire::{Malformed, Reader, Writer};

#[derive(Debug)]
pub struct AudioSender {
    recent: VecDeque<AudioPacket>,
    pub redundancy: usize,
}

impl AudioSender {
    pub fn new(redundancy: usize) -> Self {
        Self { recent: VecDeque::new(), redundancy }
    }

    /// Writes the chunk body for `packet` plus the recent ones, within `budget` bytes.
    pub fn write_chunk(&mut self, out: &mut Vec<u8>, packet: AudioPacket, budget: usize) {
        let start = out.len();
        out.push(0);
        let mut count = 0u8;
        let mut w = Writer::new(out);
        codec::put_audio(&mut w, &packet);
        count += 1;
        for old in self.recent.iter().rev() {
            if w.buf.len() - start + codec::audio_wire_len(old) > budget {
                break;
            }
            codec::put_audio(&mut w, old);
            count += 1;
        }
        out[start] = count;
        self.recent.push_back(packet);
        while self.recent.len() > self.redundancy {
            self.recent.pop_front();
        }
    }
}

#[derive(Debug, Default)]
pub struct AudioReceiver {
    /// The highest sequence number seen, and a bitmap of the 64 below it.
    top: Option<u64>,
    seen: u64,
    pub recovered: u64,
}

impl AudioReceiver {
    /// Takes a chunk body; returns the packets not seen before, oldest first.
    pub fn on_chunk(&mut self, body: &[u8]) -> Result<Vec<AudioPacket>, Malformed> {
        let mut r = Reader::new(body);
        let count = r.u8()?;
        let mut packets = Vec::with_capacity(count as usize);
        for i in 0..count {
            let p = codec::get_audio(&mut r)?;
            if self.mark(p.seq) {
                if i > 0 {
                    self.recovered += 1;
                }
                packets.push(p);
            }
        }
        packets.reverse();
        Ok(packets)
    }

    /// Records `seq`; false if it was seen already or is too old to tell.
    fn mark(&mut self, seq: u64) -> bool {
        let Some(top) = self.top else {
            self.top = Some(seq);
            self.seen = 0;
            return true;
        };
        if seq > top {
            let shift = seq - top;
            self.seen = if shift >= 64 { 0 } else { (self.seen << shift) | (1 << (shift - 1)) };
            self.top = Some(seq);
            return true;
        }
        let age = top - seq;
        if age == 0 || age > 64 || self.seen & (1 << (age - 1)) != 0 {
            return false;
        }
        self.seen |= 1 << (age - 1);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(seq: u64) -> AudioPacket {
        AudioPacket { seq, captured: seq * 5000, mic: false, data: vec![seq as u8; 120] }
    }

    #[test]
    fn a_lost_datagram_is_covered_by_the_next() {
        let mut tx = AudioSender::new(1);
        let mut rx = AudioReceiver::default();
        let mut chunks = Vec::new();
        for seq in 1..=3 {
            let mut out = Vec::new();
            tx.write_chunk(&mut out, packet(seq), 1100);
            chunks.push(out);
        }
        assert_eq!(rx.on_chunk(&chunks[0]).unwrap(), vec![packet(1)]);
        // Datagram 2 is lost; 3 carries 2 as well.
        let got = rx.on_chunk(&chunks[2]).unwrap();
        assert_eq!(got, vec![packet(2), packet(3)]);
        assert_eq!(rx.recovered, 1);
        assert!(rx.on_chunk(&chunks[1]).unwrap().is_empty(), "both already seen");
    }

    #[test]
    fn the_budget_limits_redundancy() {
        let mut tx = AudioSender::new(4);
        for seq in 1..=4 {
            tx.write_chunk(&mut Vec::new(), packet(seq), 1100);
        }
        let mut out = Vec::new();
        tx.write_chunk(&mut out, packet(5), 300);
        assert_eq!(out[0], 2);
        assert!(out.len() <= 300);
    }
}
