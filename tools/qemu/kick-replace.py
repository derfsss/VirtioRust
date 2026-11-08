#!/usr/bin/env python3
"""Make a copy of a kickstart zip with one module replaced.

    python -I tools/qemu/kick-replace.py SRC.zip DST.zip ENTRY FILE

Every entry of SRC.zip goes to DST.zip unchanged (same name, date and
compression), except ENTRY (e.g. Kickstart/virtioscsi.device), whose data
is FILE's. SRC.zip is only read; DST.zip must not be SRC.zip. Then run
tools/qemu/zip-comment.py on DST.zip (bboot's length rule).
"""
import os
import sys
import zipfile


def main():
    src, dst, entry, data_file = sys.argv[1:5]
    if os.path.abspath(src) == os.path.abspath(dst):
        print("refusing: DST is SRC")
        return 2
    data = open(data_file, "rb").read()
    found = False
    with zipfile.ZipFile(src) as zin, zipfile.ZipFile(dst, "w") as zout:
        for info in zin.infolist():
            name = info.filename.replace("\\", "/")
            body = zin.read(info)
            if name.lower() == entry.lower():
                found = True
                body = data
            out = zipfile.ZipInfo(info.filename, info.date_time)
            out.compress_type = info.compress_type
            out.external_attr = info.external_attr
            out.create_system = info.create_system
            zout.writestr(out, body)
    if not found:
        os.remove(dst)
        print("refusing: %s is not in %s" % (entry, src))
        return 1
    print("%s: %s replaced with %s (%d bytes)" % (dst, entry, data_file, len(data)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
