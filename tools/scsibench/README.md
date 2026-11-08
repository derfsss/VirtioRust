# scsibench: the virtioscsi.device test bench

Tries to break virtioscsi.device on QEMU's AmigaOne, the only machine it is
run on. The scripts run on the guest from the QEMU shared folder
(`SHARED:sbench`, results in `out/` and `soak/`); copy this folder there
with the binaries:

    SHARED:sbench/   pre fs fs-par fs-remount soak-prep soak-torture soak-fs
                     soak-end dev sb.mountlist
                     vscsitest (testprogs/vscsitest), vscsirs.device and
                     virtioscsi.device (drivers/virtio-scsi), test_virtioscsi
                     (drivers/virtio-scsi/bench/build-ctests-fixed.sh), cu9
                     (bench/build-cu9.sh), expunge (amigaos4/testprogs/expunge)

`tools/vnetbench/boot.sh` boots the machine and waits for the guest's MCPd
remote-control daemon; the runs are started on the guest through it, some
as two or three jobs at once.

## Machines

The launch script `tools/qemu/run-amigaone-vscsi.sh` takes QEMU, the
kickstart and disk paths and the shared folder from `tools/qemu/local.env`
(copy `tools/qemu/local.env.example` and edit it). Both boots use the same
five test disks on the test controller: t0.img 64 MB, t1-4k.img with
4096-byte sectors, t2-3t.qcow2 3 TB, an empty CD-ROM, t3-fs.img 256 MB.

```bash
T=/path/to/vscsi-test
export TESTDISKS="$T/t0.img $T/t1-4k.img,raw,logical_block_size=4096,physical_block_size=4096 $T/t2-3t.qcow2,qcow2 cd $T/t3-fs.img"
# the drop-in (Kickstart virtioscsi.device 1.1 in place of the C driver),
# from a kickstart copy made with tools/qemu/kick-replace.py:
KICKZIP=/path/to/kickstart_rustscsi.zip tools/vnetbench/boot.sh vscsi LOG
# the development build (stock kickstart; vscsirs.device from DEVS:):
tools/vnetbench/boot.sh vscsi LOG
```

Drop-in units: 0 the system disk, 1 t0, 2 t1-4k, 3 t2-3t, 4 the CD, 5 t3-fs.
Development build units (test controller only): 0 t0, 1 t1-4k, 2 t2-3t, 3
the CD, 4 t3-fs. Shut the guest down cleanly before stopping QEMU, so that
its filesystems are written out.

## The runs

| Script | Boot | What |
|---|---|---|
| `pre` | drop-in | vscsitest on units 1-5 (109 checks; WRITES them); SYS:Libs copied to DH0 through the driver and hashed; the C driver's suite with the start-up handshake; cu9 50 rounds with real reads and 50 with children that exit at once; `vscsitest TORTURE` on units 1-3 (8 kids, 2 x 60 s) |
| `fs`, then `fs-par A` + `fs-par B` as two jobs, reboot, `fs-remount` | drop-in | FFS2 (SBA:) and SFS (SBB:) on unit 5 (`sb.mountlist`): format, copy SYS:C and SYS:Libs, hash; two writers at once; everything hashed again after a reboot |
| `soak-prep`, then `soak-torture` + `soak-fs SBA ffs` + `soak-fs SBB sfs` as three jobs, then `soak-end` | drop-in, fresh boot | 30 minutes: the torture on units 1-3 (8 kids at priority -5, below the shells) while both filesystems copy, hash and delete SYS:Libs round after round; free memory before and after |
| `mem-isolate` | drop-in, after the soak | free memory around TORTURE alone and around two sets of 40 filesystem rounds |
| `dev` | development build | the torture on units 0-2 with a controller reset every 2 s (VSCSI_CMD_TESTFAULT; the drop-in has no such command), then 10 rounds of vscsitest + expunge with free memory |

Host checks: `tools/md5cmp.py` for every hash list (`pre-libs.md5` against
`pre-dh0copy.md5`, `fs-src.md5` against the fs lists, `pre-libs.md5`
against the fs-par lists), `tools/soakcheck.py <shared folder>/sbench/soak`
for the soak (every round of every filesystem, and at least one round each).

`vscsitest TORTURE` (testprogs/vscsitest/src/torture.rs) is the oracle for
raw I/O: every block written carries its unit, kid, block number and a
generation, every read is checked, and each kid re-reads its whole region
at the end of a round.

## Results (virtioscsi.device 1.1 drop-in, vscsirs.device 53.1)

QEMU 11.0.50 AmigaOne.

| Run | Result |
|---|---|
| `pre` (drop-in) | vscsitest 109/109 on units 1-5; SYS:Libs through the driver to DH0 and back 70/70; the C suite (handshake) 41 PASS / 10 FAIL, the 10 are deliberate differences from the C driver; cu9 50/50 real reads, 50/50 immediate exits; TORTURE units 1-3, 8 kids, 2 x 60 s: 15/15, 1.12M blocks checked, 0 wrong, 0 I/O errors, 369 aborted reads, memory equal |
| `fs`, `fs-par`, `fs-remount` (drop-in) | FFS2 and SFS on unit 5: 255/255 files each after copying, 70/70 for each of two writers at once, and all four again after a reboot |
| `dev` (development build) | TORTURE units 0-2 with 180 controller resets: 17/17, 981k blocks checked, 0 wrong, 20 I/O errors while resetting (tolerated), every region right; 10 rounds of vscsitest 79/79 + expunge: each expunge unloaded the device (it initialised again 10 times), free memory flat within 30 KB after round 2 |
| 30-minute soak (drop-in) | TORTURE units 1-3 at priority -5, 2 x 900 s: 3.82M blocks checked, 0 wrong, 0 I/O errors, 0 lost requests, every region right; meanwhile FFS2 216 rounds and SFS 1094 rounds of copy SYS:Libs + MD5 + delete, every round 70/70 (`soakcheck.py`); no crash |

Memory: free memory fell 25.8 MB over the soak (1859776512 to 1833951232,
both filesystems still mounted). Measured afterwards in the same boot
(`mem-isolate`): TORTURE alone -100 KB (0 bytes between its rounds), then 40
rounds on both filesystems +364 KB, 40 more -37 KB. So nothing keeps
leaking: the loss came while both filesystems ran at once and stopped (an
earlier soak saw the same: the filesystems hold it), and the driver's
request paths are flat in every torture run without other jobs. The
soak's TORTURE reported its in-run memory comparison as a FAIL (12.7 MB):
it was measured while the two filesystem jobs allocated, so with kidpri
below 0 the torture now shows that figure without judging it.
