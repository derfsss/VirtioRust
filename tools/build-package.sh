#!/bin/bash
# Builds one driver or test program package of this repository with the
# "amigaos" toolchain, then links it with the package's Makefile inside the
# cross-compiler image.
#
#   tools/build-package.sh drivers/virtio-scsi
#   tools/build-package.sh drivers/virtio-rng --features force-legacy
#   MAKE_ARGS=DISK=1 tools/build-package.sh drivers/virtio-rng --features disk
#
# Run on Linux (WSL works) after amigaos4/tools/build-toolchain.sh. The
# package must live inside this repository, which is mounted into the
# container (the amigaos4 submodule with it).
set -euo pipefail

PKG="${1:?usage: tools/build-package.sh <package dir> [cargo build arguments]}"
shift
IMAGE="${AMIGA_GCC_IMAGE:-walkero/amigagccondocker:os4-gcc15-3.6.1}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

docker image inspect "$IMAGE" >/dev/null 2>&1 || {
    echo "FAIL: docker cannot see $IMAGE (docker pull $IMAGE)"; exit 1; }

cd "$REPO/$PKG"
# The linked toolchain has no cargo of its own; rustup falls back to the
# default toolchain's cargo, which drives the amigaos rustc.
cargo +amigaos build --release "$@"
LIB="$(ls target/powerpc-unknown-amigaos/release/*.a)"
echo "staticlib: $LIB ($(stat -c %s "$LIB") bytes)"

# MAKE_ARGS passes make variables through, e.g. MAKE_ARGS="DISK=1".
docker run --rm -e MAKE_ARGS="${MAKE_ARGS:-}" -v "$REPO:/repo" -w "/repo/$PKG" "$IMAGE"     sh -c 'make clean $MAKE_ARGS >/dev/null && make $MAKE_ARGS'
