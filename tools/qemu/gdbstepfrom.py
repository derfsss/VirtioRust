#!/usr/bin/env python3
"""Break at PC, then single-step and log every instruction.

    python -I tools/qemu/gdbstepfrom.py PORT PC [--skip N] [--steps N] [--log FILE]

Waits for the (N+1)th hit of a hardware breakpoint at PC (--skip N), then
single-steps up to --steps instructions, logging PC, r1 and LR at each, so
an exception (PC jumps to a vector) or a task switch (r1 moves to another
stack) shows where it happens. Detaches afterwards (the guest runs on).
"""
import argparse
import importlib.util
import os
import sys
import time

here = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("gdbwatch", os.path.join(here, "gdbwatch.py"))
gw = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gw)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("port", type=int)
    ap.add_argument("pc", type=lambda x: int(x, 0))
    ap.add_argument("--skip", type=int, default=0)
    ap.add_argument("--steps", type=int, default=400)
    ap.add_argument("--seconds", type=float, default=60)
    ap.add_argument("--log", default=None)
    ap.add_argument("--quiet", action="store_true", help="log only exceptions, stack switches and every 1000th step")
    a = ap.parse_args()
    out = open(a.log, "a") if a.log else sys.stdout

    def log(m):
        print(m, file=out, flush=True)

    g = gw.Gdb(a.port)
    log("attach: %s" % g.cmd(b"?").decode(errors="replace"))
    log("break 0x%08x: %s" % (a.pc, g.cmd(b"Z1,%x,4" % a.pc).decode()))
    hits = 0
    t0 = time.time()
    while time.time() - t0 < a.seconds:
        g.s.sendall(b"$c#63")
        g._read_packet(max(1.0, a.seconds - (time.time() - t0)))
        hits += 1
        log("hit %d at 0x%08x r1 0x%08x r3 0x%08x" % (hits, g.reg(gw.PC), g.reg(1), g.reg(3)))
        g.cmd(b"z1,%x,4" % a.pc)
        if hits > a.skip:
            break
        g.cmd(b"s", timeout=10)
        g.cmd(b"Z1,%x,4" % a.pc)
    r1_start = g.reg(1)
    recent, dumped = [], False
    for i in range(a.steps):
        g.cmd(b"s", timeout=10)
        pc, r1, lr = g.reg(gw.PC), g.reg(1), g.reg(gw.LR)
        mark = ""
        if pc is not None and pc < 0x3000:
            mark = "  <== exception vector"
        if r1 is not None and r1_start is not None and abs(r1 - r1_start) > 0x10000:
            mark += "  <== other stack"
        line = "%4d pc 0x%08x r1 0x%08x lr 0x%08x r3 0x%08x r4 0x%08x%s" % (
            i, pc or 0, r1 or 0, lr or 0, g.reg(3) or 0, g.reg(4) or 0, mark)
        recent.append(line)
        if len(recent) > 60:
            recent.pop(0)
        if mark and a.quiet and not dumped:
            log("--- the 60 steps up to the first exception:")
            for l in recent:
                log(l)
            dumped = True
        elif not a.quiet or mark or i % 1000 == 0:
            log(line)
        if mark and "other stack" in mark and a.quiet:
            # Show the next few instructions in the new context, then stop.
            for j in range(20):
                g.cmd(b"s", timeout=10)
                log("     +%d pc 0x%08x r1 0x%08x" % (j, g.reg(gw.PC) or 0, g.reg(1) or 0))
            break
    g.cmd(b"D")
    log("detached")


if __name__ == "__main__":
    sys.exit(main())
