# VirtioRust

Rust drivers for QEMU's virtio devices on AmigaOS 4.1 Final Edition, built
on the [`amigaos4`](https://github.com/derfsss/rust-amigaos4-crate) crate
(the `amigaos4/` submodule) for the AmigaLabs `powerpc-unknown-amigaos`
target. MIT licence. Every virtio device we write a Rust driver for lives
here.

| Driver | Device | Where |
|---|---|---|
| virtionet.device | SANA-II network driver for `virtio-net-pci` | `drivers/virtio-net` |
| virtiorng.device | entropy device for `virtio-rng-pci` (Kickstart and disk builds) | `drivers/virtio-rng` |
| virtioscsi.device | block driver for `virtio-scsi-pci` (a drop-in for the C driver, [VirtualSCSIDevice](https://github.com/derfsss/VirtualSCSIDevice); Kickstart boot) and `vscsirs.device` (development build) | `drivers/virtio-scsi` |

The generic virtio layer (transport, modern interface, rings, queues, and
the net and scsi definitions) is the crate's `virtio` feature; the drivers
here use it.

## Layout

- `drivers/` the drivers, one Cargo package each (a staticlib linked with
  the C glue by the package's Makefile).
- `testprogs/` AmigaOS programs that test them: `s2test` (SANA-II checker
  and torture), `rngread` (virtio-rng checks and TORTURE), `vscsitest`
  (block device checks; WRITES EVERY TESTED UNIT), `netprof` (network
  profiler, any SANA-II driver and bsdsocket.library stack).
- `tools/` host side: `build-package.sh`, `build-all.sh`; `netprof-host`
  (netprof's peer on a PC); `vnetbench/`, `rngbench/` and `scsibench/` (the
  test benches); `qemu/` (launch scripts, scripts that make kickstart
  copies, the QEMU GDB stub clients); `vscsicheck.py`, `md5cmp.py`,
  `soakcheck.py`.
- `amigaos4/` the crate, a git submodule pinned to a commit.

## Building

Linux or WSL. Once per clone:

```bash
git submodule update --init
```

Then, as for the crate (its README, "Building"): build the AmigaLabs
compiler with `amigaos4/tools/build-toolchain.sh` and pull
`walkero/amigagccondocker:os4-gcc15-3.6.1` into the Docker that Linux sees.
Build one package, or all of them:

```bash
tools/build-package.sh drivers/virtio-scsi
MAKE_ARGS=DROPIN=1 tools/build-package.sh drivers/virtio-scsi --features dropin
tools/build-all.sh
```

`netprof-host` is a plain host crate: `cd tools/netprof-host && cargo build --release`.

To move to a newer crate: `git -C amigaos4 fetch && git -C amigaos4 checkout <commit>`,
rebuild everything, then commit the new pointer.

## Running the QEMU machines

The launch scripts in `tools/qemu/` (`run-<machine>-<device>.sh`) take
QEMU, the kickstart and disk paths and the shared folder from
`tools/qemu/local.env`: copy `tools/qemu/local.env.example` and edit it.
Each machine runs on a copy of your own AmigaOS 4.1 disk image for it, so
the original is untouched. The bench files reach the guest through a
virtio-9p shared folder, `SHARED:` on the guest.

The benches drive the guest through the MCPd remote-control daemon, and
`tools/vnetbench/boot.sh` waits for it to answer. Shut the guest down
cleanly before stopping QEMU: a QEMU quit straight after copying a file
into the guest can lose the file, since the guest's filesystem has not
written it yet. `boot.sh` refuses to start a machine that is still
running.

## virtio-rng driver

`drivers/virtio-rng` builds `virtiorng.device`, an interrupt-driven driver
for QEMU's `-device virtio-rng-pci`, as a Kickstart module (the same
two-Resident shape as the crate's `rusttest.device`), or as a disk-loaded
device (below).

- Several cards: each virtio-rng card is a unit (up to 4), numbered in
  the order found, transitional 1af4:1005 cards first, then modern-only
  1af4:1044 ones; `OpenDevice("virtiorng.device", n)` opens card n and
  fails with `IOERR_OPENFAIL` past the last. Every card found is a unit:
  one that cannot be set up (at boot in the Kickstart build, at its open
  in the disk build) keeps its number and fails to open with
  `IOERR_OPENFAIL`, with the reason on serial. Each unit has its own
  device state, queue and mutex, unit task ("virtiorng.device unit n"),
  interrupt server (handed its unit as `is_Data`) and counters; one reset
  callback resets every card. All of a unit's state is one `Unit` in a
  static array, so a warm restart resets it with one write per unit.
- COLDSTART: finds each PCI function (transitional 1af4:1005, or
  modern-only 1af4:1044), locks it, and opens it with
  `virtio::PciTransport::open`: the modern (virtio 1.x) interface if its
  PCI capabilities are there and a write and read-back of the status
  register works, otherwise the legacy interface (I/O BAR 0), saying why.
  The one queue is a `virtio::Virtqueue` (little-endian on the modern
  interface, the CPU's order on the legacy one), mapped once with
  `StartDMA`. It reads a first sample by polling, then creates the unit
  task. Without the device, the init fails and exec does not add it.
- AFTERDOS: opens `virtiorng.device` by name, checks two 64-byte reads and
  a 10000-byte read, checks that every piece completed by interrupt, and
  prints `virtiorng.device: self-test PASSED`.
- The unit task adds an interrupt server (`amigaos4::interrupt::IntServer`)
  on the chain from `PCIDevice->MapInterrupt()` and turns the device's
  interrupts on. `BeginIO` queues each request on the task's port. For each
  4 KB piece the task kicks the device and `Wait()`s for the interrupt's
  signal, with a 1 s `timer.device` timeout so a lost interrupt cannot hang
  a caller.
- The interrupt server asks the device first: it reads the virtio ISR
  register, which also clears the interrupt and drops the level-triggered
  INTx line. If the device raised it, the server `Signal()`s the task and
  returns 1; otherwise it returns 0 so the rest of a shared chain runs. It
  does not allocate, lock or log; it counts (raised, claimed, passed on,
  signalled) and times itself on the timebase (`arch::timebase`, `mfspr`
  268/269), and its one exit clears any `lwarx` reservation
  (`arch::clear_reservation`). A storm breaker resets the device if claims
  pile up unserved, so a stuck line ends in a failed request instead of a
  hung machine. The task records the Signal-to-run latency.
- Up to eight pieces are in flight at once (descriptor n for slot n).
  Small reads go through the slots' 4 KB bounce buffers (one physically
  contiguous block each, mapped per piece with `DmaMapping`). For a read
  whose cache-line-aligned interior is 8 KB or more, that interior is
  mapped once with `StartDMA` and the device writes it directly, in pieces
  of up to 64 KB along its physical blocks; only the head and tail up to
  the first and from the last line boundary are bounced. StartDMA and
  EndDMA each do one cache operation on the lines they cover, so a line
  shared between the device's bytes and anyone else's could lose writes on
  a machine whose caches do not snoop DMA; edges are rounded to the CPU's
  line size or 64 bytes, whichever is larger. If StartDMA refuses the
  buffer, the whole read is bounced. A failure with pieces in flight resets
  the device at once, before the caller's buffer is unmapped and returned.
  virtio-rng exists only in QEMU, which is cache-coherent on every machine
  it emulates (the Sam460 included), so this driver can never exercise the
  edge rule; it is there as the pattern for drivers on real non-coherent
  hardware.
- Commands: `CMD_READ`; `CMD_RESET` and `CMD_FLUSH` abort every queued
  request and the one in progress; `NSCMD_DEVICEQUERY` reports type 0 and
  the command list; anything else is `IOERR_NOCMD`. Requests wait on the
  driver's own queue under an exec mutex (no Forbid). `AbortIO` replies a
  queued request `IOERR_ABORTED` with nothing read, or stops the one in
  progress between pieces with what it has read. A queued request is
  marked `NT_MESSAGE` so `WaitIO` and `CheckIO` see it as pending.
- Device errors: before each request the unit task checks the device
  (DRIVER_OK gone, DEVICE_NEEDS_RESET or FAILED set, the storm breaker's
  reset, or pieces left in flight by a failure). If any holds, it resets
  the device, ends every outstanding mapping, builds a fresh queue,
  renegotiates and sets DRIVER_OK again. A successful read resets the
  count; after 8 recoveries in a row the unit's requests fail with
  `IOERR_UNITBUSY` and the reason on serial. A
  configuration-change interrupt also wakes the task.
  `VRNG_CMD_TESTFAULT` (9, nonstandard) resets the device behind the
  driver's back to test this.

To try the Kickstart build, add `Kickstart/virtiorng.device` and its
`MODULE` line to the kickstart, and add `-device virtio-rng-pci` to QEMU.

### Disk build

Loaded from disk: the same source with cargo feature `disk`, linked with
`make DISK=1` (`src/rng_glue_disk.c`, one `RTF_AUTOINIT` Resident), gives
`disk/virtiorng.device` for DEVS:

```bash
MAKE_ARGS=DISK=1 tools/build-package.sh drivers/virtio-rng --features disk
```

- Init finds and locks the cards, and nothing more.
- A unit is set up at its first open (card, queue, DMA memory, interrupt
  server, unit task; Open waits until the task is ready) and stopped at
  its last close.
- Stopping: the unit task resets its card BEFORE removing its interrupt
  server (a level-triggered line left asserted with nothing to claim it
  would storm the chain), withdraws and answers; Close then waits until
  exec has removed the task, since its last instructions are the
  segment's, before freeing the unit's memory. Both waits are bounded
  (3 s); a unit whose task does not go keeps the device loaded.
- Expunge waits for nothing (exec may call it from a low-memory handler):
  with no unit open it unlocks the cards and the device unloads. Close runs
  a deferred expunge (`LIBF_DELEXP`).
- `rngread EXPUNGE` unloads it without Forbid: OpenDevice, RemDevice (the
  Expunge defers), CloseDevice (the deferred expunge runs).

Tested on the QEMU AmigaOne with the stock kickstart (no virtiorng in it,
`KICKZIP=stock tools/qemu/run-amigaone-virtiorng.sh LOG "-device virtio-rng-pci -device virtio-rng-pci-non-transitional"`)
and the device in a RAM: directory added to DEVS: (`Assign DEVS: RAM:rusttest/devs ADD`):
three load / use / expunge cycles, rngread 9/9 on each unit, each unit's
task gone at its last close, each expunge unloading the device. Free
memory after the second and third expunge was the same to the byte (80 KB
below the first reading after one cycle, a one-off; no growth). `Avail
FLUSH` on this AmigaOS 4.1 does not expunge it (or other devices);
RemDevice does. With both units' rngread loops overlapping on the disk
build: 40 runs, all 9/9, 41 unit starts and 41 stops, the reset callback
added once per 0-to-1 change of running units (21 times) and never twice,
then the expunge.

Repeated with the device's heap in MEMF_SHARED memory
(`SharedExecAllocator`): the disk build three load / use / expunge cycles,
9/9 on both units every cycle, each expunge unloading the device, free
memory equal after the second and third; the Kickstart build 9/9 on both
units.

### rngread and test results

`testprogs/rngread` is a Shell tool for the device (`rngread [KB] [unit]`). It runs
nine checks: the device query, unsupported commands, a timed throughput
read, `AbortIO` on a queued and on an in-progress request, `CMD_FLUSH`
with one of each (on a 32 MB read; an attempt that lands before the read
starts or after it ends proves nothing and is repeated, up to 3 times), a read afterwards, a read after `VRNG_CMD_TESTFAULT`
(recovery), and 300001 bytes into a buffer at an odd address with guard
bytes either side (direct interior, bounced edges). It prints one line per check and
returns 0 when all pass. On the QEMU AmigaOne, in one session, 4 and 16 MB reads ran at 107 to 124 MB/s written straight into the caller's buffer, against 41 to 42 MB/s with every byte bounced (a build with the direct path off); earlier builds measured 46 to 66 MB/s bounced with eight pieces in flight and 19.7 to 24.4 MB/s one piece at a time. Throughput varies by up to about a quarter between QEMU sessions, so compare figures from one session only.

Two cards (QEMU AmigaOne, `tools/qemu/run-amigaone-virtiorng.sh LOG
"-device virtio-rng-pci -device virtio-rng-pci-non-transitional"`): units 0 and
1 on interrupt chains 26 and 27, both self-tests passed; `rngread` 9/9 on
each unit alone, and 40 runs with the two units' loops overlapping, all 9/9;
unit 2 is refused. One card: unit 0 9/9, unit 1 refused.

The interface each run used (QEMU 11.0.50; boot self-test and rngread 7/7,
the check count at the time, in every row):

| Machine | Device | Interface used |
|---|---|---|
| `-M amigaone` | `virtio-rng-pci` (transitional) | modern |
| `-M amigaone` | same, `--features force-legacy` build | legacy |
| `-M pegasos2` | `virtio-rng-pci` (transitional) | modern |
| `-M pegasos2` | `virtio-rng-pci-non-transitional` | modern (the only one) |

The modern interface works behind the AmigaOne's Articia S on this QEMU,
although it was seen failing there under QEMU 10.2.2. The legacy fallback
stays for machines where the probe fails; the `force-legacy` test build
exercises it here.

Earlier runs, legacy interface only, before the modern interface existed;
boot self-test and rngread 7/7 on each:

| Machine | PCI command at boot | Interrupt line | rngread 4 MB |
|---|---|---|---|
| `-M amigaone` (Articia S) | 0x0007 (already on) | line 10, chain 26, not shared in the run | 23.7 MB/s |
| `-M pegasos2` (MV64361) | 0x0000 (driver turned on I/O + bus master) | line 9, chain 25, shared: 92 raised, 5 claimed, 87 passed on | 20.6 MB/s |
| `-M sam460ex` (PPC460EX) | 0x0007 | line 32, chain 48, shared with sii3112ide.device: 1550 raised, 5 claimed, 1545 passed on | 12.4 MB/s |

`tools/qemu/run-amigaone-virtiorng.sh`, `run-pegasos2-virtiorng.sh` and
`run-sam460-virtiorng.sh` are the command lines (each machine's own, plus
`-device virtio-rng-pci`; `run-pegasos2-virtiorng.sh` takes the device as
an optional second argument). The AmigaOne and Pegasos II load a kickstart
copy with the driver added (`tools/qemu/kick-add.py` makes one), which must
go through `tools/qemu/zip-comment.py` after every change: bboot sizes the
zip from its last non-zero byte rounded up to 128, so a zip ending in its
zero comment-length field can lose the end of its directory and fail with
"Invalid zip file". The Sam460 loads its kickstart from its own disk, so
the driver and its `MODULE` line go into the guest's `SYS:Kickstart/`
first.

**What the Sam460 run does not prove.** The Sam460's caches are not
coherent with device DMA, but QEMU models every machine as coherent, so
the driver's cache maintenance ran without being tested: a missing flush
or invalidate would still pass here. Only a real Sam460 can show it. What
the run does show on the 460's core is the `mfspr` timebase (1150 MHz
there) and the driver sharing an interrupt line with a disk controller.

## virtio-net SANA-II driver

`drivers/virtio-net` builds `virtionet.device`, a SANA-II Revision 7
network driver for QEMU's `-device virtio-net-pci` (transitional, 1af4:1000)
and `virtio-net-pci-non-transitional` (modern only, 1af4:1041), loaded from
`DEVS:Networks/` by a TCP/IP stack such as Roadshow:

```bash
tools/build-package.sh drivers/virtio-net        # drivers/virtio-net/virtionet.device
tools/build-package.sh testprogs/s2test            # testprogs/s2test/s2test, the test tool
```

Interface file for Roadshow (`AddNetInterface <file>`):

```
device=virtionet.device
unit=0
address=10.0.3.15
netmask=255.255.255.0
```

What it does:

- Up to 4 cards, one unit each. Init only finds and locks them. A unit is
  set up at its first open and stopped at its last close, as the disk
  virtiorng is; Close runs a deferred expunge, and `expunge DEVICE
  networks/virtionet.device` unloads it.
- Modern interface first, legacy as the fallback (`--features force-legacy`
  test build). The header is 12 bytes with the modern interface or
  mergeable receive buffers, 10 bytes otherwise.
- Features taken: MAC, link status, MTU, mergeable receive buffers, the
  control queue (promiscuous, all-multicast, the MAC filter table, setting
  the station address), guest announce, speed and duplex. Checksum and
  segmentation offloads, multiqueue and VLAN filtering are not: SANA-II
  has no way to hand them to a stack.
- SANA-II: S2_SANA2HOOK (the stack's copy hook) or the OpenDevice copy
  functions (68k ones run through Emulate), per-opener reads and orphan
  reads, raw reads and writes, CONFIGINTERFACE, station and factory
  address, DEVICEQUERY (34 and 30 bytes), NSCMD_DEVICEQUERY, ONLINE and
  OFFLINE, events (ONLINE, OFFLINE, CONNECT, DISCONNECT, CONFIGCHANGED,
  ERROR/HARDWARE), global, extended, special and per-type statistics,
  S2_SAMPLE_THROUGHPUT, reference-counted multicast addresses and ranges,
  SANA2OPF_PROM and SANA2OPF_MINE, CMD_FLUSH and AbortIO.
- Speed: writes are copied and posted in BeginIO, in the stack's own task,
  under the unit's transmit lock. Only a write that finds every slot busy
  goes to the unit task. Each buffer pool is mapped once (virtio exists
  only in QEMU, which is cache-coherent). The driver skips a notify while
  the device says it is already working (VRING_USED_F_NO_NOTIFY).
- No receive drops for a busy stack: a frame of a type an opener reads that
  finds none of its reads queued stays in the ring (and the device holds
  back the rest) until the stack queues its next read; BeginIO wakes the
  unit task for it. After 100 ms without a read it is dropped. Without
  this, QEMU's bursts outrun Roadshow's 36 reads and each lost frame
  stalls TCP.
- Recovery: the unit task reads the device status at most every 100 ms,
  and a 1 s heartbeat makes sure it is read when the unit is idle. A reset
  device is set up again in place, with reads still pending, up to 8
  times in a row. `VNET_CMD_TESTFAULT` (0x8001, nonstandard) resets the
  device behind the driver's back to test this.

`testprogs/s2test` checks a SANA-II driver from a Shell (32 checks):
`s2test <unit> <device> [myip gwip] [mtu=N] [mbit=N]`. The `events` mode
waits up to 60 s for DISCONNECT then CONNECT (`events announce`: also
CONFIGCHANGED), driven from the host with QMP `set_link` and
`announce-self`. The `closepend` mode closes an opener with writes still
waiting; it uses e1000e_diag.device's TXMUTE command when the driver has
it. The `prom` mode holds a
promiscuous open while the host reads QMP `query-rx-filter`. `burst` starts 32
writes at once and aborts the last 8; `stats` prints the driver's special
statistics (the fourth is writes that waited for a slot), also while a stack
has the unit. The `test-tx-hold` driver build (4 slots, the device kicked only
by the unit task) makes writes wait, which QEMU otherwise never does: it sends
a frame inside the notify itself.

`tools/qemu/run-amigaone-virtionet.sh LOG [device options]` boots the
AmigaOne with no rtl8139. The first virtio-net card (unit 0) is on its own
user network, 10.0.3.0/24, and carries the guest's MCPd port; a second card
(unit 1) is on 10.0.4.0/24 (`NOVNET2=1` leaves it out, `VNET2=<device
options>` replaces its options). `VNET3=1` adds a third card on
10.0.5.0/24. `RTL=1` puts the rtl8139 back on 10.0.2.0/24, for comparing
virtionet.device against it. `PCAP=<file>` captures the first card's
network. The second argument replaces the first card's options, e.g.
`virtio-net-pci-non-transitional,netdev=vnet`.

Tested on QEMU 11.0.50 `-M amigaone`. The first rows ran on the final
build; rows marked * ran on the build before the transmit path moved to
the caller's task (50cf3a2), whose other paths did not change:

| Run | Result |
|---|---|
| final build: s2test, burst, expunge, then Roadshow on the reloaded driver | s2test 32/32; 32-write burst all returned; unloaded |
| `test-tx-hold` build (4 slots, writes wait for one) | s2test 32/32; burst 24 sent + 8 aborted while waiting, none lost |
| transitional card, modern interface | s2test 32/32 |
| `force-legacy` build, `mrg_rxbuf=off` (10-byte header) | s2test 32/32 |
| modern-only card next to a second card | s2test 32/32 on unit 1 |
| `force-legacy`, two cards, 10- and 12-byte headers * | s2test 32/32 on each |
| `host_mtu=1400,speed=100,duplex=full` * | MTU 1400 and 100 Mbit/s reported; s2test 32/32 |
| QMP link down, link up, announce * | events 0x400, 0x200, 0x100 |
| SANA2OPF_PROM openers | device promiscuous only while one is open |
| Roadshow 1.15 * | stack hook installed, interface configured and online |

Throughput through Roadshow 1.15 (figures vary by up to ~25% between QEMU
sessions); every transfer's sha256 matched:

| Transfer | virtio-net | rtl8139 |
|---|---|---|
| 64 MB download (wget 1.10.1) | 4.1-4.3 s (final build 4.16 s) | not reproduced, see below |
| 515.6 MB download, host disk, plain HTTP (wget 1.12) | 33.7 s (15.3 MB/s) | 54.8 s (9.4 MB/s) |
| 515.6 MB download, os4depot, HTTPS (curl 8.21.0) | 236 s (2.18 MB/s) | 496 s (1.04 MB/s) |
| 16 MB upload (HTTP POST) | 3.74-3.96 s (final build 3.80 s) | 4.04 s (final build's session: 3.94 s) |
| ping, steady state | 0.46-0.54 ms | ~0.47 ms (an earlier session) |

The 515.6 MB rows come from one later session. Downloads are about 1.6
times faster than over the rtl8139 on plain HTTP and about 2.1 times on
HTTPS. HTTPS is bound by TLS on the emulated CPU, not by either card. One
rtl8139 run of the 64 MB file with wget 1.10.1 took 293 s; with wget 1.12
the rtl8139 carries 9.4 MB/s, so that figure is left out. Whether wget
1.10.1 was the cause was not tested.

The upload is bound by the stack and the emulated CPU, not the card:
both cards take about the same time.

## virtio-scsi block driver

`drivers/virtio-scsi` is a Rust replacement for the C virtioscsi.device
(QEMU `-device virtio-scsi-pci` / `-non-transitional`), developed and
tested on QEMU's AmigaOne. It scans targets 0-7 x LUNs 0-7, answers the
trackdisk, TD64 and NSD commands, HD_SCSICMD (a pass-through), geometry
and change requests, and pipelines 16 commands per controller over
indirect descriptors, with AbortIO, a heartbeat, recovery and a reset
callback.

- `make` builds `vscsirs.device` for DEVS:, which claims modern-only
  controllers (1af4:1048) only, beside the C driver that runs the system
  disk; `vscsitest vscsirs.device 5` 109/109, FFS2 and SFS on one unit,
  a 20-minute soak, expunge rounds without memory drift.
- `MAKE_ARGS=DROPIN=1` with `--features dropin` builds the Kickstart
  `virtioscsi.device`; `tools/qemu/kick-replace.py` puts it in a copy of
  the kickstart zip. The AmigaOne boots from it, the system disk on the
  Rust driver.
- Speed (AmigaDiskBench 2.10, SFS, in the same boot as the C driver): 2.3x
  to 23x the C driver's MB/s, most on random 4K (130 against 5.6 MB/s).
- The C driver's test suite crashed DOS in its TEST 9 on either driver:
  an AmigaOS DOS race in its child-process start-up, not a driver defect.
  With a start-up handshake (`drivers/virtio-scsi/bench/test_virtioscsi-handshake.patch`,
  `drivers/virtio-scsi/bench/build-ctests-fixed.sh`) the suite completes on
  both drivers with no crash, and differs on the drop-in only by deliberate
  differences from the C driver; kick-check 109/109.
- Test bench: `tools/scsibench` (pre, filesystems, a 30-minute soak, the
  development build with resets and expunges; `vscsitest TORTURE`). All
  runs pass on the 1.1 drop-in; `tools/scsibench/README.md` has the runs
  and results.

## netprof: where the time goes in a network transfer

`testprogs/netprof` is a Shell program that runs a controlled TCP load and
reads every counter on the way, so the layers can be told apart. It works
with any SANA-II driver and any bsdsocket.library stack (nothing in it is
virtio-specific), on QEMU and on real hardware. `tools/netprof-host` is
its peer on a PC (Windows or Linux, plain Rust `std`):

```bash
tools/build-package.sh testprogs/netprof              # testprogs/netprof/netprof
cd tools/netprof-host && cargo build --release       # target/release/netprof-host
netprof-host                                          # on the PC: port 4500
```

```
netprof info                       CPU, Roadshow version, each interface and its driver
netprof cpu                        CopyMemQuick speed and the idle meter
netprof recv HOST [mb=64] [buf=32k] [rcvbuf=N] [sndbuf=N] [nodelay] [iface=NAME]
netprof send HOST [mb=64] ...      the same, uploading
netprof rtt HOST [count=200] [size=64]
netprof serve [port=4500] [count=N]   be netprof-host on this Amiga, for a peer Amiga
netprof watch [iface=NAME] [secs=10]   the counters while something else runs
```

HOST is the PC's address (on QEMU's user network, `10.0.3.2` for the
virtio-net card and `10.0.2.2` for the rtl8139). On a real Amiga, allow
netprof-host through the PC's firewall (Windows Defender blocks inbound TCP
4500 by default), and either keep both machines on one subnet or give
`iface=NAME`: netprof picks the interface on HOST's subnet. A missing
second in the per-second lines means no call returned in it: that is a
stall. `watch` counts inbound bytes in its MB/s column. `recv` and `send` print a
line a second and a report: throughput (and netprof-host's own figure for
uploads), CPU, the interface, the driver, TCP, IP, mbufs, the socket queues,
and findings. What it reads:

- CPU: an idle meter (a task at priority -128 counting while nothing else
  runs, calibrated for a second first); the time netprof spends inside
  `recv()`/`send()`; exec's `CopyMemQuick` speed as a reference.
- Roadshow, per interface (`QueryInterfaceTagList`): frames and bytes each
  way, drops, overruns, errors, the read and write requests it keeps at the
  driver (sampled every 100 ms), the SANA-II copy-function calls.
- Roadshow, the stack (`GetNetworkStatistics`): TCP (in-order, duplicate and
  out-of-order packets, acks, window updates, retransmits, timeouts), IP,
  mbufs, and the connection's receive and send queues.
- The driver (`S2_GETGLOBALSTATS`, `S2_GETSPECIALSTATS`): netprof opens the
  interface's device as one more opener and prints every counter under the
  driver's own name for it. These include frames the driver dropped, which
  Roadshow never sees. `nodriver` skips this.

`noidle` and `nosample` turn the meter and the sampling off, to show what
they cost (under 7% on QEMU). netprof-host also answers HTTP (`GET
/bytes/<n>`), so wget or curl on the Amiga can be measured
against the same server.

Results on the QEMU AmigaOne, before the receive fix below:

| Run | Result | What netprof showed |
|---|---|---|
| 64 MB down, virtio-net | 13.3-16.4 MB/s | 23-27 MB/s between stalls (CPU 87-100%); each stall follows virtionet.device dropping frames with no read request queued (13-26 a run, its "no read pending" counter), so TCP recovers with the CPU idle. Fixed since: see below |
| 64 MB down, rtl8139 | 9.6 MB/s | CPU-bound (98%); a steady ~270 out-of-order packets a second, no stall |
| 16 MB up, virtio-net / rtl8139 | 4.9 / 4.3 MB/s | CPU-bound (100%) on both; ~3,600 frames a second each way |
| rtt, virtio-net / rtl8139 | 0.39 / 0.41 ms median | |

QEMU's throughput moved by a factor of four between two boots of the same
machine, while the emulated CPU stayed within about 10% (CopyMemQuick
0.9-1.1 GB/s and the idle meter's rate in both): the difference is in
QEMU's user network or the host's socket path, not emulation speed. The
cause was not found, so compare runs within one boot.

The driver fix that followed, each build against the rtl8139 in its own
boot:

| 64 MB download (netprof) | virtio-net | rtl8139, same boot | virtio-net ÷ rtl8139 | "no read pending" |
|---|---|---|---|---|
| before | 6.19, 7.65 MB/s | 7.21 MB/s | 0.86, 1.06 | 36, 26 |
| after | 15.50, 13.51 MB/s | 7.28 MB/s | 2.13, 1.86 | 0, 0 |

After the fix nothing arrives out of order and the CPU is the limit
(96-98% busy); uploads are unchanged (1.12 × the rtl8139).

## rustsocket.library

rustsocket.library, the Roadshow-compatible TCP/IP stack on smoltcp that
grew up here, now has its own repository,
[RustSocket](https://github.com/derfsss/RustSocket). This repository's
QEMU machines (`tools/vnetbench/boot.sh amigaone-rs`, the `STACK=rustsocket`
option of `tools/qemu/run-amigaone-virtionet.sh`) are still where it is
tested on virtio-net.

## virtionet test bench

The bench tries to break virtionet.device before beta testers do. It runs on
QEMU's AmigaOne, Pegasos II and Sam460ex, each on a copy of its disk image,
with the driver in `DEVS:Networks/` and the bench files on the shared
folder (`SHARED:vbench`).

| Piece | What it does |
|---|---|
| `tools/vnetbench/boot.sh MACHINE LOG [device options]` | boots one machine (amigaone, pegasos2, sam460) and waits for its MCPd; refuses while it runs (shut the guest down cleanly first) |
| `tools/vnetbench/pre.script` | the driver phase, before any stack uses the card: s2test, `hold`, `torture secs=60 kids=8`, `leak` |
| `tools/vnetbench/matrix.script [MTU n] [MBIT n] [NOSTACK]` | one boot of the feature matrix: s2test, hold, a 20 s torture, then byte-checked transfers through Roadshow |
| `tools/vnetbench/soak.script [SECS n] [PERIOD n]` | the soak, with Roadshow on the card: `netprof soakrecv`, `soaksend`, `soakrtt` and `s2test faults` at once |
| `tools/vnetbench/qmpchaos.py QMPPORT SECS [period=] [down=]` | from the host during a soak: link down and up, and announce, over QMP |
| `tools/vnetbench/make-kit.sh` | builds `dist/virtionet-beta.lha`, the tester kit (`tools/vnetbench/kit/`: ReadMe, `vnetcheck`, interface file) |

s2test's torture modes (`testprogs/s2test/src/torture.rs`):
- **torture** covers:
  - bad and short requests, writes over the MTU and failing copy functions;
  - 300 aborted reads, and 200 writes with every third aborted;
  - closing an opener with 54 requests still queued;
  - 600 multicast groups (a 256-group limit, refused cleanly), 150 tracked types and 200 open/close cycles;
  - the SANA-II semantics group;
  - finally N tasks doing ICMP echo while the device is reset and taken offline every 1.5 s.
- **hold** covers holding a frame for a late read, a reader that stops, and a reader that comes back.
- **leak** measures free memory around each kind of fault, plus a reset storm and resets every 120 ms. The unit must come back by itself.
- **closepend** closes an opener with writes waiting for a slot. It needs the `test-tx-hold` build.
- **faults** resets the device on a schedule while a stack runs.

The bench found and fixed:
- frames dropped when Roadshow's reads ran out (now held in the ring);
- a stalled reader holding up the ring for every frame;
- writes left queued at close (they would have been copied through freed memory);
- a unit that gave up for good after 8 recoveries in a row;
- seven SANA-II details from a comparison with prism2.device.

Results, QEMU 11.0.50, driver sha256 `01b1387e`:

| Run | Result |
|---|---|
| driver phase (`pre.script`), each of AmigaOne, Pegasos II, Sam460ex | 81/81: s2test 32, hold 5, torture 37 (8 tasks, 60 s, 20 resets, 20 offline cycles, no request lost), leak 7 (0 bytes per reset, per offline cycle, over 2000 echoes and 4 tasks) |
| feature matrix, AmigaOne (10 boots; on the build before the prism2.device comparison changes, `526ea1d3`, then re-run on `01b1387e` for the default, `disable-modern=on` and legacy with `mrg_rxbuf=off`) | all pass: transitional, `disable-modern=on`, non-transitional, `mrg_rxbuf=off`, `ctrl_vq=off`, `status=off,guest_announce=off`, `event_idx=off,indirect_desc=off`, `rx_queue_size=1024`, legacy with `mrg_rxbuf=off` (10-byte header), `host_mtu=1400,speed=100` (no stack phase) |
| `closepend` + `burst` on the `test-tx-hold` build | 36 writes waiting at close, all 40 back (36 aborted); burst 24 sent, 8 aborted, none lost |
| 30-minute soak with Roadshow, all three machines at once (a reset every 60 s, a link flap every 120 s) | 0 wrong bytes and 0 failed rounds: AmigaOne 7.2 GB down, 4.5 GB up, 8908 connections; Pegasos II 7.0 / 4.3 GB, 6708; Sam460ex 1.7 / 1.0 GB, 6036; 30 of 30 resets recovered on each; free memory flat after Roadshow's pools grew |
| tester kit, fresh AmigaOne boot | unpacked with LhA, DHCP interface file, `vnetcheck` all PASS |

With the driver's heap in MEMF_SHARED memory (`SharedExecAllocator`,
driver sha256 `dc610ffe`, AmigaOne): driver phase 81/81 and the default
feature matrix (s2test 32, hold 5, torture 37, 200 open/close cycles 0
bytes lost; Roadshow 411 MB down and 117 MB up in 20 s each, 0 wrong
bytes). The 30-minute soak and the other machines were not re-run.

Known limit: `host_mtu` below the back end's MTU (QEMU's user network is
1500). The guest is told 1400, gets 1514-byte frames, and Roadshow
discards them (input errors) and stalls for about 130 s per connection.

## virtiorng test bench

The bench tries to break virtiorng.device on QEMU's AmigaOne with two cards
(`tools/vnetbench/boot.sh virtiorng LOG "-device virtio-rng-pci -device
virtio-rng-pci-non-transitional"`; `KICKZIP=stock` for the disk build). The
bench files go on the shared folder (`SHARED:rbench`: `rngread`, the
scripts, `disk/virtiorng.device`; results in `out/`).

| Piece | What it does |
|---|---|
| `rngread TORTURE [secs] [unit] [kids] [units]` | up to 8 tasks over one or more units: random reads (1 B to 1 MB, misaligned, guard bytes), AbortIO at random moments, two in flight, CMD_FLUSH, a NULL buffer, close and reopen; every answer checked; each card reset behind the driver's back every 1.5 s; a watchdog for requests not answered in 10 s; two rounds and a free-memory comparison (`testprogs/rngread/src/torture.rs`) |
| `tools/rngbench/pre.script` | Kickstart build: rngread's 9 checks on each unit, then a 60 s torture on each |
| `tools/rngbench/soak.script [SECS n]` | Kickstart build: one torture over both units at once, 2 x SECS (default 900: 30 minutes) |
| `tools/rngbench/disk.script` | disk build: three cycles of load, 9 checks and a 20 s torture on each unit, expunge, free memory |

One torture covers both units because two runs side by side cannot: the
kids are CPU-bound under QEMU, and a second run's shell (started by MCPd
at priority -2) never gets the CPU. For the same reason the torture's main
task runs at priority 2 for the run.

Results, QEMU 11.0.50:

| Run | Result |
|---|---|
| `pre.script` | 9/9 on both units; torture 11/11 on each (8 tasks, 2 x 60 s) |
| `soak.script`, 30 minutes, both units | 427,873 operations, 0 bad answers, 65 GB read, 16,414 aborted, 0 lost requests, every task gone after each round, 2,384 resets all recovered, free memory 118 KB higher after round 2. Its two reset-count FAILs were the checker's: a kid's CMD_FLUSH aborts a queued reset now and then (0.4%), which the check now counts (a 2-minute rerun: 13/13) |
| `disk.script` | each cycle 9/9 + 9/9, torture 11/11 + 11/11; three expunges, each unloading the device; free memory 1859964928, 1859985408, 1859973120 |

## Credits

- [derfsss/rust-amigaos4-crate](https://github.com/derfsss/rust-amigaos4-crate):
  the `amigaos4` crate the drivers are built on (the `amigaos4/` submodule).
- [AmigaLabs/rust](https://github.com/AmigaLabs/rust) (branch `amigaos-port`):
  the Rust fork with the `powerpc-unknown-amigaos` target.
- [walkero-gr/amigagccondocker](https://github.com/walkero-gr/amigagccondocker):
  the cross-compiler Docker image (gcc and the AmigaOS SDK) the packages
  link in.
- [derfsss/VirtualSCSIDevice](https://github.com/derfsss/VirtualSCSIDevice):
  the C virtioscsi.device that the Rust driver replaces; its test suite runs
  unchanged against the Rust drop-in.
- prism2.device by Neil Cafferkey (Aminet): the SANA-II driver
  virtionet.device's SANA-II details were compared with.
- [QEMU](https://github.com/qemu/qemu): the virtio devices, and the
  AmigaOne, Pegasos II and Sam460ex machines everything here is tested on.

## Licence

MIT; see `LICENSE`.

---

Richard Gibbs — rich_gibbs@hotmail.com
