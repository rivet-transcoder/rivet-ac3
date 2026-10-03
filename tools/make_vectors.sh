#!/bin/bash
# Make the AC-3 cross-check vectors for tests/ac3_decode_vectors.rs, with no
# FFmpeg anywhere: deterministic signals (ac3_signals.py) encoded by aften,
# an independent open-source AC-3 encoder, and each stream decoded by liba52
# (through GStreamer's a52dec element), an independent decoder, to f32le —
# `<name>.drc1.f32` with dynrng applied and `<name>.drc0.f32` without. Both
# are run as black boxes; no part of either's source was read.
#
# With STRIP set to the built `ac3_strip_dither` example, each stream also
# gets a `<name>_nodith` copy whose dither flags are cleared, so both decoders
# are deterministic and agree to float rounding. Two streams also get
# `blksw_<name>` copies with every channel's block 0 forced to the short
# transform (ac3_make_blksw_vector.py), besides aften's own block switching.
#
#   sudo apt-get install aften gstreamer1.0-tools gstreamer1.0-plugins-base \
#        gstreamer1.0-plugins-good gstreamer1.0-plugins-ugly
#   cargo build --release --example ac3_strip_dither
#   OUT=vectors STRIP=target/release/examples/ac3_strip_dither bash tools/make_vectors.sh
#   RIVET_AC3_VECTORS=vectors cargo test --release --test ac3_decode_vectors
#
# tools/fetch_dolby_kit.sh adds Dolby-encoded AC-3 and E-AC-3 streams to the
# same directory.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="${OUT:-vectors}"
PY="${PYTHON:-python3}"
DUR=3
mkdir -p "$OUT"

# liba52's decode of $1 (any A/52 stream) as interleaved f32le in WAVE order.
liba52() {
  local in=$1 out=$2 drc=$3
  gst-launch-1.0 -q filesrc location="$in" ! ac3parse ! a52dec drc="$drc" \
    ! audioconvert ! audio/x-raw,format=F32LE,layout=interleaved ! filesink location="$out"
}
refs() {
  liba52 "$OUT/$1.ac3" "$OUT/$1.drc1.f32" true
  liba52 "$OUT/$1.ac3" "$OUT/$1.drc0.f32" false
}

# name | signal | aften options
gen() {
  local name=$1 sig=$2; shift 2
  "$PY" "$HERE/ac3_signals.py" "$sig" "$OUT/$name.wav" "$DUR"
  aften -v 0 "$@" "$OUT/$name.wav" "$OUT/$name.ac3"
  rm "$OUT/$name.wav"
  refs "$name"
  if [ -n "${STRIP:-}" ]; then
    "$STRIP" "$OUT/$name.ac3" "$OUT/${name}_nodith.ac3" >/dev/null
    refs "${name}_nodith"
  fi
  echo "made $name"
}

# -s 1: block switching on transients. -dynrng 0..4: aften writes a dynrng
# word per block from one of its compression profiles (5, the default, is
# none), so the dynrng path is checked both applied and not.
gen mono_64k        mono_clicks    -b 64  -chconfig 1/0 -s 1
gen stereo_192k     stereo_tones   -b 192 -chconfig 2/0
gen stereo_noise    stereo_pink    -b 128 -chconfig 2/0 -dynrng 1
gen stereo_blksw    stereo_clicks  -b 160 -chconfig 2/0 -s 1 -m 0
gen surround_448k   surround_tones -b 448 -chconfig 3/2+LFE
gen surround_192k   surround_tones -b 192 -chconfig 3/2+LFE -dynrng 3
gen surround_noise  surround_white -b 384 -chconfig 3/2+LFE -s 1 -dynrng 4
gen quad_256k       quad_brown     -b 256 -chconfig 2/2
gen three_224k      three_tones    -b 224 -chconfig 3/0
gen stereo_44k      stereo_44k     -b 160 -chconfig 2/0
gen stereo_32k      stereo_32k     -b 96  -chconfig 2/0

for v in stereo_192k surround_448k; do
  "$PY" "$HERE/../tests/data/ac3_make_blksw_vector.py" "$OUT/$v.ac3" "$OUT/blksw_$v.ac3" >/dev/null
  refs "blksw_$v"
  if [ -n "${STRIP:-}" ]; then
    "$STRIP" "$OUT/blksw_$v.ac3" "$OUT/blksw_${v}_nodith.ac3" >/dev/null
    refs "blksw_${v}_nodith"
  fi
  echo "made blksw_$v"
done
ls "$OUT" | grep -c '\.ac3$' | sed 's/^/streams: /'
