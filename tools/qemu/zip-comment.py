"""Give a kickstart zip a comment, so the file does not end in zero bytes.

bboot finds where the zip ends from the last non-zero byte and rounds that
up to 128 bytes. A zip without a comment ends with the end-of-central-
directory record's comment-length field, 00 00, so when the file size sits
just past a 128-byte boundary the rounding cuts the record short and bboot
prints "Invalid zip file". A non-empty comment ending in a non-zero byte
keeps the whole record inside bboot's length.

    python -I tools/qemu/zip-comment.py <kickstart.zip> [...]
"""
import sys
import zipfile

COMMENT = b"kickstart for QEMU (VirtioRust)"

for path in sys.argv[1:]:
    with zipfile.ZipFile(path, "a") as z:
        z.comment = COMMENT
    with open(path, "rb") as f:
        tail = f.read()[-4:]
    print(path, "ends with", tail, "-> ok" if tail[-1] != 0 else "-> STILL ZERO")
