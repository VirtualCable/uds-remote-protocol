#!/usr/bin/env bash
# Regenerates the committed libvpx bindings in crates/vpx-sys/src/
# (requires bindgen CLI + libclang; Linux dev box).
# Not needed for normal builds: the bindings are committed.
#
# Usage:
#   scripts/regen-vpx-bindings.sh                 # Unix bindings (system headers)
#   scripts/regen-vpx-bindings.sh --windows DIR   # Windows bindings (vcpkg include dir)
#
# DIR must contain a `vpx/` subdirectory with the vcpkg-installed headers,
# e.g. scp -r win-host:C:/dev/vcpkg/installed/x64-windows-static-md/include/vpx DIR/vpx
#
# Windows note: bindgen 0.73 emits the VPX_* macro constants only when the
# var allowlist runs on its own, so the Windows file is generated in two
# passes (types/functions + vars) and concatenated.
set -euo pipefail
cd "$(dirname "$0")/.."

cat > /tmp/vpx_wrapper.h <<'EOF'
#include <vpx/vpx_codec.h>
#include <vpx/vpx_image.h>
#include <vpx/vpx_encoder.h>
#include <vpx/vpx_decoder.h>
#include <vpx/vp8cx.h>
#include <vpx/vp8dx.h>
#include <vpx/vp8.h>
EOF

if [[ "${1:-}" == "--windows" ]]; then
    INC_DIR="${2:?--windows requires an include dir containing vpx/}"
    OUT=crates/vpx-sys/src/bindings_windows.rs
    bindgen /tmp/vpx_wrapper.h -o /tmp/vpx_win_main.rs \
      --allowlist-function 'vpx_.*' \
      --allowlist-type 'vpx_.*|vp8e_.*|vp9e_.*|vp8_.*|vp9_.*' \
      --allowlist-var 'VPX_.*|VP8E_.*|VP9E_.*' \
      --no-layout-tests \
      --generate-inline-functions false \
      -- -I"$INC_DIR"
    bindgen /tmp/vpx_wrapper.h -o /tmp/vpx_win_vars.rs \
      --allowlist-var 'VPX_.*|VP8E_.*|VP9E_.*' \
      --no-layout-tests \
      -- -I"$INC_DIR"
    cat /tmp/vpx_win_main.rs /tmp/vpx_win_vars.rs > "$OUT"
    echo "windows bindings regenerated: $OUT ($(wc -l < "$OUT") lines)"
    grep -n 'VPX_ENCODER_ABI_VERSION' "$OUT" | head -1
else
    OUT=crates/vpx-sys/src/bindings_unix.rs
    bindgen /tmp/vpx_wrapper.h -o "$OUT" \
      --allowlist-function 'vpx_.*' \
      --allowlist-type 'vpx_.*|vp8e_.*|vp9e_.*|vp8_.*|vp9_.*' \
      --allowlist-var 'VPX_.*|VP8E_.*|VP9E_.*' \
      --no-layout-tests \
      --generate-inline-functions false
    echo "unix bindings regenerated: $OUT ($(wc -l < "$OUT") lines)"
    echo "libvpx headers version: $(pkg-config --modversion vpx)"
fi
