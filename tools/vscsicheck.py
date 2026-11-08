#!/usr/bin/env python3
"""Check a raw test disk image after `vscsitest`, with the guest stopped.

    python -I tools/vscsicheck.py IMAGE UNIT BLOCKSIZE

vscsitest writes pattern(byte offset, unit) to fixed regions of every unit;
this reads the same regions straight from the image file and compares them,
which proves the data reached the disk (a read-back in the guest goes
through the same driver that wrote it). Raw images only.
Exit 0 when every region matches, 1 otherwise.
"""
import os
import sys


def pattern(abs_off, unit):
    return ((abs_off >> 9) & 0xFF) ^ ((abs_off & 0xFF) * 31 & 0xFF) ^ ((0x5A + unit) & 0xFF)


def main():
    if len(sys.argv) != 4:
        print(__doc__)
        return 2
    image, unit, bs = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    size = os.path.getsize(image)
    regions = [
        ("CMD_WRITE 64 KiB at 0", 0, 65536),
        ("CMD_WRITE 300 KiB at 1 MiB", 1 << 20, 300 * 1024),
        ("TD_FORMAT 4 blocks at 2 MiB", 2 << 20, 4 * bs),
        ("TD_WRITE64 the last block", size - bs, bs),
    ]
    if size >= 32 << 20:
        regions.append(("4 MiB at 16 MiB", 16 << 20, 4 << 20))
    bad = 0
    with open(image, "rb") as f:
        for name, off, length in regions:
            f.seek(off)
            data = f.read(length)
            diff = next((i for i, b in enumerate(data) if b != pattern(off + i, unit)), None)
            if len(data) != length:
                diff = len(data)
            ok = diff is None
            bad += not ok
            print("%s %s: %d bytes at %d%s" % ("PASS" if ok else "FAIL", name, length, off,
                                               "" if ok else ", first difference at +%d" % diff))
    print("vscsicheck: %d/%d regions match" % (len(regions) - bad, len(regions)))
    return 0 if bad == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
