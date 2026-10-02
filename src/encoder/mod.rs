//! The AC-3 / E-AC-3 encoder: A/52:2018 §8 (and Annex E for the E-AC-3
//! syntax), with the bit stream written so that this crate's decoder — and
//! any conformant one — parses every field the way it was meant.
//!
//! See [`Encoder`] for what it does and does not do.

mod bits;
mod exponents;
mod frame;
mod mdct;
mod quant;
mod transient;

use crate::tables::{BITRATE_KBPS, DEFCPLBNDSTRC};
use crate::{Error, Speaker};
use frame::{CplRange, LFE, Params, SubEncoder};

/// Which bit stream syntax to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Format {
    /// AC-3 (Dolby Digital), `bsid` 8: six blocks per syncframe, the frame
    /// sizes of Table 5.18, 32–640 kbit/s.
    Ac3,
    /// E-AC-3 (Dolby Digital Plus), `bsid` 16 (Annex E): independent
    /// substream 0, plus a dependent substream for [`Layout::ThreeFour`].
    Eac3,
}

/// The channel layout, by A/52 audio coding mode (`acmod`, Table 5.8). The
/// LFE channel is [`Config::lfe`]. Interleaved input is in the order
/// [`Layout::speakers`] gives — the same order the decoder outputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Layout {
    /// 1+1 (`acmod` 0): two independent mono programmes, as FL FR.
    DualMono,
    /// 1/0: FC.
    Mono,
    /// 2/0: FL FR.
    Stereo,
    /// 3/0: FL FR FC.
    ThreeZero,
    /// 2/1: FL FR BC.
    TwoOne,
    /// 3/1: FL FR FC BC.
    ThreeOne,
    /// 2/2: FL FR SL SR.
    TwoTwo,
    /// 3/2: FL FR FC SL SR (5.1 with the LFE: FL FR FC LFE SL SR).
    ThreeTwo,
    /// 3/4, E-AC-3 only (7.1 with the LFE: FL FR FC LFE BL BR SL SR): a 3/2
    /// independent substream carrying FL FR FC SL SR (and the LFE) plus a
    /// 2/0 dependent substream carrying BL BR, custom channel map
    /// "Lrs/Rrs pair" (Annex E Table E2.5).
    ThreeFour,
}

impl Layout {
    fn acmod(self) -> u8 {
        match self {
            Layout::DualMono => 0,
            Layout::Mono => 1,
            Layout::Stereo => 2,
            Layout::ThreeZero => 3,
            Layout::TwoOne => 4,
            Layout::ThreeOne => 5,
            Layout::TwoTwo => 6,
            Layout::ThreeTwo | Layout::ThreeFour => 7,
        }
    }

    /// The speakers of the interleaved input, in order.
    pub fn speakers(self, lfe: bool) -> Vec<Speaker> {
        use Speaker::*;
        let mut v = match self {
            Layout::Mono => vec![FC],
            Layout::DualMono | Layout::Stereo | Layout::TwoOne | Layout::TwoTwo => vec![FL, FR],
            _ => vec![FL, FR, FC],
        };
        if lfe {
            v.push(LFE);
        }
        v.extend_from_slice(match self {
            Layout::TwoOne | Layout::ThreeOne => &[BC][..],
            Layout::TwoTwo | Layout::ThreeTwo => &[SL, SR][..],
            Layout::ThreeFour => &[BL, BR, SL, SR][..],
            _ => &[][..],
        });
        v
    }
}

/// Channel coupling policy (§8.2.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Coupling {
    /// Couple the high frequencies when the bit rate per channel is low
    /// (below 96 kbit/s per full-bandwidth channel), with a range set by the
    /// rate.
    Auto,
    /// Never couple.
    Off,
    /// Always couple (layouts with two or more full-bandwidth channels,
    /// not dual mono).
    On,
}

/// Encoder configuration. [`Config::new`] fills in the defaults.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Config {
    /// AC-3 or E-AC-3 syntax.
    pub format: Format,
    /// Samples per second: 48 000, 44 100 or 32 000.
    pub sample_rate: u32,
    /// The full-bandwidth channels.
    pub layout: Layout,
    /// An LFE channel (input after the front channels).
    pub lfe: bool,
    /// Total bit rate in kbit/s. AC-3: one of Table 5.18's rates, 32–640.
    /// E-AC-3: 32–6144, as far as the 2048-word frame allows at the sample
    /// rate (6144 needs 48 kHz); for [`Layout::ThreeFour`] the total of both
    /// substreams.
    pub bitrate_kbps: u32,
    /// `bsmod`, bit stream mode (Table 5.7): 0 = main audio, complete main.
    pub bsmod: u8,
    /// `dialnorm`, 1–31: the dialogue level is −dialnorm dBFS. Default 31.
    pub dialnorm: u8,
    /// Channel coupling policy. Default [`Coupling::Auto`].
    pub coupling: Coupling,
    /// Rematrixing in 2/0 (§8.2.6). Default on.
    pub rematrixing: bool,
    /// Block switching on transients (§8.2.2). Default on.
    pub block_switching: bool,
}

impl Config {
    /// A configuration with the defaults: `bsmod` 0, `dialnorm` 31,
    /// automatic coupling, rematrixing and block switching on.
    pub fn new(format: Format, sample_rate: u32, layout: Layout, lfe: bool, bitrate_kbps: u32) -> Self {
        Self {
            format,
            sample_rate,
            layout,
            lfe,
            bitrate_kbps,
            bsmod: 0,
            dialnorm: 31,
            coupling: Coupling::Auto,
            rematrixing: true,
            block_switching: true,
        }
    }

    /// Interleaved input channels.
    pub fn channels(&self) -> usize {
        self.layout.speakers(self.lfe).len()
    }
}

/// One substream: its encoder, where its coded channels come from in the
/// input, and its frame size bookkeeping.
struct Sub {
    enc: SubEncoder,
    /// Input slot of each coded channel (fbw in `acmod` order, then LFE).
    slots: Vec<usize>,
    /// Frame size: words per frame are `num / den`, rounded so the running
    /// total never drifts (the 44.1 kHz alternation of Table 5.18).
    num: u128,
    den: u128,
    words_out: u128,
    frames: u128,
}

impl Sub {
    fn next_words(&mut self) -> usize {
        let total = (self.frames + 1) * self.num / self.den;
        let w = (total - self.words_out) as usize;
        self.words_out = total;
        self.frames += 1;
        w
    }
}

/// An AC-3 / E-AC-3 encoder.
///
/// Takes interleaved `f32` PCM (±1.0 full scale) in [`Layout::speakers`]
/// order and returns whole syncframes. Each `Vec<u8>` returned is one
/// syncframe — for E-AC-3 7.1, the independent and the dependent substream's
/// syncframes for the same audio, concatenated (one access unit).
///
/// What it does: the §8.2.3 transform with block switching on transients
/// (§8.2.2's detector), coupling with phase flags and per-band coordinates,
/// rematrixing, exponent strategies chosen by cost (every split of the frame
/// into runs is weighed: exponent bits against the mantissa bits a shared,
/// lowered exponent wastes; E-AC-3 six-block frames also weigh the Table
/// E2.10 frame strategies), the parametric bit allocation with the SNR offset
/// searched for the largest allocation that fits, dither flags (§8.2.9), and
/// both CRCs. AC-3 frames honour §5.5 (blocks 0 and 1 within the first 5/8,
/// block 5's mantissas within the last 3/8) using skip fields where needed.
///
/// What it does not do: spectral extension, the adaptive hybrid transform,
/// enhanced coupling, transient pre-noise processing, delta bit allocation,
/// dynamic range metadata (`dynrng` / `compr` are not sent), reduced sample
/// rates (E-AC-3 `fscod2`), and more than one independent substream.
///
/// Delay: the transform's overlap delays the output by [`Encoder::delay`]
/// (256) samples — decoded sample `n + 256` is input sample `n`.
/// [`Encoder::flush`] pads with silence so every input sample is in a frame.
pub struct Encoder {
    cfg: Config,
    subs: Vec<Sub>,
    numblks: usize,
    channels: usize,
    buf: Vec<Vec<f32>>,
    samples_in: u64,
    samples_out: u64,
}

impl std::fmt::Debug for Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Encoder").field("config", &self.cfg).field("buffered", &self.buf.first().map_or(0, Vec::len)).finish()
    }
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::InvalidInput(msg.into())
}

/// Stream parameters chosen from the bit rate per channel.
struct Tuning {
    bwcod: u8,
    cpl: Option<CplRange>,
}

fn bin_of(hz: f32, fs: u32) -> f32 {
    hz * 512.0 / fs as f32
}

fn tune(kbps: f32, nf: usize, lfe: bool, acmod: u8, fs: u32, coupling: Coupling) -> Tuning {
    let r = kbps / (nf as f32 + if lfe { 0.25 } else { 0.0 });
    // Audio bandwidth for a channel coded on its own.
    let fb = (4000.0 + r * 125.0).clamp(4000.0, 20000.0);
    let bw_from = |hz: f32| -> u8 { (((bin_of(hz, fs) - 37.0) / 3.0).round() as i32 - 12).clamp(0, 60) as u8 };
    let can = nf >= 2 && acmod != 0;
    let want = match coupling {
        Coupling::Off => false,
        Coupling::On => can,
        Coupling::Auto => can && r < 96.0,
    };
    if !want {
        return Tuning { bwcod: bw_from(fb), cpl: None };
    }
    let fc = (3000.0 + r * 100.0).clamp(4500.0, 15000.0);
    let fe = (8000.0 + r * 120.0).clamp(fc + 2000.0, 20000.0);
    let begf = (((bin_of(fc, fs) - 37.0) / 12.0).round() as i32).clamp(0, 15);
    let endf = ((((bin_of(fe, fs) - 37.0) / 12.0).round() as i32) - 3).clamp((begf - 1).max(0), 15);
    let cpl = CplRange { begf: begf as u8, endf: endf as u8, bndstrc: DEFCPLBNDSTRC };
    // A channel that leaves coupling (block switched) keeps the coupling
    // range's bandwidth.
    let bwcod = ((((cpl.end() as i32) - 37) / 3) - 12).clamp(0, 60) as u8;
    Tuning { bwcod, cpl: Some(cpl) }
}

impl Encoder {
    /// An encoder for `cfg`; [`Error::InvalidInput`] for a configuration
    /// outside what the format allows or this encoder implements.
    pub fn new(cfg: Config) -> Result<Self, Error> {
        let fscod = match cfg.sample_rate {
            48_000 => 0u8,
            44_100 => 1,
            32_000 => 2,
            r => return Err(invalid(format!("sample rate {r}: AC-3 / E-AC-3 code 48000, 44100 or 32000 here"))),
        };
        if cfg.bsmod > 7 {
            return Err(invalid("bsmod is a 3-bit code"));
        }
        if !(1..=31).contains(&cfg.dialnorm) {
            return Err(invalid("dialnorm must be 1..=31"));
        }
        let fs = cfg.sample_rate;
        let mut subs = Vec::new();
        let speakers = cfg.layout.speakers(cfg.lfe);
        let numblks;
        match cfg.format {
            Format::Ac3 => {
                if cfg.layout == Layout::ThreeFour {
                    return Err(invalid("3/4 (7.1) needs E-AC-3 dependent substreams"));
                }
                let Some(idx) = BITRATE_KBPS.iter().position(|&r| u32::from(r) == cfg.bitrate_kbps) else {
                    return Err(invalid(format!("{} kbit/s is not an AC-3 bit rate (Table 5.18)", cfg.bitrate_kbps)));
                };
                numblks = 6;
                let acmod = cfg.layout.acmod();
                subs.push(make_sub(&cfg, fscod, acmod, cfg.lfe, cfg.bitrate_kbps, 6, 0, None, (idx * 2) as u8, &speakers, None)?);
            }
            Format::Eac3 => {
                if !(32..=6144).contains(&cfg.bitrate_kbps) {
                    return Err(invalid("E-AC-3 bit rate must be 32..=6144 kbit/s"));
                }
                let split: Vec<(u8, bool, f32)> = if cfg.layout == Layout::ThreeFour {
                    // shared by channel count (the LFE as a quarter), the
                    // dependent substream taking the exact remainder
                    let ind = 5.0 + if cfg.lfe { 0.25 } else { 0.0 };
                    let k = cfg.bitrate_kbps as f32;
                    let main = (k * ind / (ind + 2.0)).round();
                    vec![(7, cfg.lfe, main), (2, false, k - main)]
                } else {
                    vec![(cfg.layout.acmod(), cfg.lfe, cfg.bitrate_kbps as f32)]
                };
                // The most blocks per frame that keeps every frame within
                // 2048 words.
                let words = |kbps: f32, nb: usize| kbps * 1000.0 * (nb * 256) as f32 / (fs as f32 * 16.0);
                let Some(nb) = [6usize, 3, 2, 1].into_iter().find(|&nb| split.iter().all(|s| words(s.2, nb).ceil() <= 2048.0))
                else {
                    return Err(invalid(format!("{} kbit/s does not fit E-AC-3 frames at {fs} Hz", cfg.bitrate_kbps)));
                };
                numblks = nb;
                for (i, &(acmod, lfe, kbps)) in split.iter().enumerate() {
                    let (strmtyp, chanmap) = if i == 0 { (0, None) } else { (1, Some(0x0200u16)) };
                    subs.push(make_sub(&cfg, fscod, acmod, lfe, kbps.round() as u32, nb, strmtyp, chanmap, 0, &speakers, Some(i))?);
                }
            }
        }
        // A bit rate too low for the layout's side information is refused
        // here rather than at the first frame: a probe frame of full-band
        // noise (the most side information this encoder ever spends after
        // its fallbacks) must fit.
        for sub in &subs {
            let mut probe = sub.enc.clone();
            let fl = numblks * 256;
            let mut seed = 0x1234_5678u32;
            let pcm: Vec<Vec<f32>> = sub
                .slots
                .iter()
                .map(|_| {
                    (0..fl)
                        .map(|_| {
                            seed ^= seed << 13;
                            seed ^= seed >> 17;
                            seed ^= seed << 5;
                            0.5 * (seed as i32 as f32 / 2_147_483_648.0)
                        })
                        .collect()
                })
                .collect();
            let words = (sub.num / sub.den) as usize;
            if let Err(e) = probe.encode_frame(&pcm, words) {
                return Err(invalid(format!("{} kbit/s is too low for this layout: {e}", cfg.bitrate_kbps)));
            }
        }
        Ok(Self {
            channels: speakers.len(),
            buf: vec![Vec::new(); speakers.len()],
            cfg,
            subs,
            numblks,
            samples_in: 0,
            samples_out: 0,
        })
    }

    /// The configuration in use.
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// The speakers of the interleaved input, in order.
    pub fn speakers(&self) -> Vec<Speaker> {
        self.cfg.layout.speakers(self.cfg.lfe)
    }

    /// Samples per channel in one syncframe (256 × blocks per frame).
    pub fn frame_samples(&self) -> usize {
        self.numblks * 256
    }

    /// Samples by which the decoded output lags the input: 256.
    pub fn delay(&self) -> usize {
        256
    }

    /// Append interleaved samples and return every syncframe now complete.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<Vec<u8>>, Error> {
        if !pcm.len().is_multiple_of(self.channels) {
            return Err(invalid(format!("{} samples is not a whole number of {}-channel frames", pcm.len(), self.channels)));
        }
        for (i, &v) in pcm.iter().enumerate() {
            self.buf[i % self.channels].push(v);
        }
        self.samples_in += (pcm.len() / self.channels) as u64;
        self.drain()
    }

    /// Pad with silence until every sample given (plus the transform delay)
    /// is in a syncframe, and return the remaining syncframes. Call once, at
    /// the end of the stream.
    pub fn flush(&mut self) -> Result<Vec<Vec<u8>>, Error> {
        let need = self.samples_in + self.delay() as u64;
        let fl = self.frame_samples() as u64;
        let frames = need.saturating_sub(self.samples_out).div_ceil(fl);
        let target = self.samples_out + frames * fl;
        for b in &mut self.buf {
            b.resize((target - self.samples_out) as usize, 0.0);
        }
        self.drain()
    }

    fn drain(&mut self) -> Result<Vec<Vec<u8>>, Error> {
        let fl = self.frame_samples();
        let mut out = Vec::new();
        while self.buf[0].len() >= fl {
            let mut au = Vec::new();
            for sub in &mut self.subs {
                let pcm: Vec<Vec<f32>> = sub.slots.iter().map(|&s| self.buf[s][..fl].to_vec()).collect();
                let words = sub.next_words();
                au.extend_from_slice(&sub.enc.encode_frame(&pcm, words)?);
            }
            for b in &mut self.buf {
                b.drain(..fl);
            }
            self.samples_out += fl as u64;
            out.push(au);
        }
        Ok(out)
    }
}

#[allow(clippy::too_many_arguments)]
fn make_sub(
    cfg: &Config,
    fscod: u8,
    acmod: u8,
    lfe: bool,
    kbps: u32,
    numblks: usize,
    strmtyp: u8,
    chanmap: Option<u16>,
    frmsizecod: u8,
    speakers: &[Speaker],
    sub_index: Option<usize>,
) -> Result<Sub, Error> {
    let nf = crate::decoder::nfchans_for(acmod);
    // Input slots of the coded channels: the decoder's output order maps
    // output slot → coded channel; invert it over this substream's speakers.
    let order = crate::decoder::output_order(acmod, lfe);
    let sub_speakers: Vec<Speaker> = match sub_index {
        Some(1) => vec![Speaker::BL, Speaker::BR],
        _ if cfg.layout == Layout::ThreeFour => Layout::ThreeTwo.speakers(lfe),
        _ => speakers.to_vec(),
    };
    let mut slots = vec![usize::MAX; nf + usize::from(lfe)];
    for (out_slot, &coded) in order.iter().enumerate() {
        let spk = sub_speakers[out_slot];
        let input = speakers.iter().position(|&s| s == spk).expect("speaker in layout");
        let idx = if coded == LFE { nf } else { coded };
        slots[idx] = input;
    }
    let t = tune(kbps as f32, nf, lfe, acmod, cfg.sample_rate, cfg.coupling);
    let p = Params {
        eac3: cfg.format == Format::Eac3,
        strmtyp,
        substreamid: 0,
        chanmap,
        fscod,
        sample_rate: cfg.sample_rate,
        acmod,
        lfeon: lfe,
        nfchans: nf,
        numblks,
        bsmod: cfg.bsmod,
        dialnorm: cfg.dialnorm,
        bwcod: t.bwcod,
        cpl: t.cpl,
        rematrix: cfg.rematrixing,
        block_switch: cfg.block_switching,
        frmsizecod,
        lambda: 1.0,
    };
    Ok(Sub {
        enc: SubEncoder::new(p),
        slots,
        num: u128::from(kbps) * 1000 * (numblks as u128 * 256),
        den: u128::from(cfg.sample_rate) * 16,
        words_out: 0,
        frames: 0,
    })
}
