# rivet-ac3

[![CI](https://github.com/rivet-transcoder/rivet-ac3/actions/workflows/ci.yml/badge.svg)](https://github.com/rivet-transcoder/rivet-ac3/actions/workflows/ci.yml)

An **AC-3 / E-AC-3** (Dolby Digital / Dolby Digital Plus) **decoder and
encoder** in Rust: no C, no system libraries, no build script, nothing to
install on a build host. Written from ATSC A/52:2018, not translated from
any other implementation. The decoder agrees with liba52, an independent
decoder, to float rounding wherever the bit stream is deterministic, and to
the expected dither difference where it is not — on aften's encodes and on
Dolby's own — and decodes Dolby's E-AC-3 test streams (the figures are
[below](#how-the-decoder-is-checked)). The encoder writes AC-3 at every
Table 5.18 bit rate and E-AC-3 from 32 to 6144 kbit/s, every channel layout
with or without LFE and 7.1 through a dependent substream; every frame it
writes in the tests is checked field by field against the spec
([below](#how-the-encoder-is-checked)).

Written for the **[rivet](https://github.com/rivet-transcoder/rivet)**
transcoder, where it is the AC-3 decoder: what lets a 5.1 AC-3 or E-AC-3
track from MP4, Matroska or MPEG-TS be downmixed, filtered or transcoded to
Opus, AAC, MP3, FLAC or ALAC. It was written in the rivet repository first
and moved here with its history. Usable on its own by anything that has
AC-3 / E-AC-3 syncframes and wants PCM back, or has PCM and wants AC-3 /
E-AC-3.

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
| **E-AC-3** (bsid 16) | independent substream 0: every `numblkscod` (1, 2, 3 and 6 blocks), reduced sample rates, frame exponent strategies, the three SNR-offset strategies, standard coupling, spectral extension with attenuation, the adaptive hybrid transform (vector and gain-adaptive quantisation) | enhanced coupling (`ecplinu = 1`): `Error::Unsupported`. Dependent substreams and independent substreams other than 0 are skipped (Annex E §3.8.1), so 7.1 decodes as its 5.1 core (`FrameDecoder::set_decode_all_substreams` decodes one as a programme of its own, a diagnostic the encoder's tests use) |
| **Not applied** | — | `dialnorm` and heavy compression (`compr`) are parsed and not applied; transient pre-noise processing is parsed and ignored (an optional post-process) |

No downmix is performed. Output is interleaved `f32` at ±1.0 full scale,
256 samples per channel per audio block (1536 per AC-3 syncframe), in
WAVE order for the layout — the fronts, the LFE, then the surrounds — and each frame names its speakers ([`Speaker`](src/lib.rs)):

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

`examples/ac3_decode.rs` decodes a stream to raw interleaved f32le (`AC3_DECODE_FRAMES=1` for the tools each syncframe used; `RUST_LOG=trace`
with `--features tracing` for the syntax trace), and
`examples/ac3_strip_dither.rs` clears every `dithflag` and re-solves the
CRCs, to make a stream deterministic for cross-checking.

## What it encodes

| | done | not done |
|---|---|---|
| **AC-3** (`bsid` 8) | 32, 44.1 and 48 kHz; every `acmod` (1+1, 1/0 to 3/2) with or without LFE; all 19 Table 5.18 bit rates, 32–640 kbit/s, with the 44.1 kHz frame-size alternation; block switching on transients (§8.2.2's detector); coupling with phase flags; rematrixing; exponent strategies chosen by cost; the parametric bit allocation with the SNR offset searched to fill the frame; dither flags (§8.2.9); `crc1` / `crc2`; `bsmod`, `dialnorm`; the §5.5 rules (blocks 0–1 within the first 5/8, block 5's mantissas within the last 3/8) | delta bit allocation; `dynrng` / `compr` (not sent: unity gain); time codes, `langcod`, `audprodinfo`; Annex D (`bsid` 9/10) |
| **E-AC-3** (`bsid` 16) | independent substream 0, 32–6144 kbit/s (6144 needs 48 kHz: one block per frame at 2048 words); six blocks per frame, or 3 / 2 / 1 when the rate needs it; per-block or Table E2.10 frame exponent strategies, whichever costs less; coupling, rematrixing, block switching as in AC-3; converter exponent strategies; **7.1** as a 3/2 + LFE independent substream and a 2/0 dependent substream with custom channel map Lrs/Rrs | the adaptive hybrid transform (AHT), spectral extension, enhanced coupling, transient pre-noise processing; reduced sample rates (`fscod2`); more than one independent substream |

Input is interleaved `f32` (±1.0 full scale) in the order the decoder
outputs — for 5.1 FL FR FC LFE SL SR, for 7.1 FL FR FC LFE BL BR SL SR
(`Layout::speakers`). The decoded output lags the input by 256 samples
(`Encoder::delay`); `flush` pads with silence until every input sample is in
a frame. The LFE channel is coded as A/52 has it (its first seven
transform coefficients, 0–656 Hz at 48 kHz) with no 120 Hz low-pass in
front; filter it beforehand if the source has more in it.

At rates too low for a frame's side information the encoder falls back to
the cheapest exponents and coupling coordinates, then to long blocks only;
`Encoder::new` refuses a rate that cannot hold a frame of full-band noise
(only 7.1 below 64 kbit/s, and 6144 kbit/s at 44.1 or 32 kHz).

```rust
let cfg = ac3::Config::new(ac3::Format::Ac3, 48_000, ac3::Layout::ThreeTwo, true, 448);
let mut enc = ac3::Encoder::new(cfg)?;           // cfg.coupling, .rematrixing, .block_switching, .bsmod, .dialnorm
for chunk in pcm.chunks(6 * 4096) {               // interleaved FL FR FC LFE SL SR
    for frame in enc.encode(chunk)? {             // whole syncframes
        out.write_all(&frame)?;
    }
}
for frame in enc.flush()? { out.write_all(&frame)?; }
```

`examples/ac3_encode.rs` encodes a WAV file (`… -- in.wav out.ac3 448`, or
`out.eac3` for E-AC-3), the layout taken from the channel count.

## How the decoder is checked

- **Tables.** Every normative table — Tables 5.18, 7.6–7.16, 7.18–7.23,
  7.33, E2.10–E2.12, E3.1 / E3.2 / E3.6 / E3.13 / E3.14 and the VQ codebooks
  E4.1–E4.7 — names its PDF page, and a test per table pins its length, its
  checksum and spot values re-read from the rendered page. The KBD window is
  also derived analytically and matched to Table 7.33 to five decimals.
- **Against independent implementations, as black boxes**
  (`tests/ac3_decode_vectors.rs`; no FFmpeg anywhere). A/52 defines the bit
  allocation in exact integers but leaves the transform and dequantisation
  to floating point, and lets dither be "any reasonably random sequence", so
  two conformant decoders agree to float rounding where the stream is
  deterministic and differ by their independent noise where it is not. The
  gate is relative to a *measured* noise floor (a second decode with the
  noise fill off; the reference's noise is independent, so the expected
  difference is √2 × ours): per channel, RMS ≤ max(1 LSB16, 1.5 × √2 ×
  floor) and peak ≤ max(8 LSB16, 2.5 × floor peak). The reference decoder is
  liba52 (through GStreamer's `a52dec` element); the streams come from
  aften, an independent AC-3 encoder, and from Dolby. The committed 5.1
  fixture (`tests/data/aften_51_448k.*`, 250 ms at 448 kbit/s with a
  `dynrng` profile, `tools/make_fixture.sh`) runs on every `cargo test`; the
  full sweep runs in CI's oracle job over the vectors `tools/make_vectors.sh`
  makes and the streams `tools/fetch_dolby_kit.sh` fetches from Dolby's
  Online Delivery Kit (`RIVET_AC3_VECTORS`; `RIVET_AC3_REQUIRE_VECTORS`
  fails rather than skips when they are missing).
- **E-AC-3 against Dolby's AC-3 encode of the same programme.** No decoder
  but FFmpeg's could serve as an E-AC-3 reference, and none is used. Dolby's
  kit carries its channel-identification programme both as E-AC-3 5.1 at
  256 kbit/s (spectral extension, AHT with VQ / GAQ / large mantissas,
  coupling, `dynrng`) and as AC-3 5.1 at 640 kbit/s; this decoder's E-AC-3
  output is held to liba52's decode of the AC-3 one. The two encodes are
  lossy in different ways, so the bar is not sample-level but what only a
  correct decode of both reaches: lag 0, every full-bandwidth channel's level
  within 0.5 dB, waveform SNR ≥ 15 dB and band energies to 16 kHz within
  3 dB, the LFE below 250 Hz within 1.5 dB. Every other stream in the kit
  (stereo and 5.1, Atmos-carrying JOC, A/V sync, silence; 384–640 kbit/s)
  must decode end to end with good CRCs.

Measured 2026-10-03 in CI (liba52 0.7.4, aften 0.0.8 git 2010-01-05), in
16-bit LSBs (1 LSB16 = 1/32768):

| streams | what they exercise | result |
|---|---|---|
| 13 aften encodes, each with and without `dynrng` applied, each also dither-stripped (`ac3_strip_dither`) | mono, 2/0, 3/0, 2/2, 3/2 + LFE; 32, 44.1 and 48 kHz; 64–448 kbit/s; rematrixing; aften's `dynrng` profiles; tones, pink / white / brown noise, clicks; copies with `blksw` forced in every channel (`ac3_make_blksw_vector.py`) | dither-stripped copies within **0.001 RMS / 0.01 peak** (float rounding); dithered originals at the noise floor (RMS / expected 0.84–1.28) |
| Dolby-encoded AC-3 `ChID_voices_6ch_640kbps_dd` (72 s) | `dynrng` in half the blocks, block switching (16 blocks where only some channels switch) | dither-stripped within **0.01 peak**, mixed-transform blocks included, nothing masked |
| Dolby-encoded E-AC-3 `ChID_voices_6ch_256kbps_ddp` against the AC-3 above | spectral extension, AHT, GAQ, coupling, `dynrng` | SNR 16.8–22.0 dB, levels within 0.02 dB, bands within 2.2 dB; LFE −0.4 dB |
| the kit's other ten E-AC-3 streams | coupling, rematrixing, block switching, JOC-carrying 5.1 at 448 / 640 kbit/s | decode clean, every CRC good |

What is not covered by an independent decoder: E-AC-3 sample by sample.
The E-AC-3-only tools are checked against the spec's tables and syntax, by
the encoder's round trips, and against Dolby's AC-3 encode above, which
catches a wrong channel, level, band or transform but not an error below
about −20 dB of the signal. A block where only some channels switch to the
short transform is compared like any other (`FrameDecoder::
mixed_transform_blocks()` lists them, and `RIVET_AC3_MASK_MIXED` masks them
for a reference that departs from §7.9.4 step 6 there). Mutation check:
changing one `hth` entry fails its table test and the fixture cross-check
in frame 0 — a wrong bit-allocation table desynchronises the mantissa parse
rather than degrading the audio.

## How the encoder is checked

Only against the spec and through this crate's decoder (`tests/encoder.rs`;
no other encoder or decoder is run). Every syncframe any test writes goes
through the same strict check: the header carries the configured format,
rate, layout, `bsmod` and `dialnorm`; an AC-3 frame is exactly Table 5.18's
size for the `frmsizecod` it sends and the running total of frame sizes
stays within a word of the nominal bit rate (E-AC-3 `frmsiz` likewise);
`crc1` leaves the §7.10.1 register at zero at the 5/8 point and `crc2` at
the end; every block parses without error; blocks 0–1 end before the 5/8
point and block 5's mantissas start after it (§5.5); the bits from the last
block to `auxdatae` are zero padding. That check runs over every `acmod`
with and without LFE at 48, 44.1 and 32 kHz and every AC-3 rate (912
configurations), and over E-AC-3 at 32–6144 kbit/s for every layout, 7.1
included (whose dependent substream the test decodes on its own).

Unit tests pin the parts to the spec: the forward transform against
§8.2.3.2's printed sum (to 1e-12) and, through the decoder's IMDCT, perfect
reconstruction across long/short switches; exponent preprocessing and
grouping by a hand-worked D25 case and the §7.1.3 unpacking; the
quantisers against Tables 7.19–7.23; the §7.3.5 grouping; the bit
allocation by a hand-worked §7.2.2 case; `crc1` / `crc2` against the
register.

Round trips, measured 2026-10-02 (one second of a test signal per channel:
six harmonics of a note, a decaying 1.8 kHz pluck every 0.25 s, a
low-passed noise floor; 60 Hz in the LFE). SNR per channel in input order,
decoded with dither on:

| stream | SNR dB | tools exercised |
|---|---|---|
| AC-3 mono 64 / 96 kbit/s | 21.6 / 27.9 | |
| AC-3 stereo 128 | 19.6 21.3 | coupling (with phase flags) above ~9 kHz, rematrixing |
| AC-3 stereo 192 / 256 / 384 | 27.9 28.7 / 33.6 34.7 / 42.6 42.9 | rematrixing |
| AC-3 stereo 192 at 44.1 / 32 kHz | 27.9 38.8 / 38.9 39.5 | rematrixing |
| AC-3 5.1 384 | 27.9 29.2 36.7 28.5 34.7 32.8 | coupling above ~10 kHz |
| AC-3 5.1 448 | 28.0 29.4 38.8 28.5 36.5 35.6 | coupling above ~11 kHz |
| AC-3 5.1 640 | 33.5 34.9 41.8 35.6 39.6 38.2 | |
| E-AC-3 stereo 96 / 192 | 17.5 19.7 / 27.9 28.8 | coupling, rematrixing, Table E2.10 strategies |
| E-AC-3 5.1 384 | 27.8 28.9 36.8 28.5 34.8 32.9 | coupling, Table E2.10 strategies |
| E-AC-3 5.1 1536 (3 blocks per frame) | 47.4 47.3 47.5 62.0 47.4 47.4 | |
| E-AC-3 7.1 768 (dependent substream) | 28.6 29.6 40.6 28.9 36.8 36.3 37.3 35.2 | |

Frequency response (a sine per frequency, worst channel): within ±0.36 dB
from 50 Hz to the coded bandwidth, coupled frequencies included — AC-3
stereo 384 kbit/s flat to 19.5 kHz; AC-3 5.1 448 kbit/s to 18 kHz (−0.36 dB
at 16 kHz, in the coupling range); E-AC-3 stereo 128 kbit/s to 14 kHz
(−0.29 dB), then the band limit.

Pre-echo: a noise burst out of silence, energy in the 768 samples before
the start of the block holding the onset, relative to the burst: −114 dB
(AC-3 stereo 192), −94 dB (AC-3 5.1 448), −120 dB (E-AC-3 stereo 128) with
block switching; −41, −46 and −45 dB with it off — the long transform
smears quantisation noise up to 256 samples ahead of the onset, the short
pair keeps it inside the onset's block.

## Provenance and licensing

Written from the text of ATSC A/52:2018 (with Annex E for E-AC-3); **no
AC-3 implementation's source was read or copied** — libavcodec's, liba52's
and aften's tables included. aften and liba52 are run only as command-line
tools, to make the decoder's test streams and decode them for comparison,
and the Dolby streams are Dolby's published test data; FFmpeg is not used at
all. The encoder follows
A/52's §8 (the informative encoder description) and the normative syntax
and decoding processes it must satisfy; it reuses the decoder's tables and
bit allocation, and its tests run no other implementation at all. The tables were transcribed from the
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
