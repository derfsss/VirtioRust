#!/bin/bash
# One boot of the qemu-amigaone machine with virtio-rng and
# kickstart_virtiorng.zip. Serial goes to the log given as $1.
# Same ports as qemu-amigaone (the remote-control daemon, MCPd, on 4423;
# QMP 14423), so the same remote-control tools can query and stop it.
# Machine paths (QEMU, BBOOT, MACHINES, SHARE) come from tools/qemu/local.env:
# copy local.env.example there and edit it.
. "$(dirname "$0")/local.env" 2>/dev/null || { echo "tools/qemu/local.env is missing: copy local.env.example and set the paths" >&2; exit 1; }
LOG="${1:?log path}"
# The rng devices, word-split on purpose (default: one transitional card),
# e.g. "-device virtio-rng-pci -device virtio-rng-pci-non-transitional".
RNGDEVS="${2:--device virtio-rng-pci}"
# The kickstart: the copy with virtiorng.device in it, or (KICKZIP=stock)
# the machine's own, for the disk build loaded from DEVS:.
case "${KICKZIP:-virtiorng}" in
  stock) KICK="$MACHINES\base_a1_backup\kickstart.zip" ;;
  *)     KICK="$MACHINES\base_a1_backup\kickstart_virtiorng.zip" ;;
esac
exec "$QEMU" \
  -M amigaone \
  -kernel "$BBOOT" \
  -device "loader,addr=0x600000,file=$KICK" \
  -rtc base=localtime -accel tcg -vga none -serial stdio \
  -device rtl8139,addr=0x0a,netdev=nic \
  -netdev 'user,id=nic,hostname=base_a1_backup,hostfwd=tcp::4423-:4322,hostfwd=udp::4423-:4323' \
  -append 'serial debuglevel=1' \
  -display sdl,gl=off,show-cursor=off,full-screen=off \
  -device es1370,addr=0x09 -name base_a1_backup \
  -device virtio-scsi-pci,id=scsi0 \
  -drive "file=$MACHINES\base_a1_backup\hd0.qcow2,if=none,id=vd0,format=qcow2" \
  -device scsi-hd,drive=vd0,bus=scsi0.0,channel=0,scsi-id=0,lun=0 \
  -fsdev "local,security_model=mapped-xattr,id=fsdev0,path=$SHARE" \
  -device virtio-9p-pci,id=fs0,fsdev=fsdev0,mount_tag=SHARED \
  -m 2048M \
  -drive if=none,id=cd -device ide-cd,unit=1,drive=cd,bus=ide.1 \
  -device sm501 \
  $RNGDEVS \
  -qmp tcp:127.0.0.1:14423,server=on,wait=off \
  > "$LOG" 2>&1
