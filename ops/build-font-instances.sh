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
# THE TWO RIBBI WEIGHTS. `--update-name-table` keeps family "Geist" for 400
# and 700 and moves any other weight into the family name ("Geist Medium"),
# which the app would then have to ask for by that name; a request at 500 or
# 600 lands on the nearer of these two instead.
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
