//! AC-3 / E-AC-3 (Dolby Digital / Digital Plus) decoder and encoder, pure
//! Rust, written from ATSC A/52:2018.
//!
//! Decoding: [`Decoder`] / [`FrameDecoder`], below. Encoding: [`Encoder`],
//! configured by [`Config`] — AC-3 at every Table 5.18 bit rate and E-AC-3
//! from 32 to 6144 kbit/s, every audio coding mode with or without LFE, 7.1
//! as ETSI TS 102 366 §E.2.8.2 lays it out (a 5.1 downmix in independent
//! substream 0, a dependent substream with the channels that replace or
//! extend it). Input to the encoder is interleaved
//! `f32` in the order the decoder outputs ([`Layout::speakers`]):
//!
//! ```
//! let cfg = ac3::Config::new(ac3::Format::Ac3, 48_000, ac3::Layout::Stereo, false, 192);
//! let mut enc = ac3::Encoder::new(cfg)?;
//! let pcm = vec![0.0f32; 2 * 4800]; // 0.1 s of stereo silence
//! let mut frames = enc.encode(&pcm)?;
//! frames.extend(enc.flush()?);
//! let mut dec = ac3::Decoder::new();
//! for f in &frames {
//!     assert_eq!(f.len(), 768); // 192 kbit/s at 48 kHz: 384 words
//!     dec.decode(f)?;
//! }
//! # Ok::<(), ac3::Error>(())
//! ```
//!
//! What it decodes
//! ---------------
//! - AC-3 (bsid ≤ 8): every mode, 1–6 channels including LFE — block
//!   switching, dither, coupling with phase flags, rematrixing, delta bit
//!   allocation, dynamic range compression (`dynrng`, applied by default,
//!   scalable through [`Options::drc_scale`]).
//! - E-AC-3 (bsid 16): independent substream 0 and its dependent
//!   substreams, whose channels replace or supplement substream 0's as
//!   ETSI TS 102 366 §E.2.8.2 says (7.1 as eight channels, through
//!   [`Decoder`]; [`Decoder::set_independent_only`] for substream 0's 5.1
//!   alone, the downmix a 5.1 system plays) — all `numblkscod` frame
//!   sizes, reduced sample rates, frame-based exponent strategies, the three
//!   SNR-offset strategies, standard coupling, spectral extension (with
//!   attenuation), and the adaptive hybrid transform (vector quantisation
//!   and gain-adaptive quantisation).
//!
//! What it refuses or skips, by name
//! ---------------------------------
//! - Enhanced coupling (`ecplinu = 1`) → [`Error::Unsupported`].
//! - Independent substreams other than 0, and their dependent substreams,
//!   are skipped (Annex E §3.8.1 says a reference decoder may). A
//!   dependent substream whose channel map names a location there is no
//!   [`Speaker`] for (Lc/Rc, the heights, the wides, ...) is left out, and
//!   the programme is the rest. [`FrameDecoder`] decodes one syncframe, so
//!   one substream: 7.1 is assembled by [`Decoder`].
//! - bsid 9/10 (Annex D reduced-rate AC-3) → `Unsupported`.
//! - `dialnorm` and heavy compression (`compr`) are not applied; transient pre-noise processing is parsed and
//!   ignored (an optional post-process).
//!
//! Output is f32 interleaved in WAVE order for the layout (for 5.1: FL FR
//! FC LFE SL SR, for 7.1 FL FR FC LFE BL BR SL SR), named speaker by speaker
//! by [`Frame::layout`]. No downmix is performed here.
//!
//! Two levels of API: [`Decoder`] takes bytes in any chunking, resynchronises
//! on the 0x0B77 syncword and returns one [`Frame`] per syncframe (an E-AC-3
//! one with its dependent substreams);
//! [`FrameDecoder`] decodes one whole syncframe at a time and exposes the
//! statistics the cross-check harness reports.
//!
//! Tables live in [`tables`] with per-table checksum tests; the
//! cross-checks against liba52 and Dolby's own encodes live in
//! `tests/ac3_decode_vectors.rs`, the
//! encoder's checks (syntax, CRCs and frame sizes of every frame; round
//! trips through the decoder) in `tests/encoder.rs`.
//!
//! With the `tracing` feature the decoder logs through `tracing`: the
//! per-frame / per-block syntax trace at TRACE, resynchronisation at DEBUG
//! and skipped damaged frames at WARN. Without it nothing is logged.

pub mod bitalloc;
mod bits;
pub mod decoder;
mod encoder;
pub mod imdct;
mod programme;
pub mod tables;

pub use decoder::{Features, FrameDecoder, Header, frame_crc_ok, parse_header};
pub use encoder::{Config, Coupling, Encoder, Format, Layout};

/// What can go wrong decoding or encoding a stream.
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
    /// The encoder was given a configuration or input it cannot take: a
    /// sample rate or bit rate the format does not have, a sample count that
    /// is not a whole number of interleaved frames.
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

/// A speaker position, as the decoder names its output channels
/// ([`Header::speakers`]) and the encoder its input ([`Layout::speakers`]).
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
    /// Back left (7.1's left rear surround, carried by an E-AC-3 dependent
    /// substream: chanmap's Lrs).
    BL,
    /// Back right (7.1's right rear surround).
    BR,
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
            Speaker::BL => "BL",
            Speaker::BR => "BR",
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

/// One decoded syncframe — for E-AC-3, independent substream 0 with the
/// dependent substreams that came with it.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    /// Interleaved f32 PCM, ±1.0 full scale, `header.samples()` samples per
    /// channel in [`Frame::layout`] order.
    pub samples: Vec<f32>,
    /// Samples per second.
    pub sample_rate: u32,
    /// Output channels (full-bandwidth plus LFE), the dependent substreams'
    /// included.
    pub channels: usize,
    /// The speaker each channel feeds, in slot order (WAVE order): the
    /// header's [`Header::speakers`], with what dependent substreams carry
    /// added or put in place — 7.1 is FL FR FC LFE BL BR SL SR.
    pub layout: Vec<Speaker>,
    /// The independent substream's syncframe header.
    pub header: Header,
}

impl Frame {
    /// The speakers the channels carry, in slot order.
    pub fn speakers(&self) -> Vec<Speaker> {
        self.layout.clone()
    }
}

/// A stream decoder: takes bytes that hold one or more whole or partial
/// syncframes, in any chunking, and returns one [`Frame`] per AC-3
/// syncframe or E-AC-3 independent substream 0 syncframe.
///
/// It resynchronises on the 0x0B77 syncword (junk before a syncframe is
/// skipped), buffers a partial syncframe until the rest arrives, and skips a
/// damaged syncframe (resetting the overlap-add history) rather than failing
/// the stream. [`Error::Unsupported`] is returned as soon as it is met.
///
/// The dependent substreams of E-AC-3 independent substream 0 (Annex E) are
/// decoded, each with a state of its own, and their channels put into the
/// programme where their channel map says: a 7.1 stream comes out as eight
/// channels. An E-AC-3 frame is held back until what follows shows whether
/// dependent substreams belong to it: the next independent syncframe, or
/// at the end of the bytes given once its dependent substreams have come,
/// or at once in a stream that has had none. So bytes cut between an
/// independent substream and its dependent ones lose nothing, except at a
/// stream's very first frame, where nothing has yet said that dependent
/// substreams follow (a container sample, or a PES packet, holds the whole
/// access unit, and loses nothing at all). Independent
/// substreams other than 0, and their dependent substreams, are skipped
/// (Annex E §3.8.1 lets a decoder).
pub struct Decoder {
    inner: FrameDecoder,
    /// One decoder per dependent substream id, made on first use.
    dependent: Vec<Option<FrameDecoder>>,
    drc_scale: f32,
    /// The programme frame waiting for its dependent substreams, and
    /// whether any came.
    pending: Option<(Frame, bool)>,
    /// The stream has had a dependent substream.
    has_dependents: bool,
    /// The last independent syncframe was substream 0's.
    after_independent_zero: bool,
    /// Skip the dependent substreams (§E.2.8.2's 5.1-or-fewer decoder).
    independent_only: bool,
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
        Self {
            inner: FrameDecoder::new(opts.drc_scale),
            dependent: (0..8).map(|_| None).collect(),
            drc_scale: opts.drc_scale,
            pending: None,
            has_dependents: false,
            after_independent_zero: false,
            independent_only: false,
            buf: Vec::new(),
        }
    }

    /// Decode independent substream 0 alone and skip its dependent
    /// substreams — what ETSI TS 102 366 §E.2.8.2 has a decoder reproducing
    /// 5.1 or fewer channels do. Substream 0 of a programme of more than 5.1
    /// channels is its 5.1 downmix, so a 7.1 stream comes out as that 5.1.
    /// Off by default: the whole programme is decoded.
    pub fn set_independent_only(&mut self, enabled: bool) {
        self.independent_only = enabled;
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
        let mut frames = self.drain()?;
        frames.extend(self.pending.take().map(|(f, _)| f));
        self.buf.clear();
        Ok(frames)
    }

    /// Decode a dependent substream's syncframe into the pending programme
    /// frame.
    fn add_dependent(&mut self, frame: &[u8], substreamid: u8) -> Result<(), Error> {
        self.has_dependents = true;
        // Decoded even with no programme frame to join (one already gone
        // out), so its overlap-add history is there for the next.
        let drc = self.drc_scale;
        let dec = self.dependent[usize::from(substreamid)].get_or_insert_with(|| {
            let mut d = FrameDecoder::new(drc);
            d.set_decode_all_substreams(true);
            d
        });
        let mut pcm = Vec::new();
        let hdr = match dec.decode(frame, &mut pcm) {
            Ok(Some(h)) => h,
            Ok(None) => return Ok(()),
            Err(Error::Unsupported(e)) => return Err(Error::Unsupported(e)),
            Err(_e) => {
                #[cfg(feature = "tracing")]
                tracing::warn!(error = %_e, "eac3: dependent substream frame decode failed, skipping");
                dec.reset();
                return Ok(());
            }
        };
        let Some((base, merged)) = self.pending.as_mut() else {
            return Ok(());
        };
        let chanmap = programme::dependent_chanmap(frame)?;
        match programme::merge(base, &pcm, &hdr, chanmap) {
            Ok(()) => *merged = true,
            Err(_why) => {
                #[cfg(feature = "tracing")]
                tracing::warn!(substream = substreamid, why = %_why, "eac3: dependent substream not used");
            }
        }
        Ok(())
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
            let frame = self.buf[pos..pos + hdr.frame_len].to_vec();
            pos += hdr.frame_len;
            if hdr.eac3 && hdr.strmtyp == 1 {
                // A dependent substream: of independent substream 0 when it
                // follows that one (substream ids number the dependent
                // substreams of the independent substream before them).
                if self.after_independent_zero && !self.independent_only {
                    self.add_dependent(&frame, hdr.substreamid)?;
                }
                continue;
            }
            self.after_independent_zero = !hdr.eac3 || hdr.substreamid == 0;
            // Any other syncframe ends the programme frame before it.
            frames.extend(self.pending.take().map(|(f, _)| f));
            let mut pcm = Vec::new();
            match self.inner.decode(&frame, &mut pcm) {
                Ok(Some(h)) => {
                    let f = Frame {
                        samples: pcm,
                        sample_rate: h.sample_rate,
                        channels: h.channels(),
                        layout: h.speakers(),
                        header: h,
                    };
                    if h.eac3 && !self.independent_only {
                        self.pending = Some((f, false));
                    } else {
                        frames.push(f);
                    }
                }
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
        }
        self.buf.drain(..pos);
        // The bytes given end here: a programme frame whose dependent
        // substreams have come, or of a stream that has none, is complete.
        if self.buf.is_empty()
            && self
                .pending
                .as_ref()
                .is_some_and(|(_, merged)| *merged || !self.has_dependents)
        {
            frames.extend(self.pending.take().map(|(f, _)| f));
        }
        Ok(frames)
    }
}

impl std::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decoder")
            .field("buffered", &self.buf.len())
            .finish()
    }
}
