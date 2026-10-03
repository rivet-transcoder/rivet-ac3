#!/bin/bash
# Fetch Dolby's own encodes from the Dolby Digital Plus Online Delivery Kit
# (ott.dolby.com, published by Dolby for testing products that carry these
# formats) into $OUT/dolby, and decode the Dolby-encoded AC-3 stream with
# liba52 (GStreamer's a52dec, a black box) for tests/ac3_decode_vectors.rs:
#
# - ChID_voices_6ch_640kbps_dd.ac3: AC-3 5.1, `dynrng` in most blocks, block
#   switching — checked against liba52 like the aften vectors.
# - ChID_voices_6ch_256kbps_ddp.ec3 (and the French and stereo versions, and
#   the Atmos / silent / A/V-sync streams): E-AC-3 with spectral extension,
#   AHT (VQ, GAQ, large mantissas), coupling, rematrixing, `dynrng`. No
#   decoder other than FFmpeg's is available to compare these with, so the
#   5.1 one is checked against liba52's decode of the AC-3 encode of the same
#   programme above (same timing, same levels: band energies and waveform
#   SNR), and every stream must decode with good CRCs and no errors.
#
#   OUT=vectors [STRIP=target/release/examples/ac3_strip_dither] bash tools/fetch_dolby_kit.sh
set -euo pipefail
OUT="${OUT:-vectors}"
DIR="$OUT/dolby"
URL=https://ott.dolby.com/OnDelKits/DDP/Dolby_Digital_Plus_Online_Delivery_Kit_v1.4.1/Test_Signals/elementary_streams/audio.zip
SHA256=f94d5e3e933f756856686546763f42a8a5f16b10c264fc7af1d228acc09baa62
mkdir -p "$DIR"
if [ ! -f "$DIR/audio.zip" ]; then
  curl -fsSL --retry 3 -o "$DIR/audio.zip.part" "$URL"
  mv "$DIR/audio.zip.part" "$DIR/audio.zip"
fi
echo "$SHA256  $DIR/audio.zip" | sha256sum -c -
unzip -oqj "$DIR/audio.zip" 'audio/*.ac3' 'audio/*.ec3' -d "$DIR"
refs() {
  for tag in drc1:true drc0:false; do
    gst-launch-1.0 -q filesrc location="$DIR/$1.ac3" ! ac3parse \
      ! a52dec drc="${tag#*:}" ! audioconvert ! audio/x-raw,format=F32LE,layout=interleaved \
      ! filesink location="$DIR/$1.${tag%%:*}.f32"
  done
}
refs ChID_voices_6ch_640kbps_dd
# With STRIP (the built ac3_strip_dither example), a copy with the dither
# flags cleared, which the two decoders must agree on to float rounding.
if [ -n "${STRIP:-}" ]; then
  "$STRIP" "$DIR/ChID_voices_6ch_640kbps_dd.ac3" "$DIR/ChID_voices_6ch_640kbps_dd_nodith.ac3" >/dev/null
  refs ChID_voices_6ch_640kbps_dd_nodith
fi
ls "$DIR"
