#!/usr/bin/env bash
# Regenerate the static Latin faces the app registers, from the variable
# sources beside them. A HUMAN runs this and commits the result: nothing in
# the build, a build.rs or a gate may call it, and fontTools is NOT a
# dependency of this repo — install it only to regenerate (Debian/Ubuntu:
# `sudo apt install python3-fonttools`).
#
# Static faces, not the variable source, because the text system discards the
# requested weight once it has matched a face: `gpui-pre-wgpu`'s
# `cosmic_text_system.rs` shapes with the matched face's own usWeightClass and
# rasterizes from a Font built at `Weight::NORMAL` with no variation settings.
# One variable face therefore draws every weight at 400. A static face has no
# axes and is immune to both.
#
# THE TWO RIBBI WEIGHTS ONLY. A face's usWeightClass becomes the weight every
# FALLBACK lookup runs at, and no system face declares 500 or 600: registering
# those cost one uncached line of Korean 5.0ms (500) and 6.0ms (600) against
# 0.2ms at 400/700. See app/src/tests/font_fallback.rs.
set -euo pipefail
cd "$(dirname "$0")/.."
dir=crates/views/support/design/assets/fonts
for family in Geist GeistMono; do
  for instance in 400:Regular 700:Bold; do
    weight=${instance%%:*}
    style=${instance##*:}
    python3 -m fontTools.varLib.instancer \
      --output "$dir/$family-$style.ttf" \
      "$dir/$family[wght].ttf" \
      "wght=$weight" \
      --update-name-table
  done
done
