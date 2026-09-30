#!/usr/bin/env bash
# Regenerates crates/vpx-sys/src/bindings.rs against the libvpx headers
# installed on this machine (requires bindgen CLI + libclang; Linux dev box).
# Not needed for normal builds: the bindings are committed.
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

bindgen /tmp/vpx_wrapper.h -o crates/vpx-sys/src/bindings.rs \
  --allowlist-function 'vpx_.*' \
  --allowlist-type 'vpx_.*|vp8e_.*|vp9e_.*|vp8_.*|vp9_.*' \
  --allowlist-var 'VPX_.*|VP8E_.*|VP9E_.*' \
  --no-layout-tests \
  --generate-inline-functions false

echo "bindings regenerated: crates/vpx-sys/src/bindings.rs ($(wc -l < crates/vpx-sys/src/bindings.rs) lines)"
echo "libvpx headers version: $(pkg-config --modversion vpx)"
