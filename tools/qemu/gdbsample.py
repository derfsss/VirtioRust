#!/usr/bin/env python3
"""Sample which task runs: stop the guest N times, read ExecBase->ThisTask.

    python -I tools/qemu/gdbsample.py PORT [--n 60] [--gap 0.1]

Prints a count per task name. idle.task in the list means the CPU had idle
time. ExecBase defaults to the AmigaOne 4.1 FE address (0x021d68e2).
"""
import argparse
import collections
import importlib.util
import os
import socket
import time

here = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("gdbwatch", os.path.join(here, "gdbwatch.py"))
gw = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gw)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("port", type=int)
    ap.add_argument("--n", type=int, default=60)
    ap.add_argument("--gap", type=float, default=0.1)
    ap.add_argument("--execbase", type=lambda x: int(x, 0), default=0x021d68e2)
    a = ap.parse_args()
    counts = collections.Counter()
    for _ in range(a.n):
        g = gw.Gdb(a.port)
        g.cmd(b"?")
        t = g.u32(a.execbase + 276)
        name = g.cstr(g.u32(t + 10), 40) if t else "?"
        counts[name] += 1
        g.cmd(b"D")
        g.s.close()
        time.sleep(a.gap)
    for name, n in counts.most_common():
        print("%3d  %s" % (n, name))


if __name__ == "__main__":
    main()
