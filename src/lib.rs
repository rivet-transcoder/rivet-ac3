//! AC-3 / E-AC-3 (Dolby Digital / Digital Plus) decoder, pure Rust,
//! written from ATSC A/52:2018.
//!
//! What it decodes
//! ---------------
//! - AC-3 (bsid ≤ 8): every mode, 1–6 channels including LFE — block
//!   switching, dither, coupling with phase flags, rematrixing, delta bit
//!   allocation, dynamic range compression (`dynrng`, applied by default,
//!   scalable through [`Options::drc_scale`]).
//! - E-AC-3 (bsid 16): independent substream 0 — all `numblkscod` frame
//!   sizes, reduced sample rates, frame-based exponent strategies, the three
//!   SNR-offset strategies, standard coupling, spectral extension (with
//!   attenuation), and the adaptive hybrid transform (vector quantisation
//!   and gain-adaptive quantisation).
//!
//! What it refuses or skips, by name
//! ---------------------------------
//! - Enhanced coupling (`ecplinu = 1`) → [`Error::Unsupported`].
//! - Dependent substreams and independent substreams other than 0 are
//!   skipped (Annex E §3.8.1 says a reference decoder may), so a 7.1
//!   E-AC-3 stream decodes as its 5.1 core.
//! - bsid 9/10 (Annex D reduced-rate AC-3) → `Unsupported`.
//! - `dialnorm` and heavy compression (`compr`) are not applied, matching
//!   libavcodec's default; transient pre-noise processing is parsed and
//!   ignored (an optional post-process).
//!
//! Output is f32 interleaved in ffmpeg's native order for the layout (for
//! 5.1: FL FR FC LFE SL SR), named speaker by speaker by
//! [`Header::speakers`]. No downmix is performed here.
//!
//! Two levels of API: [`Decoder`] takes bytes in any chunking, resynchronises
//! on the 0x0B77 syncword and returns one [`Frame`] per syncframe;
//! [`FrameDecoder`] decodes one whole syncframe at a time and exposes the
//! statistics the cross-check harness reports.
//!
//! Tables live in [`tables`] with per-table checksum tests; the
//! cross-check against libavcodec lives in `tests/ac3_decode_vectors.rs`.
//!
//! With the `tracing` feature the decoder logs through `tracing`: the
//! per-frame / per-block syntax trace at TRACE, resynchronisation at DEBUG
//! and skipped damaged frames at WARN. Without it nothing is logged.

pub mod bitalloc;
mod bits;
pub mod decoder;
mod encoder;
pub mod imdct;
pub mod tables;

pub use decoder::{Features, FrameDecoder, Header, frame_crc_ok, parse_header};

/// What can go wrong decoding a stream.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The bit stream is damaged or mis-parsed: a bad sync header, a field
    /// out of range, a read past the end of the syncframe.
    #[error("decode failed: {0}")]
    Decode(String),
    /// A valid stream using something this decoder does not implement
    /// (enhanced coupling, bsid 9/10).
    #[error("unsupported: {0}")]
    Unsupported(String),
}

/// A speaker position, as the decoder names its output channels
/// ([`Header::speakers`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Speaker {
    /// Front left.
    FL,
    /// Front right.
    FR,
    /// Front centre.
    FC,
    /// Low-frequency effects.
    LFE,
    /// Back centre (the single surround of `acmod` 2/1 and 3/1).
    BC,
    /// Side left (the left surround of `acmod` 2/2 and 3/2).
    SL,
    /// Side right (the right surround of `acmod` 2/2 and 3/2).
    SR,
}

impl std::fmt::Display for Speaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Speaker::FL => "FL",
            Speaker::FR => "FR",
            Speaker::FC => "FC",
            Speaker::LFE => "LFE",
            Speaker::BC => "BC",
            Speaker::SL => "SL",
            Speaker::SR => "SR",
        })
    }
}

/// Decoder options.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Options {
    /// Fraction of the stream's dynamic range compression to apply: 1.0
    /// applies `dynrng` exactly as encoded (the A/52 default), 0.0 ignores it.
    pub drc_scale: f32,
}

impl Default for Options {
    fn default() -> Self {
        Self { drc_scale: 1.0 }
    }
}

/// One decoded syncframe.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    /// Interleaved f32 PCM, ±1.0 full scale, `header.samples()` samples per
    /// channel in [`Header::speakers`] order.
    pub samples: Vec<f32>,
    /// Samples per second.
    pub sample_rate: u32,
    /// Output channels (full-bandwidth plus LFE).
    pub channels: usize,
    /// The syncframe's header.
    pub header: Header,
}

impl Frame {
    /// The speakers the channels carry, in slot order.
    pub fn speakers(&self) -> Vec<Speaker> {
        self.header.speakers()
    }
}

/// A stream decoder: takes bytes that hold one or more whole or partial
/// syncframes, in any chunking, and returns one [`Frame`] per syncframe.
///
/// It resynchronises on the 0x0B77 syncword (junk before a syncframe is
/// skipped), buffers a partial syncframe until the rest arrives, and skips a
/// damaged syncframe (resetting the overlap-add history) rather than failing
/// the stream. [`Error::Unsupported`] is returned as soon as it is met.
pub struct Decoder {
    inner: FrameDecoder,
    buf: Vec<u8>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    /// A decoder with the default [`Options`].
    pub fn new() -> Self {
        Self::with_options(Options::default())
    }

    /// A decoder with the given options.
    pub fn with_options(opts: Options) -> Self {
        Self { inner: FrameDecoder::new(opts.drc_scale), buf: Vec::new() }
    }

    /// The header of the most recent syncframe decoded, if any.
    pub fn last_header(&self) -> Option<Header> {
        self.inner.last_header()
    }

    /// The syncframe decoder underneath, for its statistics.
    pub fn frame_decoder(&self) -> &FrameDecoder {
        &self.inner
    }

    /// Bytes held back: a partial syncframe waiting for the rest.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Append `data` and decode every whole syncframe now buffered.
    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<Frame>, Error> {
        self.buf.extend_from_slice(data);
        self.drain()
    }

    /// Decode what is buffered and drop any partial syncframe left at the
    /// end. Call once at the end of the stream.
    pub fn flush(&mut self) -> Result<Vec<Frame>, Error> {
        let frames = self.drain()?;
        self.buf.clear();
        Ok(frames)
    }

    fn drain(&mut self) -> Result<Vec<Frame>, Error> {
        let mut frames = Vec::new();
        let mut pos = 0usize;
        while self.buf.len() - pos >= 8 {
            // resync: find the next 0x0B77
            match self.buf[pos..].windows(2).position(|w| w == [0x0b, 0x77]) {
                Some(0) => {}
                Some(off) => {
                    #[cfg(feature = "tracing")]
                    tracing::debug!(skipped = off, "ac3: resynchronised on 0x0B77");
                    pos += off;
                    if self.buf.len() - pos < 8 {
                        break;
                    }
                }
                None => {
                    pos = self.buf.len().saturating_sub(1);
                    break;
                }
            }
            let hdr = match parse_header(&self.buf[pos..]) {
                Ok(h) => h,
                Err(Error::Unsupported(e)) => return Err(Error::Unsupported(e)),
                Err(_e) => {
                    #[cfg(feature = "tracing")]
                    tracing::debug!(error = %_e, "ac3: bad sync header, skipping a byte");
                    pos += 1;
                    continue;
                }
            };
            if self.buf.len() - pos < hdr.frame_len {
                break;
            }
            let frame = &self.buf[pos..pos + hdr.frame_len];
            let mut pcm = Vec::new();
            match self.inner.decode(frame, &mut pcm) {
                Ok(Some(h)) => frames.push(Frame {
                    samples: pcm,
                    sample_rate: h.sample_rate,
                    channels: h.channels(),
                    header: h,
                }),
                Ok(None) => {}
                Err(Error::Unsupported(e)) => return Err(Error::Unsupported(e)),
                Err(_e) => {
                    // A damaged frame: report it, keep the overlap history
                    // honest and carry on with the next syncframe.
                    #[cfg(feature = "tracing")]
                    tracing::warn!(error = %_e, "ac3: frame decode failed, skipping");
                    self.inner.reset();
                }
            }
            pos += hdr.frame_len;
        }
        self.buf.drain(..pos);
        Ok(frames)
    }
}

impl std::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decoder").field("buffered", &self.buf.len()).finish()
    }
}
