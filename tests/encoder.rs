//! The encoder, checked against the spec and through this crate's decoder.
//! No other implementation is involved.
//!
//! - **Syntax** (`check_stream`, applied to every syncframe every test
//!   writes): the header parses with the configured format, rate and layout;
//!   AC-3 frame sizes are exactly Table 5.18's for the `frmsizecod` sent and
//!   the running total of frame sizes keeps to the nominal bit rate within a
//!   word (the 44.1 kHz alternation), E-AC-3 `frmsiz` likewise; `crc1`
//!   leaves the §7.10.1 register at zero at the 5/8 point and `crc2` at the
//!   end; every block parses without error; blocks 0–1 end before the 5/8
//!   point and block 5's mantissas start after it (§5.5); everything from
//!   the end of the last block to `auxdatae` is zero padding.
//! - **Round trip**: per-channel SNR at several bit rates, frequency
//!   response, pre-echo before a transient with and without block
//!   switching, and the coding tools actually exercised (coupling, phase
//!   flags, rematrixing, block switching, frame exponent strategies).
//!
//! Run with `--nocapture` for the measured numbers.

use ac3::tables::{BITRATE_KBPS, FRMSIZETAB};
use ac3::{Config, Coupling, Encoder, Error, Format, FrameDecoder, Layout, Speaker, frame_crc_ok, parse_header};

/// CRC-16, x¹⁶ + x¹⁵ + x² + 1, zero initial state, MSB first (§7.10.1).
fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 };
        }
    }
    crc
}

fn bit(frame: &[u8], pos: usize) -> bool {
    frame[pos / 8] & (0x80 >> (pos % 8)) != 0
}

struct Decoded {
    /// Interleaved PCM of independent substream 0, in decoder output order.
    pcm: Vec<f32>,
    /// Interleaved PCM of the 7.1 dependent substream (BL BR), if any.
    dep: Vec<f32>,
    /// Channels in `pcm`.
    channels: usize,
    frames: usize,
    features: ac3::Features,
    /// Distinct AC-3 frame sizes seen, in bytes.
    sizes: Vec<usize>,
}

/// Strict syntax check of every syncframe, and the decode.
fn check_stream(cfg: &Config, aus: &[Vec<u8>]) -> Decoded {
    let fs = cfg.sample_rate;
    let fscod = match fs {
        48_000 => 0,
        44_100 => 1,
        _ => 2,
    };
    let seven_one = cfg.layout == Layout::ThreeFour;
    let mut dec = FrameDecoder::new(1.0);
    let mut dep_dec = FrameDecoder::new(1.0);
    dep_dec.set_decode_all_substreams(true);
    let mut out = Vec::new();
    let mut dep = Vec::new();
    let mut total_bits = 0u64;
    let mut samples_total = 0u64;
    let mut channels = 0;
    let mut sizes = Vec::new();
    for (n, au) in aus.iter().enumerate() {
        let mut pos = 0;
        let mut sub = 0;
        while pos < au.len() {
            let f = &au[pos..];
            let h = parse_header(f).unwrap_or_else(|e| panic!("frame {n}: {e}"));
            assert!(h.frame_len <= f.len(), "frame {n}: truncated");
            let frame = &f[..h.frame_len];
            let bits = h.frame_len * 8;
            let words = h.frame_len / 2;
            assert_eq!(h.sample_rate, fs);
            assert_eq!(h.fscod, fscod);
            assert!(frame_crc_ok(frame), "frame {n} substream {sub}: crc2");
            total_bits += bits as u64;
            if cfg.format == Format::Ac3 {
                assert_eq!(h.bsid, 8);
                assert_eq!(h.bsmod, cfg.bsmod);
                let code = (frame[4] & 0x3f) as usize;
                assert_eq!(h.frame_len, usize::from(FRMSIZETAB[code][fscod as usize]) * 2, "frame {n}: Table 5.18 size");
                assert_eq!(h.bitrate_kbps, cfg.bitrate_kbps);
                let five8 = (words >> 1) + (words >> 3);
                assert_eq!(crc16(&frame[2..five8 * 2]), 0, "frame {n}: crc1 at the 5/8 point");
                if !sizes.contains(&h.frame_len) {
                    sizes.push(h.frame_len);
                }
            } else {
                assert_eq!(h.bsid, 16);
                assert_eq!(h.substreamid, 0);
                assert_eq!(h.strmtyp, sub as u8, "independent first, then the dependent substream");
            }
            assert_eq!(h.dialnorm, cfg.dialnorm);
            if sub == 0 {
                let expect = if seven_one { 5 + usize::from(cfg.lfe) } else { cfg.channels() };
                assert_eq!(h.channels(), expect);
                if !seven_one {
                    assert_eq!(h.speakers(), cfg.layout.speakers(cfg.lfe));
                }
                channels = expect;
            } else {
                assert_eq!((h.acmod, h.lfeon), (2, false));
            }
            let d = if sub == 0 { &mut dec } else { &mut dep_dec };
            let mut pcm = Vec::new();
            let got = d.decode(frame, &mut pcm).unwrap_or_else(|e| panic!("frame {n} substream {sub}: {e}"));
            assert!(got.is_some(), "frame {n} substream {sub} skipped");
            let blocks = d.block_bit_ranges().to_vec();
            assert_eq!(blocks.len(), h.numblks);
            let end = blocks.last().unwrap().1;
            assert!(end <= bits - 18, "frame {n}: audio runs into errorcheck");
            for b in end..bits - 18 {
                assert!(!bit(frame, b), "frame {n}: non-zero padding at bit {b} (audio ended at {end})");
            }
            assert!(!bit(frame, bits - 18), "frame {n}: auxdatae");
            if !h.eac3 {
                let five8 = ((words >> 1) + (words >> 3)) * 16;
                assert!(blocks[1].1 <= five8, "frame {n}: blocks 0-1 end at {} > 5/8 {five8}", blocks[1].1);
                assert!(blocks[5].0 >= five8, "frame {n}: block 5 mantissas start at {} < 5/8 {five8}", blocks[5].0);
            }
            if sub == 0 {
                out.extend_from_slice(&pcm);
                samples_total += h.samples() as u64;
            } else {
                dep.extend_from_slice(&pcm);
            }
            pos += h.frame_len;
            sub += 1;
        }
        assert_eq!(sub, if seven_one { 2 } else { 1 }, "access unit {n}");
    }
    // The running total of frame sizes keeps to the nominal bit rate, within
    // a word (and the rounding of a partial word) per substream.
    let nominal = u64::from(cfg.bitrate_kbps) * 1000 * samples_total / u64::from(fs);
    let subs = if seven_one { 2 } else { 1 };
    assert!(nominal.abs_diff(total_bits) <= 32 * subs, "{total_bits} bits written vs {nominal} nominal");
    Decoded { pcm: out, dep, channels, frames: aus.len(), features: dec.features(), sizes }
}

fn encode(cfg: Config, pcm: &[f32]) -> Vec<Vec<u8>> {
    let mut enc = Encoder::new(cfg).unwrap_or_else(|e| panic!("{cfg:?}: {e}"));
    let mut aus = Vec::new();
    // an odd chunk size, to exercise the buffering
    for chunk in pcm.chunks(cfg.channels() * 1001) {
        aus.extend(enc.encode(chunk).unwrap());
    }
    aus.extend(enc.flush().unwrap());
    aus
}

/// xorshift32, uniform in (−1, 1).
struct Rng(u32);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0 as i32 as f32 / 2_147_483_648.0
    }
}

/// Music-like material, different per channel: six harmonics of a note, a
/// decaying 1.8 kHz pluck every 0.25 s, and a noise floor low-passed at
/// about 2 kHz (so the SNR measures coding error, not the band limit).
fn music(ch: usize, n: usize, fs: u32) -> Vec<f32> {
    let mut r = Rng(0x9e37_79b9 ^ (ch as u32).wrapping_mul(0x85eb_ca6b).wrapping_add(1));
    let base = [110.0f32, 146.8, 196.0, 261.6, 329.6, 392.0, 440.0, 523.3][ch % 8];
    let mut lp = 0.0f32;
    (0..n)
        .map(|i| {
            let t = i as f32 / fs as f32;
            let mut v = 0.0;
            for (k, a) in [(1.0f32, 0.25f32), (2.0, 0.12), (3.0, 0.08), (5.0, 0.04), (8.0, 0.02), (13.0, 0.01)] {
                v += a * (2.0 * std::f32::consts::PI * base * k * t).sin();
            }
            v += 0.1 * (-(t % 0.25) * 30.0).exp() * (2.0 * std::f32::consts::PI * 1800.0 * t).sin();
            lp = 0.75 * lp + 0.25 * r.next();
            v + 0.02 * lp
        })
        .collect()
}

fn interleave(chans: &[Vec<f32>]) -> Vec<f32> {
    let n = chans[0].len();
    let mut out = Vec::with_capacity(n * chans.len());
    for i in 0..n {
        for c in chans {
            out.push(c[i]);
        }
    }
    out
}

/// Interleaved test input for a configuration: `music` per channel, a 60 Hz
/// tone in the LFE.
fn signal(cfg: &Config, seconds: f32) -> Vec<f32> {
    let fs = cfg.sample_rate;
    let n = (fs as f32 * seconds) as usize;
    let chans: Vec<Vec<f32>> = cfg
        .layout
        .speakers(cfg.lfe)
        .iter()
        .enumerate()
        .map(|(c, s)| {
            if *s == Speaker::LFE {
                (0..n).map(|i| 0.3 * (2.0 * std::f32::consts::PI * 60.0 * i as f32 / fs as f32).sin()).collect()
            } else {
                music(c, n, fs)
            }
        })
        .collect();
    interleave(&chans)
}

/// SNR in dB of decoded channel `gch` of `got` (delayed by the encoder's 256
/// samples) against input channel `ch`, over input samples `from..to`.
#[allow(clippy::too_many_arguments)]
fn snr(want: &[f32], ch: usize, nw: usize, got: &[f32], gch: usize, ng: usize, from: usize, to: usize) -> f64 {
    let (mut s, mut e) = (0.0f64, 0.0f64);
    for i in from..to {
        let a = f64::from(want[i * nw + ch]);
        let b = f64::from(got[(i + 256) * ng + gch]);
        s += a * a;
        e += (a - b) * (a - b);
    }
    10.0 * (s / e.max(1e-30)).log10()
}

/// Encode a second of `signal`, check the stream, and return the SNR of
/// each input channel (in input order) with the decode.
fn roundtrip(cfg: Config) -> (Vec<f64>, Decoded) {
    let pcm = signal(&cfg, 1.0);
    let nch = cfg.channels();
    let n = pcm.len() / nch;
    let aus = encode(cfg, &pcm);
    let d = check_stream(&cfg, &aus);
    assert!(d.pcm.len() / d.channels >= n + 256, "flush must cover the input and the delay");
    let (from, to) = (1536, n - 1536);
    let spk = cfg.layout.speakers(cfg.lfe);
    let main = if cfg.layout == Layout::ThreeFour { Layout::ThreeTwo.speakers(cfg.lfe) } else { spk.clone() };
    let s = spk
        .iter()
        .enumerate()
        .map(|(c, sp)| match main.iter().position(|m| m == sp) {
            Some(g) => snr(&pcm, c, nch, &d.pcm, g, main.len(), from, to),
            None => snr(&pcm, c, nch, &d.dep, usize::from(*sp == Speaker::BR), 2, from, to),
        })
        .collect();
    (s, d)
}

const LAYOUTS: [Layout; 8] = [
    Layout::DualMono,
    Layout::Mono,
    Layout::Stereo,
    Layout::ThreeZero,
    Layout::TwoOne,
    Layout::ThreeOne,
    Layout::TwoTwo,
    Layout::ThreeTwo,
];

/// Every AC-3 audio coding mode, with and without LFE, at every sample rate
/// and every Table 5.18 bit rate: the encoder accepts it and every frame
/// passes `check_stream`.
#[test]
fn ac3_every_layout_rate_and_sample_rate() {
    let mut n = 0;
    for layout in LAYOUTS {
        for lfe in [false, true] {
            for fs in [48_000u32, 44_100, 32_000] {
                for &kbps in &BITRATE_KBPS {
                    let cfg = Config::new(Format::Ac3, fs, layout, lfe, u32::from(kbps));
                    let aus = encode(cfg, &signal(&cfg, 0.1));
                    let d = check_stream(&cfg, &aus);
                    // 44.1 kHz: both sizes of the rate occur (Table 5.18's
                    // alternation), except where the rate divides evenly.
                    if fs == 44_100 {
                        let words = f64::from(kbps) * 1000.0 * 1536.0 / 44_100.0 / 16.0;
                        if d.frames as f64 * words.fract() >= 1.5 {
                            assert_eq!(d.sizes.len(), 2, "{kbps} kbit/s at 44.1 kHz: {:?}", d.sizes);
                        }
                    }
                    n += 1;
                }
            }
        }
    }
    println!("{n} AC-3 configurations checked");
}

/// E-AC-3 from 32 to 6144 kbit/s, every layout (7.1 included): accepted and
/// valid, or refused up front by `Encoder::new` where the format cannot
/// carry it (6144 kbit/s needs 48 kHz; 7.1 needs at least 64 kbit/s).
#[test]
fn eac3_every_layout_and_rate() {
    let rates = [32u32, 48, 64, 96, 128, 192, 256, 384, 448, 640, 768, 1024, 1536, 2048, 3072, 4096, 6144];
    let mut ok = 0;
    let mut refused = Vec::new();
    for layout in LAYOUTS.iter().copied().chain([Layout::ThreeFour]) {
        for lfe in [false, true] {
            for fs in [48_000u32, 44_100, 32_000] {
                for &kbps in &rates {
                    let cfg = Config::new(Format::Eac3, fs, layout, lfe, kbps);
                    match Encoder::new(cfg) {
                        Ok(_) => {
                            let aus = encode(cfg, &signal(&cfg, 0.1));
                            check_stream(&cfg, &aus);
                            ok += 1;
                        }
                        Err(Error::InvalidInput(_)) => refused.push((layout, lfe, fs, kbps)),
                        Err(e) => panic!("{cfg:?}: {e}"),
                    }
                }
            }
        }
    }
    for &(layout, _lfe, fs, kbps) in &refused {
        let expected = (kbps == 6144 && fs != 48_000) || (layout == Layout::ThreeFour && (kbps <= 48 || (kbps == 6144 && fs == 32_000)));
        assert!(expected, "{layout:?} {fs} {kbps}: refused");
    }
    println!("{ok} E-AC-3 configurations checked, {} refused as expected", refused.len());
}

/// Per-channel SNR through the decoder, with the coding tools each case
/// should exercise. The floors are about 3 dB under what was measured.
#[test]
fn round_trip_snr() {
    struct Case {
        format: Format,
        fs: u32,
        layout: Layout,
        lfe: bool,
        kbps: u32,
        min_db: f64,
    }
    let c = |format, fs, layout, lfe, kbps, min_db| Case { format, fs, layout, lfe, kbps, min_db };
    use Format::*;
    use Layout::*;
    let cases = [
        c(Ac3, 48_000, Mono, false, 64, 18.0),
        c(Ac3, 48_000, Mono, false, 96, 24.0),
        c(Ac3, 48_000, Stereo, false, 128, 16.0),
        c(Ac3, 48_000, Stereo, false, 192, 24.0),
        c(Ac3, 48_000, Stereo, false, 256, 30.0),
        c(Ac3, 48_000, Stereo, false, 384, 39.0),
        c(Ac3, 44_100, Stereo, false, 192, 24.0),
        c(Ac3, 32_000, Stereo, false, 192, 35.0),
        c(Ac3, 48_000, DualMono, false, 192, 24.0),
        c(Ac3, 48_000, ThreeTwo, true, 384, 24.0),
        c(Ac3, 48_000, ThreeTwo, true, 448, 24.0),
        c(Ac3, 48_000, ThreeTwo, true, 640, 30.0),
        c(Eac3, 48_000, Stereo, false, 96, 14.0),
        c(Eac3, 48_000, Stereo, false, 192, 24.0),
        c(Eac3, 48_000, ThreeTwo, true, 384, 24.0),
        c(Eac3, 48_000, ThreeTwo, true, 1536, 43.0),
        c(Eac3, 48_000, ThreeFour, true, 768, 25.0),
    ];
    for k in cases {
        let cfg = Config::new(k.format, k.fs, k.layout, k.lfe, k.kbps);
        let (s, d) = roundtrip(cfg);
        let txt: Vec<String> = s.iter().map(|v| format!("{v:.1}")).collect();
        println!(
            "{:?} {} Hz {:?}{} {} kbit/s: SNR dB [{}]; {}",
            k.format,
            k.fs,
            k.layout,
            if k.lfe { "+LFE" } else { "" },
            k.kbps,
            txt.join(" "),
            d.features
        );
        for (ch, v) in s.iter().enumerate() {
            assert!(*v >= k.min_db, "{cfg:?}: channel {ch} SNR {v:.1} dB < {}", k.min_db);
        }
        let f = d.features;
        let coupled = f.cpl_blocks > 0;
        match (k.layout, k.kbps) {
            (Stereo, 96 | 128) => assert!(coupled && f.phsflg_blocks > 0 && f.remat_blocks > 0, "{f}"),
            (Stereo, _) => assert!(!coupled && f.remat_blocks > 0, "{f}"),
            (ThreeTwo, 384 | 448) => assert!(coupled, "{f}"),
            (ThreeTwo, _) | (ThreeFour, _) | (Mono, _) | (DualMono, _) => assert!(!coupled, "{f}"),
            _ => {}
        }
        // the 1.8 kHz plucks after silence-free music are not transients at
        // 8 kHz, but the stream's first note is not one either: the detector
        // stays quiet on this material.
        if k.format == Eac3 && d.frames > 0 && k.kbps <= 768 {
            assert!(f.frmexpstr_frames > 0, "Table E2.10 frame strategies never chosen: {f}");
        }
    }
}

/// Output level against input level for a sine at `f`, per channel, in dB.
fn sine_gain(cfg: Config, f: f32) -> Vec<f64> {
    let fs = cfg.sample_rate;
    let n = fs as usize / 2;
    let nch = cfg.channels();
    let chans: Vec<Vec<f32>> = (0..nch)
        .map(|c| (0..n).map(|i| 0.3 * (2.0 * std::f32::consts::PI * f * i as f32 / fs as f32 + c as f32).sin()).collect())
        .collect();
    let pcm = interleave(&chans);
    let d = check_stream(&cfg, &encode(cfg, &pcm));
    let (from, to) = (3072, n - 3072);
    (0..nch)
        .map(|c| {
            let (mut a, mut b) = (0.0f64, 0.0f64);
            for i in from..to {
                a += f64::from(pcm[i * nch + c]).powi(2);
                b += f64::from(d.pcm[(i + 256) * nch + c]).powi(2);
            }
            10.0 * (b / a).log10()
        })
        .collect()
}

/// Flat within ±0.5 dB from 50 Hz to the coded bandwidth (coupled
/// frequencies included: 5.1 at 448 kbit/s couples above about 11 kHz,
/// E-AC-3 stereo at 128 kbit/s above about 9 kHz), and gone above it.
#[test]
fn frequency_response() {
    let cases = [
        (Format::Ac3, Layout::Stereo, 384, 19_500.0f32),
        (Format::Ac3, Layout::ThreeTwo, 448, 18_000.0),
        (Format::Eac3, Layout::Stereo, 128, 14_000.0),
    ];
    for (format, layout, kbps, top) in cases {
        let cfg = Config::new(format, 48_000, layout, false, kbps);
        let mut line = String::new();
        for f in [50.0f32, 100.0, 250.0, 1000.0, 4000.0, 8000.0, 10_000.0, 12_000.0, 14_000.0, 16_000.0, 18_000.0, 19_500.0] {
            let g = sine_gain(cfg, f);
            let worst = g.iter().fold(0.0f64, |m, v| if v.abs() > m.abs() { *v } else { m });
            line += &format!(" {:.0}k:{worst:+.2}", f / 1000.0);
            if f <= top {
                assert!(worst.abs() <= 0.5, "{format:?} {layout:?} {kbps}: {f} Hz at {worst:+.2} dB");
            }
        }
        let above = sine_gain(cfg, 22_000.0);
        assert!(above.iter().all(|g| *g < -40.0), "above the bandwidth: {above:?}");
        println!("{format:?} {layout:?} {kbps} kbit/s, dB:{line}");
    }
}

/// Energy (dB relative to the burst's first 1024 samples) in the 768
/// samples before the start of the block holding a sharp onset out of
/// silence: a long transform smears quantisation noise there, the short
/// pair keeps it inside the block.
fn pre_echo(cfg: Config) -> (f64, u64) {
    let n = cfg.sample_rate as usize / 2;
    let onset = 12_000 + 77; // not block-aligned
    let mut r = Rng(12345);
    let burst: Vec<f32> = (0..n)
        .map(|i| if i >= onset { 0.7 * r.next() * (-((i - onset) as f32) / 2000.0).exp() } else { 0.0 })
        .collect();
    let nch = cfg.channels();
    let pcm = interleave(&vec![burst; nch]);
    let d = check_stream(&cfg, &encode(cfg, &pcm));
    let e = |a: usize, b: usize| -> f64 { (a..b).map(|i| f64::from(d.pcm[(i + 256) * nch]).powi(2)).sum::<f64>() / (b - a) as f64 };
    let blk_start = onset / 256 * 256;
    (10.0 * (e(blk_start - 768, blk_start) / e(onset, onset + 1024)).log10(), d.features.blksw_chblocks)
}

#[test]
fn block_switching_confines_pre_echo() {
    for (format, layout, kbps) in [(Format::Ac3, Layout::Stereo, 192), (Format::Ac3, Layout::ThreeTwo, 448), (Format::Eac3, Layout::Stereo, 128)] {
        let mut cfg = Config::new(format, 48_000, layout, false, kbps);
        let (on, switched) = pre_echo(cfg);
        cfg.block_switching = false;
        let (off, none) = pre_echo(cfg);
        println!("{format:?} {layout:?} {kbps} kbit/s: pre-echo {on:.1} dB with block switching ({switched} short channel-blocks), {off:.1} dB without");
        assert!(switched > 0 && none == 0);
        assert!(on < -80.0, "pre-echo with block switching: {on:.1} dB");
        assert!(off > on + 30.0, "block switching should matter: {on:.1} vs {off:.1}");
    }
}

#[test]
fn silence_full_scale_and_clipping_stay_valid() {
    for format in [Format::Ac3, Format::Eac3] {
        let cfg = Config::new(format, 48_000, Layout::ThreeTwo, true, 384);
        let n = 48_000 / 4;
        // digital silence: out comes at most the decoder's §7.3.4 dither at
        // exponent 24 (about −114 dBFS)
        let d = check_stream(&cfg, &encode(cfg, &vec![0.0; n * 6]));
        assert!(d.pcm.iter().all(|v| v.abs() < 1e-5));
        // a full-scale square wave, and input beyond full scale
        for amp in [1.0f32, 3.0] {
            let pcm: Vec<f32> = (0..n * 6).map(|i| if (i / 6 / 40) % 2 == 0 { amp } else { -amp }).collect();
            check_stream(&cfg, &encode(cfg, &pcm));
        }
    }
}

#[test]
fn chunking_does_not_change_the_stream() {
    let cfg = Config::new(Format::Ac3, 44_100, Layout::Stereo, false, 192);
    let pcm = signal(&cfg, 0.3);
    let whole = {
        let mut e = Encoder::new(cfg).unwrap();
        let mut a = e.encode(&pcm).unwrap();
        a.extend(e.flush().unwrap());
        a
    };
    let mut e = Encoder::new(cfg).unwrap();
    let mut pieces = Vec::new();
    for chunk in pcm.chunks(2 * 17) {
        pieces.extend(e.encode(chunk).unwrap());
    }
    pieces.extend(e.flush().unwrap());
    assert_eq!(whole, pieces);
    // flush covers every input sample plus the 256-sample delay
    let n = pcm.len() / 2;
    assert_eq!(whole.len(), (n + 256).div_ceil(1536));
}

#[test]
fn api_reports_layout_and_refuses_what_it_cannot_do() {
    let e = Encoder::new(Config::new(Format::Ac3, 48_000, Layout::ThreeTwo, true, 448)).unwrap();
    assert_eq!(e.speakers(), vec![Speaker::FL, Speaker::FR, Speaker::FC, Speaker::LFE, Speaker::SL, Speaker::SR]);
    assert_eq!((e.frame_samples(), e.delay()), (1536, 256));
    let e = Encoder::new(Config::new(Format::Eac3, 48_000, Layout::ThreeFour, true, 1024)).unwrap();
    assert_eq!(
        e.speakers(),
        vec![Speaker::FL, Speaker::FR, Speaker::FC, Speaker::LFE, Speaker::BL, Speaker::BR, Speaker::SL, Speaker::SR]
    );
    // E-AC-3 at high rates uses fewer blocks per frame to stay within 2048 words
    assert_eq!(Encoder::new(Config::new(Format::Eac3, 48_000, Layout::Stereo, false, 6144)).unwrap().frame_samples(), 256);
    let bad = |c: Config| matches!(Encoder::new(c), Err(Error::InvalidInput(_)));
    assert!(bad(Config::new(Format::Ac3, 48_000, Layout::Stereo, false, 200)), "not a Table 5.18 rate");
    assert!(bad(Config::new(Format::Ac3, 96_000, Layout::Stereo, false, 192)));
    assert!(bad(Config::new(Format::Ac3, 48_000, Layout::ThreeFour, true, 640)), "7.1 is E-AC-3 only");
    assert!(bad(Config::new(Format::Eac3, 48_000, Layout::Stereo, false, 7000)));
    let mut c = Config::new(Format::Ac3, 48_000, Layout::Stereo, false, 192);
    c.dialnorm = 0;
    assert!(bad(c));
    let mut e = Encoder::new(Config::new(Format::Ac3, 48_000, Layout::Stereo, false, 192)).unwrap();
    assert!(matches!(e.encode(&[0.0; 3]), Err(Error::InvalidInput(_))), "not whole interleaved frames");
}

#[test]
fn coupling_and_rematrixing_can_be_forced_off_and_on() {
    let mut cfg = Config::new(Format::Ac3, 48_000, Layout::Stereo, false, 384);
    cfg.coupling = Coupling::On;
    let (s, d) = roundtrip(cfg);
    assert!(d.features.cpl_blocks > 0);
    assert!(s.iter().all(|v| *v > 20.0), "{s:?}");
    cfg.coupling = Coupling::Off;
    cfg.rematrixing = false;
    let (_, d) = roundtrip(cfg);
    assert_eq!((d.features.cpl_blocks, d.features.remat_blocks), (0, 0));
    cfg.bsmod = 2;
    cfg.dialnorm = 24;
    roundtrip(cfg);
}
