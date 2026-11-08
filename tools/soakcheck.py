#!/usr/bin/env python3
"""Check a virtio-scsi soak (drivers/virtio-scsi/bench/soakfs, soakraw).

    python -I tools/soakcheck.py SOAKDIR

SOAKDIR holds src.md5 (MD5Sum of SYS:Libs), one <tag>.md5 per filesystem
job (every round's MD5Sum of the copy, appended) and raw.log (one
"tests passed" line per vscsitest round). Every round must match src.md5
file for file; every vscsitest round must pass everything.
Exit 0 when all of it holds.
"""
import os
import re
import sys


def entries(lines):
    out = []
    for line in lines:
        line = line.rstrip("\r\n")
        if line.strip():
            h, _, path = line.partition("  ")
            out.append((path.strip().lower(), h.strip().lower()))
    return out


def main():
    d = sys.argv[1]
    src = dict(entries(open(os.path.join(d, "src.md5"), encoding="latin-1")))
    n = len(src)
    if n == 0:
        print("FAIL src.md5 lists no files")
        return 1
    ok = True
    seen = 0
    for name in sorted(os.listdir(d)):
        if not name.endswith(".md5") or name == "src.md5":
            continue
        e = entries(open(os.path.join(d, name), encoding="latin-1"))
        seen += 1
        rounds = [e[i:i + n] for i in range(0, len(e), n)]
        bad = []
        for r, chunk in enumerate(rounds):
            if dict(chunk) != src or len(chunk) != n:
                bad.append(r + 1)
        # Zero rounds is a job that never ran: a failure, not a pass.
        good = bool(rounds) and not bad and len(e) % n == 0
        ok &= good
        print("%s %s: %d rounds of %d files; rounds wrong: %s" % ("PASS" if good else "FAIL", name, len(rounds), n, bad[:10] or "none"))
    if seen == 0:
        print("FAIL no <tag>.md5 file in %s" % d)
        ok = False
    raw = os.path.join(d, "raw.log")
    if os.path.exists(raw):
        tallies = re.findall(r"(\d+)/(\d+) tests passed", open(raw, encoding="latin-1").read())
        wrong = [i + 1 for i, (p, t) in enumerate(tallies) if p != t]
        good = bool(tallies) and not wrong
        ok &= good
        print("%s raw.log: %d vscsitest rounds; rounds with failures: %s" % ("PASS" if good else "FAIL", len(tallies), wrong[:10] or "none"))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
