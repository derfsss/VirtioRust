"""Compare MD5Sum lists (hash  path), paths case-insensitive.
    python -I md5cmp.py REFERENCE OTHER..."""
import sys


def load(p):
    d = {}
    for line in open(p, encoding="latin-1"):
        line = line.rstrip("\r\n")
        if not line.strip():
            continue
        h, _, path = line.partition("  ")
        d[path.strip().lower()] = h.strip().lower()
    return d


ref = load(sys.argv[1])
bad = 0
for other in sys.argv[2:]:
    o = load(other)
    missing = sorted(set(ref) - set(o))
    extra = sorted(set(o) - set(ref))
    diff = sorted(k for k in set(ref) & set(o) if ref[k] != o[k])
    ok = not missing and not extra and not diff
    bad += not ok
    print("%s %s: %d files, %d missing, %d extra, %d differ%s" % (
        "PASS" if ok else "FAIL", other, len(o), len(missing), len(extra), len(diff),
        "" if ok else " e.g. %s" % (missing + extra + diff)[:5]))
sys.exit(1 if bad else 0)
