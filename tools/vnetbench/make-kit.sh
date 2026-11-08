#!/bin/bash
# make-kit.sh: build the virtionet.device beta tester kit.
#   tools/vnetbench/make-kit.sh            (run under Linux/WSL, like build-package.sh)
# Builds virtionet.device, s2test and netprof for AmigaOS, takes
# netprof-host.exe from tools/netprof-host/target/release (build it on
# Windows first: cargo build --release), and packs dist/virtionet-beta.lha
# (dist/ is git-ignored). The kit's own files are in tools/vnetbench/kit/.
set -euo pipefail
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
IMAGE="${AMIGA_GCC_IMAGE:-walkero/amigagccondocker:os4-gcc15-3.6.1}"
cd "$REPO"
for p in drivers/virtio-net testprogs/s2test testprogs/netprof; do
  tools/build-package.sh "$p" >/dev/null
done
HOSTEXE=tools/netprof-host/target/release/netprof-host.exe
[ -f "$HOSTEXE" ] || { echo "FAIL: $HOSTEXE missing (cargo build --release in tools/netprof-host on Windows)"; exit 1; }
K=dist/virtionet-beta
rm -rf dist/virtionet-beta dist/virtionet-beta.lha
mkdir -p "$K"
cp drivers/virtio-net/virtionet.device testprogs/s2test/s2test testprogs/netprof/netprof "$K/"
cp "$HOSTEXE" "$K/"
cp tools/vnetbench/kit/ReadMe tools/vnetbench/kit/vnetcheck tools/vnetbench/kit/VIRTIONET "$K/"
( cd "$K" && sha256sum virtionet.device s2test netprof netprof-host.exe > SHA256 )
docker run --rm -v "$REPO/dist:/dist" -w /dist "$IMAGE" sh -c 'lha ao5q virtionet-beta.lha virtionet-beta'
echo "kit: dist/virtionet-beta.lha ($(stat -c %s dist/virtionet-beta.lha) bytes)"
cat "$K/SHA256"
