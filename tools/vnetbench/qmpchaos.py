"""Faults from the host side during a soak: QMP link flaps and announces.

Usage: python -I qmpchaos.py QMP_PORT SECONDS [period=60] [down=2] [name=vnet]

Every `period` seconds: the virtio-net link goes down for `down` seconds
and comes back (QMP set_link on netdev `name`), then a gratuitous
announce (announce-self) is sent. Each action and QMP's answer is printed
with the time since start; the exit code is 1 if QMP refused anything.
The machines' launch scripts name the card's netdev "vnet".
"""
import json
import socket
import sys
import time


def qmp(port):
    s = socket.create_connection(("127.0.0.1", port), timeout=10)
    f = s.makefile("rw")
    f.readline()
    f.write(json.dumps({"execute": "qmp_capabilities"}) + "\n")
    f.flush()
    f.readline()
    return s, f


def call(f, cmd, args=None):
    msg = {"execute": cmd}
    if args:
        msg["arguments"] = args
    f.write(json.dumps(msg) + "\n")
    f.flush()
    while True:
        line = f.readline()
        if not line:
            return {"error": "QMP closed"}
        r = json.loads(line)
        if "return" in r or "error" in r:
            return r


def main():
    port, secs = int(sys.argv[1]), float(sys.argv[2])
    opts = dict(a.split("=", 1) for a in sys.argv[3:])
    period = float(opts.get("period", 60))
    down = float(opts.get("down", 2))
    name = opts.get("name", "vnet")
    s, f = qmp(port)
    t0 = time.time()
    bad = 0
    n = 0
    while True:
        time.sleep(max(0.0, t0 + (n + 1) * period - time.time()))
        if time.time() - t0 > secs:
            break
        n += 1
        steps = (
            ("set_link", {"name": name, "up": False}),
            ("pause", {}),
            ("set_link", {"name": name, "up": True}),
            ("announce-self", {"initial": 50, "max": 550, "rounds": 2, "step": 100}),
        )
        for cmd, args in steps:
            if cmd == "pause":
                time.sleep(down)
                continue
            r = call(f, cmd, args)
            ok = "return" in r
            bad += 0 if ok else 1
            print(f"{time.time() - t0:7.1f} s {cmd} {args.get('up', '') if cmd == 'set_link' else ''}: {'ok' if ok else r}", flush=True)
    s.close()
    print(f"qmpchaos: {n} link flaps, {bad} refused", flush=True)
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
