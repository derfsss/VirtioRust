#!/bin/bash
# boot.sh MACHINE LOG [virtio-net device options]
# Boot one of the virtionet test machines and wait until the guest's
# remote-control daemon (MCPd) answers.
#   MACHINE: amigaone (Roadshow, MCPd 4423) | amigaone-rs (the second
#            AmigaOne, for another stack, MCPd 4425; both may run at once)
#            | pegasos2 | sam460
#            | vscsi (the AmigaOne for virtio-scsi)
#            | virtiorng (the AmigaOne for virtio-rng; the third argument is its
#            extra QEMU options, KICKZIP=stock for the disk build)
# Waits only for this machine's own QEMU (other QEMUs may be running).
# VNET2=<device options> adds a second card (see the run-*-virtionet.sh).
# Exit 0 when MCPd's port answers, 1 on a timeout.
set -u
M="${1:?machine}"; LOG="${2:?log}"; DEV="${3:-}"
R="$(cd "$(dirname "$0")/../.." && pwd)"
case "$M" in
  amigaone) S=run-amigaone-virtionet.sh; NAME=base_a1_backup; PORT=4423; export STACK=roadshow ;;
  amigaone-rs) S=run-amigaone-virtionet.sh; NAME=base_a1_rs; PORT=4425; export STACK=rustsocket ;;
  # Driver comparison, both on Roadshow (copies of the Roadshow disk), MCPd
  # on virtio unit 0 as usual, the card under test in the second slot
  # (10.0.4.0/24, Roadshow interface TESTNIC): the rtl8139 under the
  # Roadshow machine's name and ports, the Intel 82574L (e1000e.device)
  # under the second machine's. Both may run at once.
  cmp-rtl) S=run-amigaone-virtionet.sh; NAME=base_a1_backup; PORT=4423; export STACK=roadshow DISK_OVERRIDE=hd0-cmp-rtl.qcow2 VNET2="rtl8139,netdev=vnet2,mac=52:54:00:12:34:61,addr=0x0c" ;;
  cmp-e1000e) S=run-amigaone-virtionet.sh; NAME=base_a1_rs; PORT=4425; export STACK=rustsocket DISK_OVERRIDE=hd0-cmp-e1000e.qcow2 VNET2="e1000e,netdev=vnet2,mac=52:54:00:12:34:60,addr=0x0c" ;;
  vscsi)    S=run-amigaone-vscsi.sh;    NAME=base_a1_backup; PORT=4423 ;;
  virtiorng) S=run-amigaone-virtiorng.sh; NAME=base_a1_backup; PORT=4423 ;;
  pegasos2) S=run-pegasos2-virtionet.sh; NAME=mcpd-peg2-upd3; PORT=4432 ;;
  sam460)   S=run-sam460-virtionet.sh;   NAME=backup-sam460-upd3; PORT=4321 ;;
  *) echo "unknown machine $M"; exit 2 ;;
esac
mine() {
  powershell -NoProfile -Command "@(Get-CimInstance Win32_Process -Filter \"Name='qemu-system-ppc.exe'\" | Where-Object { \$_.CommandLine -match '-name $NAME' }).Count"
}
# Never quit a running machine from here: a QMP quit does not let the
# guest's filesystems write out, and a file copied just before is lost.
# Stop it with a tool that syncs the guest's disks first.
n=0
while [ "$(mine | tr -d '\r')" != "0" ]; do
  [ $n -ge 30 ] && { echo "$NAME is still running"; exit 1; }
  sleep 1; n=$((n+1))
done
rm -f "$LOG"
(MSYS_NO_PATHCONV=1 bash "$R/tools/qemu/$S" "$LOG" ${DEV:+"$DEV"} &)
# MCPd answers on its port once AmigaOS is up (the Sam460 prints nothing
# to the serial port after U-Boot, so the log cannot be used there).
n=0
until python -I -c "import socket,sys; s=socket.create_connection(('127.0.0.1',$PORT),2); s.close()" 2>/dev/null; do
  [ $n -ge 300 ] && { echo "MCPd on $PORT did not answer"; exit 1; }
  [ "$(mine | tr -d '\r')" = "0" ] && { echo "$NAME exited:"; tail -3 "$LOG"; exit 1; }
  sleep 2; n=$((n+2))
done
# Another QEMU may own the port (a clashing host forward): this one must be up.
sleep 2
[ "$(mine | tr -d '\r')" = "0" ] && { echo "$NAME exited:"; tail -3 "$LOG"; exit 1; }
# The port is forwarded by QEMU before the guest listens: also wait for the
# serial ready line where there is one, else a fixed margin.
if [ "$M" != sam460 ]; then
  until grep -a -q "MCPd\] ready" "$LOG" 2>/dev/null || [ $n -ge 300 ]; do sleep 1; n=$((n+1)); done
else
  sleep 60; n=$((n+60))
fi
echo "$M up after ${n}s"
