//! virtio-rng: virtiorng.device, an interrupt-driven driver for QEMU's
//! virtio-rng PCI device (`-device virtio-rng-pci`, or
//! `virtio-rng-pci-non-transitional`), as a kickstart module.
//!
//! - COLDSTART: find every card (vendor 0x1AF4: transitional 0x1005 first,
//!   then modern-only 0x1044), up to MAX_UNITS. Each card is one unit:
//!   claimed, set up through the modern virtio interface if it works or the
//!   legacy one (amigaos4::virtio::PciTransport), a first sample read by
//!   polling, then its unit task started.
//! - Each unit task ("virtiorng.device unit N") adds the unit's interrupt
//!   server on the chain from `PCIDevice->MapInterrupt()`, turns the
//!   device's interrupts on, and serves the unit's queue. It waits for the
//!   interrupt's signal, with a 1 s timer.device bound so a lost interrupt
//!   cannot hang a caller.
//! - The interrupt server reads its card's virtio ISR register (which also
//!   clears the interrupt). If the card raised it, it Signals the unit task
//!   and returns 1; otherwise 0, so the rest of a shared chain runs.
//! - OpenDevice unit N names card N (io_Unit). CMD_READ: BeginIO queues the
//!   request for the unit's task, which fills io_Data, up to eight pieces in
//!   flight: big reads directly into io_Data, small ones and the cache-line
//!   edges through bounce buffers (StartDMA/GetDMAList/EndDMA throughout).
//! - AFTERDOS: open each unit by name, check two 64-byte reads and a
//!   100003-byte read at an odd address, check that every completion came
//!   by interrupt, and print `virtiorng.device: self-test PASSED (N unit(s))`.
//!
//! No Forbid or Disable: each queue is under its own exec mutex, each
//! device state has one user at a time, and the counters are atomics.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::fmt::Write;
use core::panic::PanicInfo;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use amigaos4::allocator::SharedExecAllocator;
use amigaos4::device::cmd::{CMD_FLUSH, CMD_READ, CMD_RESET, NSCMD_DEVICEQUERY};
use amigaos4::device::ioerr::{IOERR_ABORTED, IOERR_BADLENGTH, IOERR_NOCMD, IOERR_OPENFAIL, IOERR_UNITBUSY};
use amigaos4::device::{answer_device_query, Abort, IoReq, RequestQueue, Worker, IOSTDREQ_SIZE, NSDEVTYPE_UNKNOWN};
use amigaos4::arch;
use amigaos4::interrupt::{IntServer, ResetCallback};
use amigaos4::mem::{DmaBuffer, DmaDirection, DmaMapping};
use amigaos4::port::AmigaMsgPort;
use amigaos4::serial_println;
use amigaos4::sys::{self, IORequest, PCIDevice, TagItem, Task};
use amigaos4::virtio::{self, ring::DESC_F_WRITE, PciTransport, Regs, Transport, Virtqueue};
use amigaos4::timer::{AmigaTimer, TimerUnit, TimerVal};

#[global_allocator]
// MEMF_SHARED: BeginIO, Open and Close run in the callers' tasks.
static ALLOCATOR: SharedExecAllocator = SharedExecAllocator;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    amigaos4::panic::default_panic_handler(info)
}

const NAME: &str = "virtiorng.device";
const DEVNAME_C: &[u8] = amigaos4::amstr!("virtiorng.device");
const TASK_PRI: i32 = 5;
const TASK_STACK: u32 = 32768;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// virtio-rng's transitional PCI device ID (it offers the legacy interface).
const VIRTIO_RNG_TRANSITIONAL: u32 = match virtio::pci_id_transitional(virtio::TYPE_ENTROPY) {
    Some(id) => id,
    None => 0,
};

const PCI_REVISION: u32 = 0x08;
const PCI_SUBSYSTEM_ID: u32 = 0x2E;
const PCI_INTERRUPT_LINE: u32 = 0x3C;

/// virtio-rng's modern-only PCI device ID (-device virtio-rng-pci-non-transitional).
const VIRTIO_RNG_MODERN: u32 = virtio::pci_id_modern(virtio::TYPE_ENTROPY);

const BOUNCE_SIZE: usize = 4096;
/// Upper bound on used-ring polls for the polled COLDSTART sample.
const POLL_LIMIT: u32 = 20_000_000;
/// How long the unit task waits for an interrupt before it looks itself.
const IRQ_TIMEOUT: TimerVal = TimerVal { secs: 1, micros: 0 };

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct Rng {
    dev: *mut PCIDevice,
    /// The device's registers (amigaos4::virtio: modern if it works, else legacy).
    t: PciTransport,
    /// Queue 0 (amigaos4::virtio).
    vq: Virtqueue,
    slots: Vec<Slot>,
    /// The unit this card is.
    u: &'static Unit,
}

/// The most cards served, one unit each, in the order found: transitional
/// devices (1af4:1005) first, then modern-only ones (1af4:1044).
const MAX_UNITS: usize = 4;

/// One card: its device state and every counter the server, the task and
/// the self-test share. Every field is an atomic or a cell, so the units
/// live in one static array, and begin_life() can overwrite each with
/// Unit::NEW.
struct Unit {
    /// The card, found and locked by the init, freed by Expunge.
    dev: AtomicPtr<PCIDevice>,
    /// Opens of this unit (disk build: it runs while any is open).
    #[cfg_attr(not(feature = "disk"), allow(dead_code))]
    opens: AtomicU32,
    /// The unit task runs.
    running: AtomicU32,
    /// A stop was asked for and the task never went: keep the code loaded.
    #[cfg_attr(not(feature = "disk"), allow(dead_code))]
    stuck: AtomicU32,
    /// Whom the unit task signals once it has stopped.
    stop_ack_task: AtomicPtr<Task>,
    stop_ack_mask: AtomicU32,

    /// The device. Written by start_unit; after it starts the unit task,
    /// only the unit task touches it (until stop_unit has seen it go).
    rng: UnsafeCell<Option<Rng>>,

    /// The ISR and status registers (either interface), for the interrupt
    /// server and the reset callback, which cannot borrow the device state.
    /// Written by the COLDSTART init before either is added; regs_ready
    /// says so.
    regs: UnsafeCell<Option<Regs>>,
    regs_ready: AtomicU32,
    /// Set once the reset callback has reset the device: from then on the
    /// interrupt server passes everything on.
    quiesced: AtomicU32,

    /// What the interrupt server reads: written by the unit task before it
    /// adds the server.
    irq_task: AtomicPtr<Task>,
    irq_mask: AtomicU32,
    /// The chain the server is on (u32::MAX: none).
    irq_chain: AtomicU32,
    // The server's counters, written only by the server.
    irq_raised: AtomicU32,    // times the chain called it while ready
    irq_claimed: AtomicU32,   // times it was this card's
    irq_passed_on: AtomicU32, // times it was someone else's (or not ready)
    irq_signalled: AtomicU32, // times the task was woken
    /// The server's own time: calls, total and longest, in timebase ticks.
    srv_runs: AtomicU32,
    srv_ticks_total: AtomicU32,
    srv_ticks_longest: AtomicU32,
    /// Timebase (low word) at the server's last Signal, for the task's
    /// Signal-to-run latency.
    tb_signal: AtomicU32,
    /// THE STORM BREAKER. Reading the ISR register drops the device's
    /// level-triggered line, so claims the task has not yet served should
    /// stay few. If they pile up past STORM_LIMIT the line is stuck asserted
    /// and exec would re-enter the chain forever: the server then resets the
    /// device (status 0, which stops it asserting) and sets irq_storm; the
    /// next request recovers it. A failed request instead of a hang.
    irq_storm: AtomicU32,
    /// Written by the unit task: the claim count it has served up to.
    irq_drained: AtomicU32,

    // The unit task's counters, read by the AFTERDOS self-test.
    /// Signal-to-run latency of the unit task, in timebase ticks.
    wakes: AtomicU32,
    wake_ticks_max: AtomicU32,
    wake_ticks_min: AtomicU32,
    done_by_irq: AtomicU32,
    done_by_timeout: AtomicU32,
    requests: AtomicU32,
    bytes: AtomicU32,
    bytes_direct: AtomicU32,
    bytes_bounced: AtomicU32,
    direct_requests: AtomicU32,
    direct_refused: AtomicU32,
    /// Set once a failure leaves descriptors the device may still own.
    broken: AtomicU32,
    recoveries: AtomicU32,

    /// The requests waiting for the unit task and the one in progress
    /// (amigaos4::device: under an exec mutex).
    queue: RequestQueue,
    /// The unit task's work signal, published once it is ready.
    worker: Worker,
    aborted_queued: AtomicU32,
    aborted_current: AtomicU32,
}

const STORM_LIMIT: u32 = 1000;

impl Unit {
    const NEW: Unit = Unit {
        dev: AtomicPtr::new(null_mut()),
        opens: AtomicU32::new(0),
        running: AtomicU32::new(0),
        stuck: AtomicU32::new(0),
        stop_ack_task: AtomicPtr::new(null_mut()),
        stop_ack_mask: AtomicU32::new(0),
        rng: UnsafeCell::new(None),
        regs: UnsafeCell::new(None),
        regs_ready: AtomicU32::new(0),
        quiesced: AtomicU32::new(0),
        irq_task: AtomicPtr::new(null_mut()),
        irq_mask: AtomicU32::new(0),
        irq_chain: AtomicU32::new(u32::MAX),
        irq_raised: AtomicU32::new(0),
        irq_claimed: AtomicU32::new(0),
        irq_passed_on: AtomicU32::new(0),
        irq_signalled: AtomicU32::new(0),
        srv_runs: AtomicU32::new(0),
        srv_ticks_total: AtomicU32::new(0),
        srv_ticks_longest: AtomicU32::new(0),
        tb_signal: AtomicU32::new(0),
        irq_storm: AtomicU32::new(0),
        irq_drained: AtomicU32::new(0),
        wakes: AtomicU32::new(0),
        wake_ticks_max: AtomicU32::new(0),
        wake_ticks_min: AtomicU32::new(u32::MAX),
        done_by_irq: AtomicU32::new(0),
        done_by_timeout: AtomicU32::new(0),
        requests: AtomicU32::new(0),
        bytes: AtomicU32::new(0),
        bytes_direct: AtomicU32::new(0),
        bytes_bounced: AtomicU32::new(0),
        direct_requests: AtomicU32::new(0),
        direct_refused: AtomicU32::new(0),
        broken: AtomicU32::new(0),
        recoveries: AtomicU32::new(0),
        queue: RequestQueue::new(),
        worker: Worker::new(),
        aborted_queued: AtomicU32::new(0),
        aborted_current: AtomicU32::new(0),
    };

    /// The device's registers, if the COLDSTART init has published them.
    #[inline]
    unsafe fn regs(&self) -> Option<Regs> {
        if self.regs_ready.load(Ordering::Acquire) != 0 { *self.regs.get() } else { None }
    }

    /// This unit's number.
    fn index(&'static self) -> usize {
        // SAFETY: every Unit lives in UNITS.
        unsafe { (self as *const Unit).offset_from(UNITS.0.get() as *const Unit) as usize }
    }
}

struct Units(UnsafeCell<[Unit; MAX_UNITS]>);
// SAFETY: each field is an atomic, or a cell with one user at a time
// (described at the field), or written by begin_life() before anything runs.
unsafe impl Sync for Units {}
static UNITS: Units = Units(UnsafeCell::new([Unit::NEW; MAX_UNITS]));
/// How many units the COLDSTART init set up.
static NUNITS: AtomicU32 = AtomicU32::new(0);

/// Unit `n` (n < MAX_UNITS).
#[inline]
fn unit(n: usize) -> &'static Unit {
    // SAFETY: shared references only; every field has interior mutability.
    unsafe { &(*UNITS.0.get())[n] }
}

/// The units set up this boot.
fn units() -> impl Iterator<Item = &'static Unit> {
    (0..NUNITS.load(Ordering::Acquire) as usize).map(unit)
}

macro_rules! pci {
    ($dev:expr, $method:ident $(, $arg:expr)*) => {
        ((*$dev).$method)($dev $(, $arg)*)
    };
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::new();
    for b in bytes {
        let _ = write!(s, "{:02x}", b);
    }
    s
}

// ---------------------------------------------------------------------------
// The queue: several pieces in flight
// ---------------------------------------------------------------------------
//
// A request is cut into pieces, in ascending order in the caller's buffer.
// A piece is either bounced (the device writes one slot's 4 KB physically
// contiguous bounce buffer, which is copied out) or direct (the device
// writes the caller's buffer itself). Each queue slot always uses the
// descriptor of the same number, so a used-ring id names the slot. As many
// pieces are posted as there are free slots, the avail index is published
// and the device kicked once; each wake takes every used buffer, copies a
// bounced one to its place, and frees its slot, which is refilled.
//
// Direct pieces: for a request whose cache-line-aligned interior is at
// least DIRECT_MIN, the interior is mapped once with StartDMA and its
// physical blocks are cut into pieces of at most DIRECT_PIECE. The head and
// tail, up to the first and from the last cache-line boundary, are bounced:
// StartDMA and EndDMA each do one cache operation on the lines they cover,
// and on a machine whose caches do not snoop DMA a line shared between the
// device's bytes and someone else's would lose one side's writes. Lines
// are counted at 64 bytes or the CPU's own size if larger (32 on the G3,
// G4 and 44x, 64 on the e5500 and PA6T). If StartDMA refuses the buffer,
// the whole request is bounced.
//
// A piece the device fills short leaves its remainder on a redo list. On
// abort nothing more is posted, but every piece in flight is waited for (the
// device owns its descriptor until it is used), so the bytes read are a
// prefix with no gap. A failure with pieces in flight resets the device at
// once, before the caller's buffer can be unmapped and handed back, and
// marks it BROKEN, so the next request rebuilds the queue.

/// The most pieces kept in flight (bounded by the queue size).
const MAX_SLOTS: usize = 8;
/// The smallest aligned interior worth a StartDMA of the caller's buffer.
const DIRECT_MIN: usize = 2 * BOUNCE_SIZE;
/// The largest direct piece.
const DIRECT_PIECE: usize = 64 * 1024;
/// What the self-test's guard bytes hold.
const GUARD: u8 = 0xa5;
/// The smallest cache line edges are rounded to.
const MIN_CACHE_LINE: u32 = 64;
/// The cache line edges are rounded to (set by setup()).
static CACHE_LINE: AtomicU32 = AtomicU32::new(MIN_CACHE_LINE);


/// One piece of a request: where it goes in the caller's buffer, its
/// length, and the physical address the device writes for a direct piece
/// (None: through a bounce buffer).
#[derive(Clone, Copy)]
struct Piece {
    off: usize,
    len: usize,
    phys: Option<u32>,
}

struct Slot {
    buf: DmaBuffer,
    /// The bounce buffer's DMA mapping while the device owns it.
    map: Option<DmaMapping<'static>>,
    /// The piece the slot holds while busy.
    piece: Piece,
    /// The device owns the slot's descriptor.
    busy: bool,
}

/// How a request waits for the device.
enum Waiter<'a> {
    /// Spin on the used ring (the COLDSTART sample, or no interrupt).
    Poll,
    /// Wait for the interrupt's signal, bounded by the timer.
    Irq(&'a mut AmigaTimer, u32),
}

/// How a request ended.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    Done,
    Aborted,
}

/// Set once a failure leaves descriptors the device may still own.

/// Bounced pieces for out[from..to].
fn bounce_pieces(v: &mut Vec<Piece>, from: usize, to: usize) {
    let mut off = from;
    while off < to {
        let len = BOUNCE_SIZE.min(to - off);
        v.push(Piece { off, len, phys: None });
        off += len;
    }
}

/// The pieces for a request of `total` bytes: all bounced, or with `direct`
/// = (offset of the mapped interior, its physical blocks) the head and tail
/// bounced and the interior direct.
fn plan(total: usize, direct: Option<(usize, &[sys::DMAEntry])>) -> Vec<Piece> {
    let mut v = Vec::new();
    match direct {
        None => bounce_pieces(&mut v, 0, total),
        Some((start, entries)) => {
            bounce_pieces(&mut v, 0, start);
            let mut off = start;
            for e in entries {
                let mut phys = e.PhysicalAddress as usize as u32;
                let mut left = e.BlockLength as usize;
                while left > 0 {
                    let len = DIRECT_PIECE.min(left);
                    v.push(Piece { off, len, phys: Some(phys) });
                    off += len;
                    phys = phys.wrapping_add(len as u32);
                    left -= len;
                }
            }
            bounce_pieces(&mut v, off, total);
        }
    }
    v
}

impl Rng {
    /// Put `p` on the queue in slot `s` (not yet published).
    unsafe fn post(&mut self, s: usize, p: Piece) -> Result<(), String> {
        let phys = match p.phys {
            Some(phys) => phys,
            None => {
                let buf = self.slots[s].buf.as_mut_ptr();
                let map = DmaMapping::new(buf, BOUNCE_SIZE, DmaDirection::FromDevice)
                    .map_err(|e| alloc::format!("map: {}", e))?;
                let e = map.entries();
                if e.len() != 1 || (e[0].BlockLength as usize) < p.len {
                    return Err(alloc::format!("bounce buffer is {} DMA blocks, needs 1", e.len()));
                }
                let phys = e[0].PhysicalAddress as usize as u32;
                self.slots[s].map = Some(map);
                phys
            }
        };
        let ring = self.vq.ring();
        ring.set_desc(s as u16, phys as u64, p.len as u32, DESC_F_WRITE, 0);
        ring.push_avail(s as u16);
        let slot = &mut self.slots[s];
        slot.piece = p;
        slot.busy = true;
        Ok(())
    }

    /// Publish the posted pieces and kick the device.
    unsafe fn publish(&mut self) {
        self.vq.ring().publish();
        self.t.notify(0);
    }

    /// The next used buffer, if any: (descriptor id, bytes written).
    unsafe fn take_used(&mut self) -> Option<(usize, usize)> {
        self.vq.ring().take_used().map(|(id, len)| (id as usize, len as usize))
    }

    /// Take every used buffer: copy a bounced one out, free its slot.
    /// Returns how many were taken.
    unsafe fn reap(&mut self, out: &mut [u8], redo: &mut Vec<Piece>, free: &mut Vec<usize>) -> Result<usize, String> {
        let u = self.u;
        let mut taken = 0;
        while let Some((id, len)) = self.take_used() {
            if id >= self.slots.len() || !self.slots[id].busy {
                return Err(alloc::format!("device used descriptor {}, which is not in flight", id));
            }
            let slot = &mut self.slots[id];
            slot.busy = false;
            let p = slot.piece;
            let got = len.min(p.len);
            match p.phys {
                None => {
                    drop(slot.map.take()); // EndDMA: the CPU now sees what the device wrote
                    out[p.off..p.off + got].copy_from_slice(&slot.buf.as_slice()[..got]);
                    u.bytes_bounced.fetch_add(got as u32, Ordering::Relaxed);
                }
                Some(_) => {
                    u.bytes_direct.fetch_add(got as u32, Ordering::Relaxed);
                }
            }
            if got < p.len {
                redo.push(Piece {
                    off: p.off + got,
                    len: p.len - got,
                    phys: p.phys.map(|x| x.wrapping_add(got as u32)),
                });
            }
            free.push(id);
            taken += 1;
        }
        Ok(taken)
    }

    /// Fill all of `out` from `pieces`, keeping up to `slots` pieces in
    /// flight. `abort` is asked between rounds. Returns the bytes read (a
    /// prefix) and why it stopped. On return the device owns no slot.
    unsafe fn read(
        &mut self,
        out: &mut [u8],
        pieces: &[Piece],
        waiter: &mut Waiter,
        abort: &dyn Fn() -> bool,
    ) -> Result<(usize, Stop), String> {
        let u = self.u;
        if u.broken.load(Ordering::Relaxed) != 0 {
            return Err("the device was left with pieces in flight by an earlier failure".into());
        }
        let r = self.read_inner(out, pieces, waiter, abort);
        if r.is_err() && self.slots.iter().any(|s| s.busy) {
            // The device may still write a direct piece into the caller's
            // buffer: stop it before that buffer is unmapped and replied.
            // Once it is reset it owns nothing, so every mapping may end.
            let _ = self.t.reset();
            for s in self.slots.iter_mut() {
                s.busy = false;
                drop(s.map.take());
            }
            u.broken.store(1, Ordering::Relaxed);
        }
        r
    }

    unsafe fn read_inner(
        &mut self,
        out: &mut [u8],
        pieces: &[Piece],
        waiter: &mut Waiter,
        abort: &dyn Fn() -> bool,
    ) -> Result<(usize, Stop), String> {
        let total = out.len();
        let mut next = 0usize;
        let mut redo: Vec<Piece> = Vec::new();
        let mut free: Vec<usize> = (0..self.slots.len()).rev().collect();
        let mut in_flight = 0usize;
        let mut aborted = false;
        loop {
            if !aborted && abort() {
                aborted = true;
            }
            let mut posted = 0;
            while !aborted && !free.is_empty() {
                let p = if let Some(p) = redo.pop() {
                    p
                } else if next < pieces.len() {
                    next += 1;
                    pieces[next - 1]
                } else {
                    break;
                };
                let s = free.pop().unwrap_or(0);
                self.post(s, p)?;
                in_flight += 1;
                posted += 1;
            }
            if posted > 0 {
                self.publish();
            }
            if in_flight == 0 {
                break;
            }
            in_flight -= self.wait_and_reap(out, waiter, &mut redo, &mut free)?;
        }
        let unread = if next < pieces.len() { pieces[next].off } else { total };
        let actual = redo.iter().map(|p| p.off).chain(core::iter::once(unread)).min().unwrap_or(total);
        Ok((actual.min(total), if aborted { Stop::Aborted } else { Stop::Done }))
    }

    /// Wait until at least one piece is used, then reap. Returns how many.
    unsafe fn wait_and_reap(
        &mut self,
        out: &mut [u8],
        waiter: &mut Waiter,
        redo: &mut Vec<Piece>,
        free: &mut Vec<usize>,
    ) -> Result<usize, String> {
        let u = self.u;
        match waiter {
            Waiter::Poll => {
                let mut polls = 0u32;
                loop {
                    let n = self.reap(out, redo, free)?;
                    if n > 0 {
                        return Ok(n);
                    }
                    polls += 1;
                    if polls >= POLL_LIMIT {
                        return Err(alloc::format!("no answer after {} polls", polls));
                    }
                }
            }
            Waiter::Irq(timer, irq_mask) => {
                let irq_mask = *irq_mask;
                // A signal still latched from an earlier wake only costs one
                // empty reap; a completion after a reap raises a new one.
                timer.start(IRQ_TIMEOUT);
                let tmask = timer.signal_mask();
                let result = loop {
                    let got = sys::exec_wait(irq_mask | tmask);
                    if got & irq_mask != 0 {
                        let lat = (arch::timebase() as u32).wrapping_sub(u.tb_signal.load(Ordering::Relaxed));
                        u.wakes.fetch_add(1, Ordering::Relaxed);
                        u.wake_ticks_max.fetch_max(lat, Ordering::Relaxed);
                        u.wake_ticks_min.fetch_min(lat, Ordering::Relaxed);
                        // Every claim so far is served by this wake.
                        u.irq_drained.store(u.irq_claimed.load(Ordering::Relaxed), Ordering::Relaxed);
                        if u.irq_storm.load(Ordering::Acquire) != 0 {
                            break Err(String::from(
                                "interrupt storm: the line stayed asserted; the server reset the device",
                            ));
                        }
                        let n = self.reap(out, redo, free)?;
                        if n > 0 {
                            u.done_by_irq.fetch_add(n as u32, Ordering::Relaxed);
                            break Ok(n);
                        }
                        continue;
                    }
                    if timer.is_done() {
                        timer.finish();
                        let n = self.reap(out, redo, free)?;
                        break if n > 0 {
                            u.done_by_timeout.fetch_add(n as u32, Ordering::Relaxed);
                            Ok(n)
                        } else {
                            Err(String::from("no completion within 1 s"))
                        };
                    }
                };
                timer.abort();
                result
            }
        }
    }
}

// ---------------------------------------------------------------------------
// COLDSTART: find and set up the cards
// ---------------------------------------------------------------------------

/// Every virtio-rng card, locked for this driver, in unit order:
/// transitional devices first, then modern-only ones, each in FindDevice's
/// index order. At most MAX_UNITS; a card another driver holds is skipped.
unsafe fn find_cards() -> Vec<*mut PCIDevice> {
    let mut found = Vec::new();
    for id in [VIRTIO_RNG_TRANSITIONAL, VIRTIO_RNG_MODERN] {
        for index in 0.. {
            if found.len() == MAX_UNITS {
                return found;
            }
            let tags = [
                TagItem { ti_Tag: sys::FDT_VENDOR_ID, ti_Data: virtio::PCI_VENDOR },
                TagItem { ti_Tag: sys::FDT_DEVICE_ID, ti_Data: id },
                TagItem { ti_Tag: sys::FDT_INDEX, ti_Data: index },
                TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
            ];
            let dev = sys::wrappers::pci::pci_find_device(tags.as_ptr());
            if dev.is_null() {
                break;
            }
            if pci!(dev, Lock, sys::PCI_LOCK_EXCLUSIVE) == 0 {
                serial_println!("{}: 1af4:{:04x} #{} is locked by another driver; skipped", NAME, id, index);
                sys::wrappers::pci::pci_free_device(dev);
                continue;
            }
            found.push(dev);
        }
    }
    found
}

/// Set up one card as unit `u`.
unsafe fn setup(u: &'static Unit, dev: *mut PCIDevice) -> Result<Rng, String> {
    let n = u.index();
    let (mut bus, mut slot, mut func) = (0u8, 0u8, 0u8);
    pci!(dev, GetAddress, &mut bus, &mut slot, &mut func);
    let vendor = pci!(dev, ReadConfigWord, 0);
    let device = pci!(dev, ReadConfigWord, 2);
    let rev = pci!(dev, ReadConfigByte, PCI_REVISION);
    let subsys = pci!(dev, ReadConfigWord, PCI_SUBSYSTEM_ID);
    let irq = pci!(dev, ReadConfigByte, PCI_INTERRUPT_LINE);
    // A modern-only device's type is its device ID less 0x1040; a
    // transitional device carries the type in its PCI subsystem ID.
    let vtype = if device as u32 >= virtio::pci_id_modern(0) { device as u32 - virtio::pci_id_modern(0) } else { subsys as u32 };
    serial_println!(
        "{} unit {}: {:04x}:{:04x} rev {} at {:02x}:{:02x}.{}, virtio type {} (4 = entropy), IRQ line {}",
        NAME, n, vendor, device, rev, bus, slot, func, vtype, irq
    );

    let (cmd, now) = virtio::enable_pci(dev, false);
    serial_println!("{} unit {}: PCI command 0x{:04x} -> 0x{:04x} (I/O + bus master)", NAME, n, cmd, now);

    #[cfg(not(feature = "force-legacy"))]
    let (mut t, why_legacy) = PciTransport::open(dev).map_err(|e| alloc::format!("{}", e))?;
    #[cfg(feature = "force-legacy")]
    let (mut t, why_legacy) = {
        virtio::enable_pci(dev, false);
        let l = virtio::LegacyPci::new(dev).map_err(|e| alloc::format!("{}", e))?;
        (PciTransport::Legacy(l), Some("force-legacy test build"))
    };
    match (&t, why_legacy) {
        (PciTransport::Modern(m), _) => serial_println!(
            "{} unit {}: modern interface (virtio 1.x), common configuration at 0x{:08x}", NAME, n, m.common_base()
        ),
        (PciTransport::Legacy(l), why) => serial_println!(
            "{} unit {}: legacy interface, BAR 0 I/O at 0x{:08x}; modern not used: {}",
            NAME, n, l.io_base(), why.unwrap_or("-")
        ),
    }

    // virtio-rng has no feature bits we need.
    let host = t.device_features();
    let accepted = virtio::init_device(&mut t, 0).map_err(|e| alloc::format!("{}", e))?;

    let qsize = t.queue_max(0);
    if qsize == 0 {
        t.set_status(virtio::STATUS_FAILED);
        return Err("queue 0 not available".into());
    }
    let mut vq = Virtqueue::new(qsize, t.ring_endian()).map_err(|e| alloc::format!("queue: {}", e))?;
    // Interrupts stay off until the unit task's server is in place.
    vq.ring().set_interrupts(false);
    t.setup_queue(0, &vq).map_err(|e| alloc::format!("{}", e))?;
    virtio::driver_ok(&mut t).map_err(|e| alloc::format!("{}", e))?;
    let l = vq.layout();
    serial_println!(
        "{} unit {}: features offered 0x{:x}, accepted 0x{:x}; queue 0 size {}, {} ring at phys 0x{:08x} ({} bytes, used ring +{}), status 0x{:02x}",
        NAME, n, host, accepted, qsize, if t.is_modern() { "little-endian" } else { "native-order" },
        vq.desc_phys(), l.total, l.used_off, t.status()
    );

    // One 4 KB, physically contiguous bounce buffer per slot.
    let nslots = MAX_SLOTS.min(qsize as usize);
    let mut slots = Vec::with_capacity(nslots);
    for _ in 0..nslots {
        let buf = DmaBuffer::alloc_contiguous(BOUNCE_SIZE, BOUNCE_SIZE as u32)
            .map_err(|e| alloc::format!("bounce alloc: {}", e))?;
        slots.push(Slot { buf, map: None, piece: Piece { off: 0, len: 0, phys: None }, busy: false });
    }

    Ok(Rng { dev, t, vq, slots, u })
}

/// The cache line the caller's buffer edges are rounded to (once per boot).
unsafe fn set_cache_line() {
    let mut line: u32 = 0;
    let tags = [
        TagItem { ti_Tag: sys::GCIT_CACHE_LINE_SIZE, ti_Data: &mut line as *mut u32 as usize as u32 },
        TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
    ];
    sys::exec_get_cpuinfo(tags.as_ptr());
    let edge = if line.is_power_of_two() { line.max(MIN_CACHE_LINE) } else { MIN_CACHE_LINE };
    CACHE_LINE.store(edge, Ordering::Relaxed);
    serial_println!(
        "{}: reads of {} KB or more go straight into the caller's buffer; CPU cache line {} bytes, edges rounded to {}",
        NAME, DIRECT_MIN / 1024, line, edge
    );
}

/// Boots this image has run, counted across warm restarts. The one static
/// begin_life() leaves alone.
static LIFE: AtomicU32 = AtomicU32::new(0);

/// THE SECOND LIFE. A warm restart (ColdReboot) re-runs the residents in the
/// kickstart image already in memory, without reloading it, so every static
/// still holds the previous boot's value: each unit's DMA mappings and
/// buffers, its queue's allocation, its task, mutex and device pointers, the
/// quiesced flag the reset callback set. The OS has forgotten everything that
/// boot allocated, so none of it may be used, freed or dropped. Each unit is
/// overwritten with Unit::NEW by ptr::write (never `=`, which would drop the
/// old value and free memory that is no longer ours), and the few statics
/// outside the units are reset. A new static outside Unit must be added here.
unsafe fn begin_life() -> u32 {
    let units = UNITS.0.get() as *mut Unit;
    for n in 0..MAX_UNITS {
        core::ptr::write(units.add(n), Unit::NEW);
    }
    NUNITS.store(0, Ordering::Relaxed);
    CACHE_LINE.store(MIN_CACHE_LINE, Ordering::Relaxed);
    QUIESCED_ALL.store(0, Ordering::Relaxed);
    core::ptr::write(RESET_CB.0.get(), None);
    RUNNING_UNITS.store(0, Ordering::Relaxed);
    OPEN_LOCK.store(null_mut(), Ordering::Relaxed);
    LIFE.fetch_add(1, Ordering::AcqRel) + 1
}

/// Unit task names, one per unit.
const TASK_NAMES: [&[u8]; MAX_UNITS] = [
    amigaos4::amstr!("virtiorng.device unit 0"),
    amigaos4::amstr!("virtiorng.device unit 1"),
    amigaos4::amstr!("virtiorng.device unit 2"),
    amigaos4::amstr!("virtiorng.device unit 3"),
];

/// Bring up unit `u` on its card: set it up, read a polled sample, create
/// its queue's mutex and publish its registers, then start its task and,
/// for the first running unit, add the reset callback. Err: the unit is not
/// usable (the card is left reset and nothing of the attempt is kept).
unsafe fn start_unit(u: &'static Unit) -> Result<(), String> {
    let n = u.index();
    let dev = u.dev.load(Ordering::Acquire);
    if dev.is_null() {
        return Err("no card".into());
    }
    let mut rng = setup(u, dev)?;
    let mut sample = [0u8; 16];
    let pieces = plan(sample.len(), None);
    match rng.read(&mut sample, &pieces, &mut Waiter::Poll, &|| false) {
        Ok((got, _)) if got == sample.len() => {
            serial_println!("{} unit {}: first sample {} (polled; no task or interrupt yet)", NAME, n, hex(&sample))
        }
        Ok((got, _)) => serial_println!("{} unit {}: first sample short: {} bytes", NAME, n, got),
        Err(e) => {
            let _ = rng.t.reset();
            return Err(alloc::format!("first read: {}", e));
        }
    }
    // The polled sample ran with VRING_AVAIL_F_NO_INTERRUPT, which is a
    // hint, not a mask. Read the ISR anyway so the level-triggered line is
    // not left asserted while no server of ours is on its chain.
    let _ = rng.t.read_isr();

    // The queue's lock, created here where the caller is provably the only
    // user, before the unit can be used; never lazily on the path it
    // protects.
    if u.queue.init().is_err() {
        let _ = rng.t.reset();
        return Err("mutex allocation failed".into());
    }
    *u.regs.get() = Some(rng.t.regs());
    u.regs_ready.store(1, Ordering::Release);
    *u.rng.get() = Some(rng);

    let tags = [
        TagItem { ti_Tag: sys::AT_PARAM1, ti_Data: n as u32 },
        TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
    ];
    let task = sys::exec_create_task(TASK_NAMES[n].as_ptr() as _, TASK_PRI, unit_task as *const c_void, TASK_STACK, tags.as_ptr());
    if task.is_null() {
        // Nothing runs on the unit: reset the card, then its memory is ours.
        u.regs_ready.store(0, Ordering::Release);
        if let Some(mut r) = core::ptr::replace(u.rng.get(), None) {
            let _ = r.t.reset();
        }
        u.queue.destroy();
        return Err("CreateTask failed".into());
    }
    u.running.store(1, Ordering::Release);
    if RUNNING_UNITS.fetch_add(1, Ordering::AcqRel) == 0 {
        add_reset_callback();
    }
    serial_println!("{} unit {}: unit task created; requests are served once it is ready", NAME, n);
    Ok(())
}

/// The reset callback, while any unit runs: on ColdReboot every running
/// card is reset. At priority 127, so it runs first in the walk.
struct ResetCell(UnsafeCell<Option<ResetCallback>>);
// SAFETY: changed only under OPEN_LOCK (or by the COLDSTART init, alone).
unsafe impl Sync for ResetCell {}
static RESET_CB: ResetCell = ResetCell(UnsafeCell::new(None));
/// Units whose task is running.
static RUNNING_UNITS: AtomicU32 = AtomicU32::new(0);

unsafe fn add_reset_callback() {
    QUIESCED_ALL.store(0, Ordering::Release);
    match ResetCallback::add(127, DEVNAME_C, rng_reset, null_mut()) {
        Ok(cb) => {
            *RESET_CB.0.get() = Some(cb);
            serial_println!("{}: reset callback added (pri 127): every running card is reset on reboot", NAME);
        }
        Err(e) => serial_println!("{}: AddResetCallback failed: {}; the cards are not reset on reboot", NAME, e),
    }
}

/// Serialises unit start and stop (Open and Close) across callers.
static OPEN_LOCK: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

/// Run `f` holding OPEN_LOCK.
#[cfg(feature = "disk")]
unsafe fn with_open_lock<R>(f: impl FnOnce() -> R) -> R {
    let m = OPEN_LOCK.load(Ordering::Acquire);
    if !m.is_null() {
        sys::exec_mutex_obtain(m as sys::APTR);
    }
    let r = f();
    if !m.is_null() {
        sys::exec_mutex_release(m as sys::APTR);
    }
    r
}

/// How long stop_unit waits for the unit task to answer, and then to be gone.
#[cfg(feature = "disk")]
const STOP_BOUND_MS: u32 = 3000;
#[cfg(feature = "disk")]
const GONE_POLL_MS: u32 = 5;

/// Stop unit `u`: ask its task to stop, wait for its answer (it has reset
/// the card and removed its interrupt server by then), wait until it is
/// gone from exec's task lists (its last instructions are this segment's,
/// which may be unloaded next), then free the unit's queue, DMA memory and
/// buffers, and, for the last running unit, remove the reset callback. The
/// card stays locked for the next open. Err: the task did not stop within
/// the bound; the unit is left as it is (and the device can never be
/// expunged, see rust_rng_expunge).
#[cfg(feature = "disk")]
unsafe fn stop_unit(u: &'static Unit) -> Result<(), String> {
    let n = u.index();
    if u.running.load(Ordering::Acquire) == 0 {
        return Ok(());
    }
    let mut timer = AmigaTimer::open(TimerUnit::MicroHz).map_err(|e| alloc::format!("timer.device: {}", e))?;
    let sig = sys::exec_alloc_signal(-1);
    if sig < 0 {
        return Err("no free signal".into());
    }
    let mask = 1u32 << sig;
    u.stop_ack_task.store(sys::exec_find_task(null_mut()), Ordering::Release);
    u.stop_ack_mask.store(mask, Ordering::Release);
    u.worker.request_stop();

    // 1. The task's answer.
    timer.start(TimerVal::from_millis(STOP_BOUND_MS));
    let answered = loop {
        let got = sys::exec_wait(mask | timer.signal_mask());
        if got & mask != 0 {
            break true;
        }
        if timer.is_done() {
            timer.finish();
            break false;
        }
    };
    timer.abort();
    sys::exec_free_signal(sig);
    u.stop_ack_mask.store(0, Ordering::Release);
    u.stop_ack_task.store(null_mut(), Ordering::Release);
    if !answered {
        u.stuck.store(1, Ordering::Release);
        return Err(alloc::format!("the unit task did not stop within {} ms", STOP_BOUND_MS));
    }

    // 2. Gone from the task lists: exec has run its exit code, so nothing
    // of this segment runs on the task any more.
    let mut waited = 0;
    while !sys::exec_find_task(TASK_NAMES[n].as_ptr() as _).is_null() {
        if waited >= STOP_BOUND_MS {
            u.stuck.store(1, Ordering::Release);
            return Err(alloc::format!("the unit task answered but was still there after {} ms", STOP_BOUND_MS));
        }
        let _ = timer.delay(TimerVal::from_millis(GONE_POLL_MS));
        waited += GONE_POLL_MS;
    }

    // 3. The card was reset by the task; its memory is now ours to free.
    u.regs_ready.store(0, Ordering::Release);
    let rng = core::ptr::replace(u.rng.get(), None);
    drop(rng); // EndDMA and FreeVec: the queue, the bounce buffers
    u.queue.destroy();
    let dev = u.dev.load(Ordering::Acquire);
    // A fresh unit for the next open: counters and flags back to NEW, the
    // card and the open count kept.
    let opens = u.opens.load(Ordering::Acquire);
    core::ptr::write(u as *const Unit as *mut Unit, Unit::NEW);
    u.dev.store(dev, Ordering::Release);
    u.opens.store(opens, Ordering::Release);
    if RUNNING_UNITS.fetch_sub(1, Ordering::AcqRel) == 1 {
        drop((*RESET_CB.0.get()).take()); // RemResetCallback
    }
    serial_println!("{} unit {}: stopped: task gone after {} ms of polling, card reset, memory freed", NAME, n, waited);
    Ok(())
}

/// Called from devInit(). Non-zero: no card; exec does not add the device.
#[no_mangle]
pub extern "C" fn rust_rng_init() -> i32 {
    // SAFETY: the init runs once per load (or boot), before the device can
    // be opened and before any unit task exists.
    unsafe {
        let life = begin_life();
        if life > 1 {
            serial_println!(
                "{}: boot {} of this image (warm restart): previous boot's state discarded",
                NAME, life
            );
        }
        set_cache_line();
        let cards = find_cards();
        if cards.is_empty() {
            serial_println!(
                "{}: setup FAILED: no virtio-rng PCI device (1af4:1005 or 1af4:1044); add -device virtio-rng-pci; device not added",
                NAME
            );
            return 1;
        }
        let m = sys::exec_alloc_sys_object(sys::ASOT_MUTEX, [TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 }].as_ptr());
        if m.is_null() {
            serial_println!("{}: mutex allocation failed; device not added", NAME);
            for dev in cards {
                pci!(dev, Unlock);
                sys::wrappers::pci::pci_free_device(dev);
            }
            return 1;
        }
        OPEN_LOCK.store(m as *mut c_void, Ordering::Release);
        let count = cards.len();
        for (n, dev) in cards.into_iter().enumerate() {
            unit(n).dev.store(dev, Ordering::Release);
        }
        NUNITS.store(count as u32, Ordering::Release);

        #[cfg(not(feature = "disk"))]
        {
            // A kickstart module: every unit runs from boot to reboot.
            for u in units() {
                if let Err(e) = start_unit(u) {
                    serial_println!("{} unit {}: not started: {}; opening it fails", NAME, u.index(), e);
                }
            }
            serial_println!("{}: {} card(s): units 0 to {}", NAME, count, count - 1);
        }
        #[cfg(feature = "disk")]
        serial_println!(
            "{}: loaded from disk: {} card(s), units 0 to {}; each unit is set up at its first open and stopped at its last close",
            NAME, count, count - 1
        );
    }
    0
}

/// Expunge (disk build): no unit is open, so none runs (each stopped at its
/// last close) and nothing here waits, as exec may call Expunge from a
/// low-memory handler. Unlock and free the cards and the open lock.
/// Returns 1 when the device may be unloaded; 0 to keep it (a unit is open,
/// or one whose task never stopped still runs this code).
#[cfg(feature = "disk")]
#[no_mangle]
pub extern "C" fn rust_rng_expunge() -> i32 {
    // SAFETY: exec calls Expunge with no other user of the device.
    unsafe {
        for u in units() {
            if u.opens.load(Ordering::Acquire) != 0 || u.running.load(Ordering::Acquire) != 0 {
                serial_println!("{}: expunge deferred: unit {} is open or running", NAME, u.index());
                return 0;
            }
            if u.stuck.load(Ordering::Acquire) != 0 {
                serial_println!("{} unit {}: its task never stopped; the device stays loaded", NAME, u.index());
                return 0;
            }
        }
        for u in units() {
            let dev = u.dev.swap(null_mut(), Ordering::AcqRel);
            if !dev.is_null() {
                pci!(dev, Unlock);
                sys::wrappers::pci::pci_free_device(dev);
            }
        }
        NUNITS.store(0, Ordering::Release);
        let m = OPEN_LOCK.swap(null_mut(), Ordering::AcqRel);
        if !m.is_null() {
            sys::exec_free_sys_object(sys::ASOT_MUTEX, m as sys::APTR);
        }
        serial_println!("{}: expunged: cards unlocked, device unloaded", NAME);
    }
    1
}

// ---------------------------------------------------------------------------
// The interrupt server
// ---------------------------------------------------------------------------

/// The server's body for unit `u`. Interrupt context: no allocation, no
/// lock, no logging (DebugPrintF is serial I/O); it counts, and the task
/// reports.
///
/// IS IT OURS? The virtio ISR register answers it, and reading it also
/// clears the device's interrupt, which drops its level-triggered INTx line
/// before the server returns. Zero: another device on the shared line
/// raised it (another virtio-rng card among them: each unit has its own
/// server), so return 0 and let exec carry on down the chain (exec.doc
/// AddIntServer: servers run "until ... one of the servers returns with a
/// non-zero return value"). Non-zero ends the chain, and is returned only
/// once the device has said the interrupt was its own.
unsafe fn rng_irq_body(u: &Unit) -> u32 {
    let r = match u.regs() {
        Some(r) if !u.irq_task.load(Ordering::Acquire).is_null() => r,
        _ => {
            u.irq_passed_on.fetch_add(1, Ordering::Relaxed);
            return 0; // not ready
        }
    };
    if u.irq_storm.load(Ordering::Relaxed) != 0 || u.quiesced.load(Ordering::Relaxed) != 0 {
        u.irq_passed_on.fetch_add(1, Ordering::Relaxed);
        return 0; // reset (breaker, reboot): not asserting
    }
    u.irq_raised.fetch_add(1, Ordering::Relaxed);
    let isr = r.read_isr();
    if isr == 0 {
        u.irq_passed_on.fetch_add(1, Ordering::Relaxed);
        return 0;
    }
    let claimed = u.irq_claimed.fetch_add(1, Ordering::Relaxed).wrapping_add(1);

    // THE BREAKER (Unit::irq_storm).
    if claimed.wrapping_sub(u.irq_drained.load(Ordering::Relaxed)) > STORM_LIMIT {
        r.write_status(0);
        u.irq_storm.store(1, Ordering::Release);
    }

    // HAND IT ON. Signal is interrupt-safe (exec.doc Signal). Several
    // interrupts before the task runs collapse into one wake, which is
    // correct: the task takes every used buffer, not one per signal.
    if isr & (virtio::ISR_QUEUE | virtio::ISR_CONFIG) != 0 || u.irq_storm.load(Ordering::Relaxed) != 0 {
        let task = u.irq_task.load(Ordering::Acquire);
        if !task.is_null() {
            u.tb_signal.store(arch::timebase() as u32, Ordering::Relaxed);
            sys::exec_signal(task, u.irq_mask.load(Ordering::Relaxed));
            u.irq_signalled.fetch_add(1, Ordering::Relaxed);
        }
    }
    1
}

/// Set by the reset callback's first walk.
static QUIESCED_ALL: AtomicU32 = AtomicU32::new(0);

/// The reset callback (exec.doc AddResetCallback): on ColdReboot, and on a
/// power-off's walk of the same list, reset every card so none goes on with
/// DMA into memory the next boot will hand out again. Status 0 is the virtio
/// reset; reading the ISR drops the interrupt line. Guarded against a second
/// walk, no lock, independent of user or supervisor mode, one line.
unsafe extern "C" fn rng_reset(_ctx: *mut c_void, _execbase: *mut c_void, _data: *mut c_void) -> u32 {
    if QUIESCED_ALL.swap(1, Ordering::AcqRel) != 0 {
        return 0;
    }
    let mut n = 0;
    for u in units() {
        u.quiesced.store(1, Ordering::Release);
        if let Some(r) = u.regs() {
            r.write_status(0);
            let _ = r.read_isr();
            n += 1;
        }
    }
    serial_println!("{}: reset walk: {} card(s) reset (status 0), the OS continues", NAME, n);
    0
}

/// V50 interrupt server (exec.doc AddIntServer); is_Data is the unit. Timed
/// around the whole call, whichever way the body answers, because the
/// question is how long this driver holds the CPU in interrupt context. Its
/// one exit clears any lwarx reservation the body left open.
unsafe extern "C" fn rng_irq(_ctx: *mut c_void, _sysbase: *mut c_void, data: *mut c_void) -> u32 {
    let u = &*(data as *const Unit);
    let t0 = arch::timebase();
    let r = rng_irq_body(u);
    let dt = arch::timebase().wrapping_sub(t0) as u32;
    u.srv_runs.fetch_add(1, Ordering::Relaxed);
    u.srv_ticks_total.fetch_add(dt, Ordering::Relaxed);
    if dt > u.srv_ticks_longest.load(Ordering::Relaxed) {
        u.srv_ticks_longest.store(dt, Ordering::Relaxed); // one writer: the server
    }
    arch::clear_reservation();
    r
}

// ---------------------------------------------------------------------------
// Requests: the queue, the unit task, the device entry points
// ---------------------------------------------------------------------------
//
// CMD_READ requests go on the driver's own queue, guarded by an exec mutex
// (no Forbid), and the unit task is Signalled. The task takes one request
// at a time and marks it current. AbortIO removes a queued request and
// replies it IOERR_ABORTED, or flags the current one, which the task ends
// between pieces with the bytes it has so far. CMD_RESET and CMD_FLUSH abort
// everything queued and the current request. NSCMD_DEVICEQUERY and the
// errors are answered in BeginIO itself, honouring IOF_QUICK.
//
// BeginIO and AbortIO take the mutex, so they are for tasks, not interrupts.

/// NSCMD_DEVICEQUERY's command list, 0-terminated.
static SUPPORTED_COMMANDS: [u16; 5] = [CMD_RESET, CMD_READ, CMD_FLUSH, NSCMD_DEVICEQUERY, 0];

// ---------------------------------------------------------------------------
// Device errors and recovery
// ---------------------------------------------------------------------------
//
// Before each request the unit task checks the device: DRIVER_OK gone (it
// was reset behind the driver's back), DEVICE_NEEDS_RESET or FAILED set, the
// storm breaker's reset, or BROKEN (a failure left pieces in flight). Any of
// them and the device is recovered: reset, every outstanding mapping ended
// (the device no longer owns them once it is reset), a fresh queue built,
// features renegotiated and DRIVER_OK set again. At most MAX_RECOVERIES in
// a row: a successful read resets the count, so a card that fails now and
// then over a long uptime keeps working, and one that fails again straight
// after each recovery ends in a named error, not a loop.

/// virtiorng-specific: reset the device behind the driver's back, as a test
/// of recovery (rngread uses it). CMD_NONSTD.
const VRNG_CMD_TESTFAULT: u16 = 9;
const MAX_RECOVERIES: u32 = 8;

impl Rng {
    /// Why the device needs recovering, if it does.
    unsafe fn fault(&self) -> Option<&'static str> {
        let u = self.u;
        if u.broken.load(Ordering::Relaxed) != 0 {
            return Some("an earlier failure left pieces in flight");
        }
        if u.irq_storm.load(Ordering::Acquire) != 0 {
            return Some("the storm breaker reset the device");
        }
        let s = self.t.status();
        if s & virtio::STATUS_FAILED != 0 {
            return Some("the device reports FAILED");
        }
        if s & virtio::STATUS_DEVICE_NEEDS_RESET != 0 {
            return Some("the device reports DEVICE_NEEDS_RESET");
        }
        if s & virtio::STATUS_DRIVER_OK == 0 {
            return Some("DRIVER_OK is gone: the device was reset");
        }
        None
    }

    /// Reset the device and set it up again with a fresh queue.
    unsafe fn recover(&mut self) -> Result<(), String> {
        let u = self.u;
        let e = |x: amigaos4::AmigaError| alloc::format!("{}", x);
        // The device stops using the queue and the buffers once it is reset;
        // only then may their mappings end.
        self.t.reset().map_err(e)?;
        for s in self.slots.iter_mut() {
            s.busy = false;
            drop(s.map.take());
        }
        let size = self.vq.layout().size;
        self.vq = Virtqueue::new(size, self.t.ring_endian()).map_err(e)?;
        self.vq.ring().set_interrupts(true);
        virtio::init_device(&mut self.t, 0).map_err(e)?;
        self.t.setup_queue(0, &self.vq).map_err(e)?;
        virtio::driver_ok(&mut self.t).map_err(e)?;
        u.irq_drained.store(u.irq_claimed.load(Ordering::Relaxed), Ordering::Relaxed);
        u.irq_storm.store(0, Ordering::Release);
        u.broken.store(0, Ordering::Relaxed);
        Ok(())
    }

    /// Recover the device if it needs it. Err: it could not be, or it has
    /// failed too often this boot.
    unsafe fn ensure_healthy(&mut self) -> Result<(), String> {
        let u = self.u;
        let why = match self.fault() {
            None => return Ok(()),
            Some(w) => w,
        };
        let n = u.recoveries.fetch_add(1, Ordering::Relaxed) + 1;
        if n > MAX_RECOVERIES {
            return Err(alloc::format!("{}; given up after {} recoveries in a row", why, MAX_RECOVERIES));
        }
        serial_println!("{} unit {}: recovering ({} in a row, at most {}): {}", NAME, u.index(), n, MAX_RECOVERIES, why);
        self.recover()?;
        serial_println!("{} unit {}: recovered: status 0x{:02x}", NAME, u.index(), self.t.status());
        Ok(())
    }
}

/// Serve one queued request on the unit task. Returns (io_Error, io_Actual).
unsafe fn serve(rng: &mut Rng, ior: IoReq, timer: &mut Option<AmigaTimer>, irq_mask: u32) -> (i8, u32) {
    let u = rng.u;
    if ior.command() == VRNG_CMD_TESTFAULT {
        if let Some(r) = u.regs() {
            r.write_status(0);
        }
        serial_println!("{} unit {}: test fault: device reset behind the driver's back", NAME, u.index());
        return (0, 0);
    }
    if let Err(e) = rng.ensure_healthy() {
        serial_println!("{} unit {}: device unusable: {}", NAME, u.index(), e);
        return (IOERR_UNITBUSY, 0);
    }
    let r = serve_read(rng, ior, timer, irq_mask);
    if r.0 == 0 {
        // The device works again: the next failure starts a new count.
        u.recoveries.store(0, Ordering::Relaxed);
    }
    r
}

/// Serve one CMD_READ on the unit task. Returns (io_Error, io_Actual).
unsafe fn serve_read(rng: &mut Rng, ior: IoReq, timer: &mut Option<AmigaTimer>, irq_mask: u32) -> (i8, u32) {
    let u = rng.u;
    let data = ior.data();
    let len = ior.length() as usize;
    if data.is_null() && len != 0 {
        return (IOERR_BADLENGTH, 0);
    }
    // timer.device is in the kickstart, so an exec task may open it
    // (exec.doc CreateTask); it is opened on the first request.
    if irq_mask != 0 && timer.is_none() {
        match AmigaTimer::open(TimerUnit::MicroHz) {
            Ok(t) => *timer = Some(t),
            Err(e) => serial_println!("{}: timer.device: {}; requests are polled", NAME, e),
        }
    }
    // The cache-line-aligned interior of the caller's buffer, mapped for
    // the device to write directly when it is big enough. The mapping ends
    // (EndDMA) after the read, when the device owns no piece of it.
    let line = CACHE_LINE.load(Ordering::Relaxed) as usize;
    let start = (data as usize).wrapping_add(line - 1) & !(line - 1);
    let end = (data as usize).wrapping_add(len) & !(line - 1);
    let mut mapping: Option<DmaMapping<'static>> = None;
    if len > 0 && end > start && end - start >= DIRECT_MIN {
        match DmaMapping::new(start as *mut u8, end - start, DmaDirection::FromDevice) {
            Ok(m) => mapping = Some(m),
            Err(_) => {
                u.direct_refused.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    let pieces = match &mapping {
        Some(m) => {
            u.direct_requests.fetch_add(1, Ordering::Relaxed);
            plan(len, Some((start - data as usize, m.entries())))
        }
        None => plan(len, None),
    };
    let out = core::slice::from_raw_parts_mut(data, len);
    let abort = || u.queue.abort_requested();
    let result = match timer.as_mut() {
        Some(t) if irq_mask != 0 => rng.read(out, &pieces, &mut Waiter::Irq(t, irq_mask), &abort),
        _ => rng.read(out, &pieces, &mut Waiter::Poll, &abort),
    };
    drop(mapping);
    let (error, actual) = match result {
        Ok((n, Stop::Done)) => (0i8, n),
        Ok((n, Stop::Aborted)) => {
            u.aborted_current.fetch_add(1, Ordering::Relaxed);
            (IOERR_ABORTED, n)
        }
        Err(e) => {
            serial_println!("{} unit {}: CMD_READ failed: {}", NAME, u.index(), e);
            (IOERR_UNITBUSY, 0)
        }
    };
    u.requests.fetch_add(1, Ordering::Relaxed);
    u.bytes.fetch_add(actual as u32, Ordering::Relaxed);
    (error, actual as u32)
}

/// A unit task's entry (exec.doc CreateTask): AT_Param1, the unit number,
/// arrives as the first argument. It never returns: a kickstart device is
/// never unloaded.
extern "C" fn unit_task(n: u32) {
    // SAFETY: from here on this task is its unit's only user.
    unsafe {
        if n as usize >= MAX_UNITS {
            return;
        }
        let u = unit(n as usize);
        let rng = match (*u.rng.get()).as_mut() {
            Some(r) => r,
            None => return,
        };
        let irq_sig = sys::exec_alloc_signal(-1);
        let work_sig = sys::exec_alloc_signal(-1);
        if work_sig < 0 {
            serial_println!("{} unit {}: unit task: no free signal; no requests will be served", NAME, n);
            return;
        }
        let me = sys::exec_find_task(null_mut());

        // The interrupt server, if the chain and signal are there. Everything
        // it reads is stored before AddIntServer: it can run at once.
        let mut server: Option<IntServer> = None;
        let irq_mask = if irq_sig >= 0 { 1u32 << irq_sig } else { 0 };
        let chain = pci!(rng.dev, MapInterrupt);
        if irq_mask != 0 && chain != 0 {
            u.irq_mask.store(irq_mask, Ordering::Relaxed);
            u.irq_task.store(me, Ordering::Release);
            match IntServer::add(chain, 0, TASK_NAMES[n as usize], rng_irq, u as *const Unit as *mut c_void) {
                Ok(s) => {
                    u.irq_chain.store(chain, Ordering::Relaxed);
                    server = Some(s);
                }
                Err(e) => serial_println!("{} unit {}: AddIntServer({}) failed: {}", NAME, n, chain, e),
            }
        }
        if server.is_some() {
            // Interrupts on: clear VRING_AVAIL_F_NO_INTERRUPT.
            rng.vq.ring().set_interrupts(true);
            serial_println!(
                "{} unit {}: unit task ready: interrupt server on chain {} (PCI line {}), signal bit {}, pri {}",
                NAME, n, chain, pci!(rng.dev, ReadConfigByte, PCI_INTERRUPT_LINE), irq_sig, TASK_PRI
            );
        } else {
            serial_println!(
                "{} unit {}: unit task ready WITHOUT interrupts (chain {}, signal {}); requests are polled",
                NAME, n, chain, irq_sig
            );
        }
        let irq_mask = if server.is_some() { irq_mask } else { 0 };
        let work_mask = 1u32 << work_sig;
        u.worker.publish(me, work_mask);

        let mut timer: Option<AmigaTimer> = None;
        loop {
            sys::exec_wait(work_mask);
            if u.worker.stop_requested() {
                break;
            }
            while let Some(ior) = u.queue.take_next() {
                let (error, actual) = serve(rng, ior, &mut timer, irq_mask);
                // Done before the reply, so an AbortIO after this point
                // finds nothing and leaves the reply alone.
                u.queue.done();
                ior.reply(error, actual);
            }
        }

        // STOP (the disk build, at the unit's last close). Nothing is open,
        // so nothing should be queued; reply anything that is.
        u.worker.withdraw();
        for ior in u.queue.abort_all() {
            ior.reply(IOERR_ABORTED, 0);
        }
        // The card is reset BEFORE the server goes: a level-triggered line
        // left asserted with no server to claim it would storm the chain.
        let _ = rng.t.reset();
        let _ = rng.t.read_isr();
        u.quiesced.store(1, Ordering::Release);
        drop(server); // RemIntServer
        drop(timer);
        u.irq_task.store(null_mut(), Ordering::Release);
        if irq_sig >= 0 {
            sys::exec_free_signal(irq_sig);
        }
        sys::exec_free_signal(work_sig);
        u.running.store(0, Ordering::Release);
        // The answer is the task's last act in this segment's code that
        // matters; stop_unit still waits until exec has removed the task.
        let t = u.stop_ack_task.load(Ordering::Acquire);
        let m = u.stop_ack_mask.load(Ordering::Acquire);
        if !t.is_null() && m != 0 {
            sys::exec_signal(t, m);
        }
    }
}

#[no_mangle]
pub extern "C" fn rust_dev_open(ior: *mut IORequest, unit_number: u32, _flags: u32) -> i8 {
    if ior.is_null() || unit_number >= NUNITS.load(Ordering::Acquire) {
        return IOERR_OPENFAIL;
    }
    let u = unit(unit_number as usize);
    // The disk build starts a unit at its first open, and waits until its
    // task is ready, so a request straight after OpenDevice is served.
    #[cfg(feature = "disk")]
    let ok = unsafe {
        with_open_lock(|| {
            if u.opens.load(Ordering::Acquire) == 0 && u.running.load(Ordering::Acquire) == 0 {
                if u.stuck.load(Ordering::Acquire) != 0 {
                    return false;
                }
                if let Err(e) = start_unit(u) {
                    serial_println!("{} unit {}: open failed: {}", NAME, u.index(), e);
                    return false;
                }
                if !wait_ready(u) {
                    serial_println!("{} unit {}: open failed: the unit task did not get ready", NAME, u.index());
                    let _ = stop_unit(u);
                    return false;
                }
            }
            u.opens.fetch_add(1, Ordering::AcqRel);
            true
        })
    };
    #[cfg(not(feature = "disk"))]
    let ok = u.running.load(Ordering::Acquire) != 0;
    if !ok {
        return IOERR_OPENFAIL;
    }
    // SAFETY: the caller's IORequest; io_Unit names the unit from here on.
    if let Some(r) = unsafe { IoReq::from_raw(ior) } {
        r.set_unit(unit(unit_number as usize) as *const Unit as *mut c_void);
    }
    0
}

/// The unit an opened request names (io_Unit, set by OpenDevice).
#[inline]
fn unit_of(ior: IoReq) -> Option<&'static Unit> {
    let p = ior.unit() as *const Unit;
    units().find(|u| core::ptr::eq(*u, p))
}

/// Wait (bounded) until unit `u`'s task has published its work signal.
#[cfg(feature = "disk")]
unsafe fn wait_ready(u: &Unit) -> bool {
    let Ok(mut timer) = AmigaTimer::open(TimerUnit::MicroHz) else { return false };
    let mut waited = 0;
    while !u.worker.is_ready() {
        if waited >= STOP_BOUND_MS || u.running.load(Ordering::Acquire) == 0 {
            return false;
        }
        let _ = timer.delay(TimerVal::from_millis(GONE_POLL_MS));
        waited += GONE_POLL_MS;
    }
    true
}

/// Close: the disk build stops a unit at its last close.
#[no_mangle]
pub extern "C" fn rust_dev_close(ior: *mut IORequest) {
    #[cfg(feature = "disk")]
    // SAFETY: the caller's request, opened on this device; start and stop
    // are serialised by OPEN_LOCK.
    unsafe {
        let Some(r) = IoReq::from_raw(ior) else { return };
        let Some(u) = unit_of(r) else { return };
        with_open_lock(|| {
            if u.opens.fetch_sub(1, Ordering::AcqRel) == 1 {
                if let Err(e) = stop_unit(u) {
                    serial_println!("{} unit {}: stop FAILED: {}", NAME, u.index(), e);
                }
            }
        });
    }
    #[cfg(not(feature = "disk"))]
    let _ = ior;
}

/// Abort everything queued and the request in progress (CMD_RESET,
/// CMD_FLUSH). Queued requests are replied here; the current one is ended
/// by the unit task.
unsafe fn abort_all(u: &Unit) {
    for ior in u.queue.abort_all() {
        u.aborted_queued.fetch_add(1, Ordering::Relaxed);
        ior.reply(IOERR_ABORTED, 0);
    }
}

#[no_mangle]
pub extern "C" fn rust_dev_begin_io(ior: *mut IORequest) {
    // SAFETY: an IOStdReq from the caller, opened on this device.
    let Some(ior) = (unsafe { IoReq::from_raw(ior) }) else { return };
    unsafe {
        let Some(u) = unit_of(ior) else {
            ior.finish(IOERR_OPENFAIL, 0);
            return;
        };
        match ior.command() {
            CMD_READ | VRNG_CMD_TESTFAULT => {
                // Not quick: the unit task replies. push() marks it pending.
                if !u.worker.is_ready() {
                    ior.finish(IOERR_UNITBUSY, 0);
                    return;
                }
                u.queue.push(ior);
                if !u.worker.wake() {
                    // Stopped between the check and the push: take it back.
                    if u.queue.abort(ior) == Abort::Removed {
                        ior.reply(IOERR_UNITBUSY, 0);
                    }
                }
            }
            CMD_RESET | CMD_FLUSH => {
                abort_all(u);
                ior.finish(0, 0);
            }
            NSCMD_DEVICEQUERY => {
                let (e, n) = answer_device_query(ior, NSDEVTYPE_UNKNOWN, 0, &SUPPORTED_COMMANDS);
                ior.finish(e, n);
            }
            _ => ior.finish(IOERR_NOCMD, 0),
        }
    }
}

/// AbortIO: a queued request is removed and replied IOERR_ABORTED; the
/// current one is flagged and ended by the unit task; a finished one is
/// left alone.
#[no_mangle]
pub extern "C" fn rust_dev_abort_io(ior: *mut IORequest) {
    // SAFETY: the caller's request; the queue takes its own mutex, and a
    // removed request is ours to reply.
    let Some(ior) = (unsafe { IoReq::from_raw(ior) }) else { return };
    let Some(u) = unit_of(ior) else { return };
    if u.queue.abort(ior) == Abort::Removed {
        u.aborted_queued.fetch_add(1, Ordering::Relaxed);
        unsafe { ior.reply(IOERR_ABORTED, 0) };
    }
}

// ---------------------------------------------------------------------------
// AFTERDOS: use the device like any client
// ---------------------------------------------------------------------------

/// How long the AFTERDOS self-test waits for one request.
const SELFTEST_BOUND: TimerVal = TimerVal { secs: 3, micros: 0 };

/// Why a self-test read failed. `Stuck`: the request never came back, even
/// after AbortIO; it still belongs to the device, so the caller must not
/// close or delete it (or free its buffer or port).
enum ReadFail {
    Error(String),
    Stuck,
}

/// One CMD_READ, bounded. The AFTERDOS init runs synchronously on the boot
/// process (dos.doc CliInit: the AFTERDOS modules start "prior to executing
/// the startup-sequence"), so an unbounded wait here would stop the whole
/// boot. SendIO, then Wait for the reply port or the timer; on timeout
/// AbortIO and wait once more; if the request still has not come back,
/// give up without WaitIO.
unsafe fn client_read(
    ior: *mut IORequest,
    port: &AmigaMsgPort,
    timer: &mut AmigaTimer,
    buf: &mut [u8],
) -> Result<usize, ReadFail> {
    let r = match IoReq::from_raw(ior) {
        Some(r) => r,
        None => return Err(ReadFail::Error("no request".into())),
    };
    r.set_command(CMD_READ);
    r.set_data(buf.as_mut_ptr());
    r.set_length(buf.len() as u32);
    r.set_actual(0);
    sys::exec_send_io(ior);
    for attempt in 0..2 {
        timer.start(SELFTEST_BOUND);
        loop {
            if !sys::exec_check_io(ior).is_null() {
                timer.abort();
                sys::exec_wait_io(ior); // done: takes it off the port
                let err = r.error();
                if err != 0 {
                    return Err(ReadFail::Error(alloc::format!("io_Error {}", err)));
                }
                return Ok(r.actual() as usize);
            }
            sys::exec_wait(port.signal_mask() | timer.signal_mask());
            if timer.is_done() && sys::exec_check_io(ior).is_null() {
                timer.finish();
                break;
            }
        }
        if attempt == 0 {
            serial_println!("{}: [AFTERDOS] a read did not complete in 3 s; AbortIO", NAME);
            sys::exec_abort_io(ior);
        }
    }
    Err(ReadFail::Stuck)
}

fn self_test(u: &'static Unit) -> Result<(), String> {
    let n = u.index();
    let port = AmigaMsgPort::new().map_err(|e| alloc::format!("CreateMsgPort: {}", e))?;
    let mut timer = AmigaTimer::open(TimerUnit::MicroHz).map_err(|e| alloc::format!("timer.device: {}", e))?;
    // SAFETY: plain exec calls; the request is closed and deleted below,
    // except when it is stuck in the device.
    unsafe {
        let ior = sys::exec_create_iorequest(port.as_ptr(), IOSTDREQ_SIZE);
        if ior.is_null() {
            return Err("CreateIORequest failed".into());
        }
        if sys::exec_open_device(DEVNAME_C.as_ptr() as _, n as u32, ior, 0) != 0 {
            sys::exec_delete_iorequest(ior);
            return Err(alloc::format!("OpenDevice(\"{}\", {}) failed", NAME, n));
        }
        // On the heap, not the stack: a stuck read may still be written by
        // the device after this function returns.
        let mut a: Vec<u8> = vec![0; 64];
        let mut b: Vec<u8> = vec![0; 64];
        // 100003 bytes at an odd offset, with guard bytes either side: the
        // interior goes direct, the edges bounced, and nothing outside moves.
        let mut big: Vec<u8> = vec![GUARD; 100016];
        let mut results = [0usize; 3];
        let mut stuck = None;
        for i in 0..3 {
            let buf: &mut [u8] = match i {
                0 => &mut a,
                1 => &mut b,
                _ => &mut big[5..100008],
            };
            match client_read(ior, &port, &mut timer, buf) {
                Ok(n) => results[i] = n,
                Err(ReadFail::Error(e)) => {
                    sys::exec_close_device(ior);
                    sys::exec_delete_iorequest(ior);
                    return Err(alloc::format!("read {}: {}", i + 1, e));
                }
                Err(ReadFail::Stuck) => {
                    stuck = Some(i + 1);
                    break;
                }
            }
        }
        if let Some(n) = stuck {
            // The device still holds the request, a buffer and the reply
            // port: leak them all rather than free them under it.
            core::mem::forget(port);
            core::mem::forget(a);
            core::mem::forget(b);
            core::mem::forget(big);
            return Err(alloc::format!(
                "read {} never completed, even after AbortIO; the boot carries on without it",
                n
            ));
        }
        sys::exec_close_device(ior);
        sys::exec_delete_iorequest(ior);

        let [na, nb, nbig] = results;
        serial_println!("{} unit {}: [AFTERDOS] read 1: {}...", NAME, n, hex(&a[..16]));
        serial_println!("{} unit {}: [AFTERDOS] read 2: {}...", NAME, n, hex(&b[..16]));
        if na != 64 || nb != 64 || nbig != 100003 {
            return Err(alloc::format!("short reads: {} {} {}", na, nb, nbig));
        }
        if a.iter().all(|&x| x == a[0]) {
            return Err("read 1 is one repeated byte".into());
        }
        if a == b {
            return Err("two reads returned the same bytes".into());
        }
        let mut seen = [false; 256];
        for &x in &big[5..100008] {
            seen[x as usize] = true;
        }
        let distinct = seen.iter().filter(|&&s| s).count();
        let guards = big[..5].iter().chain(&big[100008..]).all(|&x| x == GUARD);
        serial_println!(
            "{} unit {}: [AFTERDOS] 100003-byte read at an odd address: {} distinct byte values, guard bytes {}",
            NAME, n, distinct, if guards { "intact" } else { "OVERWRITTEN" }
        );
        if distinct < 250 {
            return Err(alloc::format!("only {} distinct byte values in 100003 bytes", distinct));
        }
        if !guards {
            return Err("the read wrote outside its buffer".into());
        }
        serial_println!(
            "{} unit {}: [AFTERDOS] bytes direct {}, bounced {}; direct requests {}, StartDMA refused {}",
            NAME, n,
            u.bytes_direct.load(Ordering::Relaxed),
            u.bytes_bounced.load(Ordering::Relaxed),
            u.direct_requests.load(Ordering::Relaxed),
            u.direct_refused.load(Ordering::Relaxed)
        );
    }

    let by_irq = u.done_by_irq.load(Ordering::Relaxed);
    let by_timeout = u.done_by_timeout.load(Ordering::Relaxed);
    let chain = u.irq_chain.load(Ordering::Relaxed);
    serial_println!(
        "{} unit {}: [AFTERDOS] {} requests; pieces completed by interrupt {}, by timeout {}",
        NAME, n, u.requests.load(Ordering::Relaxed), by_irq, by_timeout
    );
    serial_println!(
        "{} unit {}: [AFTERDOS] server on chain {}: raised {}, claimed {}, passed on {}, signalled {}, storm {}",
        NAME, n,
        if chain == u32::MAX { -1 } else { chain as i64 },
        u.irq_raised.load(Ordering::Relaxed),
        u.irq_claimed.load(Ordering::Relaxed),
        u.irq_passed_on.load(Ordering::Relaxed),
        u.irq_signalled.load(Ordering::Relaxed),
        u.irq_storm.load(Ordering::Relaxed)
    );

    // Timebase ticks to microseconds (exec.doc GetCPUInfo, GCIT_TimeBaseSpeed).
    let mut tb_hz: u64 = 0;
    // SAFETY: GCIT_TimeBaseSpeed writes one uint64 through the pointer.
    unsafe {
        let tags = [
            TagItem { ti_Tag: sys::GCIT_TIMEBASE_SPEED, ti_Data: &mut tb_hz as *mut u64 as usize as u32 },
            TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
        ];
        sys::exec_get_cpuinfo(tags.as_ptr());
    }
    let us = |ticks: u64| -> u64 { if tb_hz == 0 { 0 } else { ticks * 1_000_000 / tb_hz } };
    let runs = u.srv_runs.load(Ordering::Relaxed) as u64;
    let total = u.srv_ticks_total.load(Ordering::Relaxed) as u64;
    let wakes = u.wakes.load(Ordering::Relaxed);
    serial_println!(
        "{} unit {}: [AFTERDOS] timebase {} Hz; server: {} runs, longest {} us, mean {} us; task wake after Signal: {} wakes, {}..{} us",
        NAME, n,
        tb_hz,
        runs,
        us(u.srv_ticks_longest.load(Ordering::Relaxed) as u64),
        if runs == 0 { 0 } else { us(total / runs) },
        wakes,
        if wakes == 0 { 0 } else { us(u.wake_ticks_min.load(Ordering::Relaxed) as u64) },
        us(u.wake_ticks_max.load(Ordering::Relaxed) as u64)
    );

    if chain == u32::MAX {
        return Err("no interrupt server was installed".into());
    }
    if u.irq_storm.load(Ordering::Relaxed) != 0 {
        return Err("the storm breaker fired".into());
    }
    if by_irq == 0 || by_timeout != 0 {
        return Err(alloc::format!(
            "interrupts did not drive every completion ({} by interrupt, {} by timeout)",
            by_irq, by_timeout
        ));
    }
    Ok(())
}

/// Called from the AFTERDOS Resident's init.
#[no_mangle]
pub extern "C" fn rust_rng_afterdos() {
    let count = NUNITS.load(Ordering::Acquire);
    if count == 0 {
        serial_println!("{}: [AFTERDOS] no unit was set up; self-test skipped", NAME);
        return;
    }
    let mut failed = 0;
    for u in units() {
        match self_test(u) {
            Ok(()) => serial_println!("{} unit {}: self-test passed", NAME, u.index()),
            Err(e) => {
                failed += 1;
                serial_println!("{} unit {}: self-test FAILED: {}", NAME, u.index(), e);
            }
        }
    }
    if failed == 0 {
        serial_println!("{}: self-test PASSED ({} unit(s))", NAME, count);
    } else {
        serial_println!("{}: self-test FAILED ({} of {} units)", NAME, failed, count);
    }
}
