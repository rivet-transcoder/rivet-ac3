//! One substream's syncframes: analysis (transform, block switching,
//! coupling, rematrixing), exponent strategy, the bit allocation search, and
//! the bit stream writer for AC-3 (§5.3) and E-AC-3 (Annex E §2.2).
//!
//! The writer mirrors the decoder's parse field for field; the state the
//! syntax carries between blocks (`firstcplcos`, `firstcplleak`, which SNR
//! offsets the decoder holds) is tracked here the way the decoder tracks it,
//! so the plan never assumes the decoder knows something it was not sent.

// Blocks, channels and bins index several parallel arrays at once, as the
// spec's syntax and pseudo code do.
#![allow(clippy::needless_range_loop)]

use std::rc::Rc;

use super::bits::{Counter, Sink, Writer, crc1_word, crc16};
use super::exponents::{self, Coded, D15, D25, D45, REUSE, Set, Track};
use super::mdct::mdct_block;
use super::quant::{Packer, Tally, quantize};
use super::transient::Detector;
use crate::Error;
use crate::bitalloc::{BaParams, Kind, Mask, bap_from_mask, compute_mask, fast_gain, snr_offset};
use crate::tables::FRMSIZETAB;

const NB: usize = 256;
const MAX_FBW: usize = 5;
pub(super) const CPL: usize = MAX_FBW;
pub(super) const LFE: usize = MAX_FBW + 1;
const NCH: usize = MAX_FBW + 2;

/// Bit allocation parameter codes. AC-3 sends §8.2.12's nominal values in
/// block 0; E-AC-3 uses `bamode = 0`, which implies the Annex E defaults.
const AC3_BA: (u8, u8, u8, u8, u8) = (2, 1, 1, 2, 4);
const EAC3_BA: (u8, u8, u8, u8, u8) = (2, 1, 1, 2, 7);
const FGAINCOD: u8 = 4;
/// Coupling leak codes (`cplfleak`, `cplsleak`): 0, the least masking
/// assumed from below the coupling range.
const CPL_LEAK: (u8, u8) = (0, 0);

/// The coupling range and band structure, fixed for a stream.
#[derive(Clone, Copy, Debug)]
pub(super) struct CplRange {
    pub begf: u8,
    pub endf: u8,
    /// `cplbndstrc`, indexed by absolute sub-band (Table E2.12's layout).
    pub bndstrc: [u8; 18],
}

impl CplRange {
    pub fn nsub(&self) -> usize {
        3 + usize::from(self.endf) - usize::from(self.begf)
    }
    pub fn strt(&self) -> usize {
        usize::from(self.begf) * 12 + 37
    }
    pub fn end(&self) -> usize {
        self.strt() + self.nsub() * 12
    }
    /// Bin ranges of the coupling bands.
    pub fn bands(&self) -> Vec<(usize, usize)> {
        let b0 = usize::from(self.begf);
        let mut out: Vec<(usize, usize)> = Vec::new();
        for sb in 0..self.nsub() {
            let lo = (b0 + sb) * 12 + 37;
            if sb > 0 && self.bndstrc[b0 + sb] == 1 {
                out.last_mut().expect("band").1 = lo + 12;
            } else {
                out.push((lo, lo + 12));
            }
        }
        out
    }
}

/// Everything fixed for one substream.
#[derive(Clone, Debug)]
pub(super) struct Params {
    pub eac3: bool,
    pub strmtyp: u8,
    pub substreamid: u8,
    pub chanmap: Option<u16>,
    pub fscod: u8,
    pub sample_rate: u32,
    pub acmod: u8,
    pub lfeon: bool,
    pub nfchans: usize,
    pub numblks: usize,
    pub bsmod: u8,
    pub dialnorm: u8,
    /// `chbwcod` of channels not in coupling.
    pub bwcod: u8,
    pub cpl: Option<CplRange>,
    pub rematrix: bool,
    pub block_switch: bool,
    /// AC-3: `frmsizecod` of the shorter frame of the bit rate (even).
    pub frmsizecod: u8,
    /// Assumed mantissa bits per unit of exponent underestimate (§8.2.8).
    pub lambda: f32,
}

/// Coupling coordinate of one band as sent: `cplcoexp`, `cplcomant`.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct Co {
    exp: u8,
    mant: u8,
}

#[derive(Clone, Default)]
struct BlockPlan {
    blksw: [bool; MAX_FBW],
    dith: [bool; MAX_FBW],
    cplstre: bool,
    cplinu: bool,
    incpl: [bool; MAX_FBW],
    phsflginu: bool,
    cplcoe: [bool; MAX_FBW],
    mstrcplco: [u8; MAX_FBW],
    cplco: [[Co; 18]; MAX_FBW],
    phsflg: [bool; 18],
    rematstr: bool,
    nrematbd: usize,
    rematflg: [bool; 4],
    expstr: [u8; NCH],
    endmant: [usize; MAX_FBW],
    snroffste: bool,
    cplleake: bool,
    skip_bytes: usize,
}

/// Frame-level E-AC-3 exponent strategy: `expstre = 0` rows of Table E2.10.
#[derive(Clone, Copy, Default)]
struct FrameExp {
    rows: Option<([u8; MAX_FBW], Option<u8>)>,
}

/// Positions the writer reports while sizing a frame (side information only).
#[derive(Clone, Default, Debug)]
struct Layout {
    /// Bit position where each block's mantissas start.
    mant_start: Vec<usize>,
    /// Bit position after block 1 (AC-3 5/8 rule).
    end_blk1: usize,
    /// Total bits, trailer included.
    total: usize,
}

/// Per-substream encoder state across frames.
#[derive(Clone)]
pub(super) struct SubEncoder {
    pub p: Params,
    hist: Vec<[f32; NB]>,
    det: Vec<Detector>,
    prev_blksw: [bool; MAX_FBW],
    /// Global block counter (E-AC-3 converter exponent strategy cadence).
    blocks: u64,
}

/// The per-frame working set.
struct Work {
    plan: Vec<BlockPlan>,
    /// coeffs[blk][ch], ch = 0..nf, CPL, LFE.
    coeffs: Vec<[[f32; NB]; NCH]>,
    /// The exponent set in effect per block and channel.
    exps: Vec<[Option<Rc<Coded>>; NCH]>,
    masks: Vec<[Option<Mask>; NCH]>,
    range: Vec<[(usize, usize); NCH]>,
    frame_exp: FrameExp,
}

impl SubEncoder {
    pub fn new(p: Params) -> Self {
        let nch = p.nfchans + usize::from(p.lfeon);
        Self {
            hist: vec![[0.0; NB]; nch],
            det: vec![Detector::new(p.sample_rate); p.nfchans],
            prev_blksw: [false; MAX_FBW],
            blocks: 0,
            p,
        }
    }

    fn ba_params(&self) -> (BaParams, (u8, u8, u8, u8, u8)) {
        let c = if self.p.eac3 { EAC3_BA } else { AC3_BA };
        (BaParams::from_codes(c.0, c.1, c.2, c.3, c.4), c)
    }

    /// Encode one syncframe of `numblks · 256` samples per coded channel
    /// (`pcm[ch]`, fbw channels in `acmod` order then the LFE) into a frame
    /// of `words` 16-bit words.
    pub fn encode_frame(&mut self, pcm: &[Vec<f32>], words: usize) -> Result<Vec<u8>, Error> {
        let saved = (self.hist.clone(), self.det.clone(), self.prev_blksw);
        match self.try_frame(pcm, words, true) {
            Err(Error::Unsupported(_)) if self.p.block_switch => {
                // Short blocks cost side information (coupling leaves and
                // re-enters, exponents restart); at a bit rate too low for
                // that, code this frame with long blocks only.
                (self.hist, self.det, self.prev_blksw) = saved;
                self.try_frame(pcm, words, false)
            }
            r => r,
        }
    }

    fn try_frame(&mut self, pcm: &[Vec<f32>], words: usize, allow_switch: bool) -> Result<Vec<u8>, Error> {
        let p = self.p.clone();
        let nb = p.numblks;
        let nf = p.nfchans;
        let mut w = Work {
            plan: vec![BlockPlan::default(); nb],
            coeffs: vec![[[0.0; NB]; NCH]; nb],
            exps: vec![Default::default(); nb],
            masks: vec![Default::default(); nb],
            range: vec![[(0, 0); NCH]; nb],
            frame_exp: FrameExp::default(),
        };

        // --- transform and block switching (§8.2.2, §8.2.3) ---------------
        for blk in 0..nb {
            for ch in 0..nf {
                let new = &pcm[ch][blk * NB..(blk + 1) * NB];
                let sw = self.det[ch].detect(new) && p.block_switch && allow_switch;
                let mut input = [0.0f32; 512];
                input[..NB].copy_from_slice(&self.hist[ch]);
                input[NB..].copy_from_slice(new);
                self.hist[ch].copy_from_slice(new);
                mdct_block(&input, sw, &mut w.coeffs[blk][ch]);
                let bp = &mut w.plan[blk];
                bp.blksw[ch] = sw;
                // §8.2.9: no dither in a switched block or the one after it.
                bp.dith[ch] = !(sw || self.prev_blksw[ch]);
                self.prev_blksw[ch] = sw;
            }
            if p.lfeon {
                let new = &pcm[nf][blk * NB..(blk + 1) * NB];
                let mut input = [0.0f32; 512];
                input[..NB].copy_from_slice(&self.hist[nf]);
                input[NB..].copy_from_slice(new);
                self.hist[nf].copy_from_slice(new);
                let mut c = [0.0f32; NB];
                mdct_block(&input, false, &mut c);
                w.coeffs[blk][LFE][..7].copy_from_slice(&c[..7]);
            }
        }

        self.plan_coupling(&mut w, 0.02);
        self.plan_rematrix(&mut w);

        let bw_end = (usize::from(p.bwcod) + 12) * 3 + 37;
        for blk in 0..nb {
            let cpl = p.cpl.filter(|_| w.plan[blk].cplinu);
            for ch in 0..nf {
                let e = if w.plan[blk].incpl[ch] { cpl.expect("cpl").strt() } else { bw_end };
                w.plan[blk].endmant[ch] = e;
                w.range[blk][ch] = (0, e);
            }
            if let Some(c) = cpl {
                w.range[blk][CPL] = (c.strt(), c.end());
            }
            w.range[blk][LFE] = (0, 7);
        }

        // --- exponents (§8.2.7, §8.2.8, §8.2.10) ---------------------------
        let budget = words * 16;
        let coded_bins: usize = (0..nb)
            .map(|b| {
                (0..nf).map(|ch| w.range[b][ch].1).sum::<usize>()
                    + if w.plan[b].cplinu { w.range[b][CPL].1 - w.range[b][CPL].0 } else { 0 }
            })
            .sum::<usize>()
            .max(1);
        // λ, the mantissa bits a unit of exponent underestimate costs per
        // bin, is about the fraction of bins that get bits: estimate it from
        // what side information leaves for mantissas (a first pass at λ = 1
        // sizes that), then fall back to the cheapest exponents (λ = 0).
        let five8 = ((words >> 1) + (words >> 3)) * 16;
        let mut lam = p.lambda;
        let mut result = None;
        for attempt in 0..3 {
            if attempt > 0 {
                // Coupling coordinates follow the same trade-off: the fewer
                // bits per bin, the larger the level error tolerated before
                // they are resent; the last resort sends them only where the
                // syntax requires.
                let tol = if lam > 0.0 { 0.02 / lam.max(0.05) } else { f32::INFINITY };
                self.plan_coupling(&mut w, tol);
            }
            self.plan_exponents(&mut w, lam);
            self.plan_bit_allocation(&mut w);
            let (layout, _) = self.layout(&w, words, None);
            if attempt == 0 {
                let spare = budget.saturating_sub(layout.total);
                lam = (p.lambda * spare as f32 / coded_bins as f32 / 2.0).clamp(0.0, 1.0);
                continue;
            }
            if layout.total <= budget && (p.eac3 || layout.end_blk1 <= five8) {
                result = Some(layout);
                break;
            }
            lam = 0.0;
        }
        let Some(side) = result else {
            return Err(Error::Unsupported(format!(
                "bit rate too low: {words} words per frame cannot hold the side information of this layout"
            )));
        };

        // --- the SNR offset search (§8.2.12) -------------------------------
        let fits = |snr: usize| -> bool {
            let (bits, _) = self.tally(&w, snr);
            let total: usize = side.total + bits.iter().sum::<usize>();
            let blk01 = if p.eac3 { 0 } else { side.end_blk1 + bits[0] + bits.get(1).copied().unwrap_or(0) };
            total <= budget && (p.eac3 || blk01 <= five8)
        };
        if !fits(0) {
            return Err(Error::Unsupported("bit rate too low for the side information".into()));
        }
        let (mut lo, mut hi) = (0usize, 1023usize);
        if fits(hi) {
            lo = hi;
        }
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if fits(mid) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let mut snr = lo;

        // --- §5.5: block 5's mantissas, the aux data and errorcheck must lie
        // in the last 3/8 (AC-3). Pad with skip fields in blocks 2..4 if not.
        let (bits, baps) = loop {
            let (bits, baps) = self.tally(&w, snr);
            for bp in &mut w.plan {
                bp.skip_bytes = 0;
            }
            if p.eac3 {
                break (bits, baps);
            }
            let mant5 = side.mant_start[5] + bits[..5].iter().sum::<usize>();
            let used = side.total + bits.iter().sum::<usize>();
            if mant5 >= five8 {
                break (bits, baps);
            }
            let mut need = five8 - mant5;
            let mut spare = budget - used;
            // room before the 5/8 point for padding in blocks 0 and 1
            let mut room01 = five8.saturating_sub(side.end_blk1 + bits[0] + bits[1]);
            for blk in [4usize, 3, 2, 1, 0] {
                if need == 0 {
                    break;
                }
                // skipl (9 bits) + skipl bytes; skiple is already counted.
                let mut bytes = need.saturating_sub(9).div_ceil(8).clamp(1, 511);
                if blk <= 1 {
                    bytes = bytes.min(room01.saturating_sub(9) / 8);
                    if bytes == 0 {
                        continue;
                    }
                }
                let cost = 9 + 8 * bytes;
                if cost > spare {
                    break;
                }
                w.plan[blk].skip_bytes = bytes;
                spare -= cost;
                if blk <= 1 {
                    room01 -= cost;
                }
                need = need.saturating_sub(cost);
            }
            if need == 0 {
                break (bits, baps);
            }
            if snr == 0 {
                return Err(Error::Unsupported("cannot meet the A/52 §5.5 block-5 placement rule at this bit rate".into()));
            }
            snr -= 1;
        };

        // --- quantise and write ---------------------------------------------
        let mut packers: Vec<Packer> = Vec::with_capacity(nb);
        for blk in 0..nb {
            let mut pk = Packer::default();
            let bp = &w.plan[blk];
            let mut got_cpl = false;
            let put = |pk: &mut Packer, ch: usize| {
                let (s, e) = w.range[blk][ch];
                let exps = &w.exps[blk][ch].as_ref().expect("exps").exps;
                for bin in s..e {
                    let bap = baps[blk][ch][bin];
                    if bap != 0 {
                        let m = w.coeffs[blk][ch][bin] * (1u32 << exps[bin]) as f32;
                        pk.push(bap, quantize(m, bap));
                    }
                }
            };
            for ch in 0..nf {
                put(&mut pk, ch);
                if bp.cplinu && bp.incpl[ch] && !got_cpl {
                    put(&mut pk, CPL);
                    got_cpl = true;
                }
            }
            if p.lfeon {
                put(&mut pk, LFE);
            }
            debug_assert_eq!(pk.bits(), bits[blk]);
            packers.push(pk);
        }
        let (layout, buf) = self.layout(&w, words, Some((&packers, snr)));
        let mut frame = buf.expect("written");
        debug_assert_eq!(layout.total, budget);
        // CRCs (§7.10.1)
        let n = frame.len();
        if !p.eac3 {
            let five8_bytes = five8 / 8;
            let c1 = crc1_word(&frame[4..five8_bytes]);
            frame[2..4].copy_from_slice(&c1.to_be_bytes());
        }
        let mut c2 = crc16(&frame[2..n - 2]);
        if c2 == 0x0b77 {
            // crcrsv (§5.4.5.1) / encinfo: flip it so crc2 is not a syncword.
            frame[n - 3] ^= 0x01;
            c2 = crc16(&frame[2..n - 2]);
        }
        frame[n - 2..].copy_from_slice(&c2.to_be_bytes());
        self.blocks += nb as u64;
        Ok(frame)
    }

    /// Mantissa bits per block and the allocation pointers at SNR offset
    /// index `snr` (`csnroffst · 16 + fsnroffst`, every channel alike). Index
    /// 0 is §7.2.2.1.1's special case: no bits at all.
    fn tally(&self, w: &Work, snr: usize) -> (Vec<usize>, Vec<[[u8; NB]; NCH]>) {
        let nb = self.p.numblks;
        let (bp, _) = self.ba_params();
        let mut bits = vec![0usize; nb];
        let mut baps = vec![[[0u8; NB]; NCH]; nb];
        for blk in 0..nb {
            let mut t = Tally::default();
            for ch in 0..NCH {
                let Some(m) = &w.masks[blk][ch] else { continue };
                let (s, e) = w.range[blk][ch];
                if snr != 0 {
                    bap_from_mask(m, s, e, bp.floor, snr_offset((snr >> 4) as u8, (snr & 15) as u8), false, &mut baps[blk][ch]);
                }
                t.add(&baps[blk][ch][s..e]);
            }
            bits[blk] = t.bits();
        }
        (bits, baps)
    }

    /// Coupling (§8.2.4, §8.2.5): which channels couple in each block, the
    /// coupling channel, phase flags and quantised coordinates, and when the
    /// coordinates need sending.
    fn plan_coupling(&self, w: &mut Work, tol: f32) {
        let p = &self.p;
        let nf = p.nfchans;
        let Some(cr) = p.cpl else {
            // AC-3 block 0 always carries the coupling strategy (cplinu = 0).
            w.plan[0].cplstre = true;
            return;
        };
        let bands = cr.bands();
        let (strt, end) = (cr.strt(), cr.end());
        // what the decoder holds: coordinates per channel and band (as sent),
        // phase flags, and the previous block's coupling state
        let mut held: [Option<[Co; 18]>; MAX_FBW] = [None; MAX_FBW];
        let mut held_mstr = [0u8; MAX_FBW];
        let mut held_phs = [false; 18];
        let mut prev_inu = false;
        let mut prev_in = [false; MAX_FBW];
        let mut prev_phsinu = false;
        for blk in 0..p.numblks {
            let bp = &mut w.plan[blk];
            bp.cplcoe = [false; MAX_FBW];
            bp.phsflg = [false; 18];
            // §8.2.4.1: switched channels leave coupling.
            for ch in 0..nf {
                bp.incpl[ch] = !bp.blksw[ch];
            }
            if p.eac3 && p.acmod == 2 && !(bp.incpl[0] && bp.incpl[1]) {
                bp.incpl = [false; MAX_FBW];
            }
            bp.cplinu = bp.incpl[..nf].iter().filter(|&&x| x).count() >= 2;
            if !bp.cplinu {
                bp.incpl = [false; MAX_FBW];
            }
            bp.phsflginu = bp.cplinu && p.acmod == 2;
            bp.cplstre = blk == 0 || bp.cplinu != prev_inu || (bp.cplinu && (bp.incpl != prev_in || bp.phsflginu != prev_phsinu));
            if !bp.cplinu {
                held = [None; MAX_FBW];
                held_phs = [false; 18];
                prev_inu = false;
                prev_in = [false; MAX_FBW];
                continue;
            }
            let ncpl = bp.incpl[..nf].iter().filter(|&&x| x).count() as f32;
            let coeffs = &mut w.coeffs[blk];
            // phase flags (2/0): code the difference where L and R oppose
            let mut sign = [[1.0f32; 18]; MAX_FBW];
            if bp.phsflginu {
                // With coordinates sent only where required, keep the flags
                // the decoder holds rather than force a resend.
                let keep = tol.is_infinite() && prev_phsinu && prev_in[0] && prev_in[1];
                for (b, &(lo, hi)) in bands.iter().enumerate() {
                    let corr: f32 = (lo..hi).map(|i| coeffs[0][i] * coeffs[1][i]).sum();
                    bp.phsflg[b] = if keep { held_phs[b] } else { corr < 0.0 };
                    if bp.phsflg[b] {
                        sign[1][b] = -1.0;
                    }
                }
            }
            for (b, &(lo, hi)) in bands.iter().enumerate() {
                for i in lo..hi {
                    let mut s = 0.0;
                    for ch in 0..nf {
                        if bp.incpl[ch] {
                            s += sign[ch][b] * coeffs[ch][i];
                        }
                    }
                    coeffs[CPL][i] = s / ncpl;
                }
            }
            for i in strt..end {
                debug_assert!(coeffs[CPL][i].abs() <= 1.0 + 1e-6);
            }
            let phs_changed = bp.phsflginu && (bp.phsflg != held_phs || !prev_phsinu);
            for ch in 0..nf {
                if !bp.incpl[ch] {
                    held[ch] = None;
                    continue;
                }
                // §8.2.5.2: the square root of the band power ratio.
                let mut ratio = [0.0f32; 18];
                let mut ecs = [0.0f32; 18];
                for (b, &(lo, hi)) in bands.iter().enumerate() {
                    let ec: f32 = (lo..hi).map(|i| coeffs[CPL][i] * coeffs[CPL][i]).sum();
                    let ex: f32 = (lo..hi).map(|i| coeffs[ch][i] * coeffs[ch][i]).sum();
                    ratio[b] = if ec > 1e-30 { (ex / ec).sqrt() } else { 0.0 };
                    ecs[b] = ec;
                }
                let (mstr, cos) = quantize_coords(&ratio[..bands.len()]);
                // Resend when keeping the held coordinates would misplace
                // more than `tol` of the channel's coupled energy (2 %, about
                // a 1.2 dB level error spread evenly, at full rate): bands with
                // little energy do not force a resend.
                let stale = held[ch].is_some_and(|h| {
                    let (mut err, mut tot) = (0.0f32, 0.0f32);
                    for b in 0..bands.len() {
                        let a = coord_value(h[b], held_mstr[ch]);
                        let n = coord_value(cos[b], mstr);
                        err += ecs[b] * (a - n) * (a - n);
                        tot += ecs[b] * n * n;
                    }
                    err > tol * tot
                });
                let send = blk == 0 || !prev_in[ch] || held[ch].is_none() || phs_changed || stale;
                if send {
                    bp.cplcoe[ch] = true;
                    bp.mstrcplco[ch] = mstr;
                    bp.cplco[ch] = cos;
                    held[ch] = Some(cos);
                    held_mstr[ch] = mstr;
                }
            }
            if bp.phsflginu && (bp.cplcoe[0] || bp.cplcoe[1]) {
                held_phs = bp.phsflg;
            } else if bp.phsflginu {
                bp.phsflg = held_phs;
            }
            prev_inu = true;
            prev_in = bp.incpl;
            prev_phsinu = bp.phsflginu;
        }
    }

    /// Rematrixing (§8.2.6) in 2/0: per band, code (L+R)/2 and (L−R)/2
    /// instead of L and R where the sum or difference is the weaker pair.
    fn plan_rematrix(&self, w: &mut Work) {
        let p = &self.p;
        if p.acmod != 2 {
            return;
        }
        let bw_end = (usize::from(p.bwcod) + 12) * 3 + 37;
        for blk in 0..p.numblks {
            let bp = &mut w.plan[blk];
            bp.rematstr = true;
            bp.nrematbd = if bp.cplinu {
                match p.cpl.expect("cpl").begf {
                    0 => 2,
                    1 | 2 => 3,
                    _ => 4,
                }
            } else {
                4
            };
            if !p.rematrix {
                continue;
            }
            let end = if bp.cplinu { p.cpl.expect("cpl").strt() } else { bw_end }.max(13);
            let bounds = [13usize, 25, 37, 61, 253];
            let c = &mut w.coeffs[blk];
            for b in 0..bp.nrematbd {
                let lo = bounds[b];
                let hi = bounds[b + 1].min(end);
                if lo >= hi {
                    continue;
                }
                let (mut el, mut er, mut es, mut ed) = (0.0f32, 0.0, 0.0, 0.0);
                for i in lo..hi {
                    let (l, r) = (c[0][i], c[1][i]);
                    el += l * l;
                    er += r * r;
                    es += (l + r) * (l + r) * 0.25;
                    ed += (l - r) * (l - r) * 0.25;
                }
                if es.min(ed) < el.min(er) {
                    bp.rematflg[b] = true;
                    for i in lo..hi {
                        let (l, r) = (c[0][i], c[1][i]);
                        c[0][i] = (l + r) * 0.5;
                        c[1][i] = (l - r) * 0.5;
                    }
                }
            }
        }
    }

    /// Exponent strategies by cost and the coded sets per block.
    fn plan_exponents(&self, w: &mut Work, lambda: f32) {
        let p = &self.p;
        let nb = p.numblks;
        let nf = p.nfchans;
        let mut raw = vec![[[24u8; NB]; NCH]; nb];
        for blk in 0..nb {
            for ch in 0..NCH {
                let (s, e) = w.range[blk][ch];
                for bin in s..e {
                    raw[blk][ch][bin] = exponents::raw_exponent(w.coeffs[blk][ch][bin]);
                }
            }
        }
        let mut channels: Vec<usize> = (0..nf).collect();
        channels.push(CPL);
        if p.lfeon {
            channels.push(LFE);
        }
        let mut tracks = Vec::new();
        for &ch in &channels {
            let present: Vec<bool> = (0..nb)
                .map(|b| match ch {
                    CPL => w.plan[b].cplinu,
                    _ => true,
                })
                .collect();
            if !present.iter().any(|&x| x) {
                tracks.push(None);
                continue;
            }
            let must_new: Vec<bool> = (0..nb)
                .map(|b| b == 0 || !present[b - 1] || w.range[b][ch] != w.range[b - 1][ch])
                .collect();
            let t = Track {
                raw: (0..nb).map(|b| present[b].then_some(&raw[b][ch])).collect(),
                must_new,
                start: (0..nb).map(|b| w.range[b][ch].0).collect(),
                end: (0..nb).map(|b| w.range[b][ch].1).collect(),
                set: if ch == CPL { Set::Coupling } else { Set::Absolute },
                choices: if ch == LFE { &[D15] } else { &[D15, D25, D45] },
                lambda,
                overhead: match ch {
                    CPL | LFE => 0,
                    _ => 2 + if w.plan[0].incpl[ch] { 0 } else { 6 },
                },
            };
            tracks.push(Some(t));
        }
        let mut strat: Vec<Vec<u8>> = tracks
            .iter()
            .map(|t| t.as_ref().map_or(vec![REUSE; nb], exponents::choose))
            .collect();
        // E-AC-3 six-block frames: Table E2.10 rows (`expstre = 0`) when
        // they cost less than per-block strategies.
        w.frame_exp = FrameExp::default();
        if p.eac3 && nb == 6 {
            let mut rows = [0u8; MAX_FBW];
            let mut cplrow = None;
            let mut row_cost = 0.0f32;
            let mut dp_cost = 0.0f32;
            let mut ok = true;
            for (i, &ch) in channels.iter().enumerate() {
                if ch == LFE {
                    continue;
                }
                let Some(t) = &tracks[i] else { continue };
                let dp = run_costs(t, &strat[i]);
                match exponents::choose_frame_row(t) {
                    Some((row, c)) => {
                        row_cost += c + 5.0;
                        dp_cost += dp;
                        if ch == CPL {
                            cplrow = Some(row as u8);
                            dp_cost += 2.0 * (0..6).filter(|&b| w.plan[b].cplinu).count() as f32;
                        } else {
                            rows[ch] = row as u8;
                            dp_cost += 12.0;
                        }
                    }
                    None => ok = false,
                }
            }
            if ok && row_cost <= dp_cost {
                w.frame_exp.rows = Some((rows, cplrow));
                for (i, &ch) in channels.iter().enumerate() {
                    if tracks[i].is_none() || ch == LFE {
                        continue;
                    }
                    let row = if ch == CPL { cplrow.expect("row") } else { rows[ch] };
                    let r = crate::tables::FRMEXPSTR[row as usize];
                    strat[i] = (0..6).map(|b| if tracks[i].as_ref().unwrap().raw[b].is_some() { r[b] } else { REUSE }).collect();
                    // a row's block value can be "new" right after an absent
                    // block; code_track handles runs per present block
                }
            }
        }
        for (i, &ch) in channels.iter().enumerate() {
            let Some(t) = &tracks[i] else {
                for b in 0..nb {
                    w.exps[b][ch] = None;
                    w.plan[b].expstr[ch] = REUSE;
                }
                continue;
            };
            let coded = exponents::code_track(t, &strat[i]);
            for (b, c) in coded.into_iter().enumerate() {
                match c {
                    Some((s, set)) => {
                        w.plan[b].expstr[ch] = s;
                        w.exps[b][ch] = Some(set);
                    }
                    None => {
                        w.plan[b].expstr[ch] = REUSE;
                        w.exps[b][ch] = None;
                    }
                }
            }
        }
    }

    /// Masking curves per block and channel (§7.2.2.2–6), and the blocks
    /// that must (re)send SNR offsets and coupling leak values.
    fn plan_bit_allocation(&self, w: &mut Work) {
        let p = &self.p;
        let (ba, _) = self.ba_params();
        let mut prev_cpl = false;
        for blk in 0..p.numblks {
            let cplinu = w.plan[blk].cplinu;
            // AC-3: block 0 carries all SNR offsets; a block turning coupling
            // on carries them again (the coupling channel's fine offset
            // would otherwise be unknown) along with the leak values.
            w.plan[blk].snroffste = blk == 0 || (cplinu && !prev_cpl);
            w.plan[blk].cplleake = cplinu && (blk == 0 || !prev_cpl);
            prev_cpl = cplinu;
            for ch in 0..NCH {
                w.masks[blk][ch] = None;
                let Some(set) = &w.exps[blk][ch] else { continue };
                if ch == CPL && !cplinu {
                    continue;
                }
                let (s, e) = w.range[blk][ch];
                let kind = match ch {
                    CPL => Kind::Cpl {
                        fastleak: (i32::from(CPL_LEAK.0) << 8) + 768,
                        slowleak: (i32::from(CPL_LEAK.1) << 8) + 768,
                    },
                    LFE => Kind::Lfe,
                    _ => Kind::Fbw,
                };
                w.masks[blk][ch] = Some(compute_mask(&set.exps, s, e, p.fscod, &ba, fast_gain(FGAINCOD), kind, None));
            }
        }
    }

    /// Write (or, with `mant = None`, size) the syncframe.
    fn layout(&self, w: &Work, words: usize, mant: Option<(&[Packer], usize)>) -> (Layout, Option<Vec<u8>>) {
        match mant {
            None => {
                let mut c = Counter::default();
                let l = self.write(&mut c, w, words, None);
                (l, None)
            }
            Some(m) => {
                let mut wr = Writer::new(words * 2);
                let l = self.write(&mut wr, w, words, Some(m));
                (l, Some(wr.buf))
            }
        }
    }

    #[allow(clippy::needless_range_loop)]
    fn write<S: Sink>(&self, s: &mut S, w: &Work, words: usize, mant: Option<(&[Packer], usize)>) -> Layout {
        let p = &self.p;
        let nf = p.nfchans;
        let nb = p.numblks;
        let mut lay = Layout { mant_start: vec![0; nb], ..Default::default() };
        let snr = mant.map_or(0, |m| m.1);
        let (csnr, fsnr) = ((snr >> 4) as u32, (snr & 15) as u32);
        // ---- syncinfo + bsi ----
        s.put(0x0b77, 16);
        if p.eac3 {
            self.write_eac3_bsi(s, words);
            self.write_audfrm(s, w, csnr, fsnr);
        } else {
            s.put(0, 16); // crc1, solved last
            s.put(u32::from(p.fscod), 2);
            let pad = usize::from(FRMSIZETAB[usize::from(p.frmsizecod)][usize::from(p.fscod)]) != words;
            s.put(u32::from(p.frmsizecod) + u32::from(pad), 6);
            self.write_ac3_bsi(s);
        }
        // ---- audio blocks ----
        let cr = p.cpl;
        let mut first_cplcos = [true; MAX_FBW];
        let mut first_cplleak = true;
        for blk in 0..nb {
            let bp = &w.plan[blk];
            if !p.eac3 || p.block_switch {
                for ch in 0..nf {
                    s.bit(bp.blksw[ch]);
                }
            }
            // dithflage = 1 in E-AC-3
            for ch in 0..nf {
                s.bit(bp.dith[ch]);
            }
            s.bit(false); // dynrnge
            if p.acmod == 0 {
                s.bit(false); // dynrng2e
            }
            if p.eac3 {
                if blk == 0 {
                    s.bit(false); // spxinu (spxstre implied)
                } else {
                    s.bit(false); // spxstre
                }
            }
            // coupling strategy
            if p.eac3 {
                if bp.cplstre && bp.cplinu {
                    s.bit(false); // ecplinu
                    if p.acmod != 2 {
                        for ch in 0..nf {
                            s.bit(bp.incpl[ch]);
                        }
                    }
                    self.write_cpl_range(s, bp, true);
                } else if bp.cplstre {
                    first_cplcos = [true; MAX_FBW];
                    first_cplleak = true;
                }
            } else {
                s.bit(bp.cplstre);
                if bp.cplstre {
                    s.bit(bp.cplinu);
                    if bp.cplinu {
                        for ch in 0..nf {
                            s.bit(bp.incpl[ch]);
                        }
                        self.write_cpl_range(s, bp, false);
                    }
                }
            }
            // coupling coordinates
            if bp.cplinu {
                let nbnd = cr.expect("cpl").bands().len();
                for ch in 0..nf {
                    if !bp.incpl[ch] {
                        first_cplcos[ch] = true;
                        continue;
                    }
                    if p.eac3 && first_cplcos[ch] {
                        debug_assert!(bp.cplcoe[ch], "E-AC-3 implies cplcoe here");
                        first_cplcos[ch] = false;
                    } else {
                        s.bit(bp.cplcoe[ch]);
                    }
                    if bp.cplcoe[ch] {
                        s.put(u32::from(bp.mstrcplco[ch]), 2);
                        for b in 0..nbnd {
                            s.put(u32::from(bp.cplco[ch][b].exp), 4);
                            s.put(u32::from(bp.cplco[ch][b].mant), 4);
                        }
                    }
                }
                if p.acmod == 2 && bp.phsflginu && (bp.cplcoe[0] || bp.cplcoe[1]) {
                    for b in 0..nbnd {
                        s.bit(bp.phsflg[b]);
                    }
                }
            }
            // rematrixing
            if p.acmod == 2 {
                if !(p.eac3 && blk == 0) {
                    s.bit(bp.rematstr);
                }
                if bp.rematstr {
                    for b in 0..bp.nrematbd {
                        s.bit(bp.rematflg[b]);
                    }
                }
            }
            // exponent strategies (AC-3; E-AC-3 sent them in audfrm)
            if !p.eac3 {
                if bp.cplinu {
                    s.put(u32::from(bp.expstr[CPL]), 2);
                }
                for ch in 0..nf {
                    s.put(u32::from(bp.expstr[ch]), 2);
                }
                if p.lfeon {
                    s.put(u32::from(bp.expstr[LFE] != REUSE), 1);
                }
            }
            for ch in 0..nf {
                if bp.expstr[ch] != REUSE && !bp.incpl[ch] {
                    s.put(u32::from(p.bwcod), 6);
                }
            }
            // exponents
            if bp.cplinu && bp.expstr[CPL] != REUSE {
                let c = w.exps[blk][CPL].as_ref().expect("cpl exps");
                s.put(u32::from(c.abs), 4);
                for &g in &c.groups {
                    s.put(u32::from(g), 7);
                }
            }
            for ch in 0..nf {
                if bp.expstr[ch] != REUSE {
                    let c = w.exps[blk][ch].as_ref().expect("exps");
                    s.put(u32::from(c.abs), 4);
                    for &g in &c.groups {
                        s.put(u32::from(g), 7);
                    }
                    s.put(0, 2); // gainrng
                }
            }
            if p.lfeon && bp.expstr[LFE] != REUSE {
                let c = w.exps[blk][LFE].as_ref().expect("lfe exps");
                s.put(u32::from(c.abs), 4);
                for &g in &c.groups {
                    s.put(u32::from(g), 7);
                }
            }
            // bit allocation parameters
            if !p.eac3 {
                s.bit(blk == 0); // baie
                if blk == 0 {
                    let c = AC3_BA;
                    s.put(u32::from(c.0), 2);
                    s.put(u32::from(c.1), 2);
                    s.put(u32::from(c.2), 2);
                    s.put(u32::from(c.3), 2);
                    s.put(u32::from(c.4), 3);
                }
                // SNR offsets
                s.bit(bp.snroffste);
                if bp.snroffste {
                    s.put(csnr, 6);
                    if bp.cplinu {
                        s.put(fsnr, 4);
                        s.put(u32::from(FGAINCOD), 3);
                    }
                    for _ in 0..nf {
                        s.put(fsnr, 4);
                        s.put(u32::from(FGAINCOD), 3);
                    }
                    if p.lfeon {
                        s.put(fsnr, 4);
                        s.put(u32::from(FGAINCOD), 3);
                    }
                }
            } else if p.strmtyp == 0 {
                s.bit(false); // convsnroffste
            }
            // coupling leak
            if bp.cplinu {
                if p.eac3 && first_cplleak {
                    first_cplleak = false;
                    s.put(u32::from(CPL_LEAK.0), 3);
                    s.put(u32::from(CPL_LEAK.1), 3);
                } else {
                    let send = !p.eac3 && bp.cplleake;
                    s.bit(send);
                    if send {
                        s.put(u32::from(CPL_LEAK.0), 3);
                        s.put(u32::from(CPL_LEAK.1), 3);
                    }
                }
            }
            if !p.eac3 {
                s.bit(false); // deltbaie
                // skip field
                s.bit(bp.skip_bytes > 0);
                if bp.skip_bytes > 0 {
                    s.put(bp.skip_bytes as u32, 9);
                    for _ in 0..bp.skip_bytes {
                        s.put(0, 8);
                    }
                }
            }
            lay.mant_start[blk] = s.pos();
            if let Some((packers, _)) = mant
                && s.full()
            {
                for &(v, n) in &packers[blk].items {
                    s.put(v, n);
                }
            }
            if blk == 1 {
                lay.end_blk1 = s.pos();
            }
        }
        // ---- auxdata + errorcheck ----
        if s.full() {
            let tail = words * 16 - 18;
            let pos = s.pos();
            debug_assert!(pos <= tail, "frame overflow: {pos} > {tail}");
            for _ in pos..tail {
                s.put(0, 1);
            }
        }
        s.bit(false); // auxdatae
        s.bit(false); // crcrsv / encinfo
        s.put(0, 16); // crc2, solved last
        lay.total = s.pos();
        lay
    }

    fn write_cpl_range<S: Sink>(&self, s: &mut S, bp: &BlockPlan, eac3: bool) {
        let p = &self.p;
        let cr = p.cpl.expect("cpl");
        if p.acmod == 2 {
            s.bit(bp.phsflginu);
        }
        s.put(u32::from(cr.begf), 4);
        s.put(u32::from(cr.endf), 4);
        if eac3 {
            s.bit(true); // cplbndstrce
        }
        for bnd in 1..cr.nsub() {
            s.put(u32::from(cr.bndstrc[usize::from(cr.begf) + bnd]), 1);
        }
    }

    /// AC-3 `bsi()` (Table 5.2).
    fn write_ac3_bsi<S: Sink>(&self, s: &mut S) {
        let p = &self.p;
        s.put(8, 5); // bsid
        s.put(u32::from(p.bsmod), 3);
        s.put(u32::from(p.acmod), 3);
        if (p.acmod & 1) != 0 && p.acmod != 1 {
            s.put(0, 2); // cmixlev: −3 dB
        }
        if (p.acmod & 4) != 0 {
            s.put(0, 2); // surmixlev: −3 dB
        }
        if p.acmod == 2 {
            s.put(0, 2); // dsurmod: not indicated
        }
        s.bit(p.lfeon);
        s.put(u32::from(p.dialnorm), 5);
        s.bit(false); // compre
        s.bit(false); // langcode
        s.bit(false); // audprodie
        if p.acmod == 0 {
            s.put(u32::from(p.dialnorm), 5); // dialnorm2
            s.bit(false); // compr2e
            s.bit(false); // langcod2e
            s.bit(false); // audprodi2e
        }
        s.bit(false); // copyrightb
        s.bit(true); // origbs
        s.bit(false); // timecod1e
        s.bit(false); // timecod2e
        s.bit(false); // addbsie
    }

    /// E-AC-3 `bsi()` (Table E1.2).
    fn write_eac3_bsi<S: Sink>(&self, s: &mut S, words: usize) {
        let p = &self.p;
        s.put(u32::from(p.strmtyp), 2);
        s.put(u32::from(p.substreamid), 3);
        s.put(words as u32 - 1, 11);
        s.put(u32::from(p.fscod), 2);
        let numblkscod = match p.numblks {
            1 => 0,
            2 => 1,
            3 => 2,
            _ => 3,
        };
        s.put(numblkscod, 2);
        s.put(u32::from(p.acmod), 3);
        s.bit(p.lfeon);
        s.put(16, 5); // bsid
        s.put(u32::from(p.dialnorm), 5);
        s.bit(false); // compre
        if p.acmod == 0 {
            s.put(u32::from(p.dialnorm), 5); // dialnorm2
            s.bit(false); // compr2e
        }
        if p.strmtyp == 1 {
            s.bit(p.chanmap.is_some());
            if let Some(m) = p.chanmap {
                s.put(u32::from(m), 16);
            }
        }
        s.bit(false); // mixmdate
        s.bit(true); // infomdate
        s.put(u32::from(p.bsmod), 3);
        s.bit(false); // copyrightb
        s.bit(true); // origbs
        if p.acmod == 2 {
            s.put(0, 2); // dsurmod
            s.put(0, 2); // dheadphonmod
        }
        if p.acmod >= 6 {
            s.put(0, 2); // dsurexmod
        }
        s.bit(false); // audprodie
        if p.acmod == 0 {
            s.bit(false); // audprodi2e
        }
        if p.fscod < 3 {
            s.bit(false); // sourcefscod
        }
        if p.strmtyp == 0 && p.numblks != 6 {
            // convsync: this frame starts a group of six blocks
            s.bit(self.blocks.is_multiple_of(6));
        }
        s.bit(false); // addbsie
    }

    /// E-AC-3 `audfrm()` (Table E1.3).
    fn write_audfrm<S: Sink>(&self, s: &mut S, w: &Work, csnr: u32, fsnr: u32) {
        let p = &self.p;
        let nb = p.numblks;
        let nf = p.nfchans;
        let expstre = w.frame_exp.rows.is_none();
        if nb == 6 {
            s.bit(expstre);
            s.bit(false); // ahte
        }
        s.put(0, 2); // snroffststr: one frame-wide offset
        s.bit(false); // transproce
        s.bit(p.block_switch); // blkswe
        s.bit(true); // dithflage
        s.bit(false); // bamode
        s.bit(false); // frmfgaincode
        s.bit(false); // dbaflde
        s.bit(false); // skipflde
        s.bit(false); // spxattene
        if p.acmod > 1 {
            s.bit(w.plan[0].cplinu);
            for blk in 1..nb {
                s.bit(w.plan[blk].cplstre);
                if w.plan[blk].cplstre {
                    s.bit(w.plan[blk].cplinu);
                }
            }
        }
        let ncplblks = w.plan.iter().filter(|b| b.cplinu).count();
        match w.frame_exp.rows {
            None => {
                for blk in 0..nb {
                    if w.plan[blk].cplinu {
                        s.put(u32::from(w.plan[blk].expstr[CPL]), 2);
                    }
                    for ch in 0..nf {
                        s.put(u32::from(w.plan[blk].expstr[ch]), 2);
                    }
                }
            }
            Some((rows, cplrow)) => {
                if p.acmod > 1 && ncplblks > 0 {
                    s.put(u32::from(cplrow.expect("cpl row")), 5);
                }
                for &r in &rows[..nf] {
                    s.put(u32::from(r), 5);
                }
            }
        }
        if p.lfeon {
            for blk in 0..nb {
                s.put(u32::from(w.plan[blk].expstr[LFE] != REUSE), 1);
            }
        }
        if p.strmtyp == 0 {
            // Converter exponent strategy (§2.3.2.14): required once every
            // six blocks; describe the frame's channels by the nearest row.
            let convexpstre = nb == 6 || self.blocks.is_multiple_of(6);
            if nb != 6 {
                s.bit(convexpstre);
            }
            if convexpstre {
                for ch in 0..nf {
                    let row = match w.frame_exp.rows {
                        Some((rows, _)) => rows[ch],
                        None => nearest_row(&(0..6).map(|b| w.plan.get(b).map_or(D45, |bp| bp.expstr[ch])).collect::<Vec<_>>()),
                    };
                    s.put(u32::from(row), 5);
                }
            }
        }
        s.put(csnr, 6); // frmcsnroffst
        s.put(fsnr, 4); // frmfsnroffst
        if nb != 1 {
            s.bit(false); // blkstrtinfoe
        }
    }
}

/// Sum of the strategy-choice costs of a track's runs (for comparing the
/// E-AC-3 frame rows with per-block strategies).
fn run_costs(t: &Track, strat: &[u8]) -> f32 {
    let nb = strat.len();
    let mut cost = 0.0;
    let mut b = 0;
    while b < nb {
        if t.raw[b].is_none() {
            b += 1;
            continue;
        }
        let mut k = 1;
        while b + k < nb && strat[b + k] == REUSE && t.raw[b + k].is_some() {
            k += 1;
        }
        cost += exponents::run_cost(t, b, k, strat[b]);
        b += k;
    }
    cost
}

/// The Table E2.10 row closest to a six-block strategy pattern.
fn nearest_row(strat: &[u8]) -> u8 {
    let mut best = (usize::MAX, 0u8);
    for (i, row) in crate::tables::FRMEXPSTR.iter().enumerate() {
        let d = row.iter().zip(strat).filter(|(a, b)| a != b).count();
        if d < best.0 {
            best = (d, i as u8);
        }
    }
    best.1
}

/// Quantise coupling coordinates (§8.2.5.2): the decoder rebuilds
/// `8 · temp / 2^(cplcoexp + 3·mstrcplco)` with `temp = (16 + mant)/32`, or
/// `mant/16` when `cplcoexp = 15`. Returns `mstrcplco` and per band
/// (`cplcoexp`, `cplcomant`).
fn quantize_coords(ratio: &[f32]) -> (u8, [Co; 18]) {
    let mut e = [0i32; 18];
    let mut min_e = i32::MAX;
    for (b, &r) in ratio.iter().enumerate() {
        let v = (r / 8.0).min(31.0 / 32.0);
        e[b] = if v > 0.0 { (-v.log2()).floor().max(0.0) as i32 } else { 99 };
        // v·2^e ∈ [0.5, 1)
        min_e = min_e.min(e[b]);
    }
    let mstr = (min_e / 3).clamp(0, 3);
    let mut out = [Co::default(); 18];
    for (b, &r) in ratio.iter().enumerate() {
        let v = (r / 8.0).min(31.0 / 32.0);
        let ce = e[b] - 3 * mstr;
        if ce >= 15 {
            let m = (v * 2f32.powi(15 + 3 * mstr) * 16.0).round().clamp(0.0, 15.0);
            out[b] = Co { exp: 15, mant: m as u8 };
        } else {
            let m = (v * 2f32.powi(e[b]) * 32.0 - 16.0).round().clamp(0.0, 15.0);
            out[b] = Co { exp: ce as u8, mant: m as u8 };
        }
    }
    (mstr as u8, out)
}

/// The coordinate the decoder applies (§7.4.3, times its factor of 8).
fn coord_value(c: Co, mstr: u8) -> f32 {
    let temp = if c.exp == 15 { f32::from(c.mant) / 16.0 } else { (f32::from(c.mant) + 16.0) / 32.0 };
    8.0 * temp / (1u64 << (u32::from(c.exp) + 3 * u32::from(mstr))) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coupling_coordinates_round_trip_within_half_a_step() {
        for &r in &[7.7f32, 4.0, 1.0, 0.6, 0.3, 0.01, 0.0005] {
            let (m, c) = quantize_coords(&[r]);
            let back = coord_value(c[0], m);
            let rel = (back - r).abs() / r;
            // 4-bit mantissa on [16, 32): at most 1/32 relative error,
            // the denormal range (exp 15) is coarser.
            let tol = if c[0].exp == 15 { 0.25 } else { 1.0 / 31.0 };
            assert!(rel <= tol, "r {r}: got {back} (mstr {m} {:?})", c[0]);
        }
        // the largest band sets mstrcplco so it keeps a normal exponent
        let (m, c) = quantize_coords(&[0.01, 0.002]);
        assert_eq!(m, 3);
        assert!(c[0].exp < 15);
    }

    #[test]
    fn coupling_bands_follow_the_band_structure() {
        let cr = CplRange { begf: 6, endf: 12, bndstrc: crate::tables::DEFCPLBNDSTRC };
        // sub-bands 6..=14 (nsub 9); Table E2.12 merges 8 into 7, 10–11 into 9, 13–14 into 12
        assert_eq!(cr.nsub(), 9);
        assert_eq!(cr.strt(), 109);
        assert_eq!(cr.end(), 217);
        let b = cr.bands();
        assert_eq!(b, vec![(109, 121), (121, 145), (145, 181), (181, 217)]);
    }
}
