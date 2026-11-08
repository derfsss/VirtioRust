#!/usr/bin/env python3
"""At each hit of a hardware breakpoint, report a semaphore's state.

    python -I tools/qemu/gdbsemat.py PORT PC SEM [--max N] [--seconds S] [--log FILE]

SEM: a struct SignalSemaphore (pack 2): ss_NestCount at 14, ss_WaitQueue
(MinList) at 16, ss_Owner at 40, ss_QueueCount at 44. Logs the nest count,
the owner (address, name, state, priority) and the queue count, and walks
the owner's stack back chain to show what it is doing.
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
    ap.add_argument("pc", type=lambda x: int(x, 0))
    ap.add_argument("sem", type=lambda x: int(x, 0))
    ap.add_argument("--max", type=int, default=10)
    ap.add_argument("--seconds", type=float, default=40)
    ap.add_argument("--log", default=None)
    a = ap.parse_args()
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
        nest = struct.unpack(">h", g.mem(a.sem + 14, 2) or b"\0\0")[0]
        queue = struct.unpack(">h", g.mem(a.sem + 44, 2) or b"\0\0")[0]
        owner = g.u32(a.sem + 40)
        desc = "none"
        if owner and 0x10000000 <= owner < 0x80000000:
            st = g.mem(owner + 15, 1)
            pri = g.mem(owner + 9, 1)
            spreg = g.u32(owner + 54)   # tc_SPReg: the owner's saved stack pointer
            chain, sp = [], spreg
            for _ in range(10):
                back = g.u32(sp) if sp else None
                if not back or back <= sp or back - sp > 0x100000:
                    break
                chain.append("0x%08x" % (g.u32(back + 4) or 0))
                sp = back
            desc = "0x%08x '%s' state %d pri %d sp 0x%08x callers %s" % (
                owner, g.cstr(g.u32(owner + 10), 40), st[0] if st else -1,
                struct.unpack("b", pri)[0] if pri else 0, spreg or 0, " ".join(chain))
        log("%7.3f hit %d r3 0x%08x: sem nest %d queue %d owner %s" % (
            time.time() - t0, n, g.reg(3) or 0, nest, queue, desc))
        g.cmd(b"z1,%x,4" % a.pc)
        g.cmd(b"s", timeout=10)
        g.cmd(b"Z1,%x,4" % a.pc)
    g.cmd(b"z1,%x,4" % a.pc)
    g.cmd(b"D")
    log("detached; %d hits" % n)


if __name__ == "__main__":
    sys.exit(main())
