#!/usr/bin/env python3
"""Log breakpoints only while one call is running (from its entry to its return).

    python -I tools/qemu/gdbinside.py PORT ENTRY RET PC[=label] ... [--task NAME] [--calls N] [--log FILE]

Waits for ENTRY (a bl instruction) hit by a task whose name is NAME, then sets
the inner breakpoints and logs every hit of them (PC, LR, r1, r3, r4, running
task) until RET is reached on the caller's stack (same r1). Repeats for
--calls calls. QEMU (TCG) breakpoints. Attach only after the guest has booted,
and start the test only after "armed" is logged.
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

EXECBASE = 0x021d68e2


def running(g):
    t = g.u32(EXECBASE + 276)
    if not t or not (0x10000000 <= t < 0x80000000):
        return 0, "-"
    pri = g.mem(t + 9, 1)
    return t, "0x%08x '%s' pri %d" % (t, g.cstr(g.u32(t + 10), 32), struct.unpack("b", pri)[0] if pri else 99)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("port", type=int)
    ap.add_argument("entry", type=lambda x: int(x, 0))
    ap.add_argument("ret", type=lambda x: int(x, 0))
    ap.add_argument("points", nargs="+")
    ap.add_argument("--task", default="Background CLI")
    ap.add_argument("--calls", type=int, default=2)
    ap.add_argument("--pri", type=int, default=None, help="only callers with this priority")
    ap.add_argument("--max", type=int, default=400)
    ap.add_argument("--seconds", type=float, default=60)
    ap.add_argument("--log", default=None)
    a = ap.parse_args()
    out = open(a.log, "a") if a.log else sys.stdout

    def log(m):
        print(m, file=out, flush=True)

    inner = {}
    for p in a.points:
        addr, _, label = p.partition("=")
        inner[int(addr, 0)] = label or addr
    g = gw.Gdb(a.port)
    log("attach: %s" % g.cmd(b"?").decode(errors="replace"))
    log("armed: entry 0x%08x %s" % (a.entry, g.cmd(b"Z0,%x,4" % a.entry).decode()))
    t0 = time.time()
    calls, hits, inside, caller_r1, caller_task = 0, 0, False, None, 0

    def cont():
        g.s.sendall(b"$c#63")
        g._read_packet(max(1.0, a.seconds - (time.time() - t0)))

    def step_over(pc):
        g.cmd(b"z0,%x,4" % pc)
        g.cmd(b"s", timeout=10)
        g.cmd(b"Z0,%x,4" % pc)

    try:
        while (calls < a.calls or inside) and hits < a.max and time.time() - t0 < a.seconds:
            cont()
            pc, r1 = g.reg(gw.PC), g.reg(1)
            tptr, tdesc = running(g)
            name = g.cstr(g.u32(tptr + 10), 32) if tptr else ""
            if pc == a.entry:
                cpri = struct.unpack("b", g.mem(tptr + 9, 1) or bytes(1))[0] if tptr else None
                if inside or name != a.task or (a.pri is not None and cpri != a.pri):
                    step_over(pc)
                    continue
                inside, caller_r1, caller_task = True, r1, tptr
                calls += 1
                log("%7.3f === call %d by %s (r1 0x%08x, r4 0x%08x)" % (time.time() - t0, calls, tdesc, r1, g.reg(4) or 0))
                g.cmd(b"Z0,%x,4" % a.ret)
                for addr in inner:
                    g.cmd(b"Z0,%x,4" % addr)
                step_over(pc)
                continue
            if pc == a.ret:
                if inside and r1 == caller_r1:
                    log("%7.3f === return to the caller; running %s" % (time.time() - t0, tdesc))
                    inside = False
                    g.cmd(b"z0,%x,4" % a.ret)
                    for addr in inner:
                        g.cmd(b"z0,%x,4" % addr)
                else:
                    step_over(pc)
                continue
            hits += 1
            chain, sp = [], r1
            for _ in range(8):
                back = g.u32(sp) if sp else None
                if not back or back <= sp or back - sp > 0x100000:
                    break
                chain.append("%08x" % (g.u32(back + 4) or 0))
                sp = back
            cst = g.mem(caller_task + 15, 1) if caller_task else None
            tdesc += " [caller state %d sigwait 0x%08x sigrecvd 0x%08x]" % (
                cst[0] if cst else -1, g.u32(caller_task + 22) or 0, g.u32(caller_task + 26) or 0)
            log("%7.3f   %-10s pc 0x%08x lr 0x%08x r1 0x%08x r3 0x%08x r4 0x%08x | %s | callers %s" % (
                time.time() - t0, inner.get(pc, "?"), pc or 0, g.reg(gw.LR) or 0, r1 or 0,
                g.reg(3) or 0, g.reg(4) or 0, tdesc, " ".join(chain)))
            step_over(pc)
    except Exception as e:
        try:
            g.s.sendall(b"\x03")
            g._read_packet(5)
        except Exception:
            pass
        log("stopped: %r" % e)
    for addr in list(inner) + [a.entry, a.ret]:
        g.cmd(b"z0,%x,4" % addr)
    g.cmd(b"D")
    log("detached; %d calls, %d inner hits" % (calls, hits))


if __name__ == "__main__":
    sys.exit(main())
