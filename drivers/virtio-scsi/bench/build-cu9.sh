#!/bin/bash
set -e
O="${O:-/tmp/vscsi-ctests}"
docker run --rm -v "$O:/out" -w /out walkero/amigagccondocker:os4-gcc15-3.6.1 sh -c 'ppc-amigaos-gcc -O2 -Wall cu9.c -o cu9 -lauto'
ls -la $O/cu9
