//! Exponents: extraction (§8.2.7), preprocessing and differential coding
//! (§8.2.10, the inverse of §7.1.3), and the choice of exponent strategy by
//! cost (§8.2.8).
//!
//! Strategy codes follow Table 7.4: 0 = reuse, 1 = D15, 2 = D25, 3 = D45.

// Bins index the wanted, raw and coded exponent arrays together.
#![allow(clippy::needless_range_loop)]

use crate::tables::FRMEXPSTR;

pub(super) const REUSE: u8 = 0;
pub(super) const D15: u8 = 1;
pub(super) const D25: u8 = 2;
pub(super) const D45: u8 = 3;

/// §8.2.7: the number of leading zeros of a coefficient's binary fraction,
/// up to 24 — the `e` with `2^-(e+1) ≤ |x| < 2^-e`. Values of magnitude 1 or
/// more get 0 (their mantissa saturates), zero gets 24.
pub(super) fn raw_exponent(x: f32) -> u8 {
    let a = x.abs();
    if a >= 1.0 {
        return 0;
    }
    let biased = (a.to_bits() >> 23) & 0xff;
    // |x| = 1.m · 2^(biased−127), so e = 126 − biased.
    if biased == 0 { 24 } else { (126 - biased as i32).clamp(0, 24) as u8 }
}

/// Exponents per group for a strategy (Table 7.4 / §7.1.3).
pub(super) fn grpsize(expstr: u8) -> usize {
    match expstr {
        D15 => 1,
        D25 => 2,
        _ => 4,
    }
}

/// What kind of exponent set: the first value's meaning and range differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Set {
    /// Full-bandwidth or LFE channel over bins `0..end`: the first exponent
    /// is bin 0's, sent in 4 bits (so at most 15).
    Absolute,
    /// Coupling channel over `start..end`: a 4-bit reference (`cplabsexp`,
    /// sent halved) that belongs to no bin, then groups.
    Coupling,
}

/// One coded exponent set: what is written and what the decoder rebuilds.
#[derive(Clone, Debug)]
pub(super) struct Coded {
    /// `exps[ch][0]` / `lfeexps[0]` (bin 0's exponent) or `cplabsexp`
    /// (the reference exponent halved).
    pub abs: u8,
    /// The 7-bit grouped differentials.
    pub groups: Vec<u8>,
    /// The exponent of every bin `start..end` as the decoder will have it,
    /// indexed by absolute bin.
    pub exps: [u8; 256],
}

/// Number of groups for a set (§7.1.3): `nchgrps` / `ncplgrps` / `nlfegrps`.
pub(super) fn ngroups(set: Set, start: usize, end: usize, expstr: u8) -> usize {
    match set {
        Set::Absolute => (end - 1).div_ceil(3 * grpsize(expstr)),
        Set::Coupling => (end - start) / (3 * grpsize(expstr)),
    }
}

/// Bits an exponent set costs (absolute value plus groups; `gainrng` and
/// `chbwcod` are the caller's).
pub(super) fn set_bits(set: Set, start: usize, end: usize, expstr: u8) -> usize {
    4 + 7 * ngroups(set, start, end, expstr)
}

/// §8.2.10: from the wanted exponent of each bin `start..end` (the minimum
/// over the blocks that will share the set), build the coded set — exponents
/// shared over `grpsize` bins take the group's minimum, then successive
/// values are lowered until every difference is within ±2. Exponents only
/// ever go down, so no mantissa overflows.
pub(super) fn encode_set(want: &[u8; 256], set: Set, start: usize, end: usize, expstr: u8) -> Coded {
    let mut seq = [0i32; MAX_SEQ];
    let n = sequence(want, set, start, end, expstr, &mut seq);
    let mut groups = Vec::with_capacity(n / 3);
    for grp in 0..n / 3 {
        let d = |j: usize| seq[1 + 3 * grp + j] - seq[3 * grp + j] + 2;
        debug_assert!((0..3).all(|j| (0..=4).contains(&d(j))));
        groups.push((25 * d(0) + 5 * d(1) + d(2)) as u8);
    }
    let mut exps = [0u8; 256];
    rebuild(&seq, set, start, end, expstr, &mut exps);
    let abs = match set {
        Set::Absolute => seq[0] as u8,
        Set::Coupling => (seq[0] >> 1) as u8,
    };
    Coded { abs, groups, exps }
}

/// Longest exponent sequence: the reference and three per group of up to
/// 253 / 3 groups of one bin.
const MAX_SEQ: usize = 1 + 3 * 85;

/// The coded exponent sequence of [`encode_set`] (`seq[0]` the absolute or
/// reference value, then one per differential) into `seq`; returns the
/// number of differentials.
fn sequence(want: &[u8; 256], set: Set, start: usize, end: usize, expstr: u8, seq: &mut [i32; MAX_SEQ]) -> usize {
    let g = grpsize(expstr);
    let ngrps = ngroups(set, start, end, expstr);
    let seq = &mut seq[..1 + 3 * ngrps];
    let first_bin = match set {
        Set::Absolute => {
            seq[0] = i32::from(want[start].min(15));
            start + 1
        }
        Set::Coupling => start,
    };
    let mut last = 24i32;
    for (i, s) in seq.iter_mut().enumerate().skip(1) {
        let lo = first_bin + (i - 1) * g;
        let hi = (lo + g).min(end);
        if lo < end {
            last = want[lo..hi].iter().map(|&e| i32::from(e)).min().unwrap_or(24);
        }
        *s = last; // past `end`: repeat (difference 0)
    }
    if set == Set::Coupling {
        // The reference is a free, even value; start from the first group
        // and let the passes below settle it.
        seq[0] = seq.get(1).copied().unwrap_or(0);
    }
    for i in 1..seq.len() {
        seq[i] = seq[i].min(seq[i - 1] + 2);
    }
    for i in (1..seq.len()).rev() {
        seq[i - 1] = seq[i - 1].min(seq[i] + 2);
    }
    if set == Set::Coupling {
        // cplabsexp carries the reference halved, so it must be even.
        // Started equal to the first group and lowered by the backward pass
        // only to within 2 of it, the reference is in seq[1]..=seq[1] + 2;
        // rounding down to even keeps it in seq[1] − 1..=seq[1] + 2.
        seq[0] &= !1;
    }
    3 * ngrps
}

/// The exponent of every bin `start..end` as the decoder rebuilds it from
/// the sequence, into `exps`.
fn rebuild(seq: &[i32; MAX_SEQ], set: Set, start: usize, end: usize, expstr: u8, exps: &mut [u8; 256]) {
    let g = grpsize(expstr);
    let first_bin = match set {
        Set::Absolute => {
            exps[start] = seq[0] as u8;
            start + 1
        }
        Set::Coupling => start,
    };
    if first_bin < end {
        for (i, chunk) in exps[first_bin..end].chunks_mut(g).enumerate() {
            chunk.fill(seq[1 + i] as u8);
        }
    }
}

/// The per-channel input to the strategy choice: one entry per block.
pub(super) struct Track<'a> {
    /// Raw exponents per block, `None` where the set is absent (coupling not
    /// in use, say).
    pub raw: Vec<Option<&'a [u8; 256]>>,
    /// Blocks that must carry new exponents: block 0, a changed `start..end`,
    /// the first block after an absent one.
    pub must_new: Vec<bool>,
    pub start: Vec<usize>,
    pub end: Vec<usize>,
    pub set: Set,
    /// Strategies to consider for a new set.
    pub choices: &'a [u8],
    /// Mantissa bits a unit of exponent underestimate is assumed to cost
    /// per bin (§8.2.8's trade-off, made explicit).
    pub lambda: f32,
    /// Fixed bits per new set beyond the exponents (`gainrng`, `chbwcod`).
    pub overhead: usize,
}

/// Cost of covering blocks `b..b+k` with one set of strategy `s`.
pub(super) fn run_cost(t: &Track, b: usize, k: usize, s: u8) -> f32 {
    run_cost_for(t, b, k, s, &min_exps(t, b, k))
}

/// [`run_cost`] given the run's wanted exponents ([`min_exps`]).
fn run_cost_for(t: &Track, b: usize, k: usize, s: u8, want: &[u8; 256]) -> f32 {
    let (start, end) = (t.start[b], t.end[b]);
    // The decoder's exponents of the set, without packing the groups.
    let mut seq = [0i32; MAX_SEQ];
    sequence(want, t.set, start, end, s, &mut seq);
    let mut exps = [0u8; 256];
    rebuild(&seq, t.set, start, end, s, &mut exps);
    let mut penalty = 0u32;
    for blk in b..b + k {
        let raw = t.raw[blk].expect("present");
        for bin in start..end {
            penalty += u32::from((raw[bin] - exps[bin]).min(6));
        }
    }
    (set_bits(t.set, start, end, s) + t.overhead) as f32 + t.lambda * penalty as f32
}

fn min_exps(t: &Track, b: usize, k: usize) -> [u8; 256] {
    let mut want = [24u8; 256];
    for blk in b..b + k {
        let raw = t.raw[blk].expect("present");
        for bin in t.start[b]..t.end[b] {
            want[bin] = want[bin].min(raw[bin]);
        }
    }
    want
}

/// §8.2.8 by cost: the per-block strategies (REUSE where a block reuses the
/// previous set; REUSE for absent blocks too) minimising exponent bits plus
/// `lambda` × the exponent underestimate the sharing causes, over every
/// split of the blocks into runs — a shortest path over at most six blocks.
pub(super) fn choose(t: &Track) -> Vec<u8> {
    let nb = t.raw.len();
    let mut best = vec![f32::INFINITY; nb + 1];
    let mut from = vec![(0usize, 0u8); nb + 1];
    best[0] = 0.0;
    for b in 0..nb {
        if !best[b].is_finite() {
            continue;
        }
        if t.raw[b].is_none() {
            if best[b] < best[b + 1] {
                best[b + 1] = best[b];
                from[b + 1] = (b, REUSE);
            }
            continue;
        }
        let mut k = 1;
        // The run's wanted exponents, extended one block at a time.
        let mut want = [24u8; 256];
        loop {
            let raw = t.raw[b + k - 1].expect("present");
            for bin in t.start[b]..t.end[b] {
                want[bin] = want[bin].min(raw[bin]);
            }
            for &s in t.choices {
                let c = best[b] + run_cost_for(t, b, k, s, &want);
                if c < best[b + k] {
                    best[b + k] = c;
                    from[b + k] = (b, s);
                }
            }
            let next = b + k;
            if next >= nb || t.raw[next].is_none() || t.must_new[next] {
                break;
            }
            k += 1;
        }
    }
    let mut out = vec![REUSE; nb];
    let mut e = nb;
    while e > 0 {
        let (b, s) = from[e];
        out[b] = s;
        e = b;
    }
    out
}

/// The same choice restricted to the 32 frame strategies of Table E2.10
/// (E-AC-3, six blocks, `expstre = 0`): the row index and its cost, or `None`
/// if no row is valid (a row must not reuse where a new set is required).
pub(super) fn choose_frame_row(t: &Track) -> Option<(usize, f32)> {
    let mut best: Option<(usize, f32)> = None;
    // The rows share most of their runs: each (start, length, strategy)
    // costed once.
    let mut memo = [[[f32::NAN; 4]; 7]; 6];
    let mut run_cost = |b: usize, k: usize, s: u8| {
        let m = &mut memo[b][k][usize::from(s)];
        if m.is_nan() {
            *m = run_cost(t, b, k, s);
        }
        *m
    };
    'rows: for (row, strat) in FRMEXPSTR.iter().enumerate() {
        let mut cost = 0.0;
        let mut b = 0;
        while b < 6 {
            if t.raw[b].is_none() {
                b += 1;
                continue;
            }
            let s = strat[b];
            if s == REUSE {
                // reuse with nothing before it in this run
                continue 'rows;
            }
            let mut k = 1;
            while b + k < 6 && strat[b + k] == REUSE && t.raw[b + k].is_some() {
                if t.must_new[b + k] {
                    continue 'rows;
                }
                k += 1;
            }
            cost += run_cost(b, k, s);
            b += k;
        }
        if best.is_none_or(|(_, c)| cost < c) {
            best = Some((row, cost));
        }
    }
    best
}

/// The coded exponents per block for a strategy sequence: `None` for absent
/// blocks, the shared set repeated for reused blocks.
pub(super) fn code_track(t: &Track, strat: &[u8]) -> Vec<Option<(u8, std::rc::Rc<Coded>)>> {
    let nb = t.raw.len();
    let mut out: Vec<Option<(u8, std::rc::Rc<Coded>)>> = vec![None; nb];
    let mut b = 0;
    while b < nb {
        if t.raw[b].is_none() {
            b += 1;
            continue;
        }
        let s = strat[b];
        debug_assert_ne!(s, REUSE, "reuse at the start of a run");
        let mut k = 1;
        while b + k < nb && strat[b + k] == REUSE && t.raw[b + k].is_some() {
            k += 1;
        }
        let want = min_exps(t, b, k);
        let coded = std::rc::Rc::new(encode_set(&want, t.set, t.start[b], t.end[b], s));
        out[b] = Some((s, coded.clone()));
        for o in out.iter_mut().skip(b + 1).take(k - 1) {
            *o = Some((REUSE, coded.clone()));
        }
        b += k;
    }
    out
}

/// §7.1.3 grouping, decoded back: the exponents a decoder rebuilds from
/// `abs` and `groups`, for checking `encode_set` against the spec's own
/// unpacking arithmetic.
#[cfg(test)]
fn unpack(abs: u8, groups: &[u8], g: usize) -> Vec<i32> {
    let mut out = vec![i32::from(abs)];
    let mut prev = i32::from(abs);
    for &v in groups {
        let v = i32::from(v);
        for d in [v / 25, (v % 25) / 5, v % 5] {
            prev += d - 2;
            for _ in 0..g {
                out.push(prev);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_exponents_count_leading_zeros() {
        assert_eq!(raw_exponent(0.0), 24);
        assert_eq!(raw_exponent(0.75), 0);
        assert_eq!(raw_exponent(-0.5), 0);
        assert_eq!(raw_exponent(0.4999), 1);
        assert_eq!(raw_exponent(0.25), 1);
        assert_eq!(raw_exponent(0.125), 2);
        assert_eq!(raw_exponent(1.5), 0);
        assert_eq!(raw_exponent(2f32.powi(-24)), 23);
        assert_eq!(raw_exponent(2f32.powi(-30)), 24);
        for e in 0..24 {
            let x = 0.7 * 2f32.powi(-e);
            assert_eq!(raw_exponent(x), e as u8);
        }
    }

    /// A hand-worked D25 case: the shared exponents take the pair minimum,
    /// the slew limit lowers what rises too fast, and the §7.1.3 unpacking
    /// gives back exactly `Coded::exps`.
    #[test]
    fn d25_preprocessing_by_hand() {
        let mut want = [24u8; 256];
        // bin 0: 9; pairs (1,2)=(4,6)→4, (3,4)=(10,12)→10, (5,6)=(3,3)→3,
        // bins 7.. stay 24 up to end = 13 → pairs (7,8),(9,10),(11,12) = 24.
        let vals = [9u8, 4, 6, 10, 12, 3, 3, 24, 24, 24, 24, 24, 24];
        want[..13].copy_from_slice(&vals);
        let c = encode_set(&want, Set::Absolute, 0, 13, D25);
        // ngrps = ceil(12 / 6) = 2 → 6 differentials.
        assert_eq!(c.groups.len(), 2);
        // wanted sequence 9, 4, 10, 3, 24, 24, 24
        // forward (+2 max): 9, 4, 6, 3, 5, 7, 9
        // backward (−2 max going up-stream): 3 ≥ ... 6 → min(6, 3+2) = 5; 4 ok; 9 → min(9, 4+2) = 6
        // → 6, 4, 5, 3, 5, 7, 9
        let seq = [6, 4, 5, 3, 5, 7, 9];
        assert_eq!(c.abs, 6);
        let expect: Vec<u8> = vec![6, 4, 4, 5, 5, 3, 3, 5, 5, 7, 7, 9, 9];
        assert_eq!(&c.exps[..13], &expect[..]);
        // differentials +2-offset: (4−6, 5−4, 3−5) = (−2, 1, −2) → (0, 3, 0) → 15
        //                           (5−3, 7−5, 9−7) = (2, 2, 2) → (4, 4, 4) → 124
        assert_eq!(c.groups, vec![15, 124]);
        let un = unpack(c.abs, &c.groups, 2);
        assert_eq!(&un[..7], &[seq[0], seq[1], seq[1], seq[2], seq[2], seq[3], seq[3]]);
        // never above what was wanted
        assert!((0..13).all(|b| c.exps[b] <= want[b]));
    }

    #[test]
    fn coupling_sets_use_an_even_reference_and_whole_groups() {
        let mut want = [24u8; 256];
        for (i, w) in want.iter_mut().enumerate().take(133).skip(109) {
            *w = 5 + ((i * 7) % 9) as u8;
        }
        for s in [D15, D25, D45] {
            let c = encode_set(&want, Set::Coupling, 109, 133, s);
            assert_eq!(c.groups.len(), 24 / (3 * grpsize(s)));
            let un = unpack(c.abs * 2, &c.groups, grpsize(s));
            assert_eq!(un.len(), 1 + 24);
            for bin in 109..133 {
                assert_eq!(un[1 + bin - 109], i32::from(c.exps[bin]));
                assert!(c.exps[bin] <= want[bin]);
            }
        }
    }

    #[test]
    fn random_sets_round_trip_through_the_unpacking() {
        let mut s = 0x1234_5678u32;
        for _ in 0..200 {
            let mut want = [0u8; 256];
            for w in want.iter_mut() {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                *w = (s % 25) as u8;
            }
            let end = 37 + 3 * (s as usize % 73);
            for st in [D15, D25, D45] {
                let c = encode_set(&want, Set::Absolute, 0, end, st);
                let un = unpack(c.abs, &c.groups, grpsize(st));
                for bin in 0..end {
                    assert_eq!(un[bin], i32::from(c.exps[bin]));
                    assert!(c.exps[bin] <= want[bin] && c.exps[bin] <= 24);
                }
                assert!(c.abs <= 15);
            }
        }
    }

    #[test]
    fn stationary_input_shares_one_set_and_a_transient_splits_it() {
        let flat = [8u8; 256];
        let mut loud = [8u8; 256];
        for v in loud.iter_mut().take(200) {
            *v = 1;
        }
        let mk = |raw: Vec<Option<&'static [u8; 256]>>| raw;
        let flat: &'static [u8; 256] = Box::leak(Box::new(flat));
        let loud: &'static [u8; 256] = Box::leak(Box::new(loud));
        let t = Track {
            raw: mk(vec![Some(flat); 6]),
            must_new: vec![true, false, false, false, false, false],
            start: vec![0; 6],
            end: vec![253; 6],
            set: Set::Absolute,
            choices: &[D15, D25, D45],
            lambda: 1.0,
            overhead: 2,
        };
        let s = choose(&t);
        assert_ne!(s[0], REUSE);
        assert!(s[1..].iter().all(|&x| x == REUSE), "{s:?}");
        let t2 = Track { raw: mk(vec![Some(flat), Some(flat), Some(flat), Some(loud), Some(loud), Some(loud)]), ..t };
        let s2 = choose(&t2);
        assert_ne!(s2[3], REUSE, "the level change must start a new set: {s2:?}");
        assert!(choose_frame_row(&t2).is_some());
    }
}
