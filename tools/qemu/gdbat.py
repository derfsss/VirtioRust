#!/usr/bin/env python3
"""At each hit of a hardware breakpoint, log registers, the running task and memory.

    python -I tools/qemu/gdbat.py PORT PC [--mem ADDR:LEN ...] [--max N] [--seconds S] [--log FILE]

Attach only after the guest has booted. Logs, per hit: PC, LR, r1, r3, r4,
ExecBase->ThisTask (name, priority), and each --mem range as hex words.
ExecBase defaults to the AmigaOne 4.1 FE address (0x021d68e2).
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


def task_desc(g, t):
    if not t or not (0x10000000 <= t < 0x80000000):
        return "-"
    pri = g.mem(t + 9, 1)
    return "0x%08x '%s' pri %d" % (t, g.cstr(g.u32(t + 10), 32), struct.unpack("b", pri)[0] if pri else 99)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("port", type=int)
    ap.add_argument("pc", type=lambda x: int(x, 0))
    ap.add_argument("--mem", action="append", default=[])
    ap.add_argument("--max", type=int, default=10)
    ap.add_argument("--seconds", type=float, default=40)
    ap.add_argument("--execbase", type=lambda x: int(x, 0), default=0x021d68e2)
    ap.add_argument("--log", default=None)
    ap.add_argument("--kind", type=int, default=0, help="0 = QEMU (TCG) breakpoint, 1 = hardware")
    a = ap.parse_args()
    out = open(a.log, "a") if a.log else sys.stdout

    def log(m):
        print(m, file=out, flush=True)

    ranges = []
    for m in a.mem:
        addr, n = m.split(":")
        ranges.append((int(addr, 0), int(n, 0)))
    g = gw.Gdb(a.port)
    log("attach: %s" % g.cmd(b"?").decode(errors="replace"))
    log("break 0x%08x: %s" % (a.pc, g.cmd(b"Z%d,%x,4" % (a.kind, a.pc)).decode()))
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
        log("%7.3f hit %d pc 0x%08x lr 0x%08x r1 0x%08x r3 0x%08x r4 0x%08x | running %s" % (
            time.time() - t0, n, g.reg(gw.PC) or 0, g.reg(gw.LR) or 0, g.reg(1) or 0,
            g.reg(3) or 0, g.reg(4) or 0, task_desc(g, g.u32(a.execbase + 276))))
        for addr, ln in ranges:
            data = g.mem(addr, ln) or b""
            words = " ".join("%08x" % w for w in struct.unpack(">%dI" % (len(data) // 4), data[: len(data) // 4 * 4]))
            log("        0x%08x: %s" % (addr, words))
        g.cmd(b"z%d,%x,4" % (a.kind, a.pc))
        g.cmd(b"s", timeout=10)
        g.cmd(b"Z%d,%x,4" % (a.kind, a.pc))
    g.cmd(b"z%d,%x,4" % (a.kind, a.pc))
    g.cmd(b"D")
    log("detached; %d hits" % n)


if __name__ == "__main__":
    sys.exit(main())
