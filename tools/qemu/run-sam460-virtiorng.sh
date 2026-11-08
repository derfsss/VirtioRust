#!/bin/bash
# One boot of the Sam460ex machine with virtio-rng.
# The Sam460 boots from its U-Boot firmware and loads the kickstart from its
# own disk, so virtiorng.device must be installed in the guest's
# SYS:Kickstart/ with a MODULE line in SYS:Kickstart/Kicklayout first.
# This is the machine's usual command line plus -device virtio-rng-pci;
# the remote-control daemon (MCPd) on 4321 and QMP 14321, as usual.
# Serial goes to the log given as $1.
# Machine paths (QEMU, BBOOT, MACHINES, SHARE) come from tools/qemu/local.env:
# copy local.env.example there and edit it.
. "$(dirname "$0")/local.env" 2>/dev/null || { echo "tools/qemu/local.env is missing: copy local.env.example and set the paths" >&2; exit 1; }
LOG="${1:?log path}"
exec "$QEMU" \
  -M sam460ex -rtc base=localtime -accel tcg -serial stdio \
  -device rtl8139,netdev=nic \
  -netdev 'user,id=nic,hostname=backup-sam460-upd3,hostfwd=tcp::4321-:4322,hostfwd=udp::4321-:4323' \
  -display sdl,gl=off,show-cursor=off,full-screen=off \
  -device es1370 -name backup-sam460-upd3 \
  -device virtio-scsi-pci,id=scsi0 \
  -drive "file=$MACHINES\scsi.raw,if=none,id=vd1,format=raw" \
  -device scsi-hd,drive=vd1,bus=scsi0.0,channel=0,scsi-id=1,lun=0 \
  -fsdev "local,security_model=mapped-xattr,id=fsdev0,path=$SHARE" \
  -device virtio-9p-pci,id=fs0,fsdev=fsdev0,mount_tag=SHARED \
  -m 2048M \
  -device sii3112 -drive if=none,id=cd -device ide-cd,unit=0,drive=cd,bus=ide.1 \
  -drive "if=none,id=hd0,file=$MACHINES\backup-sam460-upd3\hd0.qcow2,format=qcow2" \
  -device ide-hd,unit=0,drive=hd0,bus=ide.0 \
  -device virtio-rng-pci \
  -qmp tcp:127.0.0.1:14321,server=on,wait=off \
  > "$LOG" 2>&1
