//! Throughput benchmark: encodes 48 kHz stereo PCM (raw little-endian f32,
//! interleaved) as AC-3 stereo, AC-3 5.1 (the stereo spread over six
//! channels) and E-AC-3 stereo, decodes each result, and prints how many
//! times faster than real time each runs (best of several passes) with a
//! hash of the frames and of the decoded PCM.
//!
//! `cargo run --release --example ac3_bench -- <pcm.f32> [passes] [filter]`

use std::time::Instant;

use ac3::{Config, Encoder, Format, FrameDecoder, Layout};

fn best<F: FnMut()>(passes: usize, mut f: F) -> f64 {
    (0..passes)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64()
        })
        .fold(f64::INFINITY, f64::min)
}

fn fnv(hash: &mut u64, bytes: impl IntoIterator<Item = u8>) {
    for b in bytes {
        *hash = (*hash ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let raw = std::fs::read(args.get(1).expect("pcm file")).unwrap();
    let passes: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let only = args.get(3).cloned().unwrap_or_default();
    let stereo: Vec<f32> = raw.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
    let secs = stereo.len() as f64 / 2.0 / 48_000.0;
    // FL FR FC LFE SL SR from L R: centre and LFE from the sum, surrounds
    // from the difference.
    let six: Vec<f32> = stereo
        .as_chunks::<2>()
        .0
        .iter()
        .flat_map(|&[l, r]| {
            [l, r, 0.5 * (l + r), 0.25 * (l + r), 0.5 * (l - r), 0.5 * (r - l)]
        })
        .collect();
    type Case<'a> = (&'a str, Format, Layout, bool, u32, &'a [f32]);
    let cases: [Case; 3] = [
        ("ac3 2.0 192k", Format::Ac3, Layout::Stereo, false, 192, &stereo),
        ("ac3 5.1 448k", Format::Ac3, Layout::ThreeTwo, true, 448, &six),
        ("eac3 2.0 128k", Format::Eac3, Layout::Stereo, false, 128, &stereo),
    ];
    for (name, format, layout, lfe, kbps, pcm) in cases {
        if !name.contains(only.as_str()) {
            continue;
        }
        let cfg = Config::new(format, 48_000, layout, lfe, kbps);
        let mut frames = Vec::new();
        let t = best(passes, || {
            let mut enc = Encoder::new(cfg).unwrap();
            frames.clear();
            for c in pcm.chunks(enc.frame_samples() * cfg.channels() * 4) {
                frames.extend(enc.encode(c).unwrap());
            }
            frames.extend(enc.flush().unwrap());
        });
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        frames.iter().for_each(|f| fnv(&mut h, f.iter().copied()));
        println!("encode  {name:<14} {:7.1} x realtime (stream hash {h:016x})", secs / t);
        let mut out = Vec::new();
        let t = best(passes, || {
            let mut dec = FrameDecoder::new(1.0);
            out.clear();
            for f in &frames {
                dec.decode(f, &mut out).unwrap();
            }
        });
        let mut out_h = 0xcbf2_9ce4_8422_2325;
        fnv(&mut out_h, out.iter().flat_map(|v| v.to_bits().to_le_bytes()));
        println!("decode  {name:<14} {:7.1} x realtime (output hash {out_h:016x})", secs / t);
    }
}
