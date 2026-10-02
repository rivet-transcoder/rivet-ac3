//! Encode a WAV file to AC-3 or E-AC-3.
//!
//!     cargo run --release --example ac3_encode -- in.wav out.ac3 [kbit/s]
//!     cargo run --release --example ac3_encode -- in.wav out.eac3 [kbit/s]
//!
//! The format follows the output's extension (`.eac3` / `.ec3` → E-AC-3,
//! anything else AC-3). The layout follows the channel count: 1 → 1/0,
//! 2 → 2/0, 3 → 3/0, 4 → 2/2, 5 → 3/2, 6 → 3/2 + LFE, 8 → 3/4 + LFE
//! (E-AC-3 only). WAV's channel order (FL FR FC LFE, then the surrounds) is
//! the encoder's input order. 16-, 24- and 32-bit integer and 32-bit float
//! PCM are read; the sample rate must be 32, 44.1 or 48 kHz.

use std::io::Write;

use ac3::{Config, Encoder, Format, Layout};

struct Wav {
    channels: usize,
    rate: u32,
    samples: Vec<f32>,
}

fn read_wav(bytes: &[u8]) -> Result<Wav, String> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let mut pos = 12;
    let mut fmt = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = &bytes[pos + 8..(pos + 8 + len).min(bytes.len())];
        match id {
            b"fmt " => {
                let u16_at = |o: usize| u16::from_le_bytes([body[o], body[o + 1]]);
                let mut tag = u16_at(0);
                if tag == 0xfffe && body.len() >= 26 {
                    tag = u16_at(24); // WAVE_FORMAT_EXTENSIBLE: the sub-format GUID's first word
                }
                let rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
                fmt = Some((tag, usize::from(u16_at(2)), rate, u16_at(14)));
            }
            b"data" => {
                let (tag, channels, rate, bits) = fmt.ok_or("data before fmt")?;
                let samples: Vec<f32> = match (tag, bits) {
                    (1, 16) => body.chunks_exact(2).map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0).collect(),
                    (1, 24) => body
                        .chunks_exact(3)
                        .map(|c| (i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8) as f32 / 8_388_608.0)
                        .collect(),
                    (1, 32) => body.chunks_exact(4).map(|c| i32::from_le_bytes(c.try_into().unwrap()) as f32 / 2_147_483_648.0).collect(),
                    (3, 32) => body.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect(),
                    _ => return Err(format!("unsupported WAV sample format (tag {tag}, {bits} bits)")),
                };
                return Ok(Wav { channels, rate, samples });
            }
            _ => {}
        }
        pos += 8 + len + (len & 1);
    }
    Err("no data chunk".into())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: ac3_encode <in.wav> <out.ac3|out.eac3> [kbit/s]");
        std::process::exit(2);
    }
    let wav = read_wav(&std::fs::read(&args[1]).expect("read input")).unwrap_or_else(|e| {
        eprintln!("{}: {e}", args[1]);
        std::process::exit(1);
    });
    let eac3 = args[2].ends_with(".eac3") || args[2].ends_with(".ec3");
    let (layout, lfe) = match wav.channels {
        1 => (Layout::Mono, false),
        2 => (Layout::Stereo, false),
        3 => (Layout::ThreeZero, false),
        4 => (Layout::TwoTwo, false),
        5 => (Layout::ThreeTwo, false),
        6 => (Layout::ThreeTwo, true),
        8 => (Layout::ThreeFour, true),
        n => {
            eprintln!("{n} channels: no layout for that count");
            std::process::exit(1);
        }
    };
    let default_kbps = match wav.channels {
        1 => 96,
        2 => 192,
        3..=6 => 448,
        _ => 1024,
    };
    let kbps = args.get(3).map_or(default_kbps, |s| s.parse().expect("bit rate in kbit/s"));
    let cfg = Config::new(if eac3 { Format::Eac3 } else { Format::Ac3 }, wav.rate, layout, lfe, kbps);
    let mut enc = Encoder::new(cfg).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let mut out = std::io::BufWriter::new(std::fs::File::create(&args[2]).expect("create output"));
    let mut frames = 0usize;
    for chunk in wav.samples.chunks(wav.channels * 4096) {
        for f in enc.encode(chunk).expect("encode") {
            out.write_all(&f).expect("write");
            frames += 1;
        }
    }
    for f in enc.flush().expect("flush") {
        out.write_all(&f).expect("write");
        frames += 1;
    }
    println!(
        "{frames} {} frames, {:?}{} at {kbps} kbit/s, {} Hz (decoder output is delayed by {} samples)",
        if eac3 { "E-AC-3" } else { "AC-3" },
        layout,
        if lfe { " + LFE" } else { "" },
        wav.rate,
        enc.delay()
    );
}
