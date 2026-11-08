#!/usr/bin/env python3
"""Follow DOS process-header blocks through their life, over QEMU's GDB stub.

    python -I tools/qemu/gdbtrace.py PORT [--setup-pc 0x01949e10] [--seconds S]
                                     [--max-blocks N] [--log FILE]

A hardware breakpoint at SETUP-PC (the instruction after dos.library stores
a new process's pointer at +8 of its 16 KB startup block, AmigaOS 4.1 FE
dos.library.kmod) finds each new block: a page-aligned
register value whose +8 holds the process pointer. Each block found gets a
write watchpoint on its first 16 bytes, and every later write is logged
with PC, LR, r1 and the caller chain (back chain at 0(r1), LR at 4(frame)).
The guest runs between hits. Detaches after S seconds.
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


def step_over(g, pc):
    """Leave a hardware breakpoint: take it out, single-step, put it back."""
    g.cmd(b"z1,%x,4" % pc)
    g.cmd(b"s", timeout=10)
    g.cmd(b"Z1,%x,4" % pc)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("port", type=int)
    ap.add_argument("--setup-pc", type=lambda x: int(x, 0), default=0x01949e10)
    ap.add_argument("--seconds", type=float, default=120)
    ap.add_argument("--max-blocks", type=int, default=12)
    ap.add_argument("--log", default=None)
    a = ap.parse_args()
    out = open(a.log, "a") if a.log else sys.stdout

    def log(m):
        print(m, file=out, flush=True)

    g = gw.Gdb(a.port)
    log("attach: %s" % g.cmd(b"?").decode(errors="replace"))
    r = g.cmd(b"Z1,%x,4" % a.setup_pc)
    log("hw breakpoint 0x%08x: %s" % (a.setup_pc, r.decode()))
    watched = {}
    t_end = time.time() + a.seconds
    n = 0
    while time.time() < t_end:
        g.s.sendall(b"$c#63")
        try:
            stop = g._read_packet(max(1.0, t_end - time.time()))
        except Exception:
            g.s.sendall(b"\x03")
            g._read_packet(5)
            log("time is up")
            break
        n += 1
        st = stop.decode(errors="replace")
        pc, lr, r1 = g.reg(gw.PC), g.reg(gw.LR), g.reg(1)
        chain, sp = [], r1
        for _ in range(12):
            back = g.u32(sp) if sp else None
            if not back or back <= sp or back - sp > 0x100000:
                break
            chain.append("0x%08x" % (g.u32(back + 4) or 0))
            sp = back
        if "watch:" in st:
            w = int(st.split("watch:")[1].split(";")[0], 16)
            base = max([b for b in watched if b <= w] or [w])
            log("%6.2f write @0x%08x (block 0x%08x, process 0x%08x): pc 0x%08x lr 0x%08x r1 0x%08x now %s\n         callers %s" % (
                time.time() - t_end + a.seconds, w, base, watched.get(base, 0), pc, lr, r1,
                (g.mem(base, 16) or b"").hex(), " ".join(chain)))
            continue
        # The setup breakpoint: find the block.
        regs = [g.reg(i) for i in range(32)]
        found = None
        for v in regs:
            if v and v % 0x1000 == 0 and 0x10000000 <= v < 0x80000000:
                p = g.u32(v + 8)
                if p and p not in (0xFEFEFEFE,) and 0x10000000 <= p < 0x80000000:
                    found = (v, p)
                    break
        if not found:
            log("%6.2f setup hit, block not found; regs %s" % (time.time() - t_end + a.seconds,
                                                              " ".join("%08x" % (v or 0) for v in regs)))
            step_over(g, a.setup_pc)
            continue
        blk, proc = found
        name = g.cstr(g.u32(proc + 10)) if proc else "?"
        log("%6.2f setup: block 0x%08x for process 0x%08x '%s' (r1 0x%08x) callers %s" % (
            time.time() - t_end + a.seconds, blk, proc, name, r1, " ".join(chain)))
        if blk not in watched and len(watched) < a.max_blocks:
            rr = g.cmd(b"Z2,%x,10" % blk)
            watched[blk] = proc
            log("         watching 0x%08x: %s" % (blk, rr.decode()))
        step_over(g, a.setup_pc)
    for b in watched:
        g.cmd(b"z2,%x,10" % b)
    g.cmd(b"z1,%x,4" % a.setup_pc)
    g.cmd(b"D")
    log("detached; %d stops" % n)


if __name__ == "__main__":
    sys.exit(main())
