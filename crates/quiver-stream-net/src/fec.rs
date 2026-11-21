//! Forward error correction: one Reed-Solomon block per video frame.
//!
//! A frame's bytes (data, then metadata) are cut into `k` equal shards, and `r` recovery
//! shards are computed over them with `reed-solomon-simd` (Leopard-style GF(2^16), SIMD,
//! O(n log n)). Any `k` of the `k + r` shards rebuild the frame. One block per frame, rather
//! than Sunshine's blocks of at most 255, means a burst anywhere in the frame is repaired as
//! long as the frame's total loss is within `r`.
//!
//! [`FecPlanner`] picks `r` per frame: the smallest count for which the chance of losing
//! more than `r` of the `k + r` shards, at the measured loss rate, is below a target. Small
//! frames therefore get proportionally more recovery than big ones, as they should.

use std::collections::HashMap;

use reed_solomon_simd::{ReedSolomonDecoder, ReedSolomonEncoder};

/// Frames larger than this are refused. It's far above any real frame (a 4K AV1 keyframe at
/// high quality is a few MB) and keeps the shard count inside what the codec supports.
pub const MAX_FRAME_BYTES: usize = 16 << 20;

/// Chooses the number of recovery shards for a frame of `k` data shards.
#[derive(Debug, Clone)]
pub struct FecPlanner {
    /// The loss rate planned for, quantised up to steps of 1/400 (0.25%).
    loss_q: u32,
    /// The acceptable chance that a frame is lost despite its recovery shards.
    target: f64,
    /// Never more recovery than this fraction of `k` (but always allowing `min_parity`, and
    /// up to 3 for tiny frames).
    max_ratio: f64,
    /// At least this many recovery shards per frame, even with no loss measured.
    min_parity: usize,
    cache: HashMap<usize, usize>,
}

impl FecPlanner {
    pub fn new(target: f64, max_ratio: f64, min_parity: usize) -> Self {
        Self { loss_q: 0, target, max_ratio, min_parity, cache: HashMap::new() }
    }

    /// The acceptable chance of losing a frame despite FEC.
    pub fn set_target(&mut self, target: f64) {
        if target != self.target {
            self.target = target;
            self.cache.clear();
        }
    }

    /// The loss rate to plan for (0.0..=1.0). The caller adds any safety margin.
    pub fn set_loss(&mut self, loss: f64) {
        let q = (loss.clamp(0.0, 0.5) * 400.0).ceil() as u32;
        if q != self.loss_q {
            self.loss_q = q;
            self.cache.clear();
        }
    }

    pub fn loss(&self) -> f64 {
        f64::from(self.loss_q) / 400.0
    }

    /// Recovery shards for a frame of `k` data shards.
    pub fn parity_for(&mut self, k: usize) -> usize {
        if let Some(&r) = self.cache.get(&k) {
            return r;
        }
        // Tiny frames may take a few whole copies: they cost little and lose a whole frame
        // to one lost packet otherwise.
        let cap = ((k as f64 * self.max_ratio).ceil() as usize).max(self.min_parity).max(3);
        let r = parity_needed(k, self.loss(), self.target, cap).max(self.min_parity);
        let r = clamp_supported(k, r);
        self.cache.insert(k, r);
        r
    }
}

/// The smallest `r ≤ cap` with P[more than r of k + r shards lost] ≤ `target`, for
/// independent loss at rate `p`. Returns `cap` if none is.
pub fn parity_needed(k: usize, p: f64, target: f64, cap: usize) -> usize {
    if p <= 0.0 {
        return 0;
    }
    (0..=cap).find(|&r| binomial_tail(k + r, p, r) <= target).unwrap_or(cap)
}

/// `P[X > r]` for `X ~ Binomial(n, p)`. Exact for n up to a few thousand; past that the normal
/// approximation, which is plenty for planning.
pub fn binomial_tail(n: usize, p: f64, r: usize) -> f64 {
    if r >= n {
        return 0.0;
    }
    if n > 3000 {
        let mean = n as f64 * p;
        let sd = (mean * (1.0 - p)).sqrt().max(1e-9);
        let z = (r as f64 + 0.5 - mean) / sd;
        return 0.5 * erfc(z / std::f64::consts::SQRT_2);
    }
    // Sum the pmf up to r in log space, then take the complement.
    let q = 1.0 - p;
    let mut log_pmf = n as f64 * q.ln();
    let ratio = (p / q).ln();
    let mut cdf = log_pmf.exp();
    for i in 0..r {
        log_pmf += ((n - i) as f64).ln() - ((i + 1) as f64).ln() + ratio;
        cdf += log_pmf.exp();
    }
    (1.0 - cdf).max(0.0)
}

/// The complementary error function (Numerical Recipes' erfc, about 1e-7 relative error).
fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.5 * z);
    let poly = -z * z - 1.265_512_23
        + t * (1.000_023_68
            + t * (0.374_091_96
                + t * (0.096_784_18
                    + t * (-0.186_288_06
                        + t * (0.278_868_07
                            + t * (-1.135_203_98 + t * (1.488_515_87 + t * (-0.822_152_23 + t * 0.170_872_77))))))));
    let r = t * poly.exp();
    if x >= 0.0 { r } else { 2.0 - r }
}

/// The largest recovery count the codec supports for `k`, at most `r`.
fn clamp_supported(k: usize, mut r: usize) -> usize {
    while r > 0 && !ReedSolomonEncoder::supports(k, r) {
        r -= 1;
    }
    r
}

/// The shard layout for a byte string of `len` bytes, with shards of at most `max_shard`
/// bytes: `(k, shard_len)`. Shards are as equal as the codec allows (an even length), so
/// the padding in the last one stays small.
pub fn layout(len: usize, max_shard: usize) -> (usize, usize) {
    let max_shard = max_shard & !1;
    let k = len.div_ceil(max_shard).max(1);
    let shard = len.div_ceil(k).next_multiple_of(2).max(2);
    (k, shard)
}

/// Computes `r` recovery shards over `k` shards of `shard` bytes laid end to end in `data`
/// (which must be exactly `k * shard` long). Returns them laid end to end.
pub fn encode(enc: &mut Option<ReedSolomonEncoder>, data: &[u8], k: usize, r: usize, shard: usize) -> Vec<u8> {
    debug_assert_eq!(data.len(), k * shard);
    if r == 0 {
        return Vec::new();
    }
    let enc = match enc {
        Some(e) => {
            e.reset(k, r, shard).expect("supported layout");
            e
        }
        None => enc.insert(ReedSolomonEncoder::new(k, r, shard).expect("supported layout")),
    };
    for s in data.chunks_exact(shard) {
        enc.add_original_shard(s).expect("k shards of the right size");
    }
    let result = enc.encode().expect("all shards added");
    let mut out = Vec::with_capacity(r * shard);
    for s in result.recovery_iter() {
        out.extend_from_slice(s);
    }
    out
}

/// Rebuilds the missing data shards in `buf`, which holds `k + r` shards of `shard` bytes
/// end to end (data first, then recovery). `have[i]` says shard `i` is present. Needs at
/// least `k` present. Returns how many data shards it restored.
pub fn recover(
    dec: &mut Option<ReedSolomonDecoder>,
    buf: &mut [u8],
    have: &[bool],
    k: usize,
    r: usize,
    shard: usize,
) -> Result<usize, reed_solomon_simd::Error> {
    let missing = have[..k].iter().filter(|h| !**h).count();
    if missing == 0 {
        return Ok(0);
    }
    let dec = match dec {
        Some(d) => {
            d.reset(k, r, shard)?;
            d
        }
        None => dec.insert(ReedSolomonDecoder::new(k, r, shard)?),
    };
    for (i, s) in buf.chunks_exact(shard).enumerate() {
        if !have[i] {
            continue;
        }
        if i < k {
            dec.add_original_shard(i, s)?;
        } else {
            dec.add_recovery_shard(i - k, s)?;
        }
    }
    let result = dec.decode()?;
    let mut restored = 0;
    for (i, s) in result.restored_original_iter() {
        buf[i * shard..(i + 1) * shard].copy_from_slice(s);
        restored += 1;
    }
    Ok(restored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_matches_hand_computed_values() {
        // n = 2, p = 0.5: P[X > 0] = 0.75, P[X > 1] = 0.25.
        assert!((binomial_tail(2, 0.5, 0) - 0.75).abs() < 1e-12);
        assert!((binomial_tail(2, 0.5, 1) - 0.25).abs() < 1e-12);
        assert_eq!(binomial_tail(3, 0.5, 3), 0.0);
        // The normal approximation agrees with the exact sum where both apply.
        let exact = binomial_tail(3000, 0.02, 75);
        let approx = binomial_tail(3001, 0.02, 75);
        assert!((exact - approx).abs() < 0.01, "{exact} vs {approx}");
    }

    #[test]
    fn planner_scales_recovery_with_loss_and_size() {
        let mut p = FecPlanner::new(1e-4, 1.0, 1);
        assert_eq!(p.parity_for(100), 1, "no loss measured: the floor");
        p.set_loss(0.02);
        let small = p.parity_for(1);
        let big = p.parity_for(130);
        assert!(small >= 2, "a one-shard frame needs two copies more at 2%: {small}");
        assert!((5..=15).contains(&big), "130 shards at 2%: {big}");
        assert!((big as f64 / 130.0) < 0.12);
        p.set_loss(0.10);
        assert!(p.parity_for(130) > big);
    }

    #[test]
    fn layout_is_even_and_tight() {
        assert_eq!(layout(1, 1184), (1, 2));
        assert_eq!(layout(1184, 1184), (1, 1184));
        assert_eq!(layout(1185, 1184), (2, 594));
        let (k, s) = layout(156_250, 1184);
        assert!(k * s >= 156_250 && k * s - 156_250 < 2 * k);
    }

    #[test]
    fn any_k_shards_rebuild_the_frame() {
        let (k, r, shard) = (10, 4, 64);
        let data: Vec<u8> = (0..k * shard).map(|i| (i * 7) as u8).collect();
        let parity = encode(&mut None, &data, k, r, shard);
        assert_eq!(parity.len(), r * shard);
        let mut buf = [data.clone(), parity].concat();
        let mut have = vec![true; k + r];
        for lost in [0, 3, 4, 9] {
            have[lost] = false;
            buf[lost * shard..(lost + 1) * shard].fill(0);
        }
        let restored = recover(&mut None, &mut buf, &have, k, r, shard).unwrap();
        assert_eq!(restored, 4);
        assert_eq!(&buf[..k * shard], &data[..]);
    }

    #[test]
    fn too_few_shards_fail_cleanly() {
        let (k, r, shard) = (4, 1, 8);
        let data = vec![1u8; k * shard];
        let mut buf = [data.clone(), encode(&mut None, &data, k, r, shard)].concat();
        let have = [false, false, true, true, true];
        assert!(recover(&mut None, &mut buf, &have, k, r, shard).is_err());
    }
}
