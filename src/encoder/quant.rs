//! Mantissa quantisation (§8.2.13, the inverse of §7.3.3) and the bit cost
//! of a block's mantissas with the §7.3.5 grouping.

use crate::tables::ASYM_MANT_BITS;

/// Levels of the symmetric quantiser for bap 1..=5 (Tables 7.19–7.23).
const SYM_LEVELS: [u32; 6] = [0, 3, 5, 7, 11, 15];

/// Quantise a normalised mantissa (`coefficient · 2^exp`, nominally in
/// [−1, 1)) with the quantiser `bap` selects. Returns the code: a level
/// index for bap 1..=5 (`(2i − (L−1))/L` is level i), the two's-complement
/// value in `bits` bits for bap ≥ 6.
pub(super) fn quantize(m: f32, bap: u8) -> u32 {
    match bap {
        0 => 0,
        1..=5 => {
            let l = SYM_LEVELS[bap as usize] as f32;
            let i = ((m * l + (l - 1.0)) * 0.5).round();
            i.clamp(0.0, l - 1.0) as u32
        }
        _ => {
            let bits = u32::from(ASYM_MANT_BITS[(bap - 6) as usize]);
            let full = (1i32 << (bits - 1)) as f32;
            let q = (m * full).round().clamp(-full, full - 1.0) as i32;
            (q as u32) & ((1u32 << bits) - 1)
        }
    }
}

/// The value a decoder reconstructs from `quantize`'s code (§7.3.3).
#[cfg(test)]
pub(super) fn dequantize(code: u32, bap: u8) -> f32 {
    match bap {
        0 => 0.0,
        1..=5 => {
            let l = SYM_LEVELS[bap as usize] as f32;
            (2.0 * code as f32 - (l - 1.0)) / l
        }
        _ => {
            let bits = u32::from(ASYM_MANT_BITS[(bap - 6) as usize]);
            let shift = 32 - bits;
            (((code << shift) as i32) >> shift) as f32 / (1u32 << (bits - 1)) as f32
        }
    }
}

/// Bits per mantissa for the ungrouped quantisers; 0 for the grouped ones.
fn plain_bits(bap: u8) -> usize {
    match bap {
        3 => 3,
        5 => 4,
        6..=15 => usize::from(ASYM_MANT_BITS[(bap - 6) as usize]),
        _ => 0,
    }
}

/// Per-block tally of mantissas by quantiser, for the §7.3.5 grouping:
/// bap 1 in triples of 5 bits, bap 2 in triples of 7, bap 4 in pairs of 7,
/// each group shared across channels within the block.
#[derive(Default, Clone, Copy)]
pub(super) struct Tally {
    pub n1: usize,
    pub n2: usize,
    pub n4: usize,
    pub plain: usize,
}

impl Tally {
    pub fn add(&mut self, baps: &[u8]) {
        for &b in baps {
            match b {
                1 => self.n1 += 1,
                2 => self.n2 += 1,
                4 => self.n4 += 1,
                _ => self.plain += plain_bits(b),
            }
        }
    }
    pub fn bits(&self) -> usize {
        self.n1.div_ceil(3) * 5 + self.n2.div_ceil(3) * 7 + self.n4.div_ceil(2) * 7 + self.plain
    }
}

/// Packs one block's mantissas in stream order: a grouped code is written at
/// the position of its group's first mantissa (§7.3.5), with later members
/// filled in as they arrive and unfilled members left at the zero level.
#[derive(Default)]
pub(super) struct Packer {
    /// (value, bits) in stream order.
    pub items: Vec<(u32, u32)>,
    open1: Option<(usize, u8)>,
    open2: Option<(usize, u8)>,
    open4: Option<(usize, u8)>,
}

impl Packer {
    pub fn push(&mut self, bap: u8, code: u32) {
        match bap {
            0 => {}
            1 => Self::grouped(&mut self.items, &mut self.open1, code, 3, 3, 5),
            2 => Self::grouped(&mut self.items, &mut self.open2, code, 5, 3, 7),
            4 => Self::grouped(&mut self.items, &mut self.open4, code, 11, 2, 7),
            _ => self.items.push((code, plain_bits(bap) as u32)),
        }
    }

    /// A group of `n` codes in base `levels`, its first code most
    /// significant (`v = 9a + 3b + c` for bap 1, `25a + 5b + c` for bap 2,
    /// `11a + b` for bap 4). A fresh group starts with every member at the
    /// zero level, `(levels − 1)/2`.
    fn grouped(
        items: &mut Vec<(u32, u32)>,
        open: &mut Option<(usize, u8)>,
        code: u32,
        levels: u32,
        n: u8,
        bits: u32,
    ) {
        let zero = (levels - 1) / 2;
        let (idx, filled) = match open.take() {
            Some(o) => o,
            None => {
                let v = (0..n).fold(0, |acc, _| acc * levels + zero);
                items.push((v, bits));
                (items.len() - 1, 0)
            }
        };
        let weight = levels.pow(u32::from(n - 1 - filled));
        items[idx].0 = items[idx].0 - zero * weight + code * weight;
        if filled + 1 < n {
            *open = Some((idx, filled + 1));
        }
    }

    pub fn bits(&self) -> usize {
        self.items.iter().map(|&(_, b)| b as usize).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::{SYM_QUANT_3, SYM_QUANT_5, SYM_QUANT_7, SYM_QUANT_11, SYM_QUANT_15};

    #[test]
    fn symmetric_levels_are_the_spec_tables() {
        let tabs: [&[f32]; 5] = [
            &SYM_QUANT_3,
            &SYM_QUANT_5,
            &SYM_QUANT_7,
            &SYM_QUANT_11,
            &SYM_QUANT_15,
        ];
        for (b, t) in tabs.iter().enumerate() {
            let bap = b as u8 + 1;
            for (i, v) in t.iter().enumerate() {
                assert!((dequantize(i as u32, bap) - v).abs() < 1e-7);
                assert_eq!(quantize(*v, bap), i as u32, "bap {bap} level {i}");
            }
        }
    }

    #[test]
    fn quantisers_pick_the_nearest_level_and_saturate() {
        for bap in 1..=15u8 {
            let mut worst = 0.0f32;
            for i in 0..=2000 {
                let m = -1.0 + i as f32 / 1000.0 * 0.9999;
                let d = dequantize(quantize(m, bap), bap);
                worst = worst.max((d - m).abs());
            }
            // half a step: 1/L for symmetric, 2^-(bits) for asymmetric, plus
            // the top level's distance to +1 for the asymmetric saturation.
            let step = match bap {
                1..=5 => 1.0 / SYM_LEVELS[bap as usize] as f32 + 1e-6,
                _ => 2.0 / (1u32 << ASYM_MANT_BITS[(bap - 6) as usize]) as f32 + 1e-6,
            };
            assert!(worst <= step, "bap {bap}: {worst} > {step}");
        }
        assert_eq!(dequantize(quantize(5.0, 15), 15), 32767.0 / 32768.0);
        assert_eq!(dequantize(quantize(-5.0, 6), 6), -1.0);
    }

    #[test]
    fn grouping_matches_the_section_7_3_5_unpacking() {
        let mut p = Packer::default();
        // bap-1 codes 2,0,1 → 9·2 + 3·0 + 1 = 19; a lone bap-4 code 7 → 11·7 + 5 = 82
        p.push(1, 2);
        p.push(4, 7);
        p.push(3, 5);
        p.push(1, 0);
        p.push(1, 1);
        p.push(1, 2); // starts a second triple: 2·9 + 1·3 + 1 = 22
        assert_eq!(p.items, vec![(19, 5), (82, 7), (5, 3), (22, 5)]);
        let mut t = Tally::default();
        t.add(&[1, 4, 3, 1, 1, 1]);
        assert_eq!(t.bits(), p.bits());
    }
}
