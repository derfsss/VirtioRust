#!/bin/bash
# make-release.sh: build the release archives in dist/release/ (git-ignored):
#   virtionet-<ver>.lha    virtionet.device with the tester kit
#                          (tools/vnetbench/kit: ReadMe, vnetcheck, VIRTIONET),
#                          s2test, netprof and netprof-host.exe
#   virtiorng-<ver>.lha    the Kickstart and disk builds, rngread
#                          (release/virtiorng/ReadMe)
#   virtioscsi-<ver>.lha   the Kickstart drop-in (release/virtioscsi/ReadMe)
# Run under Linux/WSL like build-package.sh. netprof-host.exe comes from
# tools/netprof-host/target/release: build it on Windows first
# (cargo build --release). Each archive gets a SHA256 file.
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="${AMIGA_GCC_IMAGE:-walkero/amigagccondocker:os4-gcc15-3.6.1}"
cd "$REPO"
B=tools/build-package.sh
HOSTEXE=tools/netprof-host/target/release/netprof-host.exe
[ -f "$HOSTEXE" ] || { echo "FAIL: $HOSTEXE missing (cargo build --release in tools/netprof-host on Windows)"; exit 1; }
O=dist/release
rm -rf "$O"
mkdir -p "$O/stage"
S="$O/stage"

# The version a binary's $VER string gives, e.g. "1.1".
ver() { grep -a -o "$1 [0-9]*\.[0-9]*" "$2" | head -1 | cut -d' ' -f2; }

# Build each variant and keep its output at once (two builds of one
# package share its target folder).
bash $B drivers/virtio-net >/dev/null;               cp drivers/virtio-net/virtionet.device "$S/net.device"
bash $B drivers/virtio-rng >/dev/null;               cp drivers/virtio-rng/virtiorng.device "$S/rng-kick.device"
MAKE_ARGS=DISK=1 bash $B drivers/virtio-rng --features disk >/dev/null
                                                     cp drivers/virtio-rng/disk/virtiorng.device "$S/rng-disk.device"
MAKE_ARGS=DROPIN=1 bash $B drivers/virtio-scsi --features dropin >/dev/null
                                                     cp drivers/virtio-scsi/virtioscsi.device "$S/scsi.device"
for p in testprogs/s2test testprogs/netprof testprogs/rngread; do bash $B "$p" >/dev/null; done

NV=$(ver virtionet.device "$S/net.device")
RK=$(ver virtiorng.device "$S/rng-kick.device")
RD=$(ver virtiorng.device "$S/rng-disk.device")
SV=$(ver virtioscsi.device "$S/scsi.device")
[ -n "$NV" ] && [ -n "$RK" ] && [ -n "$RD" ] && [ -n "$SV" ] || { echo "FAIL: a version string was not found ($NV $RK $RD $SV)"; exit 1; }

D="$O/virtionet-$NV"
mkdir -p "$D"
cp "$S/net.device" "$D/virtionet.device"
cp testprogs/s2test/s2test testprogs/netprof/netprof "$HOSTEXE" "$D/"
cp tools/vnetbench/kit/ReadMe tools/vnetbench/kit/vnetcheck tools/vnetbench/kit/VIRTIONET "$D/"
( cd "$D" && sha256sum virtionet.device s2test netprof netprof-host.exe > SHA256 )

D="$O/virtiorng-$RD"
mkdir -p "$D/Kickstart" "$D/DEVS"
cp "$S/rng-kick.device" "$D/Kickstart/virtiorng.device"
cp "$S/rng-disk.device" "$D/DEVS/virtiorng.device"
cp testprogs/rngread/rngread release/virtiorng/ReadMe "$D/"
( cd "$D" && sha256sum Kickstart/virtiorng.device DEVS/virtiorng.device rngread > SHA256 )

D="$O/virtioscsi-$SV"
mkdir -p "$D/Kickstart"
cp "$S/scsi.device" "$D/Kickstart/virtioscsi.device"
cp release/virtioscsi/ReadMe "$D/"
( cd "$D" && sha256sum Kickstart/virtioscsi.device > SHA256 )

rm -rf "$S"
for d in "virtionet-$NV" "virtiorng-$RD" "virtioscsi-$SV"; do
  docker run --rm -v "$REPO/$O:/o" -w /o "$IMAGE" sh -c "lha ao5q $d.lha $d"
done
( cd "$O" && sha256sum ./*.lha > SHA256SUMS && ls -l ./*.lha && cat SHA256SUMS )
echo "versions: virtionet $NV, virtiorng $RK (Kickstart) / $RD (disk), virtioscsi $SV"
