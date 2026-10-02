//! The forward transform, A/52:2018 §8.2.3 (PDF p.111): window the 512
//! samples of a block with the Table 7.33 window used back to back, then
//!
//! ```text
//! XD[k] = -2/N · Σ_{n<N} x[n] · cos( 2π/(4N)·(2n+1)(2k+1) + π/4·(2k+1)(1+α) ),  k < N/2
//! ```
//!
//! with N = 512, α = 0 for the long transform, and N = 256 with α = −1 / +1
//! for the first / second of the two short transforms (on the first and
//! second 256 windowed samples). The short pair is interleaved the way
//! §7.9.4.2 reads it back: `X1[k]` in coefficient `2k`, `X2[k]` in `2k + 1`.
//!
//! The phase term is a shift of the time index by `s = N/4·(1+α)`, so each
//! transform is a type-IV DCT of a folded sequence, computed with an
//! N/8-point complex FFT; the test below checks it against the literal sum
//! and checks that the decoder's inverse transform (`imdct::imdct_block`)
//! reconstructs the input exactly through every long/short combination.

use std::f64::consts::PI;
use std::sync::OnceLock;

use crate::imdct::kbd_window;

#[derive(Clone, Copy, Default)]
struct C {
    re: f64,
    im: f64,
}

/// In-place iterative radix-2 forward DFT: `Z[k] = Σ z[n] exp(−j2πkn/len)`.
fn fft(buf: &mut [C], tw: &[C]) {
    let len = buf.len();
    let bits = len.trailing_zeros();
    for i in 0..len {
        let j = i.reverse_bits() >> (usize::BITS - bits);
        if j > i {
            buf.swap(i, j);
        }
    }
    // `tw` holds exp(−j2πi/len) for i < len/2.
    let mut size = 2;
    while size <= len {
        let stride = len / size;
        for start in (0..len).step_by(size) {
            for k in 0..size / 2 {
                let w = tw[k * stride];
                let a = buf[start + k];
                let b = buf[start + k + size / 2];
                let t = C { re: b.re * w.re - b.im * w.im, im: b.re * w.im + b.im * w.re };
                buf[start + k] = C { re: a.re + t.re, im: a.im + t.im };
                buf[start + k + size / 2] = C { re: a.re - t.re, im: a.im - t.im };
            }
        }
        size *= 2;
    }
}

/// Precomputed twiddles for one DCT-IV size `m` (128 or 64).
struct Plan {
    m: usize,
    /// FFT twiddles exp(−j2πi/(m/2)), i < m/4.
    fft: Vec<C>,
    /// Pre-twiddle exp(−jπn/m), n < m/2.
    pre: Vec<C>,
    /// Post-twiddle exp(−jπ(k+¼)/m), k < m/2.
    post: Vec<C>,
}

impl Plan {
    fn new(m: usize) -> Self {
        let h = m / 2;
        let e = |a: f64| C { re: a.cos(), im: -a.sin() };
        Self {
            m,
            fft: (0..h / 2).map(|i| e(2.0 * PI * i as f64 / h as f64)).collect(),
            pre: (0..h).map(|n| e(PI * n as f64 / m as f64)).collect(),
            post: (0..h).map(|k| e(PI * (k as f64 + 0.25) / m as f64)).collect(),
        }
    }

    /// `out[k] = scale · Σ_{n<m} v[n] cos(π/m (n+½)(k+½))`.
    fn dct4(&self, v: &[f64], scale: f64, out: &mut [f64]) {
        let m = self.m;
        let h = m / 2;
        let mut z = [C::default(); 128];
        let z = &mut z[..h];
        for n in 0..h {
            let c = C { re: v[2 * n], im: v[m - 1 - 2 * n] };
            let p = self.pre[n];
            z[n] = C { re: c.re * p.re - c.im * p.im, im: c.re * p.im + c.im * p.re };
        }
        fft(z, &self.fft);
        for k in 0..h {
            let p = self.post[k];
            let a = C { re: z[k].re * p.re - z[k].im * p.im, im: z[k].re * p.im + z[k].im * p.re };
            out[2 * k] = scale * a.re;
            out[m - 1 - 2 * k] = -scale * a.im;
        }
    }
}

fn plans() -> &'static (Plan, Plan) {
    static P: OnceLock<(Plan, Plan)> = OnceLock::new();
    P.get_or_init(|| (Plan::new(256), Plan::new(128)))
}

/// The full 512-point window: Table 7.33 rising, then mirrored.
pub(crate) fn window512() -> &'static [f64; 512] {
    static W: OnceLock<[f64; 512]> = OnceLock::new();
    W.get_or_init(|| {
        let w = kbd_window();
        let mut out = [0.0; 512];
        for n in 0..256 {
            out[n] = f64::from(w[n]);
            out[511 - n] = f64::from(w[n]);
        }
        out
    })
}

/// One §8.2.3.2 transform of `x` (length N, already windowed) with the time
/// shift `s = N/4·(1+α)`, into `out` (length N/2).
fn transform(x: &[f64], s: usize, plan: &Plan, out: &mut [f64]) {
    let n = x.len();
    // r[(i + s) mod N] = ±x[i], negated where the index wraps (cos is
    // antiperiodic over N in this argument).
    let mut r = [0.0f64; 512];
    for (i, &xi) in x.iter().enumerate() {
        let j = i + s;
        if j < n {
            r[j] = xi;
        } else {
            r[j - n] = -xi;
        }
    }
    // Fold to the N/2-point DCT-IV input: v[m] = r[m] − r[N−1−m].
    let half = n / 2;
    let mut v = [0.0f64; 256];
    for m in 0..half {
        v[m] = r[m] - r[n - 1 - m];
    }
    plan.dct4(&v[..half], -2.0 / n as f64, out);
}

/// Window the 512 input samples of one block (the previous block's 256 new
/// samples followed by this block's) and transform them: one long transform
/// or, with `blksw`, the interleaved short pair.
pub(crate) fn mdct_block(input: &[f32; 512], blksw: bool, coeffs: &mut [f32; 256]) {
    let w = window512();
    let mut x = [0.0f64; 512];
    for n in 0..512 {
        x[n] = f64::from(input[n]) * w[n];
    }
    let (long, short) = plans();
    if !blksw {
        let mut out = [0.0f64; 256];
        transform(&x, 128, long, &mut out);
        for k in 0..256 {
            coeffs[k] = out[k] as f32;
        }
    } else {
        let mut o1 = [0.0f64; 128];
        let mut o2 = [0.0f64; 128];
        transform(&x[..256], 0, short, &mut o1);
        transform(&x[256..], 128, short, &mut o2);
        for k in 0..128 {
            coeffs[2 * k] = o1[k] as f32;
            coeffs[2 * k + 1] = o2[k] as f32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imdct::imdct_block;

    /// §8.2.3.2 as printed: the O(N²) sum.
    fn reference(x: &[f64], alpha: f64) -> Vec<f64> {
        let n = x.len();
        let nf = n as f64;
        (0..n / 2)
            .map(|k| {
                let kf = k as f64;
                -2.0 / nf
                    * x.iter()
                        .enumerate()
                        .map(|(i, &xi)| {
                            let a = 2.0 * PI / (4.0 * nf) * (2.0 * i as f64 + 1.0) * (2.0 * kf + 1.0)
                                + PI / 4.0 * (2.0 * kf + 1.0) * (1.0 + alpha);
                            xi * a.cos()
                        })
                        .sum::<f64>()
            })
            .collect()
    }

    fn noise(len: usize, seed: u32) -> Vec<f64> {
        let mut s = seed;
        (0..len)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                f64::from(s as i32) / 2_147_483_648.0
            })
            .collect()
    }

    #[test]
    fn fast_transforms_match_the_section_8_2_3_2_sum() {
        let (long, short) = plans();
        let x = noise(512, 7);
        let mut out = [0.0f64; 256];
        transform(&x, 128, long, &mut out);
        let r = reference(&x, 0.0);
        let err = out.iter().zip(&r).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
        assert!(err < 1e-12, "long: {err}");
        for (alpha, s) in [(-1.0, 0usize), (1.0, 128)] {
            let x = noise(256, 11 + s as u32);
            let mut out = [0.0f64; 128];
            transform(&x, s, short, &mut out);
            let r = reference(&x, alpha);
            let err = out.iter().zip(&r).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
            assert!(err < 1e-12, "short α={alpha}: {err}");
        }
    }

    /// Forward here, inverse in the decoder (§7.9.4): every long/short
    /// sequence reconstructs the input, delayed by one block, to float
    /// precision — the transforms are each other's inverse, signs, scale
    /// and the short-block phase included.
    #[test]
    fn decoder_imdct_inverts_the_forward_transform_through_block_switches() {
        let blocks = 24;
        let sig: Vec<f32> = noise(256 * (blocks + 1), 3).iter().map(|v| *v as f32 * 0.5).collect();
        let pattern = [false, false, true, false, true, true, false, true, false, false, true, true];
        let mut delay = [0.0f32; 256];
        let mut prev = [0.0f32; 256];
        let mut out = Vec::new();
        for b in 0..blocks {
            let mut input = [0.0f32; 512];
            input[..256].copy_from_slice(&prev);
            input[256..].copy_from_slice(&sig[b * 256..(b + 1) * 256]);
            prev.copy_from_slice(&input[256..]);
            let mut c = [0.0f32; 256];
            let sw = pattern[b % pattern.len()];
            mdct_block(&input, sw, &mut c);
            let mut pcm = [0.0f32; 256];
            imdct_block(&c, sw, &mut delay, &mut pcm);
            out.extend_from_slice(&pcm);
        }
        // Output block b is input block b − 1.
        let err = (256..blocks * 256).map(|i| (out[i] - sig[i - 256]).abs()).fold(0.0f32, f32::max);
        assert!(err < 2e-6, "max reconstruction error {err}");
    }
}
