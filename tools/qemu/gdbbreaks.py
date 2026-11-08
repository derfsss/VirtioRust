#!/usr/bin/env python3
"""Log the order in which hardware breakpoints are reached (QEMU GDB stub).

    python -I tools/qemu/gdbbreaks.py PORT ADDR[=label] [ADDR[=label] ...]
                                      [--seconds S] [--max N] [--log FILE]

Each hit logs the time, the label, PC, LR, r1 and r3, then steps over the
breakpoint and lets the guest run on. QEMU allows several hardware
breakpoints on TCG. For finding which of two tasks gets somewhere first.
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


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("port", type=int)
    ap.add_argument("points", nargs="+")
    ap.add_argument("--seconds", type=float, default=60)
    ap.add_argument("--max", type=int, default=400)
    ap.add_argument("--log", default=None)
    a = ap.parse_args()
    out = open(a.log, "a") if a.log else sys.stdout

    def log(m):
        print(m, file=out, flush=True)

    points = {}
    for p in a.points:
        addr, _, label = p.partition("=")
        points[int(addr, 0)] = label or addr
    g = gw.Gdb(a.port)
    log("attach: %s" % g.cmd(b"?").decode(errors="replace"))
    for addr, label in points.items():
        log("break 0x%08x %s: %s" % (addr, label, g.cmd(b"Z1,%x,4" % addr).decode()))
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
        pc = g.reg(gw.PC)
        lr, r1, r3 = g.reg(gw.LR), g.reg(1), g.reg(3)
        # The running task (ExecBase at 4, ThisTask at +276) and, when r3
        # looks like a task, its name and priority (ln_Pri at 9, ln_Name at 10).
        def tdesc(t):
            if not t or not (0x10000000 <= t < 0x80000000):
                return "-"
            pri = g.mem(t + 9, 1)
            return "'%s' pri %d" % (g.cstr(g.u32(t + 10), 32), struct.unpack("b", pri)[0] if pri else 99)
        # ExecBase: from address 4 when it reads, else from IExec (r27 in
        # dos.library's CreateNewProc) -> Data.LibBase.
        eb = g.u32(4)
        cur = g.u32(eb + 276) if eb else None
        if not cur:
            r27 = g.reg(27)
            for off in (14, 16, 18, 20):
                base = g.u32(r27 + off) if r27 else None
                t = g.u32(base + 276) if base and 0x01000000 <= base < 0x80000000 else None
                if t and 0x10000000 <= t < 0x80000000 and g.cstr(g.u32(t + 10), 32) not in ("?", ""):
                    cur = t
                    break
        log("%7.3f %-14s pc 0x%08x lr 0x%08x r1 0x%08x r3 0x%08x | running %s | r3 %s" % (
            time.time() - t0, points.get(pc, "?"), pc or 0, lr or 0, r1 or 0, r3 or 0, tdesc(cur), tdesc(r3)))
        if pc in points:
            g.cmd(b"z1,%x,4" % pc)
            g.cmd(b"s", timeout=10)
            g.cmd(b"Z1,%x,4" % pc)
    for addr in points:
        g.cmd(b"z1,%x,4" % addr)
    g.cmd(b"D")
    log("detached; %d hits" % n)


if __name__ == "__main__":
    sys.exit(main())
