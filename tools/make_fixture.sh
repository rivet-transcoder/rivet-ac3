#!/bin/bash
# Make the committed fixture tests/data/aften_51_448k.ac3 and its reference
# tests/data/aften_51_448k.liba52.s16le: 250 ms of 5.1 (a tone pair, pink
# noise and a transient burst in every channel; ac3_signals.py `fixture51`)
# encoded by aften at 448 kbit/s with block switching and a `dynrng` profile,
# and liba52's decode of it (GStreamer's a52dec, dynrng applied) as 16-bit
# interleaved PCM in WAVE order. Both tools are black boxes; no FFmpeg.
#
#   bash tools/make_fixture.sh        # needs aften and gstreamer1.0-plugins-ugly
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DATA="$HERE/../tests/data"
PY="${PYTHON:-python3}"
TMP="$(mktemp -d)"
"$PY" "$HERE/ac3_signals.py" fixture51 "$TMP/in.wav" 0.25
aften -v 0 -b 448 -chconfig 3/2+LFE -s 1 -dynrng 1 "$TMP/in.wav" "$DATA/aften_51_448k.ac3"
gst-launch-1.0 -q filesrc location="$DATA/aften_51_448k.ac3" ! ac3parse ! a52dec drc=true \
  ! audioconvert ! audio/x-raw,format=F32LE,layout=interleaved ! filesink location="$TMP/ref.f32"
"$PY" - "$TMP/ref.f32" "$DATA/aften_51_448k.liba52.s16le" <<'PY'
import struct, sys
raw = open(sys.argv[1], "rb").read()
vals = struct.unpack("<%df" % (len(raw) // 4), raw)
out = struct.pack("<%dh" % len(vals), *(max(-32768, min(32767, round(v * 32768))) for v in vals))
open(sys.argv[2], "wb").write(out)
PY
rm -r "$TMP"
ls -l "$DATA"/aften_51_448k.*
