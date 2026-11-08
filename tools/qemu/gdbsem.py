#!/usr/bin/env python3
"""At a breakpoint on an ObtainSemaphore call, say who owns the semaphore.

    python -I tools/qemu/gdbsem.py PORT PC [--seconds S] [--max N] [--log FILE]

PC is the bctrl that calls ObtainSemaphore with r4 = the semaphore (for
AmigaOS 4.1 FE dos.library's process-list link, 0x0196bc0c). Each hit logs
the semaphore, its nest count, its owner (ss_Owner at +40) with the owner's
name and state, and the waiters (ss_WaitQueue at +16), then steps over.
struct SignalSemaphore (pack 2): ss_Link Node 0..13, ss_NestCount 14,
ss_WaitQueue MinList 16..27, ss_MultipleLink 28..39, ss_Owner 40,
ss_QueueCount 44. Task: tc_Node.ln_Name at 10, tc_State at 15.
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
    if not t:
        return "none"
    name = g.cstr(g.u32(t + 10))
    st = g.mem(t + 15, 1)
    pri = g.mem(t + 9, 1)
    return "0x%08x '%s' state %d pri %d" % (t, name, st[0] if st else -1,
                                             struct.unpack("b", pri)[0] if pri else 0)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("port", type=int)
    ap.add_argument("pc", type=lambda x: int(x, 0))
    ap.add_argument("--seconds", type=float, default=60)
    ap.add_argument("--max", type=int, default=100)
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
        sem, r1 = g.reg(4), g.reg(1)
        nest = struct.unpack(">h", g.mem(sem + 14, 2) or b"\0\0")[0]
        owner = g.u32(sem + 40)
        queue = struct.unpack(">h", g.mem(sem + 44, 2) or b"\0\0")[0]
        waiters, node, k = [], g.u32(sem + 16), 0
        while node and k < 8:
            succ = g.u32(node)
            if not succ:
                break
            waiters.append(task_desc(g, g.u32(node + 8)))
            node, k = succ, k + 1
        log("%7.3f obtain sem 0x%08x by r1 0x%08x: nest %d queue %d owner %s; waiting %s" % (
            time.time() - t0, sem or 0, r1 or 0, nest, queue, task_desc(g, owner), waiters or "none"))
        g.cmd(b"z1,%x,4" % a.pc)
        g.cmd(b"s", timeout=10)
        g.cmd(b"Z1,%x,4" % a.pc)
    g.cmd(b"z1,%x,4" % a.pc)
    g.cmd(b"D")
    log("detached; %d hits" % n)


if __name__ == "__main__":
    sys.exit(main())
