# rivet-ac3

[![CI](https://github.com/rivet-transcoder/rivet-ac3/actions/workflows/ci.yml/badge.svg)](https://github.com/rivet-transcoder/rivet-ac3/actions/workflows/ci.yml)

An **AC-3 / E-AC-3** (Dolby Digital / Dolby Digital Plus) decoder in Rust:
no C, no system libraries, no build script, nothing to install on a build
host. Written from ATSC A/52:2018, not translated from any other
implementation. It agrees with libavcodec to float rounding wherever the
bit stream is deterministic, and to the expected dither difference where it
is not (the figures are [below](#how-it-is-checked)).

Written for the **[rivet](https://github.com/rivet-transcoder/rivet)**
transcoder, where it is the AC-3 decoder: what lets a 5.1 AC-3 or E-AC-3
track from MP4, Matroska or MPEG-TS be downmixed, filtered or transcoded to
Opus, AAC, MP3, FLAC or ALAC. It was written in the rivet repository first
and moved here with its history. Usable on its own by anything that has
AC-3 / E-AC-3 syncframes and wants PCM back.

Published as `rivet-ac3`; **imported as `ac3`** (`use ac3::…`). One
dependency (`thiserror`); an optional `tracing` feature; no build script.

```toml
[dependencies]
ac3 = { package = "rivet-ac3", git = "https://github.com/rivet-transcoder/rivet-ac3", branch = "develop" }
```

## What it decodes

| | supported | refused or skipped |
|---|---|---|
| **AC-3** (bsid ≤ 8) | every `acmod`, 1–6 channels including LFE; block switching, dither, coupling with phase flags, rematrixing, delta bit allocation, dynamic range compression (`dynrng`, applied by default, scalable) | bsid 9 / 10 (Annex D reduced-rate AC-3): `Error::Unsupported` |
| **E-AC-3** (bsid 16) | independent substream 0: every `numblkscod` (1, 2, 3 and 6 blocks), reduced sample rates, frame exponent strategies, the three SNR-offset strategies, standard coupling, spectral extension with attenuation, the adaptive hybrid transform (vector and gain-adaptive quantisation) | enhanced coupling (`ecplinu = 1`): `Error::Unsupported`. Dependent substreams and independent substreams other than 0 are skipped (Annex E §3.8.1), so 7.1 decodes as its 5.1 core |
| **Not applied** | — | `dialnorm` and heavy compression (`compr`) are parsed and not applied, as libavcodec does by default; transient pre-noise processing is parsed and ignored (an optional post-process) |

No downmix is performed. Output is interleaved `f32` at ±1.0 full scale,
256 samples per channel per audio block (1536 per AC-3 syncframe), in
ffmpeg's native order for the layout — the fronts, the LFE, then the
surrounds — and each frame names its speakers ([`Speaker`](src/lib.rs)):

| `acmod` | output (LFE, when present, after the fronts) |
|---|---|
| 0 (1+1, dual mono) | FL FR |
| 1 (1/0) | FC |
| 2 (2/0) | FL FR |
| 3 (3/0) | FL FR FC |
| 4 (2/1) | FL FR BC |
| 5 (3/1) | FL FR FC BC |
| 6 (2/2) | FL FR SL SR |
| 7 (3/2) | FL FR FC SL SR (5.1: FL FR FC LFE SL SR) |

## Using it

```rust
// Bytes in any chunking (an .ac3 / .eac3 file, TS or MP4 / Matroska packets):
// resynchronises on 0x0B77, buffers a partial syncframe, skips a damaged one.
let mut dec = ac3::Decoder::new();
for chunk in chunks {
    for frame in dec.decode(chunk)? {
        // frame.samples: interleaved f32; frame.sample_rate; frame.channels;
        // frame.speakers(); frame.header (acmod, lfeon, bit rate, ...)
    }
}
let tail = dec.flush()?;

// Dynamic range compression: 1.0 (the default) applies dynrng as encoded,
// 0.0 ignores it.
let dec = ac3::Decoder::with_options(ac3::Options { drc_scale: 0.0 });

// One whole syncframe at a time, with the decoder's statistics.
let hdr = ac3::parse_header(bytes)?;          // size and layout from syncinfo/bsi
let mut fd = ac3::FrameDecoder::new(1.0);
let mut pcm = Vec::new();
let header = fd.decode(&bytes[..hdr.frame_len], &mut pcm)?; // None: a skipped substream
```

`examples/ac3_decode.rs` is the counterpart of `ffmpeg -i x.ac3 -f f32le`
(`AC3_DECODE_FRAMES=1` for the tools each syncframe used; `RUST_LOG=trace`
with `--features tracing` for the syntax trace), and
`examples/ac3_strip_dither.rs` clears every `dithflag` and re-solves the
CRCs, to make a stream deterministic for cross-checking.

## How it is checked

- **Tables.** Every normative table — Tables 5.18, 7.6–7.16, 7.18–7.23,
  7.33, E2.10–E2.12, E3.1 / E3.2 / E3.6 / E3.13 / E3.14 and the VQ codebooks
  E4.1–E4.7 — names its PDF page, and a test per table pins its length, its
  checksum and spot values re-read from the rendered page. The KBD window is
  also derived analytically and matched to Table 7.33 to five decimals.
- **Against libavcodec, as a black box** (`tests/ac3_decode_vectors.rs`).
  A/52 defines the bit allocation in exact integers but leaves the transform
  and dequantisation to floating point, and lets dither and the
  spectral-extension noise be "any reasonably random sequence", so two
  conformant decoders agree to float rounding where the stream is
  deterministic and differ by their independent noise where it is not. The
  gate is relative to a *measured* noise floor (a second decode with the
  noise fill off; libavcodec's noise is independent, so the expected
  difference is √2 × ours): per channel, RMS ≤ max(1 LSB16, 1.5 × √2 ×
  floor) and peak ≤ max(8 LSB16, 2.5 × floor peak). The committed 5.1 fixture
  (250 ms at 448 kbit/s, ffmpeg's encoder) runs on every `cargo test`; the
  full sweep runs when `RIVET_AC3_VECTORS` points at the vectors made by
  `tests/data/ac3_make_vectors.sh`, and skips otherwise.

Measured 2026-09-13, in 16-bit LSBs (1 LSB16 = 1/32768):

| streams | what they exercise | result |
|---|---|---|
| 30 made by ffmpeg's encoders, each with and without `dynrng` | mono to 5.1; 32, 44.1 and 48 kHz; AC-3 64–448 kbit/s, E-AC-3 48–256 kbit/s; tones, pink / white / brown noise, clicks; copies with `blksw` forced (`ac3_make_blksw_vector.py`) | dither-stripped copies within **0.03 RMS / 0.32 peak** (float rounding); dithered originals at the noise floor (RMS / expected 0.9–1.1) |
| Dolby-encoded, from ffmpeg's FATE suite | `monsters_inc_5.1_448` (AC-3, coupling, `dynrng` every block), `matrix2_commentary1_stereo_192` (E-AC-3, coupling), `serenity_english_5.1_1536` (E-AC-3, one block per frame), `millers_crossing_4.0`, `monsters_inc_2.0_192` | `monsters_inc_5.1_448` RMS / expected 1.04–1.09 (dither-stripped 0.03 RMS); `matrix2_commentary1` 0.99–1.01; `serenity_english` ≤ 0.11 RMS; `millers_crossing_4.0` and `monsters_inc_2.0_192` identical (dither-stripped 0.01–0.07 RMS) but for one block each, masked (below) |
| Dolby-encoded `csi_miami_5.1_256_spx`, `csi_miami_stereo_128_spx` | E-AC-3 spectral extension and AHT (VQ, GAQ, large mantissas), `dynrng` | full-bandwidth channels at 1.2–1.8 × the dither-only expectation: the SPX noise blend's random sequence, which Annex E §3.6.4.2 does not fix; the gate widens for SPX streams and says so |

Where the two disagree beyond that, the disagreement is localised and the
spec is followed: in two Dolby streams libavcodec overlap-adds a
block-switched channel's previous tail onto a neighbouring channel in one
block, contrary to §7.9.4 step 6. `FrameDecoder::mixed_transform_blocks()`
lists such blocks and the harness masks and counts them in every report
line. Mutation check: changing one `hth` entry fails its table test and the
fixture cross-check in frame 0 — a wrong bit-allocation table
desynchronises the mantissa parse rather than degrading the audio.

## Provenance and licensing

Written from the text of ATSC A/52:2018 (with Annex E for E-AC-3); **no
AC-3 implementation's source was read or copied** — libavcodec's tables
included — and ffmpeg was used only as a command-line tool, to make test
streams and decode them for comparison. The tables were transcribed from the
spec PDF, each with its table number and page. See [NOTICE](NOTICE).

**Patents.** AC-3 and E-AC-3 may be subject to patent licensing in some
jurisdictions. Nothing here is a licence to any patent, and the authors make
no claim about whether anyone needs one. Dolby, Dolby Digital and Dolby
Digital Plus are trademarks of Dolby Laboratories; this project is not
affiliated with Dolby.

## License

Open Encoding Attribution License v1.0 — a source-available (not OSI open-source)
license, royalty-free, with a commercial-attribution requirement. See
[LICENSE.md](LICENSE.md) and [NOTICE](NOTICE).
