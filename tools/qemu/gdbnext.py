#!/usr/bin/env python3
"""At each hit of a breakpoint, step until the stack changes and say who runs.

    python -I tools/qemu/gdbnext.py PORT PC [--max N] [--seconds S] [--log FILE]

For a scheduler entry (AmigaOS 4.1 FE kernel: the deferred-reschedule branch
at 0x018176cc). Each hit: the task that is leaving (ExecBase->ThisTask via
IExec in r3's Data.LibBase if address 4 does not read), then up to 4000
single steps until r1 leaves the old stack by >64 KiB twice (kernel stack,
then the next task's), then the task that now runs: name, priority, PC.
"""
import argparse
import importlib.util
import os
import struct
import sys
import time

here = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("gdbwatch", os.path.join(here, "gdbwatch.py"))
gw = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gw)

EXECBASE = None


def this_task(g):
    eb = g.u32(4) or EXECBASE
    t = g.u32(eb + 276) if eb else None
    if t and 0x10000000 <= t < 0x80000000:
        pri = g.mem(t + 9, 1)
        return "0x%08x '%s' pri %d" % (t, g.cstr(g.u32(t + 10), 32), struct.unpack("b", pri)[0] if pri else 99)
    return "?"


def main():
    global EXECBASE
    ap = argparse.ArgumentParser()
    ap.add_argument("port", type=int)
    ap.add_argument("pc", type=lambda x: int(x, 0))
    ap.add_argument("--max", type=int, default=30)
    ap.add_argument("--seconds", type=float, default=40)
    ap.add_argument("--execbase", type=lambda x: int(x, 0), default=0x021d68e2)
    ap.add_argument("--log", default=None)
    a = ap.parse_args()
    EXECBASE = a.execbase
    out = open(a.log, "a") if a.log else sys.stdout

    def log(m):
        print(m, file=out, flush=True)

    g = gw.Gdb(a.port)
    log("attach: %s" % g.cmd(b"?").decode(errors="replace"))
    log("break 0x%08x: %s" % (a.pc, g.cmd(b"Z1,%x,4" % a.pc).decode()))
    t0 = time.time()
    n = 0
    while n < a.max and time.time() - t0 < a.seconds:
        g.s.sendall(b"$c#63")
        try:
            g._read_packet(max(1.0, a.seconds - (time.time() - t0)))
        except Exception:
            g.s.sendall(b"\x03")
            g._read_packet(5)
            log("time is up")
            break
        n += 1
        leaving = this_task(g)
        r1_0 = g.reg(1)
        g.cmd(b"z1,%x,4" % a.pc)
        switched, steps, r1 = 0, 0, r1_0
        last_far = None
        while steps < 4000:
            g.cmd(b"s", timeout=10)
            steps += 1
            r1 = g.reg(1)
            if r1 and r1_0 and abs(r1 - r1_0) > 0x10000 and (last_far is None or abs(r1 - last_far) > 0x10000):
                switched += 1
                last_far = r1
                if switched == 2:
                    break
        log("%7.3f hit %d: leaving %s; after %d steps r1 0x%08x pc 0x%08x running %s" % (
            time.time() - t0, n, leaving, steps, r1 or 0, g.reg(gw.PC) or 0, this_task(g)))
        g.cmd(b"Z1,%x,4" % a.pc)
    g.cmd(b"z1,%x,4" % a.pc)
    g.cmd(b"D")
    log("detached; %d hits" % n)


if __name__ == "__main__":
    sys.exit(main())
