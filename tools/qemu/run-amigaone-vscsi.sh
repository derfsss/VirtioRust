#!/bin/bash
# One boot of the qemu-amigaone machine for the Rust virtio-scsi driver, on
# a copy of the machine's disk (the original is untouched) with the stock
# kickstart. Serial goes to the log given as $1.
#
# Two virtio-scsi controllers:
#   scsi0  virtio-scsi-pci (1af4:1004): the system disk, driven by the C
#          virtioscsi.device in the kickstart, exactly as the machine has it
#   scsi1  virtio-scsi-pci-non-transitional (1af4:1048) at PCI 00:0f: the
#          test disks, for the Rust vscsirs.device (it claims 1048 only)
# Test disks (scsi1, target 0, 1, ...): $TESTDISKS, a space-separated list of
# "file[,format][,opts]" (default: the 64 MB raw vscsi-test/t0.img), e.g.
#   TESTDISKS="D:/vscsi-test/t0.img D:/vscsi-test/big.qcow2,qcow2 D:/vscsi-test/4k.img,raw,logical_block_size=4096,physical_block_size=4096 cd"
# ("cd" adds an empty CD-ROM drive).
# The rtl8139 carries the guest's remote-control daemon (MCPd) (10.0.2.0/24,
# host port 4423, QMP 14423), as on the stock qemu-amigaone setup. No work
# partition.
# Machine paths (QEMU, BBOOT, MACHINES, SHARE) come from tools/qemu/local.env:
# copy local.env.example there and edit it.
. "$(dirname "$0")/local.env" 2>/dev/null || { echo "tools/qemu/local.env is missing: copy local.env.example and set the paths" >&2; exit 1; }
LOG="${1:?log path}"
# KICKZIP=<zip>: another kickstart (default: the machine's stock one), e.g.
# kickstart_rustscsi.zip, a copy with the Rust virtioscsi.device in it
# (tools/qemu/kick-replace.py). Then the Rust driver runs the system disk.
KICK="${KICKZIP:-$MACHINES\base_a1_backup\kickstart.zip}"
T="$MACHINES\vscsi-test"
DISKS="${TESTDISKS:-$T\\t0.img}"
DISKARGS=()
i=0
for spec in $DISKS; do
  if [ "$spec" = cd ]; then
    # An empty CD-ROM drive.
    DISKARGS+=(-device "scsi-cd,bus=scsi1.0,channel=0,scsi-id=$i,lun=0")
    i=$((i+1))
    continue
  fi
  IFS=',' read -r file fmt opts <<< "$spec"
  fmt="${fmt:-raw}"
  DISKARGS+=(-drive "file=$file,if=none,id=td$i,format=$fmt" \
             -device "scsi-hd,drive=td$i,bus=scsi1.0,channel=0,scsi-id=$i,lun=0${opts:+,$opts}")
  i=$((i+1))
done
# SYSDISK2=<raw file>: a second disk on the SYSTEM controller (scsi0, target 1,
# snapshot: never written; SYSDISK2SNAP=off writes it, e.g. for a benchmark
# disk), so the C driver, which drives only that
# controller, has a unit 1 (the C driver's own test suite needs one).
# NOTESTCTRL=1: no second controller (and no test disks) at all.
TESTCTRL=(-device virtio-scsi-pci-non-transitional,id=scsi1,addr=0x0f "${DISKARGS[@]}")
if [ -n "${NOTESTCTRL:-}" ]; then
  TESTCTRL=()
fi
# GDBPORT=<port>: QEMU's GDB stub on 127.0.0.1:<port> (the guest runs; attach
# to stop it), for tools/qemu/gdbwatch.py.
GDBARGS=()
if [ -n "${GDBPORT:-}" ]; then
  GDBARGS=(-gdb "tcp:127.0.0.1:$GDBPORT")
fi
SYS2ARGS=()
if [ -n "${SYSDISK2:-}" ]; then
  SYS2ARGS=(-drive "file=$SYSDISK2,if=none,id=sys2,format=raw,snapshot=${SYSDISK2SNAP:-on}" \
            -device "scsi-hd,drive=sys2,bus=scsi0.0,channel=0,scsi-id=1,lun=0")
fi
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
  -drive "file=$MACHINES\base_a1_backup\hd0-vscsi.qcow2,if=none,id=vd0,format=qcow2" \
  -device scsi-hd,drive=vd0,bus=scsi0.0,channel=0,scsi-id=0,lun=0 \
  "${SYS2ARGS[@]}" \
  "${GDBARGS[@]}" \
  "${TESTCTRL[@]}" \
  -fsdev "local,security_model=mapped-xattr,id=fsdev0,path=$SHARE" \
  -device virtio-9p-pci,id=fs0,fsdev=fsdev0,mount_tag=SHARED \
  -m 2048M \
  -drive if=none,id=cd -device ide-cd,unit=1,drive=cd,bus=ide.1 \
  -device sm501 \
  -qmp tcp:127.0.0.1:14423,server=on,wait=off \
  > "$LOG" 2>&1
