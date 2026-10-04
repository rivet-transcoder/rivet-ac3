//! Transient detection, A/52:2018 §8.2.2 (PDF p.110): high-pass filter the
//! block's 256 new samples (a cascade of two direct-form-I biquads, 8 kHz),
//! take segment peaks over a three-level tree (256, 2 × 128, 4 × 64), and
//! flag a transient when a segment's peak times the level's threshold
//! (0.1, 0.075, 0.05) exceeds the previous segment's peak — the segment
//! before the first being the last one of the previous tree. A block whose
//! overall peak is below 100/32768 is never flagged.

/// A direct-form-I biquad.
#[derive(Clone, Copy, Default)]
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    x: [f32; 2],
    y: [f32; 2],
}

impl Biquad {
    /// Second-order Butterworth-section high-pass at `fc` with quality `q`,
    /// by the bilinear transform.
    fn highpass(fs: f32, fc: f32, q: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * fc / fs;
        let (s, c) = w0.sin_cos();
        let alpha = s / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b: [(1.0 + c) / 2.0 / a0, -(1.0 + c) / a0, (1.0 + c) / 2.0 / a0],
            a: [-2.0 * c / a0, (1.0 - alpha) / a0],
            ..Self::default()
        }
    }

    fn run(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1];
        self.x = [x, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }
}

const THRESHOLD: [f32; 3] = [0.1, 0.075, 0.05];
const SILENCE: f32 = 100.0 / 32768.0;

#[derive(Clone)]
pub(super) struct Detector {
    hp: [Biquad; 2],
    /// `P[j][0]`: the last segment's peak of each level of the previous tree.
    /// Infinite before the first block: the stream's start is not a
    /// transient (there is nothing before it to pre-echo into).
    last: [f32; 3],
}

impl Detector {
    pub fn new(sample_rate: u32) -> Self {
        let fs = sample_rate as f32;
        // Fourth-order Butterworth: section Qs 1/(2cos(π/8)), 1/(2cos(3π/8)).
        Self {
            hp: [
                Biquad::highpass(fs, 8000.0, 0.541_196_1),
                Biquad::highpass(fs, 8000.0, 1.306_563),
            ],
            last: [f32::MAX; 3],
        }
    }

    /// Feed a block's 256 new samples; true if they hold a transient.
    pub fn detect(&mut self, block: &[f32]) -> bool {
        debug_assert_eq!(block.len(), 256);
        let mut x = [0.0f32; 256];
        for (o, &v) in x.iter_mut().zip(block) {
            let a = self.hp[0].run(v);
            *o = self.hp[1].run(a);
        }
        let mut transient = false;
        let mut overall = 0.0f32;
        for (j, &t) in THRESHOLD.iter().enumerate() {
            let segs = 1usize << j;
            let len = 256 >> j;
            let mut prev = self.last[j];
            for k in 0..segs {
                let p = x[k * len..(k + 1) * len]
                    .iter()
                    .fold(0.0f32, |m, v| m.max(v.abs()));
                if j == 0 {
                    overall = p;
                }
                if p * t > prev {
                    transient = true;
                }
                prev = p;
            }
            self.last[j] = prev;
        }
        transient && overall >= SILENCE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_click_after_quiet_is_a_transient_a_steady_tone_is_not() {
        let mut d = Detector::new(48_000);
        let tone: Vec<f32> = (0..256 * 20)
            .map(|n| 0.5 * (n as f32 * 0.37).sin())
            .collect();
        let mut flagged = 0;
        for b in 1..20 {
            flagged += usize::from(d.detect(&tone[b * 256..(b + 1) * 256]));
        }
        assert!(flagged <= 1, "steady tone flagged {flagged} times");
        let mut d = Detector::new(48_000);
        let mut blk = [0.0f32; 256];
        for _ in 0..4 {
            assert!(!d.detect(&blk));
        }
        blk[200] = 0.9;
        blk[201] = -0.8;
        assert!(d.detect(&blk));
        // quiet clicks below the silence threshold are ignored
        let mut d = Detector::new(48_000);
        let mut blk = [0.0f32; 256];
        assert!(!d.detect(&blk));
        blk[100] = 50.0 / 32768.0;
        assert!(!d.detect(&blk));
    }
}
