#!/bin/bash
# One boot of the Sam460ex machine for virtionet.device testing, on a copy
# of its disk (the original is untouched). Its usual command line (the
# remote-control daemon, MCPd, on 4321; QMP 14321) without the extra
# scsi.raw disk, plus the virtio-net card, added last: with it earlier in
# the PCI order U-Boot does not boot. Serial goes to $1.
#
#   rtl8139    10.0.2.0/24 (MCPd)
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
  -M sam460ex -rtc base=localtime -accel tcg -serial stdio \
  -device rtl8139,netdev=nic \
  -netdev 'user,id=nic,hostname=backup-sam460-upd3,hostfwd=tcp::4321-:4322,hostfwd=udp::4321-:4323' \
  -display sdl,gl=off,show-cursor=off,full-screen=off \
  -device es1370 -name backup-sam460-upd3 \
  -device virtio-scsi-pci,id=scsi0 \
  -fsdev "local,security_model=mapped-xattr,id=fsdev0,path=$SHARE" \
  -device virtio-9p-pci,id=fs0,fsdev=fsdev0,mount_tag=SHARED \
  -m 2048M \
  -device sii3112 -drive if=none,id=cd -device ide-cd,unit=0,drive=cd,bus=ide.1 \
  -drive "if=none,id=hd0,file=$MACHINES\backup-sam460-upd3\hd0-vnet.qcow2,format=qcow2" \
  -device ide-hd,unit=0,drive=hd0,bus=ide.0 \
  -netdev 'user,id=vnet,net=10.0.3.0/24,dhcpstart=10.0.3.15' \
  -device "$VNETDEV" \
  "${PCAPARGS[@]}" \
  "${VNET2ARGS[@]}" \
  -qmp tcp:127.0.0.1:14321,server=on,wait=off \
  > "$LOG" 2>&1
