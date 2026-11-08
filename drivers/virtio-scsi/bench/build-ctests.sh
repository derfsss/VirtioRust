#!/bin/bash
# Build the C virtioscsi.device's own test programs (from a read-only
# checkout of derfsss/VirtualSCSIDevice) for a parity run against the Rust
# drop-in. Set A to that checkout and O to the output folder.
set -e
A="${A:?set A to a checkout of derfsss/VirtualSCSIDevice}"
O="${O:-/tmp/vscsi-ctests}"
docker run --rm -v "$A:/src:ro" -v "$O:/out" -w /src walkero/amigagccondocker:os4-gcc15-3.6.1 sh -c '
  for t in test_inquiry test_virtioscsi; do
    ppc-amigaos-gcc -O2 -I./include -fno-tree-loop-distribute-patterns tests/$t.c -o /out/$t -lauto || exit 1
  done'
ls -la "$O"
