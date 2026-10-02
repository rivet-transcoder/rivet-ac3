//! The parametric bit allocation routine, A/52:2018 §7.2.2 (PDF p.64–70),
//! plus the E-AC-3 `hebap` variant of the final step (Annex E §3.4.3.1).
//!
//! Every step is the spec's pseudo code in fixed-point integer arithmetic;
//! this must be bit-exact with the encoder or the mantissa field widths
//! diverge and the block cannot be parsed. Nothing here is approximated.

use super::tables::{
    BAPTAB, BNDSZ, BNDTAB, DBPBTAB, FASTDEC, FASTGAIN, FLOORTAB, HEBAPTAB, HTH, LATAB, MASKTAB,
    SLOWDEC, SLOWGAIN,
};

/// The five per-block bit-allocation parameters after the Table 7.6–7.10
/// lookups (§7.2.2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BaParams {
    pub sdecay: i32,
    pub fdecay: i32,
    pub sgain: i32,
    pub dbknee: i32,
    pub floor: i32,
}

impl BaParams {
    pub fn from_codes(sdcycod: u8, fdcycod: u8, sgaincod: u8, dbpbcod: u8, floorcod: u8) -> Self {
        Self {
            sdecay: SLOWDEC[sdcycod as usize & 3],
            fdecay: FASTDEC[fdcycod as usize & 3],
            sgain: SLOWGAIN[sgaincod as usize & 3],
            dbknee: DBPBTAB[dbpbcod as usize & 3],
            floor: FLOORTAB[floorcod as usize & 7],
        }
    }
}

/// Delta bit allocation side information for one channel (§5.4.3.50–57).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct DeltaBa {
    /// Number of segments, 1..=8 (the coded 3-bit value plus one); 0 = none.
    pub nseg: u8,
    pub offst: [u8; 8],
    pub len: [u8; 8],
    pub ba: [u8; 8],
}

/// Which channel kind is being allocated — it changes the excitation
/// function's initialisation (§7.2.2.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// Full-bandwidth channel: bands start at 0, `lowcomp` is used.
    Fbw,
    /// LFE: like fbw but the last band (bin 6) skips `calc_lowcomp`.
    Lfe,
    /// Coupling channel: starts mid-spectrum with explicit leak initialisers.
    Cpl { fastleak: i32, slowleak: i32 },
}

/// `snroffset` for a channel: `(((csnroffst - 15) << 4) + fsnroffst) << 2`.
pub(super) fn snr_offset(csnroffst: u8, fsnroffst: u8) -> i32 {
    (((i32::from(csnroffst) - 15) << 4) + i32::from(fsnroffst)) << 2
}

/// The spec's `fgain` lookup, Table 7.11.
pub(super) fn fast_gain(fgaincod: u8) -> i32 {
    FASTGAIN[fgaincod as usize & 7]
}

fn logadd(a: i32, b: i32) -> i32 {
    let c = a - b;
    let address = ((c.abs() >> 1) as usize).min(255);
    if c >= 0 { a + i32::from(LATAB[address]) } else { b + i32::from(LATAB[address]) }
}

fn calc_lowcomp(a: i32, b0: i32, b1: i32, bin: usize) -> i32 {
    if bin < 7 {
        if b0 + 256 == b1 {
            384
        } else if b0 > b1 {
            (a - 64).max(0)
        } else {
            a
        }
    } else if bin < 20 {
        if b0 + 256 == b1 {
            320
        } else if b0 > b1 {
            (a - 64).max(0)
        } else {
            a
        }
    } else {
        (a - 128).max(0)
    }
}

/// Compute the bit allocation pointers for bins `start..end` of one channel.
///
/// `exps[bin]` are the decoded absolute exponents (0..=24). `bap[bin]` is
/// written for `start..end` only. With `hebap` set the E-AC-3 high-efficiency
/// pointer table replaces Table 7.16 (Annex E §3.4.3.1) — the masking curve
/// is identical, only the final lookup differs.
// `begin` is set in each branch of the spec's two excitation initialisations
// (§7.2.2.4), kept apart as the spec writes them.
#[allow(clippy::too_many_arguments)]
pub(super) fn compute_bap(
    exps: &[u8],
    start: usize,
    end: usize,
    fscod: u8,
    params: &BaParams,
    fgain: i32,
    snroffset: i32,
    kind: Kind,
    delta: Option<&DeltaBa>,
    hebap: bool,
    bap: &mut [u8],
) {
    if end <= start {
        return;
    }
    let m = compute_mask(exps, start, end, fscod, params, fgain, kind, delta);
    bap_from_mask(&m, start, end, params.floor, snroffset, hebap, bap);
}

/// The output of §7.2.2.2–7.2.2.6: the PSD per bin and the masking curve per
/// band, everything the allocation needs that does not depend on the SNR
/// offset. An encoder searching for the offset that fills a frame computes
/// this once per channel and block and calls [`bap_from_mask`] per trial.
#[derive(Clone)]
pub(crate) struct Mask {
    pub psd: [i32; 256],
    pub mask: [i32; 50],
}

/// §7.2.2.2–7.2.2.6 for bins `start..end` (`end > start`).
#[allow(clippy::too_many_arguments, clippy::needless_late_init)]
pub(crate) fn compute_mask(
    exps: &[u8],
    start: usize,
    end: usize,
    fscod: u8,
    params: &BaParams,
    fgain: i32,
    kind: Kind,
    delta: Option<&DeltaBa>,
) -> Mask {
    // 7.2.2.2 exponent mapping into PSD
    let mut psd = [0i32; 256];
    for bin in start..end {
        psd[bin] = 3072 - (i32::from(exps[bin]) << 7);
    }

    // 7.2.2.3 PSD integration
    let mut bndpsd = [0i32; 50];
    let mut j = start;
    let mut k = MASKTAB[start] as usize;
    loop {
        let lastbin = (BNDTAB[k] as usize + BNDSZ[k] as usize).min(end);
        bndpsd[k] = psd[j];
        j += 1;
        while j < lastbin {
            bndpsd[k] = logadd(bndpsd[k], psd[j]);
            j += 1;
        }
        k += 1;
        if end <= lastbin {
            break;
        }
    }

    // 7.2.2.4 excitation function
    let bndstrt = MASKTAB[start] as usize;
    let bndend = MASKTAB[end - 1] as usize + 1;
    let mut excite = [0i32; 50];
    let (mut fastleak, mut slowleak) = match kind {
        Kind::Cpl { fastleak, slowleak } => (fastleak, slowleak),
        _ => (0, 0),
    };
    let begin;
    if bndstrt == 0 {
        // fbw and lfe channels
        let mut lowcomp = 0;
        lowcomp = calc_lowcomp(lowcomp, bndpsd[0], bndpsd[1], 0);
        excite[0] = bndpsd[0] - fgain - lowcomp;
        lowcomp = calc_lowcomp(lowcomp, bndpsd[1], bndpsd[2], 1);
        excite[1] = bndpsd[1] - fgain - lowcomp;
        let mut b = 7;
        for bin in 2..7 {
            let not_last_lfe = bndend != 7 || bin != 6;
            if not_last_lfe {
                lowcomp = calc_lowcomp(lowcomp, bndpsd[bin], bndpsd[bin + 1], bin);
            }
            fastleak = bndpsd[bin] - fgain;
            slowleak = bndpsd[bin] - params.sgain;
            excite[bin] = fastleak - lowcomp;
            if not_last_lfe && bndpsd[bin] <= bndpsd[bin + 1] {
                b = bin + 1;
                break;
            }
        }
        for bin in b..bndend.min(22) {
            if bndend != 7 || bin != 6 {
                lowcomp = calc_lowcomp(lowcomp, bndpsd[bin], bndpsd[bin + 1], bin);
            }
            fastleak -= params.fdecay;
            fastleak = fastleak.max(bndpsd[bin] - fgain);
            slowleak -= params.sdecay;
            slowleak = slowleak.max(bndpsd[bin] - params.sgain);
            excite[bin] = (fastleak - lowcomp).max(slowleak);
        }
        begin = 22;
    } else {
        begin = bndstrt;
    }
    for bin in begin..bndend {
        fastleak -= params.fdecay;
        fastleak = fastleak.max(bndpsd[bin] - fgain);
        slowleak -= params.sdecay;
        slowleak = slowleak.max(bndpsd[bin] - params.sgain);
        excite[bin] = fastleak.max(slowleak);
    }

    // 7.2.2.5 masking curve
    let hth = &HTH[fscod as usize & 3];
    let mut mask = [0i32; 50];
    for bin in bndstrt..bndend {
        if bndpsd[bin] < params.dbknee {
            excite[bin] += (params.dbknee - bndpsd[bin]) >> 2;
        }
        mask[bin] = excite[bin].max(i32::from(hth[bin]));
    }

    // 7.2.2.6 delta bit allocation
    if let Some(d) = delta {
        let mut band = 0usize;
        for seg in 0..d.nseg as usize {
            band += d.offst[seg] as usize;
            let delta = if d.ba[seg] >= 4 {
                (i32::from(d.ba[seg]) - 3) << 7
            } else {
                (i32::from(d.ba[seg]) - 4) << 7
            };
            for _ in 0..d.len[seg] {
                if band >= 50 {
                    break;
                }
                mask[band] += delta;
                band += 1;
            }
        }
    }

    Mask { psd, mask }
}

/// §7.2.2.7: bit allocation pointers for bins `start..end` from a masking
/// curve and an SNR offset (`floor` is the `floortab` value in use).
pub(crate) fn bap_from_mask(m: &Mask, start: usize, end: usize, floor: i32, snroffset: i32, hebap: bool, bap: &mut [u8]) {
    let mut i = start;
    let mut j = MASKTAB[start] as usize;
    loop {
        let lastbin = (BNDTAB[j] as usize + BNDSZ[j] as usize).min(end);
        let mut mask = m.mask[j];
        mask -= snroffset;
        mask -= floor;
        if mask < 0 {
            mask = 0;
        }
        mask &= 0x1fe0;
        mask += floor;
        while i < lastbin {
            let address = ((m.psd[i] - mask) >> 5).clamp(0, 63) as usize;
            bap[i] = if hebap { HEBAPTAB[address] } else { BAPTAB[address] };
            i += 1;
        }
        j += 1;
        if end <= lastbin {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logadd_matches_the_spec_definition() {
        // c >= 0 → a + latab[min(|c|>>1, 255)]
        assert_eq!(logadd(100, 100), 100 + 0x40);
        assert_eq!(logadd(100, 0), 100 + i32::from(LATAB[50]));
        assert_eq!(logadd(0, 100), 100 + i32::from(LATAB[50]));
        assert_eq!(logadd(1000, 0), 1000 + i32::from(LATAB[255]));
    }

    #[test]
    fn all_zero_snr_offset_and_silence_allocate_nothing_loud_signal_allocates() {
        let params = BaParams::from_codes(2, 1, 1, 2, 7);
        let mut bap = [0u8; 256];
        // Exponent 24 everywhere = the quietest possible spectrum.
        let quiet = [24u8; 256];
        compute_bap(&quiet, 0, 253, 0, &params, fast_gain(4), snr_offset(15, 0), Kind::Fbw, None, false, &mut bap);
        assert!(bap[..253].iter().all(|&b| b == 0), "quiet spectrum must get no bits");
        // Exponent 0 everywhere = full-scale in every bin; a modest snroffset
        // must hand out bits.
        let loud = [0u8; 256];
        compute_bap(&loud, 0, 253, 0, &params, fast_gain(4), snr_offset(20, 0), Kind::Fbw, None, false, &mut bap);
        assert!(bap[..253].iter().all(|&b| b > 0), "loud spectrum must get bits: {:?}", &bap[..16]);
        // And the hebap variant reads the other pointer table, whose entries
        // for the same address are never smaller.
        let mut hb = [0u8; 256];
        compute_bap(&loud, 0, 253, 0, &params, fast_gain(4), snr_offset(20, 0), Kind::Fbw, None, true, &mut hb);
        assert!(hb[..253].iter().zip(&bap[..253]).all(|(h, b)| h >= b));
    }

    /// §7.2.2 worked by hand for an LFE block (bins 0..7, one bin per band,
    /// fscod 0) with the §8.2.12 nominal parameters: sdecay 0x13, fdecay
    /// 0x53, sgain 0x4d8, dbknee 0x900, floor 0x1f0 (floorcod 4), fgain 0x280.
    #[test]
    fn hand_computed_lfe_allocation() {
        let params = BaParams::from_codes(2, 1, 1, 2, 4);
        assert_eq!((params.sdecay, params.fdecay, params.sgain, params.dbknee, params.floor), (0x13, 0x53, 0x4d8, 0x900, 0x1f0));
        let mut bap = [0u8; 256];
        // Exponents all 10: psd = 3072 − (10 << 7) = 1792 in every band.
        // excite[0], excite[1] = 1792 − 640 − lowcomp 0 = 1152; bin 2 starts
        // the leaks at fastleak 1152 / slowleak 552 and, as bndpsd[2] ≤
        // bndpsd[3], hands over to the decay loop, where max(fastleak − 83,
        // 1152) keeps 1152. bndpsd 1792 < dbknee 2304 adds (2304 − 1792) >> 2
        // = 128 → 1280, above hth[0][0..7] (0x4d0 = 1232 at most) → mask 1280.
        // snroffset(15, 0) = 0: (1280 − 496) & 0x1fe0 = 768, + 496 = 1264;
        // address (1792 − 1264) >> 5 = 16 → baptab[16] = 6.
        let mut exps = [10u8; 256];
        compute_bap(&exps, 0, 7, 0, &params, fast_gain(4), snr_offset(15, 0), Kind::Lfe, None, false, &mut bap);
        assert_eq!(&bap[..7], &[6; 7]);
        // snroffset(20, 8) = ((5 << 4) + 8) << 2 = 352: (1280 − 352 − 496) =
        // 432 & 0x1fe0 = 416, + 496 = 912; (1792 − 912) >> 5 = 27 → baptab[27] = 9.
        compute_bap(&exps, 0, 7, 0, &params, fast_gain(4), snr_offset(20, 8), Kind::Lfe, None, false, &mut bap);
        assert_eq!(&bap[..7], &[9; 7]);
        // A louder bin 0 (exponent 4, psd 2560): calc_lowcomp sees b0 > b1
        // and leaves lowcomp at 0, excite[0] = 2560 − 640 = 1920 (no dbknee
        // boost, 2560 ≥ 2304) → (1920 − 496) & 0x1fe0 = 1408, + 496 = 1904;
        // (2560 − 1904) >> 5 = 20 → baptab[20] = 7. Bins 1..7 as before.
        exps[0] = 4;
        compute_bap(&exps, 0, 7, 0, &params, fast_gain(4), snr_offset(15, 0), Kind::Lfe, None, false, &mut bap);
        assert_eq!(&bap[..7], &[7, 6, 6, 6, 6, 6, 6]);
    }

    #[test]
    fn delta_bit_allocation_moves_the_mask() {
        let params = BaParams::from_codes(2, 1, 1, 2, 7);
        let exps = [10u8; 256];
        let mut base = [0u8; 256];
        compute_bap(&exps, 0, 253, 0, &params, fast_gain(4), snr_offset(18, 0), Kind::Fbw, None, false, &mut base);
        // +24 dB (code 7 → delta = (7-3)<<7) on bands 30..=33 lowers the
        // allocation there and nowhere else.
        let d = DeltaBa { nseg: 1, offst: [30, 0, 0, 0, 0, 0, 0, 0], len: [4, 0, 0, 0, 0, 0, 0, 0], ba: [7, 0, 0, 0, 0, 0, 0, 0] };
        let mut with = [0u8; 256];
        compute_bap(&exps, 0, 253, 0, &params, fast_gain(4), snr_offset(18, 0), Kind::Fbw, Some(&d), false, &mut with);
        let lo = BNDTAB[30] as usize;
        let hi = BNDTAB[34] as usize;
        assert!(with[lo..hi].iter().zip(&base[lo..hi]).all(|(w, b)| w < b), "{:?} vs {:?}", &with[lo..hi], &base[lo..hi]);
        assert_eq!(&with[..lo], &base[..lo]);
        assert_eq!(&with[hi..253], &base[hi..253]);
    }
}
