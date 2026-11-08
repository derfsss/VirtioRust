#!/bin/bash
# One boot of the qemu-amigaone machine for virtionet.device testing, and
# for testing another bsdsocket.library stack beside or instead of Roadshow,
# on a copy of the machine's disk (the original is untouched) with the stock
# kickstart. Serial goes to the log given as $1.
#
# Network (all stacks on virtio-net; no rtl8139 for this project's tests):
#   virtio-net 10.0.3.0/24 (netdev "vnet", unit 0): the guest's
#              remote-control daemon (MCPd) ports 4423 tcp/udp are forwarded
#              to it (10.0.3.15:4322/4323), so it is reachable through
#              whichever stack owns unit 0 (Roadshow, or another stack as
#              the drop-in); 127.0.0.1:4520/tcp and 4521/udp go to
#              10.0.3.15:4510/4511
#   virtio-net 10.0.4.0/24 (netdev "vnet2", unit 1, slot 0x0c): a second
#              stack beside Roadshow; 127.0.0.1:4510/4511 to 10.0.4.15
# VNET3=1 adds a third card, 10.0.5.0/24 (unit 2, slot 0x0d), for a
# two-interface test of a second stack beside Roadshow;
# 127.0.0.1:4530/4531 go to 10.0.5.15:4510/4511.
# NOVNET2=1 leaves the second card out. RTL=1 is the old layout: the
# rtl8139 back (10.0.2.0/24) with MCPd's 4423 on it and the first virtio
# card's 4322 on host 4433; only for a disk whose Roadshow still uses the
# rtl8139, or for comparing virtionet.device against it.
# The guest's Roadshow needs DEVS:NetInterfaces/VIRTIONET (unit 0) for MCPd
# to come up on this layout: tools/qemu/dropin/roadshow-virtio sets it.
# $2 replaces the first virtio-net device's options (default: one
# transitional card), e.g. "virtio-net-pci-non-transitional,netdev=vnet" or
# "virtio-net-pci,netdev=vnet,mrg_rxbuf=off".
# STACK=rustsocket starts a second machine, so that Roadshow and another
# stack can run at once: its own disk (with the other stack as
# bsdsocket.library), its own QEMU name, MCPd on host 4425, QMP 14425, and
# the forwards below moved up by 100 (4620/4621 unit 0, 4610/4611 unit 1,
# 4630/4631 unit 2). The default (STACK=roadshow) is the machine described
# above.
# Machine paths (QEMU, BBOOT, MACHINES, SHARE) come from tools/qemu/local.env:
# copy local.env.example there and edit it.
. "$(dirname "$0")/local.env" 2>/dev/null || { echo "tools/qemu/local.env is missing: copy local.env.example and set the paths" >&2; exit 1; }
case "${STACK:-roadshow}" in
  roadshow)  NAME=base_a1_backup; DISK=hd0-vnet.qcow2;    MCPD=4423; QMP=14423; P0=4520; P1=4510; P2=4530; L4=12360; R4=12361; MAC4=52:54:00:12:34:6a ;;
  rustsocket) NAME=base_a1_rs;    DISK=hd0-vnet-rs.qcow2; MCPD=4425; QMP=14425; P0=4620; P1=4610; P2=4630; L4=12361; R4=12360; MAC4=52:54:00:12:34:6b ;;
  *) echo "STACK must be roadshow or rustsocket"; exit 2 ;;
esac
if [ "${RTL:-}" = 1 ] && [ "$NAME" != base_a1_backup ]; then echo "RTL=1 is for the Roadshow machine only"; exit 2; fi
LOG="${1:?log path}"
VNETDEV="${2:-virtio-net-pci,netdev=vnet,mac=52:54:00:12:34:57}"
# PCAP=<windows path> also captures the first virtio network to that file.
PCAPARGS=()
if [ -n "${PCAP:-}" ]; then PCAPARGS=(-object "filter-dump,id=vnetdump,netdev=vnet,file=$PCAP"); fi
# VNET2=<device options> replaces the second card's options.
VNET2ARGS=()
if [ "${NOVNET2:-}" != 1 ]; then
  VNET2ARGS=(-netdev "user,id=vnet2,net=10.0.4.0/24,dhcpstart=10.0.4.15,hostfwd=tcp:127.0.0.1:$P1-10.0.4.15:4510,hostfwd=udp:127.0.0.1:$((P1+1))-10.0.4.15:4511" -device "${VNET2:-virtio-net-pci,netdev=vnet2,mac=52:54:00:12:34:58,addr=0x0c}")
fi
VNET3ARGS=()
if [ "${VNET3:-}" = 1 ]; then
  VNET3ARGS=(-netdev "user,id=vnet3,net=10.0.5.0/24,dhcpstart=10.0.5.15,hostfwd=tcp:127.0.0.1:$P2-10.0.5.15:4510,hostfwd=udp:127.0.0.1:$((P2+1))-10.0.5.15:4511" -device 'virtio-net-pci,netdev=vnet3,mac=52:54:00:12:34:59,addr=0x0d')
fi
# VNET4=1 adds a card on a segment the two machines share (a dgram netdev
# between 127.0.0.1 ports 12360 and 12361; no slirp, no DHCP: 10.0.6.0/24,
# addresses set by hand), for one machine routing for the other (IP
# forwarding).
VNET4ARGS=()
if [ "${VNET4:-}" = 1 ]; then
  VNET4ARGS=(-netdev "dgram,id=vnet4,local.type=inet,local.host=127.0.0.1,local.port=$L4,remote.type=inet,remote.host=127.0.0.1,remote.port=$R4" -device "virtio-net-pci,netdev=vnet4,mac=$MAC4,addr=0x0b")
fi
RTLARGS=()
MCPDFWD="hostfwd=tcp::$MCPD-10.0.3.15:4322,hostfwd=udp::$MCPD-10.0.3.15:4323"
if [ "${RTL:-}" = 1 ]; then
  RTLARGS=(-device rtl8139,addr=0x0a,netdev=nic -netdev 'user,id=nic,hostname=base_a1_backup,hostfwd=tcp::4423-:4322,hostfwd=udp::4423-:4323')
  MCPDFWD='hostfwd=tcp::4433-10.0.3.15:4322'
fi
exec "$QEMU" \
  -M amigaone \
  -kernel "$BBOOT" \
  -device "loader,addr=0x600000,file=$MACHINES\base_a1_backup\kickstart.zip" \
  -rtc base=localtime -accel tcg -vga none -serial stdio \
  "${RTLARGS[@]}" \
  -netdev "user,id=vnet,net=10.0.3.0/24,dhcpstart=10.0.3.15,hostname=$NAME,$MCPDFWD,hostfwd=tcp:127.0.0.1:$P0-10.0.3.15:4510,hostfwd=udp:127.0.0.1:$((P0+1))-10.0.3.15:4511" \
  -device "$VNETDEV" \
  "${PCAPARGS[@]}" \
  "${VNET2ARGS[@]}" \
  "${VNET3ARGS[@]}" \
  "${VNET4ARGS[@]}" \
  -append 'serial debuglevel=1' \
  -display sdl,gl=off,show-cursor=off,full-screen=off \
  -device es1370,addr=0x09 -name "$NAME" \
  -device virtio-scsi-pci,id=scsi0 \
  -drive "file=$MACHINES\\base_a1_backup\\$DISK,if=none,id=vd0,format=qcow2" \
  -device scsi-hd,drive=vd0,bus=scsi0.0,channel=0,scsi-id=0,lun=0 \
  -fsdev "local,security_model=mapped-xattr,id=fsdev0,path=$SHARE" \
  -device virtio-9p-pci,id=fs0,fsdev=fsdev0,mount_tag=SHARED \
  -m 2048M \
  -drive if=none,id=cd -device ide-cd,unit=1,drive=cd,bus=ide.1 \
  -device sm501 \
  -qmp "tcp:127.0.0.1:$QMP,server=on,wait=off" \
  > "$LOG" 2>&1
