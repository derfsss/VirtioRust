#!/usr/bin/env python3
"""Watch guest memory for writes through QEMU's GDB stub (-gdb tcp:...).

    python -I tools/qemu/gdbwatch.py PORT ADDR [LEN] [--hits N] [--stop-on-zero]
                                     [--seconds S] [--log FILE]

Attaches (QEMU stops the CPU), sets a write watchpoint (Z2) on ADDR..ADDR+LEN
(virtual addresses, as the CPU sees them), and lets the guest run. At every
hit it logs the hit number, PC (the instruction after the store), LR, r1,
the words now at ADDR, and the running task's address and name (ExecBase at
address 4, ThisTask at +276, ln_Name at +10), then continues. It stops after
N hits, or (--stop-on-zero) at the first hit that leaves the first word 0,
or after S seconds; then it removes the watchpoint, detaches and leaves
the guest running.
"""
import argparse
import socket
import struct
import sys
import time

PC, MSR, CR, LR, CTR = 64, 65, 66, 67, 68   # QEMU ppc32 gdb register numbers


class Gdb:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=10)
        self.buf = b""

    def _read_packet(self, timeout=None):
        self.s.settimeout(timeout)
        while True:
            i = self.buf.find(b"$")
            if i > 0:
                self.buf = self.buf[i:]          # acks ('+') before a packet
                i = 0
            j = self.buf.find(b"#") if i == 0 else -1
            if i == 0 and j >= 0 and len(self.buf) >= j + 3:
                pkt = self.buf[1:j]
                self.buf = self.buf[j + 3:]
                self.s.sendall(b"+")
                return pkt
            chunk = self.s.recv(65536)
            if not chunk:
                raise EOFError("stub closed")
            self.buf += chunk

    def cmd(self, payload, timeout=10):
        cs = sum(payload) & 0xFF
        self.s.sendall(b"$" + payload + b"#%02x" % cs)
        return self._read_packet(timeout)

    def reg(self, n):
        r = self.cmd(b"p%x" % n)
        return int(r.decode(), 16) if r and not r.startswith(b"E") else None

    def mem(self, addr, n):
        r = self.cmd(b"m%x,%x" % (addr, n))
        if not r or r.startswith(b"E"):
            return None
        return bytes.fromhex(r.decode())

    def u32(self, addr):
        b = self.mem(addr, 4)
        return struct.unpack(">I", b)[0] if b else None

    def cstr(self, addr, n=48):
        b = self.mem(addr, n) if addr else None
        return b.split(b"\0")[0].decode("latin-1") if b else "?"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("port", type=int)
    ap.add_argument("addr", type=lambda x: int(x, 0))
    ap.add_argument("length", type=lambda x: int(x, 0), nargs="?", default=4)
    ap.add_argument("--hits", type=int, default=200)
    ap.add_argument("--stop-on-zero", action="store_true")
    ap.add_argument("--seconds", type=float, default=300)
    ap.add_argument("--log", default=None)
    ap.add_argument("--also", type=lambda x: int(x, 0), action="append", default=[],
                    help="another address to watch (repeatable)")
    a = ap.parse_args()
    out = open(a.log, "a") if a.log else sys.stdout

    def log(msg):
        print(msg, file=out, flush=True)

    g = Gdb(a.port)
    # QEMU stops the CPU when a client connects; '?' reports why.
    log("attach: %s" % g.cmd(b"?").decode(errors="replace"))
    addrs = [a.addr] + a.also
    for w in addrs:
        r = g.cmd(b"Z2,%x,%x" % (w, a.length))
        log("watch 0x%08x+%d: %s; now %s" % (w, a.length, r.decode() or "(unsupported)",
                                            (g.mem(w, 16) or b"").hex()))
        if r != b"OK":
            g.cmd(b"D")
            return 2
    t_end = time.time() + a.seconds
    hits = 0
    while hits < a.hits and time.time() < t_end:
        g.s.sendall(b"$c#63")
        try:
            stop = g._read_packet(max(1.0, t_end - time.time()))
        except socket.timeout:
            g.s.sendall(b"\x03")
            stop = g._read_packet(5)
            log("time is up (%s)" % stop.decode(errors="replace"))
            break
        hits += 1
        pc, lr, r1 = g.reg(PC), g.reg(LR), g.reg(1)
        # Which watch fired: the stop reply says "watch:<addr>".
        st = stop.decode(errors="replace")
        hit_addr = a.addr
        if "watch:" in st:
            try:
                hit_addr = int(st.split("watch:")[1].split(";")[0], 16)
            except ValueError:
                pass
        base = max([w for w in addrs if w <= hit_addr] or [a.addr])
        words = g.mem(base, 16) or b""
        execbase = g.u32(4)
        task = g.u32(execbase + 276) if execbase else None
        name = g.cstr(g.u32(task + 10)) if task else "?"
        first = struct.unpack(">I", words[:4])[0] if len(words) >= 4 else None
        log("hit %d @0x%08x: %s pc 0x%08x lr 0x%08x r1 0x%08x task 0x%08x '%s' now %s" % (
            hits, base, stop.decode(errors="replace")[:40], pc or 0, lr or 0, r1 or 0, task or 0, name, words.hex()))
        # The caller chain: PowerPC SysV frames, back chain at 0(r1), saved
        # LR at 4(back chain).
        chain, sp = [], r1
        for _ in range(12):
            if not sp:
                break
            back = g.u32(sp)
            if not back or back <= sp or back - sp > 0x100000:
                break
            ret = g.u32(back + 4)
            chain.append("0x%08x" % (ret or 0))
            sp = back
        log("    callers: %s" % " ".join(chain))
        if a.stop_on_zero and first == 0:
            log("first word is 0: stopping here, the guest is left running")
            break
    for w in addrs:
        g.cmd(b"z2,%x,%x" % (w, a.length))
    g.cmd(b"D")
    log("detached after %d hits" % hits)
    return 0


if __name__ == "__main__":
    sys.exit(main())
