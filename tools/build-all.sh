#!/bin/bash
# Build every driver and test program in this repository (Linux or WSL,
# after amigaos4/tools/build-toolchain.sh), each variant the benches use.
# Stops at the first failure.
set -euo pipefail
cd "$(dirname "$0")/.."
B=tools/build-package.sh
bash $B drivers/virtio-net
bash $B drivers/virtio-rng
MAKE_ARGS=DISK=1 bash $B drivers/virtio-rng --features disk
bash $B drivers/virtio-scsi
MAKE_ARGS=DROPIN=1 bash $B drivers/virtio-scsi --features dropin
bash $B testprogs/s2test
bash $B testprogs/rngread
bash $B testprogs/vscsitest
bash $B testprogs/netprof
ls -la drivers/virtio-net/virtionet.device drivers/virtio-rng/virtiorng.device \
       drivers/virtio-rng/disk/virtiorng.device drivers/virtio-scsi/virtioscsi.device \
       testprogs/s2test/s2test testprogs/rngread/rngread testprogs/vscsitest/vscsitest \
       testprogs/netprof/netprof
