#!/bin/bash
# One boot of the Pegasos II machine with virtio-rng and
# kickstart_virtiorng.zip (its own kickstart plus Kickstart/virtiorng.device
# and the MODULE line). Serial goes to the log given as $1.
# Same ports as run-pegasos2-virtionet.sh (the remote-control daemon, MCPd,
# on 4432; QMP 14432), so the same tools can query and stop it.
# Machine paths (QEMU, BBOOT, MACHINES, SHARE) come from tools/qemu/local.env:
# copy local.env.example there and edit it.
. "$(dirname "$0")/local.env" 2>/dev/null || { echo "tools/qemu/local.env is missing: copy local.env.example and set the paths" >&2; exit 1; }
LOG="${1:?log path}"
# The rng device: virtio-rng-pci (transitional, both interfaces) or
# virtio-rng-pci-non-transitional (the modern interface only).
RNGDEV="${2:-virtio-rng-pci}"
exec "$QEMU" \
  -M pegasos2 \
  -kernel "$BBOOT" \
  -initrd "$MACHINES\mcpd-peg2-upd3\kickstart_virtiorng.zip" \
  -rtc base=localtime -accel tcg -vga none -serial stdio \
  -device es1370,addr=0x08 \
  -append 'serial debuglevel=1' \
  -display sdl,gl=off,show-cursor=off,full-screen=off \
  -device rtl8139,addr=0x09,netdev=nic \
  -netdev 'user,id=nic,hostname=mcpd-peg2-upd3,hostfwd=tcp::4432-:4322,hostfwd=udp::4432-:4323' \
  -name mcpd-peg2-upd3 \
  -device virtio-scsi-pci,id=scsi0 \
  -drive "file=$MACHINES\mcpd-peg2-upd3\hd0.qcow2,if=none,id=vd0,format=qcow2" \
  -device scsi-hd,drive=vd0,bus=scsi0.0,channel=0,scsi-id=0,lun=0 \
  -fsdev "local,security_model=mapped-xattr,id=fsdev0,path=$SHARE" \
  -device virtio-9p-pci,id=fs0,fsdev=fsdev0,mount_tag=SHARED \
  -m 2048M \
  -drive if=none,id=cd -device ide-cd,unit=1,drive=cd,bus=ide.1 \
  -device sm501 \
  -device "$RNGDEV" \
  -qmp tcp:127.0.0.1:14432,server=on,wait=off \
  > "$LOG" 2>&1
