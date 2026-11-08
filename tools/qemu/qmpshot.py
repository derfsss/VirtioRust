"""Take a PNG screenshot of a QEMU machine through its QMP port.

    python -I tools/qemu/qmpshot.py 14423 shot.png

The launch scripts in tools/qemu name each guest's QMP port (-qmp).
"""
import json
import socket
import sys


def main() -> int:
    port, out = int(sys.argv[1]), sys.argv[2]
    s = socket.create_connection(("127.0.0.1", port), timeout=10)
    f = s.makefile("rw", encoding="utf-8", newline="\n")
    f.readline()  # greeting
    for cmd in ({"execute": "qmp_capabilities"},
                {"execute": "screendump", "arguments": {"filename": out, "format": "png"}}):
        f.write(json.dumps(cmd) + "\n")
        f.flush()
        while True:
            r = json.loads(f.readline())
            if "return" in r or "error" in r:
                break
        if "error" in r:
            print(r["error"])
            return 1
    print(out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
