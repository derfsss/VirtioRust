#!/bin/bash
# Build the C virtioscsi.device suite with the start-up handshake: an LF copy
# of the C driver's tests/test_virtioscsi.c (its checkout is only read) with
# bench/test_virtioscsi-handshake.patch applied. Its child processes get the
# dos.doc _start prototype and wait for CTRL-F, which the parent sends after
# CreateNewProcTags() returns. Without this a child that exits before DOS
# has linked it into its process list crashes DOS (a DSI in Remove()).
# Set A, B and O to the C driver checkout, this folder and the output folder.
set -e
A="${A:?set A to a checkout of derfsss/VirtualSCSIDevice}"
B="${B:-$(cd "$(dirname "$0")" && pwd)}"
O="${O:-/tmp/vscsi-ctests-fixed}"
rm -rf "$O"
mkdir -p "$O"
tr -d '\r' < "$A/tests/test_virtioscsi.c" > "$O/test_virtioscsi.c"
(cd "$O" && patch -p0 -s test_virtioscsi.c < "$B/test_virtioscsi-handshake.patch")
n=$(grep -c "start-up handshake" "$O/test_virtioscsi.c")
[ "$n" -ge 7 ] || { echo "patch did not apply: $n handshake lines"; exit 1; }
docker run --rm -v "$A:/src:ro" -v "$O:/out" -w /src walkero/amigagccondocker:os4-gcc15-3.6.1 sh -c \
  'ppc-amigaos-gcc -O2 -Wall -I./include -fno-tree-loop-distribute-patterns /out/test_virtioscsi.c -o /out/test_virtioscsi -lauto'
ls -la "$O/test_virtioscsi"
