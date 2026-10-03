"""Deterministic test signals for the AC-3 cross-check vectors, as 16-bit WAV.

    python ac3_signals.py <name> <out.wav> [seconds]

Every signal is computed from closed-form tones and a fixed-seed
pseudo-random generator, so the vectors `make_vectors.sh` encodes from them
are the same on every machine. Channels are in WAVE order (FL FR FC LFE SL
SR for 5.1; FL FR BL BR for quad), which is what aften reads by default.

Names: see SIGNALS below. `tones51` is the per-channel identity signal
(FL 400 Hz, FR 600, FC 800, LFE 50, SL 1000, SR 1200, each at 0.25).
"""
import math
import random
import struct
import sys
import wave

TAU = 2 * math.pi


def tones(*parts):
    """A sum of (amplitude, Hz) sines."""
    return lambda t, i: sum(a * math.sin(TAU * f * t) for a, f in parts)


def clicks(t, i):
    """A 500 Hz tone with a 4 kHz burst for the last 10 ms of every 250 ms:
    transients that make an encoder switch to short blocks."""
    burst = 0.8 * math.sin(TAU * 4000 * t) if (t % 0.25) > 0.24 else 0.0
    return 0.3 * math.sin(TAU * 500 * t) + burst


class Noise:
    """White, pink (Paul Kellet's three-pole approximation of -3 dB/oct) or
    brown (leaky integrated) noise from a seeded generator."""

    def __init__(self, colour, amplitude, seed):
        self.colour, self.amp, self.rng = colour, amplitude, random.Random(seed)
        self.b = [0.0, 0.0, 0.0]
        self.last = None

    def __call__(self, t, i):
        w = self.rng.uniform(-1.0, 1.0)
        if self.colour == "white":
            return self.amp * w
        if self.colour == "pink":
            b = self.b
            b[0] = 0.99765 * b[0] + w * 0.0990460
            b[1] = 0.96300 * b[1] + w * 0.2965164
            b[2] = 0.57000 * b[2] + w * 1.0526913
            return self.amp * 0.3 * (b[0] + b[1] + b[2] + w * 0.1848)
        self.b[0] = 0.98 * self.b[0] + 0.1 * w
        return self.amp * 2.0 * self.b[0]


STEREO = tones((0.4, 440), (0.2, 3000), (0.1, 9000), (0.05, 15000))
SIX = [
    tones((0.4, 440)),
    tones((0.3, 880), (0.1, 7000)),
    tones((0.3, 1320)),
    tones((0.4, 60)),
    tones((0.2, 5000), (0.1, 12000)),
    tones((0.2, 6500), (0.1, 14000)),
]


def sig_5k(t, i):
    return 0.4 * math.sin(TAU * 440 * t) + 0.2 * math.sin(TAU * 5000 * t)


# name -> (sample rate, list of per-channel generators, built per call)
SIGNALS = {
    "mono_clicks": lambda: (48000, [clicks]),
    "stereo_tones": lambda: (48000, [STEREO, STEREO]),
    "stereo_pink": lambda: (48000, [Noise("pink", 0.3, 7), Noise("pink", 0.3, 8)]),
    "stereo_clicks": lambda: (48000, [clicks, tones((0.3, 700))]),
    "surround_tones": lambda: (48000, SIX),
    "surround_white": lambda: (48000, [Noise("white", 0.25, 3 + c) for c in range(6)]),
    "quad_brown": lambda: (48000, [Noise("brown", 0.3, 11 + c) for c in range(4)]),
    "three_tones": lambda: (48000, SIX[:3]),
    "stereo_44k": lambda: (44100, [sig_5k, sig_5k]),
    "stereo_32k": lambda: (32000, [sig_5k, sig_5k]),
    "tones51": lambda: (48000, [tones((0.25, f)) for f in (400, 600, 800, 50, 1000, 1200)]),
    # The committed fixture: tones, noise and a transient in every channel,
    # at half scale so neither the decode nor its 16-bit reference clips.
    "fixture51": lambda: (48000, [
        (lambda g, n: (lambda t, i: 0.5 * (g(t, i) + n(t, i) + 0.5 * clicks(t, i))))(SIX[c], Noise("pink", 0.1, 21 + c))
        for c in range(6)
    ]),
}


def main():
    name, out = sys.argv[1], sys.argv[2]
    seconds = float(sys.argv[3]) if len(sys.argv) > 3 else 3.0
    rate, gens = SIGNALS[name]()
    n = round(rate * seconds)
    frames = bytearray()
    for i in range(n):
        t = i / rate
        for g in gens:
            v = max(-1.0, min(1.0, g(t, i)))
            frames += struct.pack("<h", round(v * 32767))
    with wave.open(out, "wb") as w:
        w.setnchannels(len(gens))
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes(bytes(frames))


if __name__ == "__main__":
    main()
