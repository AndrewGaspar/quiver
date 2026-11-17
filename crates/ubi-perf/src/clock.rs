//! Clocks (SIGHT-HUD §3.9). Everything ends up as CLOCK_MONOTONIC_RAW nanoseconds.
//!
//! Wine's `QueryPerformanceCounter` is `clock_gettime(CLOCK_MONOTONIC_RAW)` in 100 ns ticks
//! (`dlls/ntdll/unix/sync.c`: `monotonic_counter()`, `TICKSPERSEC` = 10 MHz), so a mod's QPC
//! value is `floor(raw_ns / 100)` and `qpc × 100` is the RAW time to within 100 ns. A future
//! Wine that moves QPC to the TSC would show up as a different `clock_freq` in the ring
//! header or as a systematic offset in [`OffsetCheck`]; then quiverd fits a [`ClockMap`].

/// Wine's QPC frequency (`TICKSPERSEC`).
pub const WINE_QPC_FREQ: u64 = 10_000_000;
pub const NS_PER_SEC: u64 = 1_000_000_000;

/// CLOCK_MONOTONIC_RAW now, in ns.
#[cfg(all(feature = "std", unix))]
pub fn mono_raw_ns() -> i64 {
    clock_ns(libc::CLOCK_MONOTONIC_RAW)
}

/// CLOCK_MONOTONIC now, in ns (Wayland presentation feedback's clock).
#[cfg(all(feature = "std", unix))]
pub fn mono_ns() -> i64 {
    clock_ns(libc::CLOCK_MONOTONIC)
}

#[cfg(all(feature = "std", unix))]
#[allow(clippy::unnecessary_cast)] // the fields are i32 on 32-bit targets (a 32-bit layer)
fn clock_ns(id: libc::clockid_t) -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointer; these clocks always exist on Linux.
    unsafe { libc::clock_gettime(id, &mut ts) };
    ts.tv_sec as i64 * NS_PER_SEC as i64 + ts.tv_nsec as i64
}

/// `QueryPerformanceCounter` (on Windows; under Wine this is RAW / 100).
#[cfg(all(feature = "std", windows))]
pub fn qpc() -> (i64, u64) {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn QueryPerformanceCounter(c: *mut i64) -> i32;
        fn QueryPerformanceFrequency(f: *mut i64) -> i32;
    }
    let (mut c, mut f) = (0i64, 0i64);
    // SAFETY: valid pointers.
    unsafe {
        QueryPerformanceCounter(&mut c);
        QueryPerformanceFrequency(&mut f);
    }
    (c, f as u64)
}

/// The QPC value Wine returns at RAW time `ns`.
pub const fn raw_ns_to_wine_qpc(ns: i64) -> i64 {
    ns.div_euclid(100)
}

/// The RAW time (lower bound, within 100 ns) of a Wine QPC value.
pub const fn wine_qpc_to_raw_ns(qpc: i64) -> i64 {
    qpc.saturating_mul(100)
}

/// Ticks at `freq` per second to ns, exactly (128-bit), saturating; `None` if `freq` is 0.
pub fn ticks_to_ns(ticks: i64, freq: u64) -> Option<i64> {
    if freq == 0 {
        return None;
    }
    let ns = ticks as i128 * NS_PER_SEC as i128 / freq as i128;
    Some(ns.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
}

/// ns to ticks at `freq` per second (floor), saturating.
pub fn ns_to_ticks(ns: i64, freq: u64) -> Option<i64> {
    if freq == 0 {
        return None;
    }
    let t = (ns as i128 * freq as i128).div_euclid(NS_PER_SEC as i128);
    Some(t.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
}

/// `ns = a · ticks + b`: a producer clock mapped onto CLOCK_MONOTONIC_RAW.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockMap {
    /// ns per tick.
    pub a: f64,
    /// ns at tick 0.
    pub b: f64,
}

impl ClockMap {
    /// The exact mapping a ring's `clock_freq` implies (Wine: 100 ns per tick, no offset).
    pub fn from_freq(freq: u64) -> ClockMap {
        ClockMap { a: NS_PER_SEC as f64 / freq.max(1) as f64, b: 0.0 }
    }

    pub fn wine_qpc() -> ClockMap {
        ClockMap::from_freq(WINE_QPC_FREQ)
    }

    pub fn to_ns(&self, ticks: i64) -> i64 {
        let ns = self.a * ticks as f64 + self.b;
        if ns.is_finite() { ns as i64 } else { 0 }
    }

    /// Least-squares fit over `(ticks, ns)` pairs (centred, so large timestamps keep their
    /// precision). `None` with fewer than 2 pairs or no spread in `ticks`.
    pub fn fit(pairs: &[(i64, i64)]) -> Option<ClockMap> {
        if pairs.len() < 2 {
            return None;
        }
        let (t0, n0) = pairs[0];
        let n = pairs.len() as f64;
        let (mut sx, mut sy) = (0.0, 0.0);
        for &(t, ns) in pairs {
            sx += (t - t0) as f64;
            sy += (ns - n0) as f64;
        }
        let (mx, my) = (sx / n, sy / n);
        let (mut sxx, mut sxy) = (0.0, 0.0);
        for &(t, ns) in pairs {
            let dx = (t - t0) as f64 - mx;
            sxx += dx * dx;
            sxy += dx * ((ns - n0) as f64 - my);
        }
        if sxx == 0.0 {
            return None;
        }
        let a = sxy / sxx;
        // ns - n0 = a (t - t0) + c  →  ns = a t + (n0 - a t0 + c)
        let c = my - a * mx;
        Some(ClockMap { a, b: n0 as f64 - a * t0 as f64 + c })
    }
}

/// How a producer's timestamps line up with ours, from correlated pairs
/// `(producer ticks, our RAW ns)` for the same event (a mod's present call and the layer's
/// present entry for that frame; or two clock reads back to back).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OffsetCheck {
    pub samples: usize,
    /// Median of `ours − map(theirs)`, ns.
    pub median_ns: i64,
    /// 10th and 90th percentiles of the same, ns.
    pub p10_ns: i64,
    pub p90_ns: i64,
}

impl OffsetCheck {
    /// `None` without samples. Allocation-free: sorts `scratch` (resized by the caller to
    /// at least `pairs.len()`).
    pub fn measure(pairs: &[(i64, i64)], map: &ClockMap, scratch: &mut [i64]) -> Option<OffsetCheck> {
        let n = pairs.len().min(scratch.len());
        if n == 0 {
            return None;
        }
        for (s, &(t, ns)) in scratch.iter_mut().zip(pairs) {
            *s = ns.saturating_sub(map.to_ns(t));
        }
        let d = &mut scratch[..n];
        d.sort_unstable();
        let pct = |p: usize| d[(n - 1) * p / 100];
        Some(OffsetCheck { samples: n, median_ns: pct(50), p10_ns: pct(10), p90_ns: pct(90) })
    }

    /// The mapping is believable: the median offset lies in `expected` (for present pairs,
    /// `0..=50 ms`: DXVK presents a few ms after the DXGI call) and the spread is under
    /// `max_spread_ns`. Otherwise mark the producer `clock: suspect` and fit a map.
    pub fn is_consistent(&self, expected: core::ops::RangeInclusive<i64>, max_spread_ns: i64) -> bool {
        expected.contains(&self.median_ns) && self.p90_ns.saturating_sub(self.p10_ns) <= max_spread_ns
    }
}

/// Offset to add to a CLOCK_MONOTONIC time to get CLOCK_MONOTONIC_RAW, from reads taken
/// back to back (the tightest of a few tries). The two drift apart slowly (NTP slews
/// MONOTONIC only), so resample now and then.
#[cfg(all(feature = "std", unix))]
pub fn mono_to_raw_offset() -> i64 {
    let mut best = (i64::MAX, 0);
    for _ in 0..5 {
        let a = mono_ns();
        let r = mono_raw_ns();
        let b = mono_ns();
        if b - a < best.0 {
            best = (b - a, r - (a + (b - a) / 2));
        }
    }
    best.1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wine_qpc_conversions() {
        assert_eq!(raw_ns_to_wine_qpc(1_234_567_899), 12_345_678);
        assert_eq!(wine_qpc_to_raw_ns(12_345_678), 1_234_567_800);
        assert_eq!(raw_ns_to_wine_qpc(-1), -1);
        assert_eq!(ticks_to_ns(12_345_678, WINE_QPC_FREQ), Some(1_234_567_800));
        assert_eq!(ns_to_ticks(1_234_567_899, WINE_QPC_FREQ), Some(12_345_678));
        assert_eq!(ticks_to_ns(3, 0), None);
        assert_eq!(ticks_to_ns(i64::MAX, 1), Some(i64::MAX));
        assert_eq!(ClockMap::wine_qpc().to_ns(10), 1000);
        // Property: qpc(ns) * 100 ≤ ns < (qpc(ns) + 1) * 100.
        for _ in 0..10_000 {
            let ns = fastrand::i64(0..i64::MAX / 2);
            let q = raw_ns_to_wine_qpc(ns);
            assert!(wine_qpc_to_raw_ns(q) <= ns && ns < wine_qpc_to_raw_ns(q + 1));
            assert_eq!(ticks_to_ns(q, WINE_QPC_FREQ), Some(wine_qpc_to_raw_ns(q)));
            assert_eq!(ns_to_ticks(ns, WINE_QPC_FREQ), Some(q));
        }
    }

    #[test]
    fn fit_recovers_a_tsc_like_clock() {
        // A 3 GHz TSC with an offset; ns timestamps around 10^13 (days of uptime).
        let (a, b) = (1.0 / 3.0, 7_000_000_000_000.0);
        let pairs: Vec<(i64, i64)> = (0..200)
            .map(|i| {
                let t = 9_000_000_000_000i64 + i * 50_000_000 + fastrand::i64(0..1000);
                (t, (a * t as f64 + b) as i64 + fastrand::i64(-500..500))
            })
            .collect();
        let m = ClockMap::fit(&pairs).unwrap();
        assert!((m.a - a).abs() < 1e-6, "{m:?}");
        for &(t, ns) in &pairs {
            assert!((m.to_ns(t) - ns).abs() < 2_000, "{m:?}");
        }
        assert_eq!(ClockMap::fit(&pairs[..1]), None);
        assert_eq!(ClockMap::fit(&[(5, 1), (5, 2)]), None);
    }

    #[test]
    fn offset_check() {
        // Layer presents 2-6 ms after the mod's present call, in QPC.
        let pairs: Vec<(i64, i64)> = (0..100)
            .map(|i| {
                let ns = 50_000_000_000 + i * 16_000_000;
                (raw_ns_to_wine_qpc(ns), ns + 2_000_000 + fastrand::i64(0..4_000_000))
            })
            .collect();
        let mut scratch = vec![0; pairs.len()];
        let c = OffsetCheck::measure(&pairs, &ClockMap::wine_qpc(), &mut scratch).unwrap();
        assert!(c.median_ns > 2_000_000 && c.median_ns < 6_000_000, "{c:?}");
        assert!(c.is_consistent(0..=50_000_000, 10_000_000));
        // A QPC that isn't RAW/100 (say, 1.5 s off): inconsistent.
        let shifted: Vec<_> = pairs.iter().map(|&(q, ns)| (q + 15_000_000, ns)).collect();
        let c = OffsetCheck::measure(&shifted, &ClockMap::wine_qpc(), &mut scratch).unwrap();
        assert!(!c.is_consistent(0..=50_000_000, 10_000_000), "{c:?}");
        assert_eq!(OffsetCheck::measure(&[], &ClockMap::wine_qpc(), &mut scratch), None);
    }

    #[test]
    #[cfg(unix)]
    fn clocks_read() {
        let a = mono_raw_ns();
        let b = mono_raw_ns();
        assert!(a > 0 && b >= a);
        let off = mono_to_raw_offset();
        assert!((mono_ns() + off - mono_raw_ns()).abs() < 50_000_000);
    }
}
