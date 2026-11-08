#!/bin/bash
# One boot of the Pegasos II machine for virtionet.device testing, on a
# copy of its disk (the original is untouched) with its stock kickstart.zip.
# Serial goes to the log given as $1.
#
# Two network cards on separate QEMU user-mode networks, as on the
# AmigaOne (run-amigaone-virtionet.sh):
#   rtl8139    10.0.2.0/24 (the guest's remote-control daemon, MCPd, on
#              host port 4432; QMP 14432)
#   virtio-net 10.0.3.0/24 (the card under test)
# $2 replaces the virtio-net device options (default: one transitional card).
# PCAP=<windows path> captures the virtio network; VNET2=<device options>
# adds a second card on 10.0.4.0/24.
# Machine paths (QEMU, BBOOT, MACHINES, SHARE) come from tools/qemu/local.env:
# copy local.env.example there and edit it.
. "$(dirname "$0")/local.env" 2>/dev/null || { echo "tools/qemu/local.env is missing: copy local.env.example and set the paths" >&2; exit 1; }
LOG="${1:?log path}"
VNETDEV="${2:-virtio-net-pci,netdev=vnet,mac=52:54:00:12:34:57}"
PCAPARGS=()
if [ -n "${PCAP:-}" ]; then PCAPARGS=(-object "filter-dump,id=vnetdump,netdev=vnet,file=$PCAP"); fi
VNET2ARGS=()
if [ -n "${VNET2:-}" ]; then
  VNET2ARGS=(-netdev 'user,id=vnet2,net=10.0.4.0/24,dhcpstart=10.0.4.15' -device "$VNET2")
fi
exec "$QEMU" \
  -M pegasos2 \
  -kernel "$BBOOT" \
  -initrd "$MACHINES\mcpd-peg2-upd3\kickstart.zip" \
  -rtc base=localtime -accel tcg -vga none -serial stdio \
  -device es1370,addr=0x08 \
  -append 'serial debuglevel=1' \
  -display sdl,gl=off,show-cursor=off,full-screen=off \
  -device rtl8139,addr=0x09,netdev=nic \
  -netdev 'user,id=nic,hostname=mcpd-peg2-upd3,hostfwd=tcp::4432-:4322,hostfwd=udp::4432-:4323' \
  -netdev 'user,id=vnet,net=10.0.3.0/24,dhcpstart=10.0.3.15' \
  -device "$VNETDEV" \
  "${PCAPARGS[@]}" \
  "${VNET2ARGS[@]}" \
  -name mcpd-peg2-upd3 \
  -device virtio-scsi-pci,id=scsi0 \
  -drive "file=$MACHINES\mcpd-peg2-upd3\hd0-vnet.qcow2,if=none,id=vd0,format=qcow2" \
  -device scsi-hd,drive=vd0,bus=scsi0.0,channel=0,scsi-id=0,lun=0 \
  -fsdev "local,security_model=mapped-xattr,id=fsdev0,path=$SHARE" \
  -device virtio-9p-pci,id=fs0,fsdev=fsdev0,mount_tag=SHARED \
  -m 2048M \
  -drive if=none,id=cd -device ide-cd,unit=1,drive=cd,bus=ide.1 \
  -device sm501 \
  -qmp tcp:127.0.0.1:14432,server=on,wait=off \
  > "$LOG" 2>&1
