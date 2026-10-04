//! Cross-check of this crate's AC-3 / E-AC-3 decoder against independent
//! implementations, run as black boxes. No FFmpeg is involved anywhere.
//!
//! - The committed fixture (`tests/data/aften_51_448k.*`, made by
//!   `tools/make_fixture.sh`) is a stream from aften, an independent AC-3
//!   encoder, with liba52's decode of it (GStreamer's `a52dec` element) as
//!   16-bit PCM. It runs on every `cargo test`.
//! - The sweep runs over the directory `RIVET_AC3_VECTORS` names: the aften
//!   vectors `tools/make_vectors.sh` makes (each an elementary stream plus
//!   liba52's f32le decode of it, `<name>.drc1.f32` with dynrng applied and
//!   `<name>.drc0.f32` without), and in its `dolby/` subdirectory the
//!   Dolby-encoded streams `tools/fetch_dolby_kit.sh` fetches from Dolby's
//!   own test kit. Without the variable the sweep is skipped, unless
//!   `RIVET_AC3_REQUIRE_VECTORS` is set (as in CI's oracle job), when a
//!   missing directory fails.
//! - E-AC-3 has no decoder available to compare with but FFmpeg's, which is
//!   not used. Dolby's E-AC-3 encode of its channel-identification programme
//!   is instead checked against liba52's decode of Dolby's AC-3 encode of the
//!   same programme: same timing, levels, and band energies, and a waveform
//!   SNR only a correct decode of both reaches (see `dolby_eac3_*`).
//!
//! Pass criterion
//! --------------
//! A/52:2018 defines the bit allocation in exact integer arithmetic (§7.2.1)
//! but leaves the transform and dequantisation to floating point, and lets
//! the dither and spectral-extension noise be "any reasonably random
//! sequence" (§7.3.4). Two conformant decoders therefore agree to float
//! rounding wherever the bit stream is deterministic, and differ by the
//! dither wherever it is not. So the gate (see `check`) is relative to a
//! measured *noise floor*: how much of our own output is noise fill, found
//! by decoding once more with the fill switched off. Numbers are in 16-bit LSBs
//! (1 LSB16 = 1/32768 ≈ 3.05e-5). A mis-transcribed table does not nudge
//! these numbers — it desynchronises the mantissa parse and the error jumps
//! to signal level (see `ac3_table_mutation` in this crate's tests).

use std::path::{Path, PathBuf};

use ac3::{Decoder, Features, FrameDecoder, Header, Options, frame_crc_ok, parse_header};

const LSB16: f32 = 1.0 / 32768.0;

struct Stats {
    channels: usize,
    rms: Vec<f32>,
    peak: Vec<f32>,
    compared: usize,
    /// Blocks left out of the comparison (see `FrameDecoder::mixed_transform_blocks`).
    masked_blocks: usize,
}

/// Per-channel RMS and peak difference over the common length, skipping
/// the 256-sample blocks listed in `masked` (global block indices).
fn compare(ours: &[f32], reference: &[f32], channels: usize, masked: &[u64]) -> Stats {
    let n = ours.len().min(reference.len()) / channels;
    let mut rms = vec![0.0f64; channels];
    let mut peak = vec![0.0f32; channels];
    let mut counted = 0usize;
    let mut masked_blocks = 0usize;
    for i in 0..n {
        if i % 256 == 0 && masked.contains(&((i / 256) as u64)) {
            masked_blocks += 1;
        }
        if masked.contains(&((i / 256) as u64)) {
            continue;
        }
        counted += 1;
        for c in 0..channels {
            let d = ours[i * channels + c] - reference[i * channels + c];
            rms[c] += f64::from(d) * f64::from(d);
            peak[c] = peak[c].max(d.abs());
        }
    }
    Stats {
        channels,
        rms: rms
            .iter()
            .map(|s| (s / counted.max(1) as f64).sqrt() as f32)
            .collect(),
        peak,
        compared: counted,
        masked_blocks,
    }
}

fn read_f32le(path: &Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&c| f32::from_le_bytes(c))
        .collect()
}

fn read_s16le(path: &Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| f32::from(i16::from_le_bytes(c)) / 32768.0)
        .collect()
}

struct Decoded {
    pcm: Vec<f32>,
    channels: usize,
    frames: usize,
    /// Zero-bit mantissas that were noise-filled, out of all mantissas.
    dithered: (u64, u64),
    /// Blocks carrying a non-unity `dynrng`, out of all blocks.
    drc: (u64, u64),
    /// Which coding tools the stream exercised.
    features: Features,
    /// The stream ended inside a syncframe (dropped).
    truncated_tail: bool,
    /// Blocks where the fbw channels used different transform lengths. One
    /// earlier reference decoder overlap-added the switched channel's previous
    /// tail onto a neighbouring channel there, contrary to §7.9.4 step 6, so
    /// these blocks can be masked out of the comparison and counted in the
    /// report; against liba52 they are compared like any other block
    /// (`MASK_MIXED`).
    mixed_blocks: Vec<u64>,
    /// The last syncframe's header (layout, LFE).
    header: Option<Header>,
}

/// Decode a whole elementary stream with the `FrameDecoder`, frame by
/// frame, checking every frame's CRC on the way. Leading junk before the
/// first syncword is skipped (some captured streams start mid-frame) and a
/// truncated final frame is dropped, so the sample counts still line up.
fn decode_es(es: &[u8], drc_scale: f32, noise_fill: bool) -> Decoded {
    let mut dec = FrameDecoder::new(drc_scale);
    dec.set_noise_fill(noise_fill);
    let mut pcm = Vec::new();
    let mut pos = 0;
    let mut frames = 0;
    let mut channels = 0;
    let mut truncated_tail = false;
    while pos + 8 <= es.len() {
        let hdr = match parse_header(&es[pos..]) {
            Ok(h) => h,
            Err(e) => {
                assert_eq!(frames, 0, "frame {frames} at {pos}: {e}");
                pos += 1;
                continue;
            }
        };
        if pos + hdr.frame_len > es.len() {
            truncated_tail = true;
            break;
        }
        let frame = &es[pos..pos + hdr.frame_len];
        assert!(
            frame_crc_ok(frame),
            "frame {frames}: CRC-16 remainder is not zero"
        );
        if let Some(h) = dec
            .decode(frame, &mut pcm)
            .unwrap_or_else(|e| panic!("frame {frames}: {e}"))
        {
            channels = h.channels();
        }
        frames += 1;
        pos += hdr.frame_len;
    }
    Decoded {
        pcm,
        channels,
        frames,
        dithered: dec.dither_stats(),
        drc: dec.drc_stats(),
        features: dec.features(),
        truncated_tail,
        mixed_blocks: dec.mixed_transform_blocks().to_vec(),
        header: dec.last_header(),
    }
}

/// How much of our own output is §7.3.4 noise fill: the difference between
/// a normal decode and one with the noise fill switched off. Where nothing
/// is dithered it is 0. The reference decoder's noise has the same amplitude
/// and is independent of ours, so the expected disagreement is √2 × this RMS.
fn noise_floor(es: &[u8], drc_scale: f32, ours: &Decoded) -> Stats {
    let silent = decode_es(es, drc_scale, false);
    compare(&ours.pcm, &silent.pcm, ours.channels, mask(ours))
}

fn report(name: &str, s: &Stats) -> String {
    let mut line = format!("{name}: {} samples/ch, {} ch;", s.compared, s.channels);
    if s.masked_blocks > 0 {
        line.push_str(&format!(
            " {} mixed-transform blocks masked;",
            s.masked_blocks
        ));
    }
    for c in 0..s.channels {
        line.push_str(&format!(
            " ch{c} rms={:.3} peak={:.2} LSB16;",
            s.rms[c] / LSB16,
            s.peak[c] / LSB16
        ));
    }
    line
}

/// The gate. Per channel, against the reference decoder:
/// - RMS error ≤ max(1 LSB16, 1.5 × √2 × our noise-fill RMS). Two
///   independent ±0.707 noise sequences differ by √2 × one of them in
///   expectation; the 1.5 covers the realisation variance of short files
///   whose noise is dominated by a few high-exponent bins, and a decoder's
///   freedom to use ±0.5 or ±0.75 instead (§7.3.4). 1 LSB16 is float
///   rounding plus a 16-bit reference's own rounding.
/// - peak error ≤ max(8 LSB16, 2.5 × our noise-fill peak): the peaks of two
///   independent noises add at most, and rarely coincide.
///
/// Everything deterministic in the decode — exponents, bit allocation,
/// mantissas, coupling, rematrixing, the transform — contributes only
/// float rounding, so a real bug shows up as a jump far beyond either bound
/// (the `hth` mutation in the report raises the RMS ~1000×).
///
/// The references are AC-3 only (liba52 decodes no E-AC-3), so the
/// E-AC-3 tools whose noise is not what the floor measures (spectral
/// extension's noise blend, AHT) never reach this gate.
fn check(name: &str, s: &Stats, noise: &Stats, d: &Decoded) {
    let mut line = report(name, s);
    line.push_str(&format!(
        " noise rms={} peak={} LSB16; dithered {}/{} bins; dynrng blocks {}/{}; features: {}",
        noise
            .rms
            .iter()
            .map(|f| format!("{:.3}", f / LSB16))
            .collect::<Vec<_>>()
            .join("/"),
        noise
            .peak
            .iter()
            .map(|f| format!("{:.2}", f / LSB16))
            .collect::<Vec<_>>()
            .join("/"),
        d.dithered.0,
        d.dithered.1,
        d.drc.0,
        d.drc.1,
        d.features
    ));
    println!("{line}");
    assert!(s.compared > 0, "{name}: nothing compared");
    // `RIVET_AC3_REPORT_ONLY` turns the gate into a report, to see every
    // vector's numbers in one run while tuning.
    let report_only = std::env::var_os("RIVET_AC3_REPORT_ONLY").is_some();
    let (rms_factor, peak_factor) = (1.5, 2.5);
    let (rms_floor, peak_floor) = (1.0, 8.0);
    for c in 0..s.channels {
        let expected = std::f32::consts::SQRT_2 * noise.rms[c];
        let rms_limit = (rms_factor * expected).max(rms_floor * LSB16);
        let peak_limit = (peak_factor * noise.peak[c]).max(peak_floor * LSB16);
        println!(
            "  ch{c}: rms/expected = {:.2}, peak/noise-peak = {:.2}",
            s.rms[c] / expected.max(1e-12),
            s.peak[c] / noise.peak[c].max(1e-12)
        );
        let ok = s.rms[c] <= rms_limit && s.peak[c] <= peak_limit;
        if report_only && !ok {
            println!(
                "  ch{c} OVER: rms limit {:.3}, peak limit {:.3} LSB16",
                rms_limit / LSB16,
                peak_limit / LSB16
            );
            continue;
        }
        assert!(
            s.rms[c] <= rms_limit,
            "{line}\n  ch{c} RMS over {:.3} LSB16",
            rms_limit / LSB16
        );
        assert!(
            s.peak[c] <= peak_limit,
            "{line}\n  ch{c} peak over {:.3} LSB16",
            peak_limit / LSB16
        );
    }
}

/// The blocks to leave out of a comparison: the mixed-transform blocks when
/// `RIVET_AC3_MASK_MIXED` is set, none otherwise (liba52 follows §7.9.4
/// there, so by default they are compared like every other block).
fn mask(d: &Decoded) -> &[u64] {
    if std::env::var_os("RIVET_AC3_MASK_MIXED").is_some() {
        &d.mixed_blocks
    } else {
        &[]
    }
}

/// The committed fixture: 250 ms of 5.1 AC-3 at 448 kbit/s from aften (block
/// switching on, a `dynrng` profile), with liba52's decode as s16le
/// (`tools/make_fixture.sh`). Always runs.
#[test]
fn committed_5_1_fixture_matches_liba52() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let es = std::fs::read(dir.join("aften_51_448k.ac3")).expect("fixture aften_51_448k.ac3");
    let reference = read_s16le(&dir.join("aften_51_448k.liba52.s16le"));
    let ours = decode_es(&es, 1.0, true);
    assert_eq!(ours.channels, 6);
    assert!(
        ours.frames >= 7,
        "expected ≥ 7 syncframes, got {}",
        ours.frames
    );
    assert!(ours.drc.0 > 0, "the fixture should carry dynrng words");
    assert_eq!(
        ours.pcm.len(),
        reference.len(),
        "sample count differs from liba52"
    );
    let s = compare(&ours.pcm, &reference, 6, mask(&ours));
    let noise = noise_floor(&es, 1.0, &ours);
    // The reference is 16-bit, so its own rounding contributes up to 0.5 LSB16.
    check("aften_51_448k (s16 ref)", &s, &noise, &ours);
}

/// The stream `Decoder` must produce the same PCM as the frame decoder when
/// the stream arrives as arbitrary chunk boundaries.
#[test]
fn stream_decoder_reassembles_split_frames() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let es = std::fs::read(dir.join("aften_51_448k.ac3")).expect("fixture");
    let direct = decode_es(&es, 1.0, true).pcm;
    let mut dec = Decoder::with_options(Options { drc_scale: 1.0 });
    let mut out = Vec::new();
    let mut frames = 0;
    for chunk in es.chunks(1000) {
        for f in dec.decode(chunk).unwrap() {
            assert_eq!(f.channels, 6);
            assert_eq!(f.sample_rate, 48_000);
            assert_eq!(
                f.samples.len(),
                1536 * 6,
                "one syncframe is 1536 samples per channel"
            );
            assert_eq!(
                f.speakers(),
                [
                    ac3::Speaker::FL,
                    ac3::Speaker::FR,
                    ac3::Speaker::FC,
                    ac3::Speaker::LFE,
                    ac3::Speaker::SL,
                    ac3::Speaker::SR
                ]
            );
            frames += 1;
            out.extend_from_slice(&f.samples);
        }
    }
    out.extend(dec.flush().unwrap().into_iter().flat_map(|f| f.samples));
    assert_eq!(dec.buffered(), 0);
    assert!(frames >= 2);
    assert_eq!(out, direct);
}

/// `RIVET_AC3_VECTORS`, or `None` (with a message) when it is not set —
/// a failure instead when `RIVET_AC3_REQUIRE_VECTORS` is set.
fn vectors_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("RIVET_AC3_VECTORS")
        .map(PathBuf::from)
        .filter(|d| d.is_dir());
    if dir.is_none() {
        assert!(
            std::env::var_os("RIVET_AC3_REQUIRE_VECTORS").is_none(),
            "RIVET_AC3_REQUIRE_VECTORS is set but RIVET_AC3_VECTORS does not name a directory \
             (tools/make_vectors.sh and tools/fetch_dolby_kit.sh make it)"
        );
        eprintln!("RIVET_AC3_VECTORS not set — skipping the liba52 sweep");
    }
    dir
}

fn streams_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("ac3" | "eac3" | "ec3")
            )
        })
        .collect();
    names.sort();
    names
}

/// Every stream in `RIVET_AC3_VECTORS` (and its `dolby/` subdirectory) that
/// has a liba52 reference beside it, both with and without dynamic range
/// compression.
#[test]
fn vector_sweep_matches_liba52() {
    let Some(dir) = vectors_dir() else { return };
    let mut names = streams_in(&dir);
    names.extend(streams_in(&dir.join("dolby")));
    let mut lines = Vec::new();
    let mut checked = 0;
    for es_path in &names {
        let stem = es_path.file_stem().unwrap().to_string_lossy().to_string();
        let es = std::fs::read(es_path).unwrap();
        for (tag, drc) in [("drc1", 1.0f32), ("drc0", 0.0)] {
            let ref_path = es_path.with_file_name(format!("{stem}.{tag}.f32"));
            if !ref_path.is_file() {
                continue;
            }
            let reference = read_f32le(&ref_path);
            let ours = decode_es(&es, drc, true);
            assert!(
                !ours.truncated_tail,
                "{stem}: the stream ends inside a syncframe"
            );
            assert_eq!(
                ours.pcm.len(),
                reference.len(),
                "{stem}: sample count differs from liba52"
            );
            let s = compare(&ours.pcm, &reference, ours.channels, mask(&ours));
            let noise = noise_floor(&es, drc, &ours);
            let name = format!("{stem}.{tag}");
            check(&name, &s, &noise, &ours);
            checked += 1;
            lines.push(format!(
                "{} noise rms={} peak={}; dithered {}/{}; dynrng {}/{}; mixed-transform blocks {}; features: {}",
                report(&name, &s),
                noise.rms.iter().map(|f| format!("{:.3}", f / LSB16)).collect::<Vec<_>>().join("/"),
                noise.peak.iter().map(|f| format!("{:.2}", f / LSB16)).collect::<Vec<_>>().join("/"),
                ours.dithered.0,
                ours.dithered.1,
                ours.drc.0,
                ours.drc.1,
                ours.mixed_blocks.len(),
                ours.features
            ));
        }
    }
    assert!(
        checked > 0,
        "no stream with a liba52 reference in {}",
        dir.display()
    );
    // A summary the report can quote.
    std::fs::write(dir.join("ac3_sweep_report.txt"), lines.join("\n") + "\n").unwrap();
}

/// The Dolby kit's directory, when the sweep's directory has one; with
/// `RIVET_AC3_REQUIRE_VECTORS` set it must.
fn dolby_dir() -> Option<PathBuf> {
    let dir = vectors_dir()?.join("dolby");
    let ok = dir.join("ChID_voices_6ch_640kbps_dd.ac3").is_file();
    assert!(
        ok || std::env::var_os("RIVET_AC3_REQUIRE_VECTORS").is_none(),
        "RIVET_AC3_REQUIRE_VECTORS is set but {} has no Dolby kit (tools/fetch_dolby_kit.sh)",
        dir.display()
    );
    ok.then_some(dir)
}

/// Every Dolby-encoded stream in the kit decodes from end to end: every
/// syncframe's CRC good, no decode error, no partial frame, and the channel
/// count the file name gives.
#[test]
fn dolby_kit_streams_decode_clean() {
    let Some(dir) = dolby_dir() else { return };
    let names = streams_in(&dir);
    assert!(
        names.len() >= 10,
        "expected the kit's 12 AC-3 / E-AC-3 streams, found {}",
        names.len()
    );
    for path in &names {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let d = decode_es(&std::fs::read(path).unwrap(), 1.0, true);
        let want = if name.contains("_2ch_") { 2 } else { 6 };
        assert_eq!(d.channels, want, "{name}: channels");
        assert!(!d.truncated_tail, "{name}: ends inside a syncframe");
        assert_eq!(
            d.pcm.len(),
            d.frames * 1536 * want,
            "{name}: 1536 samples per channel per syncframe"
        );
        println!("{name}: {} frames; features: {}", d.frames, d.features);
    }
}

/// An in-place iterative radix-2 FFT (`re.len()` a power of two).
fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f64::consts::PI / len as f64;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (wr, wi) = ((ang * k as f64).cos(), (ang * k as f64).sin());
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * wr - im[b] * wi;
                let ti = re[b] * wi + im[b] * wr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        len <<= 1;
    }
}

/// Energy of channel `c` of interleaved `pcm` in each band `[edges[i],
/// edges[i + 1])` Hz, from Hann-windowed 4096-point spectra summed over
/// the signal.
fn band_energy(pcm: &[f32], channels: usize, c: usize, edges: &[f64]) -> Vec<f64> {
    const N: usize = 4096;
    let frames = pcm.len() / channels / N;
    let window: Vec<f64> = (0..N)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / N as f64).cos())
        .collect();
    let mut power = vec![0.0f64; N / 2 + 1];
    for f in 0..frames {
        let mut re: Vec<f64> = (0..N)
            .map(|i| f64::from(pcm[(f * N + i) * channels + c]) * window[i])
            .collect();
        let mut im = vec![0.0f64; N];
        fft(&mut re, &mut im);
        for (k, p) in power.iter_mut().enumerate() {
            *p += re[k] * re[k] + im[k] * im[k];
        }
    }
    edges
        .windows(2)
        .map(|b| {
            power
                .iter()
                .enumerate()
                .filter(|(k, _)| {
                    let hz = *k as f64 * 48_000.0 / N as f64;
                    hz >= b[0] && hz < b[1]
                })
                .map(|(_, p)| p)
                .sum()
        })
        .collect()
}

fn db(x: f64) -> f64 {
    10.0 * x.max(1e-30).log10()
}

/// Dolby's E-AC-3 encode of its channel-identification programme (5.1 at
/// 256 kbit/s: spectral extension, AHT with VQ / GAQ / large mantissas,
/// coupling, `dynrng`) against liba52's decode of Dolby's AC-3 encode of the
/// same programme (5.1 at 640 kbit/s). No E-AC-3 decoder but FFmpeg's exists
/// to compare with, and the two encodes are lossy in different ways, so this
/// is not a sample-level match; it is what only a correct decode of both
/// reaches. Both decoded without dynrng:
///
/// - aligned: the cross-correlation of the front left peaks at lag 0;
/// - per full-bandwidth channel, waveform SNR ≥ 15 dB (measured 16.8–22.0),
///   level within 0.5 dB, and the energy in each band up to 16 kHz within
///   3 dB (measured ≤ 2.2 dB; the band above is the 256 kbit/s stream's
///   bandwidth limit) — a channel swap, a wrong SPX band or a broken AHT
///   inverse is tens of dB;
/// - the LFE's energy below 250 Hz within 1.5 dB.
#[test]
fn dolby_eac3_matches_dolby_ac3_of_same_programme() {
    let Some(dir) = dolby_dir() else { return };
    let eac3 = decode_es(
        &std::fs::read(dir.join("ChID_voices_6ch_256kbps_ddp.ec3")).unwrap(),
        0.0,
        true,
    );
    let h = eac3.header.expect("header");
    assert!(h.acmod == 7 && h.lfeon, "5.1 expected");
    let f = &eac3.features;
    assert!(
        f.spx_blocks > 0 && f.aht_channels > 0 && f.gaq_channels > 0,
        "the stream should exercise SPX, AHT and GAQ: {f}"
    );
    let reference = read_f32le(&dir.join("ChID_voices_6ch_640kbps_dd.drc0.f32"));
    let n = (eac3.pcm.len().min(reference.len())) / 6;
    assert!(
        eac3.pcm.len().abs_diff(reference.len()) <= 1536 * 6,
        "lengths differ by more than a frame"
    );
    let (ours, theirs) = (&eac3.pcm[..n * 6], &reference[..n * 6]);

    // Alignment, on the first ten seconds of the front left.
    let span = (10 * 48_000).min(n - 2048);
    let xcorr = |lag: isize| -> f64 {
        (2048..span)
            .map(|i| f64::from(ours[i * 6]) * f64::from(theirs[(i as isize + lag) as usize * 6]))
            .sum()
    };
    let at0 = xcorr(0);
    for lag in [-1536, -512, -256, -1, 1, 256, 512, 1024] {
        assert!(
            at0 > xcorr(lag),
            "cross-correlation at lag {lag} beats lag 0"
        );
    }

    let edges = [
        0.0, 250.0, 500.0, 1000.0, 2000.0, 3000.0, 4000.0, 6000.0, 8000.0, 10000.0, 12000.0,
        14000.0, 16000.0,
    ];
    for c in 0..6 {
        let (mut sig, mut err, mut e_ours) = (0.0f64, 0.0f64, 0.0f64);
        for i in 0..n {
            let (a, b) = (f64::from(theirs[i * 6 + c]), f64::from(ours[i * 6 + c]));
            sig += a * a;
            e_ours += b * b;
            err += (a - b) * (a - b);
        }
        let ba = band_energy(theirs, 6, c, &edges);
        let bo = band_energy(ours, 6, c, &edges);
        let bands: Vec<f64> = ba.iter().zip(&bo).map(|(a, o)| db(o / a)).collect();
        let snr = db(sig / err);
        let level = db(e_ours / sig);
        println!(
            "ch{c}: SNR {snr:.1} dB, level {level:+.2} dB, bands {}",
            bands
                .iter()
                .map(|b| format!("{b:+.1}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        if c == 3 {
            assert!(
                bands[0].abs() <= 1.5,
                "LFE below 250 Hz off by {:.2} dB",
                bands[0]
            );
            continue;
        }
        assert!(
            snr >= 15.0,
            "ch{c}: SNR {snr:.1} dB against the AC-3 encode"
        );
        assert!(level.abs() <= 0.5, "ch{c}: level off by {level:.2} dB");
        for (b, d) in bands.iter().enumerate() {
            assert!(
                d.abs() <= 3.0,
                "ch{c}: band {}–{} Hz off by {d:.2} dB",
                edges[b],
                edges[b + 1]
            );
        }
    }
}
