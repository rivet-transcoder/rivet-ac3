//! An E-AC-3 programme assembled from its substreams (ETSI TS 102 366
//! §E.2.8): independent substream 0 and the dependent substreams that follow
//! it in the same span of time, each decoded by a [`FrameDecoder`] of its
//! own, the dependent substreams' channels put where their channel map says.
//!
//! §E.2.8.2: a dependent substream carries channels that replace or
//! supplement those of independent substream 0. With its `chanmape` clear,
//! the channels its own `acmod` / `lfeon` name overwrite the independent
//! substream's channels of those names; with it set, the locations
//! `chanmap` names (Table E.1.4, bit 0 the field's most significant), a
//! location substream 0 already has replacing that channel, any other
//! adding one. The coded channels are assigned to the set bits in order —
//! the full-bandwidth channels in coding order to the full-bandwidth
//! locations (a pair location taking two), the LFE channel to the LFE
//! location. A 7.1 programme is the usual case: substream 0 is a 5.1
//! downmix of it (L C R Ls Rs, LFE, the back surrounds folded into the
//! surrounds), and a 2/2 dependent substream mapped to Ls, Rs and the
//! Lrs/Rrs pair puts the discrete side surrounds in place of the downmixed
//! ones and adds the back surrounds. (A 2/0 dependent substream mapped to
//! Lrs/Rrs alone, adding the back surrounds to a discrete 5.1, is assembled
//! the same way.)

use crate::bits::BitReader;
use crate::decoder::{Header, coded_output_channels};
use crate::{Error, Frame, Speaker};

/// The `chanmap` of a dependent substream's syncframe, when it has one
/// (Annex E §2.2.2 `bsi()`: `chanmape`, then the 16-bit map).
pub(crate) fn dependent_chanmap(frame: &[u8]) -> Result<Option<u16>, Error> {
    let mut br = BitReader::new(frame);
    br.skip(16)?; // syncword
    let strmtyp = br.read(2)?;
    br.skip(3 + 11 + 2 + 2)?; // substreamid, frmsiz, fscod, fscod2 or numblkscod
    let acmod = br.read(3)?;
    br.skip(1 + 5 + 5)?; // lfeon, bsid, dialnorm
    if br.read_bit()? {
        br.skip(8)?; // compr
    }
    if acmod == 0 {
        br.skip(5)?; // dialnorm2
        if br.read_bit()? {
            br.skip(8)?; // compr2
        }
    }
    if strmtyp == 1 && br.read_bit()? {
        return Ok(Some(br.read(16)? as u16));
    }
    Ok(None)
}

/// The speakers of the full-bandwidth channels of `acmod`, in coding order
/// (Table 5.8: L C R, then the surround or surrounds).
fn coded_speakers(acmod: u8) -> &'static [Speaker] {
    use Speaker::*;
    match acmod {
        0 | 2 => &[FL, FR],
        1 => &[FC],
        3 => &[FL, FC, FR],
        4 => &[FL, FR, BC],
        5 => &[FL, FC, FR, BC],
        6 => &[FL, FR, SL, SR],
        _ => &[FL, FC, FR, SL, SR],
    }
}

/// The channel locations of the 16 `chanmap` bits, bit 0 (the field's most
/// significant bit) first: ETSI TS 102 366 Table E.1.4.
const CHANMAP_LOCATIONS: [&str; 16] = [
    "Left",
    "Centre",
    "Right",
    "Left Surround",
    "Right Surround",
    "Lc/Rc pair",
    "Lrs/Rrs pair",
    "Cs",
    "Ts",
    "Lsd/Rsd pair",
    "Lw/Rw pair",
    "Vhl/Vhr pair",
    "Vhc",
    "Lts/Rts pair",
    "LFE2",
    "LFE",
];

/// The `chanmap` bit of the LFE location (bit 15, the least significant).
const CHANMAP_LFE: u16 = 1;

/// The full-bandwidth locations a `chanmap` names, in bit order, a pair as
/// two (the LFE location, bit 15, is not one of them); `Err` naming the
/// first location this decoder has no output for.
fn chanmap_locations(chanmap: u16) -> Result<Vec<Speaker>, String> {
    use Speaker::*;
    let mut out = Vec::new();
    for (bit, name) in CHANMAP_LOCATIONS[..15].iter().enumerate() {
        if chanmap & (0x8000 >> bit) == 0 {
            continue;
        }
        match bit {
            0 => out.push(FL),
            1 => out.push(FC),
            2 => out.push(FR),
            3 => out.push(SL),
            4 => out.push(SR),
            6 => out.extend([BL, BR]),
            7 => out.push(BC),
            _ => return Err(format!("chanmap location {name}")),
        }
    }
    Ok(out)
}

/// Rank in WAVE (`WAVEFORMATEXTENSIBLE` channel mask) order, the order the
/// decoder emits a layout in.
fn wave_rank(s: Speaker) -> u8 {
    match s {
        Speaker::FL => 0,
        Speaker::FR => 1,
        Speaker::FC => 2,
        Speaker::LFE => 3,
        Speaker::BL => 4,
        Speaker::BR => 5,
        Speaker::BC => 8,
        Speaker::SL => 9,
        Speaker::SR => 10,
    }
}

/// Put a dependent substream's decoded channels (`pcm`, interleaved in the
/// decoder's output order for `hdr`) into the programme `base`: replacing
/// the channels `base` already has at their locations, adding the rest, the
/// whole re-ordered to WAVE order. `Err` (and `base` untouched) for a
/// substream that does not fit: another length, or a location this decoder
/// has no output for.
pub(crate) fn merge(
    base: &mut Frame,
    pcm: &[f32],
    hdr: &Header,
    chanmap: Option<u16>,
) -> Result<(), String> {
    let n = base.samples.len() / base.channels.max(1);
    let dep_ch = hdr.channels();
    if hdr.samples() != n || pcm.len() != n * dep_ch || hdr.sample_rate != base.sample_rate {
        return Err(format!(
            "{} samples at {} Hz against the independent substream's {n} at {}",
            hdr.samples(),
            hdr.sample_rate,
            base.sample_rate
        ));
    }
    let fbw: Vec<Speaker> = match chanmap {
        Some(m) => {
            let locations = chanmap_locations(m)?;
            // §E.1.3.1.8: the locations named are the coded channels, the
            // LFE among them.
            if hdr.lfeon != (m & CHANMAP_LFE != 0) {
                return Err(format!(
                    "lfeon {} against the chanmap's LFE bit",
                    u8::from(hdr.lfeon)
                ));
            }
            locations
        }
        None => coded_speakers(hdr.acmod).to_vec(),
    };
    if fbw.len() != hdr.nfchans {
        return Err(format!(
            "{} full-bandwidth channels for {} chanmap locations",
            hdr.nfchans,
            fbw.len()
        ));
    }
    // The location of each of the dependent substream's output slots.
    let dep_speakers: Vec<Speaker> = coded_output_channels(hdr.acmod, hdr.lfeon)
        .into_iter()
        .map(|c| c.map_or(Speaker::LFE, |i| fbw[i]))
        .collect();
    let mut layout = base.layout.clone();
    for s in &dep_speakers {
        if !layout.contains(s) {
            layout.push(*s);
        }
    }
    layout.sort_by_key(|&s| wave_rank(s));
    let source = |s: Speaker| -> (&[f32], usize, usize) {
        match dep_speakers.iter().position(|&d| d == s) {
            Some(slot) => (pcm, dep_ch, slot),
            None => (
                &base.samples,
                base.channels,
                base.layout
                    .iter()
                    .position(|&b| b == s)
                    .expect("in one of the two"),
            ),
        }
    };
    let ch = layout.len();
    let mut out = vec![0.0f32; n * ch];
    for (slot, &s) in layout.iter().enumerate() {
        let (src, stride, at) = source(s);
        for i in 0..n {
            out[i * ch + slot] = src[i * stride + at];
        }
    }
    base.samples = out;
    base.channels = ch;
    base.layout = layout;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chanmap_bits_are_counted_from_the_most_significant() {
        assert_eq!(
            chanmap_locations(0x0200),
            Ok(vec![Speaker::BL, Speaker::BR])
        );
        assert_eq!(
            chanmap_locations(0xA000),
            Ok(vec![Speaker::FL, Speaker::FR])
        );
        assert_eq!(chanmap_locations(0x0100), Ok(vec![Speaker::BC]));
        assert!(chanmap_locations(0x0400).unwrap_err().contains("Lc/Rc"));
        // The LFE location is not a full-bandwidth one.
        assert_eq!(
            chanmap_locations(0x0201),
            Ok(vec![Speaker::BL, Speaker::BR])
        );
        // The 7.1 encoder's map: Ls, Rs, then the Lrs/Rrs pair.
        assert_eq!(
            chanmap_locations(crate::encoder::SEVEN_ONE_CHANMAP),
            Ok(vec![Speaker::SL, Speaker::SR, Speaker::BL, Speaker::BR])
        );
    }

    /// Table E.1.4, every bit: the locations by name, bit 13 the Lts/Rts
    /// pair (top surrounds), and each one this decoder has no output for
    /// refused by that name.
    #[test]
    fn chanmap_names_follow_table_e_1_4() {
        assert_eq!(CHANMAP_LOCATIONS[11], "Vhl/Vhr pair");
        assert_eq!(CHANMAP_LOCATIONS[12], "Vhc");
        assert_eq!(CHANMAP_LOCATIONS[13], "Lts/Rts pair");
        assert_eq!(CHANMAP_LOCATIONS[14], "LFE2");
        assert_eq!(CHANMAP_LOCATIONS[15], "LFE");
        for bit in [5usize, 8, 9, 10, 11, 12, 13, 14] {
            let e = chanmap_locations(0x8000 >> bit).unwrap_err();
            assert!(e.contains(CHANMAP_LOCATIONS[bit]), "bit {bit}: {e}");
        }
    }
}
