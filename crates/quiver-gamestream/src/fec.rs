//! Reed-Solomon erasure coding over GF(2^8), the way Moonlight decodes it.
//!
//! Moonlight treats a block of `k` equal-sized data shards and `m` parity shards as a
//! systematic code: shard `i < k` is data, and parity row `j` is the GF(2^8) dot product of
//! row `j` of an `m × k` parity matrix with the data shards, byte by byte. Any `k` of the
//! `k + m` shards then recover the rest. The field uses the polynomial x⁸+x⁴+x³+x²+1 (0x11D).
//!
//! Two parity matrices are in use:
//! - **video**: a Cauchy matrix, row `j` column `i` = 1 / ((m + i) ⊕ j). It is built for any
//!   `k + m ≤ 255`.
//! - **audio**: always 4 data + 2 parity shards, with a fixed matrix inherited from
//!   GeForce Experience ([`ParityMatrix::audio`]). Moonlight hard-codes the same one.

use anyhow::{Result, bail};

/// The field's reducing polynomial (without the x⁸ term it is 0x1D).
const POLY: u16 = 0x11D;

/// The most shards (data plus parity) a block can have.
pub const MAX_SHARDS: usize = 255;

const fn build_exp() -> [u8; 512] {
    let mut exp = [0u8; 512];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        exp[i + 255] = x as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= POLY;
        }
        i += 1;
    }
    exp[510] = exp[0];
    exp[511] = exp[1];
    exp
}

const fn build_log(exp: &[u8; 512]) -> [u8; 256] {
    let mut log = [0u8; 256];
    let mut i = 0;
    while i < 255 {
        log[exp[i] as usize] = i as u8;
        i += 1;
    }
    log
}

static EXP: [u8; 512] = build_exp();
static LOG: [u8; 256] = build_log(&EXP);

/// a · b in GF(2^8).
pub fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    EXP[LOG[a as usize] as usize + LOG[b as usize] as usize]
}

/// 1 / a in GF(2^8). `a` must not be 0.
pub fn inv(a: u8) -> u8 {
    debug_assert!(a != 0, "0 has no inverse");
    EXP[255 - LOG[a as usize] as usize]
}

/// The 256 products `c · x`, so the inner loops are one lookup per byte.
fn mul_row(c: u8) -> [u8; 256] {
    let mut row = [0u8; 256];
    for (x, r) in row.iter_mut().enumerate() {
        *r = mul(c, x as u8);
    }
    row
}

/// `dst ^= c · src`, byte by byte.
fn mul_add(dst: &mut [u8], src: &[u8], c: u8) {
    match c {
        0 => {}
        1 => dst.iter_mut().zip(src).for_each(|(d, s)| *d ^= s),
        _ => {
            let row = mul_row(c);
            dst.iter_mut().zip(src).for_each(|(d, s)| *d ^= row[*s as usize]);
        }
    }
}

/// The `parity × data` matrix that makes the parity shards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParityMatrix {
    data: usize,
    parity: usize,
    /// Row-major, `parity` rows of `data` coefficients.
    rows: Vec<u8>,
}

impl ParityMatrix {
    /// The video matrix for `data` data shards and `parity` parity shards.
    pub fn cauchy(data: usize, parity: usize) -> Result<Self> {
        if data == 0 || parity == 0 || data + parity > MAX_SHARDS {
            bail!("a Reed-Solomon block can't have {data} data and {parity} parity shards");
        }
        let mut rows = Vec::with_capacity(data * parity);
        for j in 0..parity {
            for i in 0..data {
                rows.push(inv(((parity + i) ^ j) as u8));
            }
        }
        Ok(Self { data, parity, rows })
    }

    /// The audio matrix: 4 data shards, 2 parity shards. These coefficients are what
    /// GeForce Experience used and what every Moonlight client decodes with.
    pub fn audio() -> Self {
        Self { data: 4, parity: 2, rows: vec![0x77, 0x40, 0x38, 0x0e, 0xc7, 0xa7, 0x0d, 0x6c] }
    }

    pub fn data_shards(&self) -> usize {
        self.data
    }

    pub fn parity_shards(&self) -> usize {
        self.parity
    }

    /// Coefficient for parity row `j`, data column `i`.
    pub fn get(&self, j: usize, i: usize) -> u8 {
        self.rows[j * self.data + i]
    }

    /// Fills `parity` (one buffer per parity shard, each as long as the data shards) from
    /// `data` (all the same length).
    pub fn encode(&self, data: &[&[u8]], parity: &mut [&mut [u8]]) {
        assert_eq!(data.len(), self.data, "data shard count");
        assert_eq!(parity.len(), self.parity, "parity shard count");
        let len = data.first().map_or(0, |d| d.len());
        for (j, out) in parity.iter_mut().enumerate() {
            assert_eq!(out.len(), len, "parity shard length");
            out.fill(0);
            for (i, d) in data.iter().enumerate() {
                assert_eq!(d.len(), len, "data shard length");
                mul_add(out, d, self.get(j, i));
            }
        }
    }

    /// Fills in the missing data shards (`None`) from the shards present: data shards
    /// first (`0..data`), then parity (`data..data + parity`). Missing parity shards stay
    /// missing. Fails when fewer than `data` shards are present.
    pub fn reconstruct(&self, shards: &mut [Option<Vec<u8>>]) -> Result<()> {
        assert_eq!(shards.len(), self.data + self.parity, "shard count");
        let missing: Vec<usize> = (0..self.data).filter(|&i| shards[i].is_none()).collect();
        if missing.is_empty() {
            return Ok(());
        }
        let parity_present: Vec<usize> = (0..self.parity).filter(|&j| shards[self.data + j].is_some()).collect();
        if parity_present.len() < missing.len() {
            bail!("{} data shards lost, only {} parity shards to rebuild them", missing.len(), parity_present.len());
        }
        let len = shards.iter().flatten().next().map_or(0, Vec::len);
        let use_rows = &parity_present[..missing.len()];
        // For each parity row used: rhs = parity − Σ(known data · coefficient), which leaves
        // a square system in the missing data shards.
        let n = missing.len();
        let mut a = vec![0u8; n * n];
        let mut rhs: Vec<Vec<u8>> = Vec::with_capacity(n);
        for (r, &j) in use_rows.iter().enumerate() {
            let mut v = shards[self.data + j].clone().expect("present");
            for i in 0..self.data {
                match &shards[i] {
                    Some(d) => mul_add(&mut v, d, self.get(j, i)),
                    None => {}
                }
            }
            for (c, &m) in missing.iter().enumerate() {
                a[r * n + c] = self.get(j, m);
            }
            rhs.push(v);
        }
        // Gauss-Jordan elimination over GF(2^8).
        for col in 0..n {
            let pivot = (col..n).find(|&r| a[r * n + col] != 0);
            let Some(p) = pivot else { bail!("the parity rows don't determine the lost shards") };
            if p != col {
                for k in 0..n {
                    a.swap(p * n + k, col * n + k);
                }
                rhs.swap(p, col);
            }
            let scale = inv(a[col * n + col]);
            for k in 0..n {
                a[col * n + k] = mul(a[col * n + k], scale);
            }
            let row = mul_row(scale);
            rhs[col].iter_mut().for_each(|b| *b = row[*b as usize]);
            for r in 0..n {
                if r != col && a[r * n + col] != 0 {
                    let f = a[r * n + col];
                    for k in 0..n {
                        a[r * n + k] ^= mul(f, a[col * n + k]);
                    }
                    let (src, dst) = if r < col {
                        let (lo, hi) = rhs.split_at_mut(col);
                        (&hi[0], &mut lo[r])
                    } else {
                        let (lo, hi) = rhs.split_at_mut(r);
                        (&lo[col], &mut hi[0])
                    };
                    mul_add(dst, src, f);
                }
            }
        }
        for (c, &m) in missing.iter().enumerate() {
            debug_assert_eq!(rhs[c].len(), len);
            shards[m] = Some(std::mem::take(&mut rhs[c]));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_basics() {
        // 2 generates the field: 2^8 = 0x1D under 0x11D.
        assert_eq!(mul(0x80, 2), 0x1D);
        for a in 1..=255u8 {
            assert_eq!(mul(a, inv(a)), 1, "a = {a}");
            assert_eq!(mul(a, 1), a);
        }
        // Distributive over XOR.
        for (a, b, c) in [(3u8, 7u8, 200u8), (0x53, 0xCA, 0x11), (255, 254, 253)] {
            assert_eq!(mul(a, b ^ c), mul(a, b) ^ mul(a, c));
        }
        // Known inverses for this polynomial.
        assert_eq!(inv(2), 0x8E);
        assert_eq!(inv(3), 0xF4);
    }

    #[test]
    fn cauchy_known_rows() {
        // Row j, column i = 1 / ((m + i) ^ j). For 4 + 2: row 0 is 1/2, 1/3, 1/4, 1/5.
        let m = ParityMatrix::cauchy(4, 2).unwrap();
        assert_eq!([m.get(0, 0), m.get(0, 1), m.get(0, 2), m.get(0, 3)], [inv(2), inv(3), inv(4), inv(5)]);
        assert_eq!([m.get(1, 0), m.get(1, 1)], [inv(3), inv(2)]);
        assert!(ParityMatrix::cauchy(200, 56).is_err());
        assert!(ParityMatrix::cauchy(200, 55).is_ok());
    }

    #[test]
    fn known_parity_vector() {
        // One-byte shards make the parity a plain dot product we can check by hand.
        let m = ParityMatrix::cauchy(2, 1).unwrap(); // row 0: 1/1, 1/2
        let d0 = [0x10u8];
        let d1 = [0x01u8];
        let mut p0 = [0u8];
        m.encode(&[&d0, &d1], &mut [&mut p0]);
        assert_eq!(p0[0], 0x10 ^ inv(2));
        // The audio matrix: p0 = 0x77·a ⊕ 0x40·b ⊕ 0x38·c ⊕ 0x0e·d.
        let a = ParityMatrix::audio();
        let data = [[1u8], [0u8], [0u8], [0u8]];
        let mut p = [[0u8], [0u8]];
        let refs: Vec<&[u8]> = data.iter().map(|d| &d[..]).collect();
        let [p0, p1] = &mut p;
        a.encode(&refs, &mut [&mut p0[..], &mut p1[..]]);
        assert_eq!((p[0][0], p[1][0]), (0x77, 0xc7));
    }

    fn roundtrip(matrix: &ParityMatrix, len: usize, lose: &[usize]) {
        let k = matrix.data_shards();
        let m = matrix.parity_shards();
        let data: Vec<Vec<u8>> = (0..k).map(|i| (0..len).map(|b| (i * 31 + b * 7 + (b >> 3)) as u8).collect()).collect();
        let mut parity: Vec<Vec<u8>> = vec![vec![0; len]; m];
        {
            let refs: Vec<&[u8]> = data.iter().map(Vec::as_slice).collect();
            let mut outs: Vec<&mut [u8]> = parity.iter_mut().map(Vec::as_mut_slice).collect();
            matrix.encode(&refs, &mut outs);
        }
        let mut shards: Vec<Option<Vec<u8>>> = data.iter().cloned().chain(parity).map(Some).collect();
        for &i in lose {
            shards[i] = None;
        }
        matrix.reconstruct(&mut shards).unwrap();
        for i in 0..k {
            assert_eq!(shards[i].as_ref().unwrap(), &data[i], "shard {i} after losing {lose:?}");
        }
    }

    #[test]
    fn recovers_any_k_of_n() {
        let m = ParityMatrix::cauchy(10, 3).unwrap();
        roundtrip(&m, 64, &[]);
        roundtrip(&m, 64, &[0]);
        roundtrip(&m, 64, &[9, 3, 5]);
        roundtrip(&m, 64, &[1, 10, 12]); // two parity lost, one data
        let big = ParityMatrix::cauchy(212, 43).unwrap();
        roundtrip(&big, 32, &(0..43).map(|i| i * 4).collect::<Vec<_>>());
        roundtrip(&ParityMatrix::audio(), 100, &[0, 3]);
        roundtrip(&ParityMatrix::audio(), 100, &[2, 4]);
    }

    #[test]
    fn too_many_losses_fail() {
        let m = ParityMatrix::cauchy(4, 2).unwrap();
        let mut shards: Vec<Option<Vec<u8>>> = vec![None, None, None, Some(vec![1]), Some(vec![2]), Some(vec![3])];
        assert!(m.reconstruct(&mut shards).is_err());
    }
}
