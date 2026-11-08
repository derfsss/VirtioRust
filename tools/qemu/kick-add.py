#!/usr/bin/env python3
"""Make a copy of a kickstart zip with one module added and listed.

    python -I tools/qemu/kick-add.py SRC.zip DST.zip ENTRY FILE [AFTER]

Every entry of SRC.zip goes to DST.zip unchanged, plus ENTRY (e.g.
Kickstart/vscsirs.device) with FILE's data, and Kickstart/Kicklayout gets a
"MODULE ENTRY" line after the line "MODULE AFTER" (default: after the last
MODULE line). SRC.zip is only read. Then run tools/qemu/zip-comment.py.
"""
import os
import sys
import zipfile


def main():
    src, dst, entry, data_file = sys.argv[1:5]
    after = sys.argv[5] if len(sys.argv) > 5 else None
    if os.path.abspath(src) == os.path.abspath(dst):
        print("refusing: DST is SRC")
        return 2
    data = open(data_file, "rb").read()
    with zipfile.ZipFile(src) as zin, zipfile.ZipFile(dst, "w") as zout:
        names = [i.filename.replace("\\", "/") for i in zin.infolist()]
        if entry in names:
            print("refusing: %s is already in %s (use kick-replace.py)" % (entry, src))
            return 1
        last = None
        for info in zin.infolist():
            body = zin.read(info)
            name = info.filename.replace("\\", "/")
            if name == "Kickstart/Kicklayout":
                lines = body.decode("latin-1").split("\n")
                idx = None
                for i, line in enumerate(lines):
                    if line.strip().startswith("MODULE "):
                        if after is None or line.strip() == "MODULE " + after:
                            idx = i
                if idx is None:
                    print("refusing: no MODULE line to put it after")
                    return 1
                lines.insert(idx + 1, "MODULE " + entry)
                body = "\n".join(lines).encode("latin-1")
            out = zipfile.ZipInfo(info.filename, info.date_time)
            out.compress_type = info.compress_type
            out.external_attr = info.external_attr
            out.create_system = info.create_system
            zout.writestr(out, body)
            last = info
        add = zipfile.ZipInfo(entry.replace("/", "\\") if "\\" in last.filename else entry, last.date_time)
        add.compress_type = last.compress_type
        add.external_attr = last.external_attr
        add.create_system = last.create_system
        zout.writestr(add, data)
    print("%s: %s added (%d bytes) and listed in Kickstart/Kicklayout" % (dst, entry, len(data)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
