//! virtio-net: virtionet.device, a SANA-II network driver for QEMU's
//! virtio-net PCI device (`-device virtio-net-pci`, or
//! `virtio-net-pci-non-transitional`), loaded from DEVS:Networks/.
//!
//! - Init finds and locks every card (transitional 1af4:1000 first, then
//!   modern-only 1af4:1041), one unit each, up to MAX_UNITS.
//! - A unit is set up at its first open and stopped at its last close: the
//!   receive, transmit and (if offered) control queues, receive buffers
//!   kept posted, an interrupt server and a unit task.
//! - Each OpenDevice is an opener with its own buffer management (the
//!   stack's copy hook, S2_SANA2HOOK, or its OpenDevice copy functions),
//!   returned as ios2_BufferManagement.
//! - Received frames go to one pending CMD_READ of their type per opener;
//!   with none, to one S2_READORPHAN per opener. Frames to other stations
//!   and multicast groups nobody added are dropped unless an opener is
//!   promiscuous.
//! - CMD_WRITE, S2_BROADCAST and S2_MULTICAST copy the frame into one of
//!   the transmit slots in BeginIO itself, in the caller's task, and reply
//!   once it is posted; with every slot in flight they wait (pending) and
//!   the unit task sends them as slots come back.
//! - Statistics (global, extended, special, per tracked type), events,
//!   throughput sampling, multicast with reference counts (the device's
//!   filter table through the control queue, or all multicast when it is
//!   full), promiscuous and exclusive opens.
//!
//! No Forbid or Disable: each unit's shared state and its transmit ring are
//! under their own exec mutexes (taken in that order: transmit, then
//! shared). The transmit queue's notify register is written by whoever
//! holds the transmit lock; everything else on the device is touched only
//! by the unit task (and its interrupt server, which only reads the ISR).
//! Counters are atomics.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::panic::PanicInfo;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use amigaos4::allocator::SharedExecAllocator;
use amigaos4::arch;
use amigaos4::device::cmd::{CMD_FLUSH, CMD_READ, CMD_WRITE, NSCMD_DEVICEQUERY};
use amigaos4::device::ioerr::{IOERR_ABORTED, IOERR_BADLENGTH, IOERR_NOCMD, IOERR_OPENFAIL, IOERR_UNITBUSY};
use amigaos4::device::{answer_device_query, Abort, IoReq, RequestQueue, Worker};
use amigaos4::interrupt::{IntServer, ResetCallback};
use amigaos4::mem::{DmaBuffer, DmaDirection, DmaMapping};
use amigaos4::sana2::*;
use amigaos4::serial_println;
use amigaos4::sys::{self, IORequest, PCIDevice, TagItem, Task};
use amigaos4::timer::{AmigaTimer, TimerUnit, TimerVal};
use amigaos4::virtio::net as vn;
use amigaos4::virtio::ring::{DESC_F_NEXT, DESC_F_WRITE};
use amigaos4::virtio::{self, Notifier, PciTransport, Regs, Transport, Virtqueue};

#[global_allocator]
// MEMF_SHARED: BeginIO, Open and Close run in the callers' tasks.
static ALLOCATOR: SharedExecAllocator = SharedExecAllocator;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    amigaos4::panic::default_panic_handler(info)
}

const NAME: &str = "virtionet.device";
const DEVNAME_C: &[u8] = amigaos4::amstr!("virtionet.device");
const TASK_PRI: i32 = 5;
const TASK_STACK: u32 = 65536;
const MAX_UNITS: usize = 4;

/// One receive or transmit buffer: the virtio-net header and a frame.
const BUF_SIZE: usize = 2048;
/// Receive buffers kept posted (bounded by the queue size).
const RX_BUFFERS: usize = 128;
/// Transmit slots (bounded by the queue size).
// The `test-tx-hold` build has 4 slots and kicks the device only from the
// unit task, so writes find every slot busy and wait (QEMU otherwise sends
// a frame inside the notify itself and the ring never fills).
const TX_SLOTS: usize = if cfg!(feature = "test-tx-hold") { 4 } else { 64 };
/// Multicast addresses the device's filter table takes before the unit
/// falls back to receiving all multicast.
const MCAST_TABLE_MAX: usize = 64;
/// Ranges (S2_ADDMULTICASTADDRESSES) larger than this receive all multicast.
const MCAST_RANGE_MAX: u64 = 64;
/// Polls of the control queue's used ring before a command is given up.
const CTRL_POLL_LIMIT: u32 = 2_000_000;
const STORM_LIMIT: u32 = 1000;
const MAX_RECOVERIES: u32 = 8;

/// Test only: reset the device behind the driver's back (recovery test).
const VNET_CMD_TESTFAULT: u16 = 0x8001;
/// How often an idle unit task wakes to read the device's status.
const HEARTBEAT_MS: u32 = 1000;
/// A received frame that finds no read waits in the ring at most this long
/// for the stack's next read, and the unit task looks again this often.
const HOLD_MS: u64 = 100;
const HOLD_POLL_MS: u32 = 10;

/// Features asked for. Offloads (checksum, TSO, UFO), multiqueue and VLAN
/// filtering are not: SANA-II has no way to hand them to a stack.
const WANTED: u64 = vn::F_MAC
    | vn::F_STATUS
    | vn::F_MTU
    | vn::F_MRG_RXBUF
    | vn::F_CTRL_VQ
    | vn::F_CTRL_RX
    | vn::F_CTRL_MAC_ADDR
    | vn::F_GUEST_ANNOUNCE
    | vn::F_SPEED_DUPLEX;

const PCI_INTERRUPT_LINE: u32 = 0x3C;

macro_rules! pci {
    ($dev:expr, $method:ident $(, $arg:expr)*) => {
        ((*$dev).$method)($dev $(, $arg)*)
    };
}

// ---------------------------------------------------------------------------
// Shared unit state, under the unit's mutex
// ---------------------------------------------------------------------------

/// A value under an exec mutex, const-constructible for a static.
struct Locked<T> {
    value: UnsafeCell<T>,
    mutex: AtomicPtr<c_void>,
}
// SAFETY: `value` is only touched with `mutex` held (or by one user before
// init / after destroy).
unsafe impl<T> Sync for Locked<T> {}

impl<T> Locked<T> {
    const fn new(v: T) -> Self {
        Self { value: UnsafeCell::new(v), mutex: AtomicPtr::new(null_mut()) }
    }
    unsafe fn init(&self) -> bool {
        let m = sys::exec_alloc_sys_object(sys::ASOT_MUTEX, [TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 }].as_ptr());
        self.mutex.store(m as *mut c_void, Ordering::Release);
        !m.is_null()
    }
    unsafe fn destroy(&self) {
        let m = self.mutex.swap(null_mut(), Ordering::AcqRel);
        if !m.is_null() {
            sys::exec_free_sys_object(sys::ASOT_MUTEX, m as sys::APTR);
        }
    }
    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let m = self.mutex.load(Ordering::Acquire);
        // SAFETY: the mutex guards the value.
        unsafe {
            if !m.is_null() {
                sys::exec_mutex_obtain(m as sys::APTR);
            }
            let r = f(&mut *self.value.get());
            if !m.is_null() {
                sys::exec_mutex_release(m as sys::APTR);
            }
            r
        }
    }
}

/// One OpenDevice: the stack's buffer management and open flags. Its
/// address is the ios2_BufferManagement cookie the stack's requests carry.
struct Opener {
    bm: BufferManagement,
    flags: u32,
}

#[derive(Clone, Copy, Default)]
struct Stats {
    rx_packets: u64,
    tx_packets: u64,
    rx_bytes: u64,
    tx_bytes: u64,
    bad_data: u64,
    overruns: u64,
    unknown_types: u64,
    reconfigurations: u64,
    rx_no_read: u64,
    rx_filtered: u64,
    tx_waits: u64,
    mcast_rejects: u64,
    recoveries: u64,
}

struct Shared {
    openers: Vec<*mut Opener>,
    reads: Vec<S2Req>,
    orphans: Vec<S2Req>,
    events: Vec<S2Req>,
    samples: Vec<S2Req>,
    /// S2_TRACKTYPE: (type, statistics, opener), one entry per opener.
    tracked: Vec<(u32, Sana2PacketTypeStats, *mut Opener)>,
    stats: Stats,
    online: bool,
    configured: bool,
    link_up: bool,
    station: [u8; 6],
    factory: [u8; 6],
    mtu: u32,
    bps: u32,
    last_start: TimeVal,
    mcast: Vec<([u8; 6], u32)>,
    /// Large S2_ADDMULTICASTADDRESSES ranges in force (all multicast).
    mcast_wide: Vec<(u64, u64, u32)>,
    promisc_openers: u32,
    exclusive: bool,
    /// Packet types each opener has queued CMD_READs for: a frame of one of
    /// these that finds no read is held in the ring, not dropped.
    read_types: Vec<(u32, *mut Opener)>,
    /// A frame is held for a read: BeginIO wakes the unit task when one comes.
    rx_held: bool,
}

impl Shared {
    const fn new() -> Self {
        Shared {
            openers: Vec::new(),
            reads: Vec::new(),
            orphans: Vec::new(),
            events: Vec::new(),
            samples: Vec::new(),
            tracked: Vec::new(),
            stats: Stats {
                rx_packets: 0,
                tx_packets: 0,
                rx_bytes: 0,
                tx_bytes: 0,
                bad_data: 0,
                overruns: 0,
                unknown_types: 0,
                reconfigurations: 0,
                rx_no_read: 0,
                rx_filtered: 0,
                tx_waits: 0,
                mcast_rejects: 0,
                recoveries: 0,
            },
            online: false,
            configured: false,
            link_up: true,
            station: [0; 6],
            factory: [0; 6],
            mtu: vn::ETH_MTU as u32,
            bps: 1_000_000_000,
            last_start: TimeVal { Seconds: 0, Microseconds: 0 },
            mcast: Vec::new(),
            mcast_wide: Vec::new(),
            promisc_openers: 0,
            exclusive: false,
            read_types: Vec::new(),
            rx_held: false,
        }
    }
    fn opener_ok(&self, bm: *mut c_void) -> bool {
        !bm.is_null() && self.openers.iter().any(|&o| o as *mut c_void == bm)
    }
    fn mcast_wanted(&self, mac: &[u8; 6]) -> bool {
        if self.mcast.iter().any(|(m, _)| m == mac) {
            return true;
        }
        let v = mac_u64(mac);
        self.mcast_wide.iter().any(|&(lo, hi, _)| v >= lo && v <= hi)
    }
}

fn mac_u64(m: &[u8; 6]) -> u64 {
    m.iter().fold(0u64, |a, &b| a << 8 | b as u64)
}
fn u64_mac(v: u64) -> [u8; 6] {
    let b = v.to_be_bytes();
    [b[2], b[3], b[4], b[5], b[6], b[7]]
}

// ---------------------------------------------------------------------------
// Units
// ---------------------------------------------------------------------------

struct Unit {
    dev: AtomicPtr<PCIDevice>,
    opens: AtomicU32,
    running: AtomicU32,
    stuck: AtomicU32,
    stop_ack_task: AtomicPtr<Task>,
    stop_ack_mask: AtomicU32,
    /// The device, owned by the unit task while it runs.
    net: UnsafeCell<Option<Net>>,
    regs: UnsafeCell<Option<Regs>>,
    regs_ready: AtomicU32,
    quiesced: AtomicU32,
    irq_task: AtomicPtr<Task>,
    irq_mask: AtomicU32,
    irq_raised: AtomicU32,
    irq_claimed: AtomicU32,
    irq_passed_on: AtomicU32,
    irq_storm: AtomicU32,
    irq_drained: AtomicU32,
    irq_config: AtomicU32,
    /// The transmit ring, used by BeginIO in the stack's task and by the
    /// unit task, under its own mutex.
    tx: Locked<Option<TxRing>>,
    /// The time the unit task last read (for statistics stamped in BeginIO).
    now_s: AtomicU32,
    now_us: AtomicU32,
    /// Requests that need the device, for the unit task.
    cmdq: RequestQueue,
    worker: Worker,
    shared: Locked<Shared>,
    /// The receive mode or filter table must be sent again.
    rx_mode_dirty: AtomicU32,
    /// Out of service after repeated failed recoveries: writes fail at once.
    out_of_service: AtomicU32,
    /// Cost counters (S2_GETSPECIALSTATS), indexed by `perf::*`.
    perf: [AtomicU32; perf::N],
}

/// What each frame costs: counters for S2_GETSPECIALSTATS, so a profiler
/// can show interrupts, wakes, notifications and ACKs per frame for any
/// stack on the card. Atomics: they are counted in BeginIO, in the unit
/// task and in the interrupt server without the lock.
mod perf {
    pub const WAKES: usize = 0;
    pub const WAKES_RX: usize = 1;
    pub const TX_PUBLISH: usize = 2;
    pub const TX_NOTIFY: usize = 3;
    pub const RX_NOTIFY: usize = 4;
    pub const TX_ACKS: usize = 5;
    pub const RX_ACKS: usize = 6;
    /// Time inside the transmit notification (a register write that QEMU
    /// serves at once), in tenths of a microsecond.
    pub const TX_NOTIFY_T: usize = 7;
    /// Time the interrupt handler spends reading the ISR register.
    pub const ISR_T: usize = 8;
    /// Time in the driver's write path in the caller's task (all of it,
    /// the notification included).
    pub const TX_CALL_T: usize = 9;
    pub const N: usize = 10;
}

/// Timebase ticks per microsecond, set when the first unit starts.
static TB_PER_US: AtomicU32 = AtomicU32::new(0);

/// Tenths of a microsecond since `t0` (a timebase reading); 0 before the
/// tick rate is known.
fn tenths_since(t0: u64) -> u32 {
    let per = TB_PER_US.load(Ordering::Relaxed) as u64;
    if per == 0 {
        return 0;
    }
    (arch::timebase().wrapping_sub(t0) * 10 / per) as u32
}

/// Whether an Ethernet frame is an IPv4 TCP segment with no data and only
/// the ACK flag (a pure acknowledgement).
fn pure_ack(f: &[u8]) -> bool {
    if f.len() < 14 + 20 + 20 || f[12] != 0x08 || f[13] != 0x00 || f[14] >> 4 != 4 || f[23] != 6 {
        return false;
    }
    let ihl = ((f[14] & 15) as usize) * 4;
    let total = u16::from_be_bytes([f[16], f[17]]) as usize;
    let t = 14 + ihl;
    if f.len() < t + 20 {
        return false;
    }
    let doff = ((f[t + 12] >> 4) as usize) * 4;
    total == ihl + doff && f[t + 13] == 0x10
}

impl Unit {
    fn count(&self, i: usize) {
        self.perf[i].fetch_add(1, Ordering::Relaxed);
    }

    fn add_time(&self, i: usize, t0: u64) {
        self.perf[i].fetch_add(tenths_since(t0), Ordering::Relaxed);
    }

    const NEW: Unit = Unit {
        dev: AtomicPtr::new(null_mut()),
        opens: AtomicU32::new(0),
        running: AtomicU32::new(0),
        stuck: AtomicU32::new(0),
        stop_ack_task: AtomicPtr::new(null_mut()),
        stop_ack_mask: AtomicU32::new(0),
        net: UnsafeCell::new(None),
        regs: UnsafeCell::new(None),
        regs_ready: AtomicU32::new(0),
        quiesced: AtomicU32::new(0),
        irq_task: AtomicPtr::new(null_mut()),
        irq_mask: AtomicU32::new(0),
        irq_raised: AtomicU32::new(0),
        irq_claimed: AtomicU32::new(0),
        irq_passed_on: AtomicU32::new(0),
        irq_storm: AtomicU32::new(0),
        irq_drained: AtomicU32::new(0),
        irq_config: AtomicU32::new(0),
        tx: Locked::new(None),
        now_s: AtomicU32::new(0),
        now_us: AtomicU32::new(0),
        cmdq: RequestQueue::new(),
        worker: Worker::new(),
        shared: Locked::new(Shared::new()),
        rx_mode_dirty: AtomicU32::new(0),
        out_of_service: AtomicU32::new(0),
        perf: [const { AtomicU32::new(0) }; perf::N],
    };

    unsafe fn regs(&self) -> Option<Regs> {
        if self.regs_ready.load(Ordering::Acquire) != 0 { *self.regs.get() } else { None }
    }

    fn now(&self) -> TimeVal {
        TimeVal { Seconds: self.now_s.load(Ordering::Relaxed), Microseconds: self.now_us.load(Ordering::Relaxed) }
    }

    fn index(&'static self) -> usize {
        // SAFETY: every Unit lives in UNITS.
        unsafe { (self as *const Unit).offset_from(UNITS.0.get() as *const Unit) as usize }
    }
}

struct Units(UnsafeCell<[Unit; MAX_UNITS]>);
// SAFETY: each field is an atomic or a cell with one user at a time.
unsafe impl Sync for Units {}
static UNITS: Units = Units(UnsafeCell::new([Unit::NEW; MAX_UNITS]));
static NUNITS: AtomicU32 = AtomicU32::new(0);
static OPEN_LOCK: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

fn unit(n: usize) -> &'static Unit {
    // SAFETY: shared references; every field has interior mutability.
    unsafe { &(*UNITS.0.get())[n] }
}
fn units() -> impl Iterator<Item = &'static Unit> {
    (0..NUNITS.load(Ordering::Acquire) as usize).map(unit)
}

const TASK_NAMES: [&[u8]; MAX_UNITS] = [
    amigaos4::amstr!("virtionet.device unit 0"),
    amigaos4::amstr!("virtionet.device unit 1"),
    amigaos4::amstr!("virtionet.device unit 2"),
    amigaos4::amstr!("virtionet.device unit 3"),
];

// ---------------------------------------------------------------------------
// The device (owned by the unit task)
// ---------------------------------------------------------------------------

struct Net {
    dev: *mut PCIDevice,
    t: PciTransport,
    features: u64,
    modern: bool,
    hdr: usize,
    rxq: Virtqueue,
    ctrlq: Option<Virtqueue>,
    rx_n: usize,
    // Mappings before their buffers: they drop (EndDMA) first. Each pool
    // is mapped once for the unit's life: virtio exists only in QEMU,
    // which is cache-coherent, so the device and the CPU always see the
    // same bytes and per-frame StartDMA/EndDMA would only cost time. (A
    // driver for non-coherent hardware maps each transfer, as virtiorng
    // does.)
    _rx_pool_map: Option<DmaMapping<'static>>,
    ctrl_map: Option<DmaMapping<'static>>,
    rx_phys: u64,
    /// The device owns receive buffer i.
    rx_posted: Vec<bool>,
    rx_bufs: DmaBuffer,
    ctrl_buf: DmaBuffer,
    u: &'static Unit,
    /// What the device was last told (to avoid repeating control commands).
    sent_promisc: Option<bool>,
    sent_allmulti: Option<bool>,
    sent_table: Option<Vec<[u8; 6]>>,
    sent_mac: Option<[u8; 6]>,
    /// The used buffer (id, length) waiting for a read, and since when
    /// (timebase). While it waits, no further buffer is taken: the ring
    /// fills and the device holds back, instead of frames being dropped.
    held: Option<(u32, u32, u64)>,
    /// HOLD_MS in timebase ticks.
    hold_limit: u64,
}

/// Find every virtio-net card, locked for this driver, in unit order.
unsafe fn find_cards() -> Vec<*mut PCIDevice> {
    let mut found = Vec::new();
    for id in [vn::PCI_ID_TRANSITIONAL, vn::PCI_ID_MODERN] {
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

fn hex_mac(m: &[u8; 6]) -> String {
    alloc::format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

impl Net {
    /// Set the card up: features, queues, buffers. Interrupts stay off
    /// until the unit task's server is in place.
    unsafe fn setup(u: &'static Unit, dev: *mut PCIDevice) -> Result<Net, String> {
        let n = u.index();
        let e = |x: amigaos4::AmigaError| alloc::format!("{}", x);
        let device = pci!(dev, ReadConfigWord, 2);
        let irq = pci!(dev, ReadConfigByte, PCI_INTERRUPT_LINE);
        virtio::enable_pci(dev, false);
        #[cfg(not(feature = "force-legacy"))]
        let (mut t, why_legacy) = PciTransport::open(dev).map_err(e)?;
        #[cfg(feature = "force-legacy")]
        let (mut t, why_legacy) = {
            let l = virtio::LegacyPci::new(dev).map_err(e)?;
            (PciTransport::Legacy(l), Some("force-legacy test build"))
        };
        let modern = t.is_modern();
        let offered = t.device_features();
        let features = virtio::init_device(&mut t, WANTED).map_err(e)?;
        let hdr = vn::hdr_len(modern, features);

        let rx_max = t.queue_max(vn::RX_QUEUE);
        let tx_max = t.queue_max(vn::TX_QUEUE);
        if rx_max == 0 || tx_max == 0 {
            t.set_status(virtio::STATUS_FAILED);
            return Err("no receive or transmit queue".into());
        }
        // The legacy interface needs the size the device reports; the
        // modern one takes any power of two up to it.
        let pick = |max: u16, want: usize| -> u16 {
            if modern { (max as usize).min(want.next_power_of_two()).max(1) as u16 } else { max }
        };
        let rx_size = pick(rx_max, RX_BUFFERS);
        let tx_size = pick(tx_max, TX_SLOTS);
        let endian = t.ring_endian();
        let mut rxq = Virtqueue::new(rx_size, endian).map_err(e)?;
        let mut txq = Virtqueue::new(tx_size, endian).map_err(e)?;
        rxq.ring().set_interrupts(false);
        txq.ring().set_interrupts(false);
        t.setup_queue(vn::RX_QUEUE, &rxq).map_err(e)?;
        t.setup_queue(vn::TX_QUEUE, &txq).map_err(e)?;
        let ctrlq = if features & vn::F_CTRL_VQ != 0 {
            let cmax = t.queue_max(vn::CTRL_QUEUE);
            if cmax == 0 {
                None
            } else {
                let mut q = Virtqueue::new(pick(cmax, 8), endian).map_err(e)?;
                q.ring().set_interrupts(false);
                t.setup_queue(vn::CTRL_QUEUE, &q).map_err(e)?;
                Some(q)
            }
        } else {
            None
        };
        let rx_n = (rx_size as usize).min(RX_BUFFERS);
        let tx_n = (tx_size as usize).min(TX_SLOTS);
        let rx_bufs = DmaBuffer::alloc_contiguous(rx_n * BUF_SIZE, 4096).map_err(e)?;
        let tx_bufs = DmaBuffer::alloc_contiguous(tx_n * BUF_SIZE, 4096).map_err(e)?;
        let ctrl_buf = DmaBuffer::alloc_contiguous(1024, 64).map_err(e)?;
        let map_pool = |buf: &mut DmaBuffer, len: usize, dir: DmaDirection| -> Result<(DmaMapping<'static>, u64), String> {
            let m = DmaMapping::new(buf.as_mut_ptr(), len, dir).map_err(|x| alloc::format!("pool map: {}", x))?;
            let ent = m.entries();
            if ent.len() != 1 {
                return Err(alloc::format!("a buffer pool is {} DMA blocks, needs 1", ent.len()));
            }
            let phys = ent[0].PhysicalAddress as usize as u64;
            Ok((m, phys))
        };
        let mut rx_bufs = rx_bufs;
        let mut tx_bufs = tx_bufs;
        let (rx_pool_map, rx_phys) = map_pool(&mut rx_bufs, rx_n * BUF_SIZE, DmaDirection::FromDevice)?;
        let (tx_pool_map, tx_phys) = map_pool(&mut tx_bufs, tx_n * BUF_SIZE, DmaDirection::ToDevice)?;
        // Without it a frame would be posted and never sent, silently.
        let notifier = Some(t.notifier(vn::TX_QUEUE).ok_or("the transmit queue has no notify address")?);
        virtio::driver_ok(&mut t).map_err(e)?;
        u.tx.with(|tx| {
            *tx = Some(TxRing {
                txq,
                _pool_map: Some(tx_pool_map),
                bufs: tx_bufs,
                phys: tx_phys,
                n: tx_n,
                free: (0..tx_n as u16).rev().collect(),
                busy: alloc::vec![false; tx_n],
                unpublished: false,
                unkicked: false,
                pending: VecDeque::new(),
                hdr,
                notifier,
            })
        });

        let mut factory = [0u8; 6];
        if features & vn::F_MAC != 0 {
            for (i, b) in factory.iter_mut().enumerate() {
                *b = t.config_u8(vn::CFG_MAC + i as u32);
            }
        } else {
            // No address offered: a locally administered one from the timebase.
            let tb = arch::timebase().to_be_bytes();
            factory = [0x02, 0x00, tb[4], tb[5], tb[6], tb[7] ^ n as u8];
        }
        let mtu = if features & vn::F_MTU != 0 { (t.config_u16(vn::CFG_MTU) as u32).clamp(68, 1500) } else { 1500 };
        let speed = if features & vn::F_SPEED_DUPLEX != 0 { t.config_u32(vn::CFG_SPEED) } else { vn::SPEED_UNKNOWN };
        let bps = if speed == vn::SPEED_UNKNOWN || speed == 0 { 1_000_000_000 } else { (speed as u64 * 1_000_000).min(u32::MAX as u64) as u32 };
        let link_up = if features & vn::F_STATUS != 0 { t.config_u16(vn::CFG_STATUS) & vn::S_LINK_UP != 0 } else { true };

        match (&t, why_legacy) {
            (PciTransport::Modern(_), _) => serial_println!("{} unit {}: 1af4:{:04x}, modern interface, IRQ line {}", NAME, n, device, irq),
            (PciTransport::Legacy(_), why) => serial_println!(
                "{} unit {}: 1af4:{:04x}, legacy interface (modern not used: {}), IRQ line {}",
                NAME, n, device, why.unwrap_or("-"), irq
            ),
        }
        serial_println!(
            "{} unit {}: features offered 0x{:x}, accepted 0x{:x}; header {} bytes; rx {} buffers (queue {}), tx {} slots (queue {}), control queue {}",
            NAME, n, offered, features, hdr, rx_n, rx_size, tx_n, tx_size, if ctrlq.is_some() { "yes" } else { "no" }
        );
        serial_println!(
            "{} unit {}: MAC {}, MTU {}, {} Mbit/s, link {}",
            NAME, n, hex_mac(&factory), mtu, bps / 1_000_000, if link_up { "up" } else { "down" }
        );
        u.shared.with(|s| {
            s.factory = factory;
            s.station = factory;
            s.mtu = mtu;
            s.bps = bps;
            s.link_up = link_up;
        });

        let mut net = Net {
            dev,
            t,
            features,
            modern,
            hdr,
            rxq,
            ctrlq,
            rx_n,
            _rx_pool_map: Some(rx_pool_map),
            ctrl_map: None,
            rx_phys,
            rx_posted: alloc::vec![false; rx_n],
            rx_bufs,
            ctrl_buf,
            u,
            sent_promisc: None,
            sent_allmulti: None,
            sent_table: None,
            sent_mac: None,
            held: None,
            hold_limit: timebase_hz() * HOLD_MS / 1000,
        };
        for i in 0..net.rx_n {
            net.post_rx(i)?;
        }
        net.rxq.ring().publish();
        net.t.notify(vn::RX_QUEUE);
        Ok(net)
    }

    unsafe fn rx_ptr(&mut self, i: usize) -> *mut u8 {
        self.rx_bufs.as_mut_ptr().add(i * BUF_SIZE)
    }

    /// Give receive buffer `i` to the device (not yet published).
    unsafe fn post_rx(&mut self, i: usize) -> Result<(), String> {
        let phys = self.rx_phys + (i * BUF_SIZE) as u64;
        self.rx_posted[i] = true;
        let ring = self.rxq.ring();
        ring.set_desc(i as u16, phys, BUF_SIZE as u32, DESC_F_WRITE, 0);
        ring.push_avail(i as u16);
        Ok(())
    }

    /// Send a control command: class, command, data. True: CTRL_OK.
    unsafe fn ctrl(&mut self, class: u8, cmd: u8, data: &[u8]) -> bool {
        let Some(q) = self.ctrlq.as_mut() else { return false };
        if data.len() + 3 > 1024 {
            return false;
        }
        let base = self.ctrl_buf.as_mut_ptr();
        *base = class;
        *base.add(1) = cmd;
        core::ptr::copy_nonoverlapping(data.as_ptr(), base.add(2), data.len());
        let out_len = 2 + data.len();
        let ack = base.add(1023);
        *ack = 0xff;
        let Ok(map) = DmaMapping::new(base, 1024, DmaDirection::ToDevice) else { return false };
        let ent = map.entries();
        if ent.len() != 1 {
            return false;
        }
        let phys = ent[0].PhysicalAddress as usize as u64;
        let ring = q.ring();
        ring.set_desc(0, phys, out_len as u32, DESC_F_NEXT, 1);
        ring.set_desc(1, phys + 1023, 1, DESC_F_WRITE, 0);
        ring.push_avail(0);
        ring.publish();
        self.ctrl_map = Some(map);
        self.t.notify(vn::CTRL_QUEUE);
        let mut polls = 0u32;
        let done = loop {
            if let Some(q) = self.ctrlq.as_mut() {
                if q.ring().take_used().is_some() {
                    break true;
                }
            }
            polls += 1;
            if polls >= CTRL_POLL_LIMIT {
                break false;
            }
        };
        drop(self.ctrl_map.take()); // EndDMA: the ack byte is visible
        done && *ack == vn::CTRL_OK
    }

    /// Bring the device's receive mode, filter table and MAC address in line
    /// with the openers and multicast list.
    unsafe fn apply_rx_mode(&mut self) {
        let (promisc, mut table, wide, station, factory) = self.u.shared.with(|s| {
            (
                s.promisc_openers > 0,
                s.mcast.iter().map(|(m, _)| *m).collect::<Vec<_>>(),
                !s.mcast_wide.is_empty(),
                s.station,
                s.factory,
            )
        });
        let allmulti = wide || table.len() > MCAST_TABLE_MAX;
        if allmulti {
            table.clear();
        }
        if self.features & vn::F_CTRL_RX != 0 {
            if self.sent_promisc != Some(promisc) {
                let ok = self.ctrl(vn::CTRL_RX, vn::CTRL_RX_PROMISC, &[promisc as u8]);
                self.sent_promisc = if ok { Some(promisc) } else { None };
            }
            if self.sent_allmulti != Some(allmulti) {
                let ok = self.ctrl(vn::CTRL_RX, vn::CTRL_RX_ALLMULTI, &[allmulti as u8]);
                self.sent_allmulti = if ok { Some(allmulti) } else { None };
            }
        }
        if self.features & vn::F_CTRL_MAC_ADDR != 0 && self.sent_mac != Some(station) {
            let ok = self.ctrl(vn::CTRL_MAC, vn::CTRL_MAC_ADDR_SET, &station);
            self.sent_mac = if ok { Some(station) } else { None };
        }
        if self.ctrlq.is_some() && self.sent_table.as_ref() != Some(&table) {
            // The unicast table holds the station address when it is not
            // the factory one and the device cannot be told it directly.
            let uni: Vec<[u8; 6]> =
                if station != factory && self.features & vn::F_CTRL_MAC_ADDR == 0 { alloc::vec![station] } else { Vec::new() };
            let data = vn::mac_table(&uni, &table, self.modern);
            let ok = self.ctrl(vn::CTRL_MAC, vn::CTRL_MAC_TABLE_SET, &data);
            self.sent_table = if ok { Some(table) } else { None };
        }
    }

    /// Drop every DMA mapping (the device must be reset first).
    /// After a device reset: the device owns no buffer any more. The pool
    /// mappings stay (they end when the Net is dropped).
    fn end_all_mappings(&mut self) {
        self.rx_posted.iter_mut().for_each(|b| *b = false);
        self.held = None;
        self.u.shared.with(|s| s.rx_held = false);
        drop(self.ctrl_map.take());
    }

    /// Reset the device and set it up again in place (recovery): queues,
    /// features, receive buffers, receive mode. Reads stay pending.
    unsafe fn recover(&mut self) -> Result<(), String> {
        // The transmit side is held for the whole recovery, so no caller
        // posts into the queue while it is replaced.
        let u = self.u;
        u.tx.with(|tx| -> Result<(), String> {
            let e = |x: amigaos4::AmigaError| alloc::format!("{}", x);
            self.t.reset().map_err(e)?;
            self.end_all_mappings();
            let endian = self.t.ring_endian();
            let rx_size = self.rxq.layout().size;
            self.rxq = Virtqueue::new(rx_size, endian).map_err(e)?;
            if let Some(tx) = tx.as_mut() {
                let tx_size = tx.txq.layout().size;
                tx.txq = Virtqueue::new(tx_size, endian).map_err(e)?;
                tx.busy.iter_mut().for_each(|b| *b = false);
                tx.free = (0..tx.n as u16).rev().collect();
                tx.unpublished = false;
                tx.unkicked = false;
            }
            let features = virtio::init_device(&mut self.t, WANTED).map_err(e)?;
            if features != self.features {
                return Err("the device offers other features after its reset".into());
            }
            self.t.setup_queue(vn::RX_QUEUE, &self.rxq).map_err(e)?;
            if let Some(tx) = tx.as_mut() {
                self.t.setup_queue(vn::TX_QUEUE, &tx.txq).map_err(e)?;
                tx.notifier = Some(self.t.notifier(vn::TX_QUEUE).ok_or("the transmit queue has no notify address")?);
                tx.txq.ring().set_interrupts(!cfg!(feature = "test-no-tx-irq"));
            }
            if let Some(q) = self.ctrlq.as_ref() {
                let size = q.layout().size;
                let mut nq = Virtqueue::new(size, endian).map_err(e)?;
                nq.ring().set_interrupts(false);
                self.t.setup_queue(vn::CTRL_QUEUE, &nq).map_err(e)?;
                self.ctrlq = Some(nq);
            }
            self.rxq.ring().set_interrupts(true);
            virtio::driver_ok(&mut self.t).map_err(e)?;
            for i in 0..self.rx_n {
                self.post_rx(i)?;
            }
            self.rxq.ring().publish();
            self.t.notify(vn::RX_QUEUE);
            self.sent_promisc = None;
            self.sent_allmulti = None;
            self.sent_table = None;
            self.sent_mac = None;
            self.apply_rx_mode();
            u.irq_drained.store(u.irq_claimed.load(Ordering::Relaxed), Ordering::Relaxed);
            u.irq_storm.store(0, Ordering::Release);
            Ok(())
        })
    }

    fn device_broken(&self) -> Option<&'static str> {
        if self.u.irq_storm.load(Ordering::Acquire) != 0 {
            return Some("the storm breaker reset the device");
        }
        // SAFETY: a status read.
        let s = unsafe { self.t.status() };
        if s & virtio::STATUS_FAILED != 0 {
            Some("the device reports FAILED")
        } else if s & virtio::STATUS_DEVICE_NEEDS_RESET != 0 {
            Some("the device reports DEVICE_NEEDS_RESET")
        } else if s & virtio::STATUS_DRIVER_OK == 0 {
            Some("DRIVER_OK is gone: the device was reset")
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Statistics, events, throughput samples (task and BeginIO, under the lock)
// ---------------------------------------------------------------------------

/// Reply every S2_ONEVENT waiting for any of `events`.
fn signal_events(u: &Unit, events: u32) {
    let hit: Vec<(S2Req, u32)> = u.shared.with(|s| {
        let mut hit = Vec::new();
        s.events.retain(|&r| {
            let want = r.wire_error();
            if want & events != 0 {
                hit.push((r, want & events));
                false
            } else {
                true
            }
        });
        hit
    });
    for (r, got) in hit {
        // SAFETY: a queued request, ours to reply.
        unsafe { r.reply(0, got) };
    }
}

/// Count a frame in the throughput samples (bytes sent or received).
fn sample_update(s: &mut Shared, now: TimeVal, sent: u32, received: u32) {
    for &r in s.samples.iter() {
        let st = r.stat_data() as *mut Sana2ThroughputStats;
        if st.is_null() {
            continue;
        }
        // SAFETY: the caller's statistics block, at least s2ts_Actual bytes.
        unsafe {
            let actual = core::ptr::addr_of!((*st).s2ts_Actual).read_unaligned() as usize;
            let fits = |off: usize, len: usize| actual >= off + len;
            if fits(32, 8) {
                let p = core::ptr::addr_of_mut!((*st).s2ts_BytesSent);
                let v = p.read_unaligned();
                let n = ((v.s2q_High as u64) << 32 | v.s2q_Low as u64) + sent as u64;
                p.write_unaligned(S2Quad::from(n));
            }
            if fits(40, 8) {
                let p = core::ptr::addr_of_mut!((*st).s2ts_BytesReceived);
                let v = p.read_unaligned();
                let n = ((v.s2q_High as u64) << 32 | v.s2q_Low as u64) + received as u64;
                p.write_unaligned(S2Quad::from(n));
            }
            if fits(48, 8) {
                let p = core::ptr::addr_of_mut!((*st).s2ts_Updates);
                let v = p.read_unaligned();
                let n = ((v.s2q_High as u64) << 32 | v.s2q_Low as u64) + 1;
                p.write_unaligned(S2Quad::from(n));
            }
            if fits(24, 8) {
                core::ptr::addr_of_mut!((*st).s2ts_EndTime).write_unaligned(now);
            }
            if fits(16, 4) {
                let task = core::ptr::addr_of!((*st).s2ts_NotifyTask).read_unaligned();
                let mask = core::ptr::addr_of!((*st).s2ts_NotifyMask).read_unaligned();
                if !task.is_null() && mask != 0 {
                    sys::exec_signal(task, mask);
                }
            }
        }
    }
}

/// Update every opener's statistics for `ptype` (each opener that sent
/// S2_TRACKTYPE for it has its own).
fn type_stats(s: &mut Shared, ptype: u32, mut f: impl FnMut(&mut Sana2PacketTypeStats)) {
    for (t, st, _) in s.tracked.iter_mut() {
        if *t == ptype {
            f(st);
        }
    }
}

// ---------------------------------------------------------------------------
// Receive and transmit (the unit task)
// ---------------------------------------------------------------------------

impl Net {
    /// Take every received frame, deliver it, and give its buffer back.
    unsafe fn rx(&mut self, now: TimeVal) -> u32 {
        let mut n = 0;
        let mut repost = false;
        loop {
            // A held frame first; then the next used buffer.
            let (id, len, since) = match self.held {
                Some(h) => h,
                None => match self.rxq.ring().take_used() {
                    Some((id, len)) => (id, len, 0),
                    None => break,
                },
            };
            let i = id as usize;
            if i >= self.rx_n || !self.rx_posted[i] {
                self.held = None;
                continue;
            }
            let len = len as usize;
            let p = self.rx_ptr(i);
            let hdr = self.hdr;
            if len >= hdr + vn::ETH_HLEN && len <= BUF_SIZE {
                let hdr_bytes = core::slice::from_raw_parts(p, hdr);
                if vn::rx_num_buffers(hdr_bytes, self.modern, self.features) > 1 {
                    // A frame spread over several buffers cannot happen with
                    // 2 KB buffers and a 1500-byte MTU; drop it if it does.
                    self.u.shared.with(|s| s.stats.bad_data += 1);
                } else {
                    let frame = core::slice::from_raw_parts(p.add(hdr), len - hdr);
                    let may_hold = self.held.is_none() || arch::timebase().wrapping_sub(since) < self.hold_limit;
                    if !self.deliver(frame, now, may_hold) {
                        // No read for it yet: keep it (and everything behind
                        // it) in the ring until the stack queues one.
                        if self.held.is_none() {
                            self.held = Some((id, len as u32, arch::timebase()));
                        }
                        break;
                    }
                    if pure_ack(frame) {
                        self.u.count(perf::RX_ACKS);
                    }
                }
            } else {
                self.u.shared.with(|s| s.stats.bad_data += 1);
            }
            self.held = None;
            self.rx_posted[i] = false;
            if self.post_rx(i).is_ok() {
                repost = true;
            }
            n += 1;
        }
        if repost {
            self.rxq.ring().publish();
            if self.rxq.ring().device_wants_notify() {
                self.t.notify(vn::RX_QUEUE);
                self.u.count(perf::RX_NOTIFY);
            }
        }
        n
    }

    /// Hand one frame to the openers' reads. False: the frame is of a type
    /// an opener reads, but none of its reads is queued right now and
    /// `may_hold` allows waiting: nothing was done, try again later.
    unsafe fn deliver(&mut self, frame: &[u8], now: TimeVal, may_hold: bool) -> bool {
        let dst = [frame[0], frame[1], frame[2], frame[3], frame[4], frame[5]];
        let src = [frame[6], frame[7], frame[8], frame[9], frame[10], frame[11]];
        let ptype = u16::from_be_bytes([frame[12], frame[13]]) as u32;
        let bcast = dst == vn::BROADCAST;
        let mcast = !bcast && vn::is_multicast(&dst);
        let len = frame.len() as u32;
        // Choose the reads under the lock; copy and reply outside it.
        let taken: Option<Vec<(S2Req, *mut Opener)>> = self.u.shared.with(|s| {
            // A frame that waits is counted when it is finally taken.
            let wants = s.read_types.iter().any(|&(t, _)| t == ptype);
            let readable = s.reads.iter().any(|r| r.packet_type() == ptype) || !s.orphans.is_empty();
            let filtered_out = s.promisc_openers == 0
                && ((mcast && !s.mcast_wanted(&dst)) || (!bcast && !mcast && dst != s.station));
            if s.online && wants && !readable && !filtered_out {
                if may_hold {
                    s.rx_held = true;
                    return None;
                }
                // It waited HOLD_MS and no read came: whoever reads this
                // type has stopped (or died without closing). Forget them
                // until they queue a read again, so the next frames of the
                // type do not each hold the ring for HOLD_MS.
                s.read_types.retain(|&(t, _)| t != ptype);
            }
            s.rx_held = false;
            if !s.online {
                return Some(Vec::new());
            }
            let promisc = s.promisc_openers > 0;
            if !promisc {
                if mcast && !s.mcast_wanted(&dst) {
                    s.stats.mcast_rejects += 1;
                    return Some(Vec::new());
                }
                if !bcast && !mcast && dst != s.station {
                    s.stats.rx_filtered += 1;
                    return Some(Vec::new());
                }
            }
            // Counted once past the address filter, as a card that filters
            // in hardware would (the filter counters keep the others).
            s.stats.rx_packets += 1;
            s.stats.rx_bytes += len as u64;
            sample_update(s, now, 0, len);
            type_stats(s, ptype, |st| {
                st.PacketsReceived += 1;
                st.BytesReceived += len;
            });
            let mut taken: Vec<(S2Req, *mut Opener)> = Vec::new();
            let mut i = 0;
            while i < s.reads.len() {
                let r = s.reads[i];
                let op = r.buffer_management() as *mut Opener;
                if r.packet_type() == ptype && !taken.iter().any(|&(_, o)| o == op) {
                    taken.push((r, op));
                    s.reads.remove(i);
                } else {
                    i += 1;
                }
            }
            if taken.is_empty() {
                let mut i = 0;
                while i < s.orphans.len() {
                    let r = s.orphans[i];
                    let op = r.buffer_management() as *mut Opener;
                    if !taken.iter().any(|&(_, o)| o == op) {
                        taken.push((r, op));
                        s.orphans.remove(i);
                    } else {
                        i += 1;
                    }
                }
                s.stats.unknown_types += 1;
                if taken.is_empty() {
                    s.stats.rx_no_read += 1;
                    type_stats(s, ptype, |st| st.PacketsDropped += 1);
                }
            }
            Some(taken)
        });
        let Some(taken) = taken else { return false };
        for (r, op) in taken {
            let raw = r.io().flags() & SANA2IOF_RAW != 0;
            let (data, dlen) = if raw { (frame.as_ptr(), len) } else { (frame.as_ptr().add(vn::ETH_HLEN), len - vn::ETH_HLEN as u32) };
            r.set_src_mac(&src);
            r.set_dst_mac(&dst);
            r.set_packet_type(ptype);
            r.set_data_length(dlen);
            let io = r.io();
            let mut flags = io.flags() & !(SANA2IOF_BCAST | SANA2IOF_MCAST);
            if bcast {
                flags |= SANA2IOF_BCAST;
            } else if mcast {
                flags |= SANA2IOF_MCAST;
            }
            io.set_flags(flags);
            let bm = &(*op).bm;
            // S2_PacketFilter is for CMD_READ; an orphan read gets everything.
            if io.command() == CMD_READ && !bm.filter(r, data) {
                // The stack does not want this one: the read stays pending.
                self.u.shared.with(|s| {
                    if io.command() == CMD_READ { s.reads.insert(0, r) } else { s.orphans.insert(0, r) }
                });
                continue;
            }
            if bm.copy_to_buff(r, r.data(), data, dlen) {
                r.reply(0, 0);
            } else {
                r.reply(S2ERR_NO_RESOURCES, S2WERR_BUFF_ERROR);
                signal_events(self.u, S2EVENT_ERROR | S2EVENT_RX | S2EVENT_BUFF);
            }
        }
        true
    }

    /// The link status (F_STATUS) and announce request (F_GUEST_ANNOUNCE).
    unsafe fn config_changed(&mut self) {
        if self.features & vn::F_STATUS == 0 {
            return;
        }
        let st = self.t.config_u16(vn::CFG_STATUS);
        let up = st & vn::S_LINK_UP != 0;
        let was = self.u.shared.with(|s| core::mem::replace(&mut s.link_up, up));
        if was != up {
            serial_println!("{} unit {}: link {}", NAME, self.u.index(), if up { "up" } else { "down" });
            signal_events(self.u, if up { S2EVENT_CONNECT } else { S2EVENT_DISCONNECT | S2EVENT_ERROR });
        }
        if st & vn::S_ANNOUNCE != 0 && self.features & vn::F_GUEST_ANNOUNCE != 0 {
            // The stack is told through CONFIGCHANGED; the device is told
            // the request was seen.
            let ok = self.ctrl(vn::CTRL_ANNOUNCE, vn::CTRL_ANNOUNCE_ACK, &[]);
            serial_println!("{} unit {}: announce requested (acknowledged: {})", NAME, self.u.index(), ok);
            signal_events(self.u, S2EVENT_CONFIGCHANGED);
        }
    }
}

// ---------------------------------------------------------------------------
// Transmit, in the caller's task (BeginIO) or the unit task, under the unit's
// transmit lock
// ---------------------------------------------------------------------------
//
// A write is copied into a free slot and posted in BeginIO itself, in the
// stack's task: no hand-off to the unit task, so no two task switches per
// frame. Only when every slot is in flight does it wait (marked pending) in
// the ring's own list; the unit task sends those as slots come back, on the
// transmit interrupt. Lock order: the transmit lock, then the shared state;
// never the other way round.

struct TxRing {
    txq: Virtqueue,
    // The mapping before the buffer: it drops (EndDMA) first.
    _pool_map: Option<DmaMapping<'static>>,
    bufs: DmaBuffer,
    phys: u64,
    n: usize,
    free: Vec<u16>,
    /// The device owns slot i.
    busy: Vec<bool>,
    /// Frames posted and not yet published.
    unpublished: bool,
    /// Frames published and the device not yet told.
    unkicked: bool,
    /// Writes waiting for a slot (marked pending: replied when sent).
    pending: VecDeque<S2Req>,
    hdr: usize,
    notifier: Option<Notifier>,
}

impl TxRing {
    /// Take back every slot the device is done with.
    unsafe fn reclaim(&mut self) -> u32 {
        let mut n = 0;
        while let Some((id, _)) = self.txq.ring().take_used() {
            let i = id as usize;
            if i < self.n && self.busy[i] {
                self.busy[i] = false;
                self.free.push(i as u16);
                n += 1;
            }
        }
        n
    }

    /// Send one CMD_WRITE / S2_BROADCAST / S2_MULTICAST, ending it with
    /// `finish` (quick-aware; a pending request is not quick). Returns false
    /// when every slot is in flight (the request is not ended).
    unsafe fn send(&mut self, u: &Unit, r: S2Req, now: TimeVal) -> bool {
        let io = r.io();
        let cmd = io.command();
        let (online, configured, station, mtu) = u.shared.with(|s| (s.online, s.configured, s.station, s.mtu));
        if !configured {
            r.finish(S2ERR_BAD_STATE, S2WERR_NOT_CONFIGURED);
            return true;
        }
        if !online {
            r.finish(S2ERR_OUTOFSERVICE, S2WERR_UNIT_OFFLINE);
            return true;
        }
        if u.out_of_service.load(Ordering::Acquire) != 0 {
            // Resting between recovery tries: the ring is not being served.
            r.finish(S2ERR_OUTOFSERVICE, S2WERR_RCVREL_HDW_ERR);
            return true;
        }
        let raw = io.flags() & SANA2IOF_RAW != 0;
        let dlen = r.data_length() as usize;
        let frame_len = if raw { dlen } else { vn::ETH_HLEN + dlen };
        if (raw && (dlen < vn::ETH_HLEN || dlen > vn::ETH_HLEN + mtu as usize)) || (!raw && dlen > mtu as usize) {
            r.finish(S2ERR_MTU_EXCEEDED, S2WERR_GENERIC_ERROR);
            return true;
        }
        let dst = match cmd {
            S2_BROADCAST => vn::BROADCAST,
            _ => r.dst_mac(),
        };
        if cmd == S2_MULTICAST && (!vn::is_multicast(&dst) || dst == vn::BROADCAST) {
            r.finish(S2ERR_BAD_ADDRESS, S2WERR_BAD_MULTICAST);
            return true;
        }
        let Some(slot) = self.free.pop() else { return false };
        let i = slot as usize;
        let p = self.bufs.as_mut_ptr().add(i * BUF_SIZE);
        core::ptr::write_bytes(p, 0, self.hdr);
        let f = p.add(self.hdr);
        let payload = if raw {
            f
        } else {
            core::ptr::copy_nonoverlapping(dst.as_ptr(), f, 6);
            core::ptr::copy_nonoverlapping(station.as_ptr(), f.add(6), 6);
            let t = (r.packet_type() as u16).to_be_bytes();
            *f.add(12) = t[0];
            *f.add(13) = t[1];
            f.add(vn::ETH_HLEN)
        };
        let op = r.buffer_management() as *mut Opener;
        if !(*op).bm.copy_from_buff(r, payload, r.data(), dlen as u32) {
            self.free.push(slot);
            r.finish(S2ERR_NO_RESOURCES, S2WERR_BUFF_ERROR);
            signal_events(u, S2EVENT_ERROR | S2EVENT_TX | S2EVENT_BUFF);
            return true;
        }
        // Pad to the 60-byte Ethernet minimum.
        let wire = frame_len.max(60);
        if wire > frame_len {
            core::ptr::write_bytes(f.add(frame_len), 0, wire - frame_len);
        }
        let total = self.hdr + wire;
        let phys = self.phys + (i * BUF_SIZE) as u64;
        self.busy[i] = true;
        let ring = self.txq.ring();
        ring.set_desc(slot, phys, total as u32, 0, 0);
        ring.push_avail(slot);
        self.unpublished = true;
        let ptype = if raw { u16::from_be_bytes([*f.add(12), *f.add(13)]) as u32 } else { r.packet_type() };
        if pure_ack(core::slice::from_raw_parts(f, frame_len)) {
            u.count(perf::TX_ACKS);
        }
        u.shared.with(|s| {
            s.stats.tx_packets += 1;
            s.stats.tx_bytes += frame_len as u64;
            sample_update(s, now, frame_len as u32, 0);
            type_stats(s, ptype, |st| {
                st.PacketsSent += 1;
                st.BytesSent += frame_len as u32;
            });
        });
        r.finish(0, 0);
        true
    }

    /// Publish what was posted and kick the device, unless it is already
    /// sending (VRING_USED_F_NO_NOTIFY: a notify would be a wasted
    /// register write, a VM exit under QEMU).
    unsafe fn push_out(&mut self, u: &Unit, notify: bool) {
        if self.unpublished {
            self.unpublished = false;
            self.txq.ring().publish();
            self.unkicked = true;
            u.count(perf::TX_PUBLISH);
        }
        if notify && self.unkicked {
            self.unkicked = false;
            if self.txq.ring().device_wants_notify() {
                if let Some(nf) = self.notifier {
                    let t0 = arch::timebase();
                    nf.notify();
                    u.add_time(perf::TX_NOTIFY_T, t0);
                    u.count(perf::TX_NOTIFY);
                }
            }
        }
    }

    /// Send what waits for a slot, then push everything out.
    unsafe fn flush(&mut self, u: &Unit, now: TimeVal) {
        while let Some(&r) = self.pending.front() {
            if !self.send(u, r, now) {
                break;
            }
            self.pending.pop_front();
        }
        self.push_out(u, true);
    }
}

/// BeginIO's CMD_WRITE / S2_BROADCAST / S2_MULTICAST: send at once in the
/// caller's task, or leave it for the unit task when every slot is busy.
unsafe fn tx_from_caller(u: &Unit, r: S2Req) {
    let t0 = arch::timebase();
    let now = u.now();
    let state = u.tx.with(|tx| {
        let Some(tx) = tx.as_mut() else { return None };
        tx.reclaim();
        if tx.pending.is_empty() && tx.send(u, r, now) {
            tx.push_out(u, !cfg!(feature = "test-tx-hold"));
            return Some(true);
        }
        r.io().mark_pending();
        tx.pending.push_back(r);
        Some(false)
    });
    match state {
        None => r.finish(IOERR_UNITBUSY, 0),
        Some(true) => {}
        Some(false) => {
            u.shared.with(|s| s.stats.tx_waits += 1);
            u.worker.wake();
        }
    }
    u.add_time(perf::TX_CALL_T, t0);
}

/// The unit task's share: take back slots, send what waited.
unsafe fn tx_service(u: &Unit, now: TimeVal) {
    u.tx.with(|tx| {
        if let Some(tx) = tx.as_mut() {
            tx.reclaim();
            tx.flush(u, now);
        }
    });
}

// ---------------------------------------------------------------------------
// Commands that need the device (the unit task)
// ---------------------------------------------------------------------------

/// Abort every pending read and orphan read, and what waits to be sent.
unsafe fn abort_traffic(u: &Unit, net: &mut Net, error: i8, wire: u32) {
    let (reads, orphans) = u.shared.with(|s| (core::mem::take(&mut s.reads), core::mem::take(&mut s.orphans)));
    for r in reads.into_iter().chain(orphans) {
        r.reply(error, wire);
    }
    let _ = net;
    let pending = u.tx.with(|tx| tx.as_mut().map(|t| core::mem::take(&mut t.pending)).unwrap_or_default());
    for r in pending {
        r.reply(error, wire);
    }
}

unsafe fn serve(u: &'static Unit, net: &mut Net, r: S2Req, timer: &mut AmigaTimer) {
    let now = timer.get_sys_time();
    let now = TimeVal { Seconds: now.secs, Microseconds: now.micros };
    match r.io().command() {
        S2_CONFIGINTERFACE => {
            let addr = r.src_mac();
            let res = u.shared.with(|s| {
                if s.configured {
                    return Err((S2ERR_BAD_STATE, S2WERR_IS_CONFIGURED));
                }
                if vn::is_multicast(&addr) || addr == [0; 6] {
                    return Err((S2ERR_BAD_ADDRESS, S2WERR_SRC_ADDRESS));
                }
                s.station = addr;
                s.configured = true;
                s.online = true;
                s.last_start = now;
                Ok(())
            });
            match res {
                Ok(()) => {
                    net.apply_rx_mode();
                    serial_println!("{} unit {}: configured, station {}, online", NAME, u.index(), hex_mac(&addr));
                    r.set_src_mac(&addr);
                    r.reply(0, 0);
                    signal_events(u, S2EVENT_ONLINE | S2EVENT_CONFIGCHANGED);
                }
                Err((e, w)) => r.reply(e, w),
            }
        }
        S2_ONLINE => {
            let res = u.shared.with(|s| {
                if !s.configured {
                    return Err((S2ERR_BAD_STATE, S2WERR_NOT_CONFIGURED));
                }
                let was = s.online;
                s.online = true;
                if !was {
                    let keep = (s.stats.recoveries, s.stats.reconfigurations + 1);
                    s.stats = Stats::default();
                    s.stats.recoveries = keep.0;
                    s.stats.reconfigurations = keep.1;
                    s.last_start = now;
                }
                Ok(was)
            });
            match res {
                Ok(was) => {
                    r.reply(0, 0);
                    if !was {
                        serial_println!("{} unit {}: online", NAME, u.index());
                        signal_events(u, S2EVENT_ONLINE);
                    }
                }
                Err((e, w)) => r.reply(e, w),
            }
        }
        S2_OFFLINE => {
            let was = u.shared.with(|s| core::mem::replace(&mut s.online, false));
            if was {
                abort_traffic(u, net, S2ERR_OUTOFSERVICE, S2WERR_UNIT_OFFLINE);
                serial_println!("{} unit {}: offline", NAME, u.index());
            }
            r.reply(0, 0);
            if was {
                signal_events(u, S2EVENT_OFFLINE);
            }
        }
        S2_ADDMULTICASTADDRESS | S2_DELMULTICASTADDRESS | S2_ADDMULTICASTADDRESSES | S2_DELMULTICASTADDRESSES => {
            let cmd = r.io().command();
            let lo = r.src_mac();
            let hi = if cmd == S2_ADDMULTICASTADDRESSES || cmd == S2_DELMULTICASTADDRESSES { r.dst_mac() } else { lo };
            let (l, h) = (mac_u64(&lo), mac_u64(&hi));
            if !vn::is_multicast(&lo) || !vn::is_multicast(&hi) || h < l {
                r.reply(S2ERR_BAD_ADDRESS, S2WERR_BAD_MULTICAST);
                return;
            }
            let add = cmd == S2_ADDMULTICASTADDRESS || cmd == S2_ADDMULTICASTADDRESSES;
            let res = u.shared.with(|s| {
                if h - l + 1 > MCAST_RANGE_MAX {
                    if add {
                        match s.mcast_wide.iter_mut().find(|e| e.0 == l && e.1 == h) {
                            Some(e) => e.2 += 1,
                            None => s.mcast_wide.push((l, h, 1)),
                        }
                    } else if let Some(pos) = s.mcast_wide.iter().position(|e| e.0 == l && e.1 == h) {
                        s.mcast_wide[pos].2 -= 1;
                        if s.mcast_wide[pos].2 == 0 {
                            s.mcast_wide.remove(pos);
                        }
                    }
                    return Ok(());
                }
                for v in l..=h {
                    let mac = u64_mac(v);
                    if add {
                        match s.mcast.iter_mut().find(|e| e.0 == mac) {
                            Some(e) => e.1 += 1,
                            None => {
                                if s.mcast.len() >= 4 * MCAST_TABLE_MAX {
                                    return Err((S2ERR_NO_RESOURCES, S2WERR_MULTICAST_FULL));
                                }
                                s.mcast.push((mac, 1));
                            }
                        }
                    } else if let Some(pos) = s.mcast.iter().position(|e| e.0 == mac) {
                        s.mcast[pos].1 -= 1;
                        if s.mcast[pos].1 == 0 {
                            s.mcast.remove(pos);
                        }
                    }
                }
                Ok(())
            });
            match res {
                Ok(()) => {
                    net.apply_rx_mode();
                    r.reply(0, 0);
                }
                Err((e, w)) => r.reply(e, w),
            }
        }
        S2_SAMPLE_THROUGHPUT => {
            let st = r.stat_data() as *mut Sana2ThroughputStats;
            let len = core::ptr::addr_of!((*st).s2ts_Length).read_unaligned() as usize;
            let actual = len.min(core::mem::size_of::<Sana2ThroughputStats>());
            let mut fresh: Sana2ThroughputStats = core::mem::zeroed();
            fresh.s2ts_Length = len as u32;
            fresh.s2ts_Actual = actual as u32;
            // Keep the caller's notify task and mask.
            let keep = core::cmp::min(actual, 16);
            core::ptr::copy_nonoverlapping((st as *const u8).add(8), (&mut fresh as *mut _ as *mut u8).add(8), keep.saturating_sub(8));
            fresh.s2ts_StartTime = now;
            fresh.s2ts_EndTime = now;
            core::ptr::copy_nonoverlapping(&fresh as *const _ as *const u8, st as *mut u8, actual);
            u.shared.with(|s| s.samples.push(r));
        }
        CMD_FLUSH => {
            // Everything queued: reads, orphans, writes, and (unlike
            // S2_OFFLINE, which keeps them for the ONLINE event) events and
            // throughput samples.
            abort_traffic(u, net, IOERR_ABORTED, 0);
            let (ev, sa) = u.shared.with(|s| (core::mem::take(&mut s.events), core::mem::take(&mut s.samples)));
            for x in ev.into_iter().chain(sa) {
                x.reply(IOERR_ABORTED, 0);
            }
            r.reply(0, 0);
        }
        VNET_CMD_TESTFAULT => {
            net.t.set_status(0);
            serial_println!("{} unit {}: test fault: device reset behind the driver's back", NAME, u.index());
            r.reply(0, 0);
        }
        _ => r.reply(IOERR_NOCMD, 0),
    }
}

/// The timebase frequency (exec.doc GetCPUInfo, GCIT_TimeBaseSpeed).
fn timebase_hz() -> u64 {
    let mut hz: u64 = 0;
    // SAFETY: GCIT_TimeBaseSpeed writes one uint64.
    unsafe {
        let tags = [
            TagItem { ti_Tag: sys::GCIT_TIMEBASE_SPEED, ti_Data: &mut hz as *mut u64 as usize as u32 },
            TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
        ];
        sys::exec_get_cpuinfo(tags.as_ptr());
    }
    hz.max(1)
}

// ---------------------------------------------------------------------------
// The interrupt server and reset callback
// ---------------------------------------------------------------------------

unsafe fn irq_body(u: &Unit) -> u32 {
    let r = match u.regs() {
        Some(r) if !u.irq_task.load(Ordering::Acquire).is_null() => r,
        _ => {
            u.irq_passed_on.fetch_add(1, Ordering::Relaxed);
            return 0;
        }
    };
    if u.irq_storm.load(Ordering::Relaxed) != 0 || u.quiesced.load(Ordering::Relaxed) != 0 {
        u.irq_passed_on.fetch_add(1, Ordering::Relaxed);
        return 0;
    }
    u.irq_raised.fetch_add(1, Ordering::Relaxed);
    let t0 = arch::timebase();
    let isr = r.read_isr();
    u.add_time(perf::ISR_T, t0);
    if isr == 0 {
        u.irq_passed_on.fetch_add(1, Ordering::Relaxed);
        return 0;
    }
    let claimed = u.irq_claimed.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    if claimed.wrapping_sub(u.irq_drained.load(Ordering::Relaxed)) > STORM_LIMIT {
        r.write_status(0);
        u.irq_storm.store(1, Ordering::Release);
    }
    if isr & virtio::ISR_CONFIG != 0 {
        u.irq_config.store(1, Ordering::Release);
    }
    let task = u.irq_task.load(Ordering::Acquire);
    if !task.is_null() {
        sys::exec_signal(task, u.irq_mask.load(Ordering::Relaxed));
    }
    1
}

unsafe extern "C" fn net_irq(_ctx: *mut c_void, _sysbase: *mut c_void, data: *mut c_void) -> u32 {
    let r = irq_body(&*(data as *const Unit));
    arch::clear_reservation();
    r
}

static QUIESCED_ALL: AtomicU32 = AtomicU32::new(0);
struct ResetCell(UnsafeCell<Option<ResetCallback>>);
// SAFETY: changed only under OPEN_LOCK.
unsafe impl Sync for ResetCell {}
static RESET_CB: ResetCell = ResetCell(UnsafeCell::new(None));
static RUNNING_UNITS: AtomicU32 = AtomicU32::new(0);

/// On ColdReboot, reset every running card so none goes on writing into
/// memory the next boot hands out again.
unsafe extern "C" fn net_reset(_ctx: *mut c_void, _execbase: *mut c_void, _data: *mut c_void) -> u32 {
    if QUIESCED_ALL.swap(1, Ordering::AcqRel) != 0 {
        return 0;
    }
    for u in units() {
        u.quiesced.store(1, Ordering::Release);
        if let Some(r) = u.regs() {
            r.write_status(0);
            let _ = r.read_isr();
        }
    }
    0
}

// ---------------------------------------------------------------------------
// The unit task
// ---------------------------------------------------------------------------

extern "C" fn unit_task(n: u32) {
    // SAFETY: from here on this task is its unit's device's only user.
    unsafe {
        if n as usize >= MAX_UNITS {
            return;
        }
        let u = unit(n as usize);
        let net = match (*u.net.get()).as_mut() {
            Some(x) => x,
            None => return,
        };
        let irq_sig = sys::exec_alloc_signal(-1);
        let work_sig = sys::exec_alloc_signal(-1);
        // The heartbeat has its own request: `timer` serves get_sys_time.
        // So has the hold poll (a frame waiting in the ring for a read).
        let timers = AmigaTimer::open(TimerUnit::MicroHz)
            .and_then(|t| Ok((t, AmigaTimer::open(TimerUnit::MicroHz)?, AmigaTimer::open(TimerUnit::MicroHz)?)));
        let (mut timer, mut beat, mut holdt) = match timers {
            Ok(t) => t,
            Err(_) => {
                serial_println!("{} unit {}: no timer.device; the unit does not start", NAME, n);
                return;
            }
        };
        if irq_sig < 0 || work_sig < 0 {
            serial_println!("{} unit {}: no free signal; the unit does not start", NAME, n);
            return;
        }
        let me = sys::exec_find_task(null_mut());
        let irq_mask = 1u32 << irq_sig;
        let work_mask = 1u32 << work_sig;
        let chain = pci!(net.dev, MapInterrupt);
        u.irq_mask.store(irq_mask, Ordering::Relaxed);
        u.irq_task.store(me, Ordering::Release);
        let server = if chain != 0 {
            match IntServer::add(chain, 0, TASK_NAMES[n as usize], net_irq, u as *const Unit as *mut c_void) {
                Ok(s) => Some(s),
                Err(e) => {
                    serial_println!("{} unit {}: AddIntServer({}) failed: {}", NAME, n, chain, e);
                    None
                }
            }
        } else {
            None
        };
        // Without a server, the timer drives the unit (polling every 10 ms).
        let polled = server.is_none();
        let spin = cfg!(feature = "test-rx-poll");
        net.rxq.ring().set_interrupts(!spin);
        u.tx.with(|tx| {
            if let Some(tx) = tx.as_mut() {
                tx.txq.ring().set_interrupts(!cfg!(feature = "test-no-tx-irq") && !spin);
            }
        });
        if spin {
            // Just above idle.task (-128), so that ordinary tasks still run.
            sys::exec_set_task_pri(me as _, -127);
        }
        net.apply_rx_mode();
        serial_println!(
            "{} unit {}: unit task ready: {} chain {}, pri {}",
            NAME, n, if polled { "POLLED, no interrupt server on" } else { "interrupt server on" }, chain, TASK_PRI
        );
        u.worker.publish(me, work_mask);
        let mut recoveries_in_a_row = 0u32;
        // After MAX_RECOVERIES in a row: out of service, the next try at
        // retry_at (timebase), rest_s seconds after the last.
        let mut resting = false;
        u.out_of_service.store(0, Ordering::Release);
        let mut retry_at = 0u64;
        let mut rest_s = 1u64;
        // The device's status is read (a register access, a VM exit under
        // QEMU) at most this often, unless the storm breaker fired.
        let tb_hz = timebase_hz();
        TB_PER_US.store((tb_hz / 1_000_000).max(1) as u32, Ordering::Relaxed);
        let check_every = tb_hz / 10; // 100 ms
        let mut next_check = 0u64;

        loop {
            if polled {
                timer.start(TimerVal::from_millis(10));
                sys::exec_wait(work_mask | timer.signal_mask());
                if timer.is_done() {
                    timer.finish();
                } else {
                    timer.abort();
                }
            } else {
                // Writes are sent in the stack's task and do not wake this
                // one, and a device that was reset raises no interrupt: the
                // heartbeat makes sure its status is still read.
                if beat.is_done() {
                    beat.finish();
                    beat.start(TimerVal::from_millis(HEARTBEAT_MS));
                }
                // A held frame is looked at again soon even if no read
                // comes, so it is dropped after HOLD_MS at the latest.
                if net.held.is_some() && holdt.is_done() {
                    holdt.finish();
                    holdt.start(TimerVal::from_millis(HOLD_POLL_MS));
                }
                let all = work_mask | irq_mask | beat.signal_mask() | holdt.signal_mask();
                if spin {
                    // Measurement: never Wait; clear what was signalled and poll.
                    sys::exec_set_signal(0, all);
                } else {
                    sys::exec_wait(all);
                }
            }
            if u.worker.stop_requested() {
                break;
            }
            u.irq_drained.store(u.irq_claimed.load(Ordering::Relaxed), Ordering::Relaxed);
            let tb = arch::timebase();
            let due = tb >= next_check || u.irq_storm.load(Ordering::Acquire) != 0;
            if due {
                next_check = tb + check_every;
            }
            let broken = if due { net.device_broken() } else { None };
            if due && broken.is_none() && (recoveries_in_a_row != 0 || resting) {
                // Healthy at a status check after a recovery: it held.
                if resting || recoveries_in_a_row >= MAX_RECOVERIES {
                    serial_println!("{} unit {}: the device works again", NAME, n);
                }
                recoveries_in_a_row = 0;
                rest_s = 1;
                resting = false;
                u.out_of_service.store(0, Ordering::Release);
            }
            if let Some(why) = broken {
                if resting && tb < retry_at {
                    // Out of service until the next try: fail what needs the device.
                    while let Some(r) = u.cmdq.take_next() {
                        u.cmdq.done();
                        S2Req::from_raw(r.as_ptr()).unwrap().reply(S2ERR_OUTOFSERVICE, S2WERR_RCVREL_HDW_ERR);
                    }
                    continue;
                }
                recoveries_in_a_row += 1;
                if recoveries_in_a_row > MAX_RECOVERIES && !resting {
                    // A device that breaks again straight after every
                    // recovery: rest, and try again later, more slowly each
                    // time (1 s, 2 s, ... 32 s), rather than for good: a
                    // stack keeps the unit open, so a unit that gave up for
                    // good would stay dead until a reboot.
                    resting = true;
                    u.out_of_service.store(1, Ordering::Release);
                    retry_at = tb + rest_s * tb_hz;
                    serial_println!("{} unit {}: {}; {} recoveries in a row, out of service, next try in {} s", NAME, n, why, MAX_RECOVERIES, rest_s);
                    while let Some(r) = u.cmdq.take_next() {
                        u.cmdq.done();
                        S2Req::from_raw(r.as_ptr()).unwrap().reply(S2ERR_OUTOFSERVICE, S2WERR_RCVREL_HDW_ERR);
                    }
                    continue;
                }
                serial_println!("{} unit {}: recovering ({} in a row): {}", NAME, n, recoveries_in_a_row, why);
                match net.recover() {
                    Ok(()) => {
                        u.shared.with(|s| s.stats.recoveries += 1);
                        serial_println!("{} unit {}: recovered", NAME, n);
                        signal_events(u, S2EVENT_ERROR | S2EVENT_HARDWARE);
                        if resting {
                            // Back on probation: one more failure rests again, longer.
                            resting = false;
                            u.out_of_service.store(0, Ordering::Release);
                            recoveries_in_a_row = MAX_RECOVERIES;
                            rest_s = (rest_s * 2).min(32);
                        }
                    }
                    Err(e) => {
                        serial_println!("{} unit {}: recovery failed: {}", NAME, n, e);
                        if resting || recoveries_in_a_row >= MAX_RECOVERIES {
                            resting = true;
                            u.out_of_service.store(1, Ordering::Release);
                            retry_at = tb + rest_s * tb_hz;
                            rest_s = (rest_s * 2).min(32);
                        }
                        continue;
                    }
                }
                // Carry on: the request that woke the task is still queued.
            }
            let now = timer.get_sys_time();
            u.now_s.store(now.secs, Ordering::Relaxed);
            u.now_us.store(now.micros, Ordering::Relaxed);
            let now = TimeVal { Seconds: now.secs, Microseconds: now.micros };
            if u.irq_config.swap(0, Ordering::AcqRel) != 0 {
                net.config_changed();
            }
            u.count(perf::WAKES);
            if net.rx(now) > 0 {
                recoveries_in_a_row = 0;
                u.count(perf::WAKES_RX);
            }
            tx_service(u, now);
            while let Some(ior) = u.cmdq.take_next() {
                u.cmdq.done();
                if let Some(r) = S2Req::from_raw(ior.as_ptr()) {
                    serve(u, net, r, &mut timer);
                }
            }
            if u.rx_mode_dirty.swap(0, Ordering::AcqRel) != 0 {
                net.apply_rx_mode();
            }
            tx_service(u, now);
        }

        // STOP (the unit's last close). Reset the card BEFORE the server
        // goes: a level-triggered line left asserted with no server to
        // claim it would storm the chain.
        u.worker.withdraw();
        let st = u.shared.with(|s| (s.stats.tx_packets, s.stats.rx_packets, s.stats.tx_waits, s.stats.recoveries));
        serial_println!(
            "{} unit {}: totals: sent {}, received {}, writes that waited for a slot {}, recoveries {}",
            NAME, n, st.0, st.1, st.2, st.3
        );
        abort_traffic(u, net, IOERR_ABORTED, 0);
        while let Some(r) = u.cmdq.take_next() {
            u.cmdq.done();
            r.reply(IOERR_ABORTED, 0);
        }
        let _ = net.t.reset();
        let _ = net.t.read_isr();
        u.quiesced.store(1, Ordering::Release);
        drop(server);
        drop(beat); // aborts a pending heartbeat
        drop(holdt);
        drop(timer);
        u.irq_task.store(null_mut(), Ordering::Release);
        sys::exec_free_signal(irq_sig);
        sys::exec_free_signal(work_sig);
        u.running.store(0, Ordering::Release);
        let t = u.stop_ack_task.load(Ordering::Acquire);
        let m = u.stop_ack_mask.load(Ordering::Acquire);
        if !t.is_null() && m != 0 {
            sys::exec_signal(t, m);
        }
    }
}

// ---------------------------------------------------------------------------
// Unit lifetime: start at first open, stop at last close
// ---------------------------------------------------------------------------

const STOP_BOUND_MS: u32 = 3000;
const GONE_POLL_MS: u32 = 5;

unsafe fn start_unit(u: &'static Unit) -> Result<(), String> {
    let n = u.index();
    let dev = u.dev.load(Ordering::Acquire);
    if dev.is_null() {
        return Err("no card".into());
    }
    if !u.cmdq_init() {
        return Err("mutex allocation failed".into());
    }
    let net = match Net::setup(u, dev) {
        Ok(x) => x,
        Err(e) => {
            drop(u.tx.with(|t| t.take()));
            u.cmdq_destroy();
            return Err(e);
        }
    };
    let _ = net.t.read_isr();
    *u.regs.get() = Some(net.t.regs());
    u.regs_ready.store(1, Ordering::Release);
    u.quiesced.store(0, Ordering::Release);
    *u.net.get() = Some(net);
    let tags = [
        TagItem { ti_Tag: sys::AT_PARAM1, ti_Data: n as u32 },
        TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
    ];
    let task = sys::exec_create_task(TASK_NAMES[n].as_ptr() as _, TASK_PRI, unit_task as *const c_void, TASK_STACK, tags.as_ptr());
    if task.is_null() {
        u.regs_ready.store(0, Ordering::Release);
        if let Some(mut net) = core::ptr::replace(u.net.get(), None) {
            let _ = net.t.reset();
            net.end_all_mappings();
        }
        drop(u.tx.with(|t| t.take()));
        u.cmdq_destroy();
        return Err("CreateTask failed".into());
    }
    u.running.store(1, Ordering::Release);
    if RUNNING_UNITS.fetch_add(1, Ordering::AcqRel) == 0 {
        QUIESCED_ALL.store(0, Ordering::Release);
        match ResetCallback::add(127, DEVNAME_C, net_reset, null_mut()) {
            Ok(cb) => *RESET_CB.0.get() = Some(cb),
            Err(e) => serial_println!("{}: AddResetCallback failed: {}", NAME, e),
        }
    }
    Ok(())
}

impl Unit {
    unsafe fn cmdq_init(&self) -> bool {
        if self.cmdq.init().is_err() {
            return false;
        }
        if !self.shared.init() {
            self.cmdq.destroy();
            return false;
        }
        if !self.tx.init() {
            self.cmdq.destroy();
            self.shared.destroy();
            return false;
        }
        true
    }
    unsafe fn cmdq_destroy(&self) {
        self.cmdq.destroy();
        self.shared.destroy();
        self.tx.destroy();
    }
}

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

/// Stop unit `u`: ask its task to stop, wait for its answer and until exec
/// has removed it, then free the device's memory. The card stays locked.
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
    let mut waited = 0;
    while !sys::exec_find_task(TASK_NAMES[n].as_ptr() as _).is_null() {
        if waited >= STOP_BOUND_MS {
            u.stuck.store(1, Ordering::Release);
            return Err("the unit task answered but did not go".into());
        }
        let _ = timer.delay(TimerVal::from_millis(GONE_POLL_MS));
        waited += GONE_POLL_MS;
    }
    u.regs_ready.store(0, Ordering::Release);
    drop(u.tx.with(|t| t.take())); // the card is reset: EndDMA and FreeVec
    drop(core::ptr::replace(u.net.get(), None));
    // Openers are gone (last close); drop what is left of the shared state.
    let leftover = u.shared.with(|s| core::mem::replace(s, Shared::new()));
    drop(leftover);
    u.cmdq_destroy();
    let dev = u.dev.load(Ordering::Acquire);
    core::ptr::write(u as *const Unit as *mut Unit, Unit::NEW);
    u.dev.store(dev, Ordering::Release);
    if RUNNING_UNITS.fetch_sub(1, Ordering::AcqRel) == 1 {
        drop((*RESET_CB.0.get()).take());
    }
    serial_println!("{} unit {}: stopped: task gone, card reset, memory freed", NAME, n);
    Ok(())
}

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

// ---------------------------------------------------------------------------
// Device entry points
// ---------------------------------------------------------------------------

/// io_Message.mn_Length: the request's size.
fn req_len(ior: *mut IORequest) -> u16 {
    // SAFETY: every IORequest starts with a Message (mn_Length at 18).
    unsafe { core::ptr::read_unaligned((ior as *const u8).add(18) as *const u16) }
}
const S2REQ_SIZE: u16 = 88;

#[no_mangle]
pub extern "C" fn rust_net_init() -> i32 {
    // SAFETY: the init runs once per load, before any open.
    unsafe {
        let cards = find_cards();
        if cards.is_empty() {
            serial_println!("{}: no virtio-net PCI device (1af4:1000 or 1af4:1041); device not added", NAME);
            return 1;
        }
        let m = sys::exec_alloc_sys_object(sys::ASOT_MUTEX, [TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 }].as_ptr());
        if m.is_null() {
            for dev in cards {
                pci!(dev, Unlock);
                sys::wrappers::pci::pci_free_device(dev);
            }
            return 1;
        }
        OPEN_LOCK.store(m as *mut c_void, Ordering::Release);
        let count = cards.len();
        for (i, dev) in cards.into_iter().enumerate() {
            unit(i).dev.store(dev, Ordering::Release);
        }
        NUNITS.store(count as u32, Ordering::Release);
        serial_println!("{}: {} card(s), units 0 to {}; each is set up at its first open", NAME, count, count - 1);
    }
    0
}

#[no_mangle]
pub extern "C" fn rust_net_expunge() -> i32 {
    // SAFETY: exec calls Expunge with no other user of the device.
    unsafe {
        for u in units() {
            if u.opens.load(Ordering::Acquire) != 0 || u.running.load(Ordering::Acquire) != 0 || u.stuck.load(Ordering::Acquire) != 0 {
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
        serial_println!("{}: expunged", NAME);
    }
    1
}

#[no_mangle]
pub extern "C" fn rust_dev_open(ior: *mut IORequest, unit_number: u32, flags: u32) -> i8 {
    if ior.is_null() || unit_number >= NUNITS.load(Ordering::Acquire) {
        return IOERR_OPENFAIL;
    }
    let u = unit(unit_number as usize);
    // SAFETY: the caller's request; start/stop under OPEN_LOCK.
    unsafe {
        with_open_lock(|| {
            let full = req_len(ior) >= S2REQ_SIZE;
            if u.opens.load(Ordering::Acquire) == 0 && u.running.load(Ordering::Acquire) == 0 {
                if u.stuck.load(Ordering::Acquire) != 0 {
                    return IOERR_OPENFAIL;
                }
                if let Err(e) = start_unit(u) {
                    serial_println!("{} unit {}: open failed: {}", NAME, unit_number, e);
                    return IOERR_OPENFAIL;
                }
                if !wait_ready(u) {
                    serial_println!("{} unit {}: open failed: the unit task did not get ready", NAME, unit_number);
                    let _ = stop_unit(u);
                    return IOERR_OPENFAIL;
                }
            }
            // A SANA-II opener: its buffer management from the tags in
            // ios2_BufferManagement, replaced by its cookie.
            if full {
                let r = S2Req::from_raw(ior).unwrap();
                let bm = BufferManagement::from_tags(r.buffer_management() as *const TagItem);
                let refused = u.shared.with(|s| {
                    if s.exclusive || (flags & SANA2OPF_MINE != 0 && !s.openers.is_empty()) {
                        return true;
                    }
                    false
                });
                if refused {
                    if u.opens.load(Ordering::Acquire) == 0 {
                        let _ = stop_unit(u);
                    }
                    return IOERR_UNITBUSY;
                }
                let op = Box::into_raw(Box::new(Opener { bm, flags }));
                u.shared.with(|s| {
                    s.openers.push(op);
                    if flags & SANA2OPF_MINE != 0 {
                        s.exclusive = true;
                    }
                    if flags & SANA2OPF_PROM != 0 {
                        s.promisc_openers += 1;
                    }
                });
                if flags & SANA2OPF_PROM != 0 {
                    u.rx_mode_dirty.store(1, Ordering::Release);
                    u.worker.wake();
                }
                r.set_buffer_management(op as *mut c_void);
            }
            if let Some(r) = IoReq::from_raw(ior) {
                r.set_unit(u as *const Unit as *mut c_void);
            }
            u.opens.fetch_add(1, Ordering::AcqRel);
            0
        })
    }
}

fn unit_of(ior: *mut IORequest) -> Option<&'static Unit> {
    // SAFETY: an opened request's io_Unit.
    let p = unsafe { IoReq::from_raw(ior) }?.unit() as *const Unit;
    units().find(|u| core::ptr::eq(*u, p))
}

#[no_mangle]
pub extern "C" fn rust_dev_close(ior: *mut IORequest) {
    let Some(u) = unit_of(ior) else { return };
    // SAFETY: the caller's request, opened on this device.
    unsafe {
        with_open_lock(|| {
            if req_len(ior) >= S2REQ_SIZE {
                let r = S2Req::from_raw(ior).unwrap();
                let op = r.buffer_management() as *mut Opener;
                let (found, leftover) = u.shared.with(|s| {
                    let Some(pos) = s.openers.iter().position(|&o| o == op) else { return (false, Vec::new()) };
                    s.openers.remove(pos);
                    s.read_types.retain(|&(_, o)| o != op);
                    s.tracked.retain(|&(_, _, o)| o != op);
                    if (*op).flags & SANA2OPF_MINE != 0 {
                        s.exclusive = false;
                    }
                    if (*op).flags & SANA2OPF_PROM != 0 {
                        s.promisc_openers = s.promisc_openers.saturating_sub(1);
                    }
                    // Requests the stack left behind (it should not).
                    let mine = |r: &S2Req| r.buffer_management() as *mut Opener == op;
                    let mut left = Vec::new();
                    for list in [&mut s.reads, &mut s.orphans, &mut s.events, &mut s.samples] {
                        let mut i = 0;
                        while i < list.len() {
                            if mine(&list[i]) {
                                left.push(list.remove(i));
                            } else {
                                i += 1;
                            }
                        }
                    }
                    (true, left)
                });
                for r in leftover {
                    r.reply(IOERR_ABORTED, 0);
                }
                // Writes still waiting for a transmit slot would be copied
                // through this opener's buffer management after it is freed.
                if found {
                    let mine: Vec<S2Req> = u.tx.with(|tx| {
                        let Some(t) = tx.as_mut() else { return Vec::new() };
                        let mut out = Vec::new();
                        let mut i = 0;
                        while i < t.pending.len() {
                            if t.pending[i].buffer_management() as *mut Opener == op {
                                out.push(t.pending.remove(i).unwrap());
                            } else {
                                i += 1;
                            }
                        }
                        out
                    });
                    for r in mine {
                        r.reply(IOERR_ABORTED, 0);
                    }
                }
                if found {
                    drop(Box::from_raw(op));
                    u.rx_mode_dirty.store(1, Ordering::Release);
                    u.worker.wake();
                }
            }
            if u.opens.fetch_sub(1, Ordering::AcqRel) == 1 {
                if let Err(e) = stop_unit(u) {
                    serial_println!("{} unit {}: stop FAILED: {}", NAME, u.index(), e);
                }
            }
        });
    }
}

/// NSCMD_DEVICEQUERY's command list.
static SUPPORTED_COMMANDS: [u16; 27] = [
    CMD_READ, CMD_WRITE, CMD_FLUSH,
    S2_DEVICEQUERY, S2_GETSTATIONADDRESS, S2_CONFIGINTERFACE,
    S2_ADDMULTICASTADDRESS, S2_DELMULTICASTADDRESS, S2_MULTICAST, S2_BROADCAST,
    S2_TRACKTYPE, S2_UNTRACKTYPE, S2_GETTYPESTATS, S2_GETSPECIALSTATS, S2_GETGLOBALSTATS,
    S2_ONEVENT, S2_READORPHAN, S2_ONLINE, S2_OFFLINE,
    S2_ADDMULTICASTADDRESSES, S2_DELMULTICASTADDRESSES, S2_GETEXTENDEDGLOBALSTATS,
    S2_SAMPLE_THROUGHPUT, S2_SANA2HOOK, NSCMD_DEVICEQUERY, 0, 0,
];

/// S2_GETSPECIALSTATS' record names.
/// The order is the record types (index + 1): add at the end only.
static SPECIAL_NAMES: [&[u8]; 17] = [
    amigaos4::amstr!("Frames dropped: no read pending"),
    amigaos4::amstr!("Frames for other stations dropped"),
    amigaos4::amstr!("Multicast frames for groups not added"),
    amigaos4::amstr!("Writes that waited for a transmit slot"),
    amigaos4::amstr!("Device recoveries"),
    amigaos4::amstr!("Bad frames"),
    amigaos4::amstr!("Interrupts claimed"),
    amigaos4::amstr!("Unit task wakes"),
    amigaos4::amstr!("Unit task wakes that received frames"),
    amigaos4::amstr!("Transmit ring publishes"),
    amigaos4::amstr!("Transmit notifications"),
    amigaos4::amstr!("Receive ring notifications"),
    amigaos4::amstr!("Pure TCP ACKs sent"),
    amigaos4::amstr!("Pure TCP ACKs received"),
    amigaos4::amstr!("Transmit notification time (0.1 us)"),
    amigaos4::amstr!("Interrupt ISR read time (0.1 us)"),
    amigaos4::amstr!("Write path time in the caller (0.1 us)"),
];

const ALL_EVENTS: u32 = (1 << 11) - 1;

#[no_mangle]
pub extern "C" fn rust_dev_begin_io(ior: *mut IORequest) {
    // SAFETY: an opened request.
    let Some(io) = (unsafe { IoReq::from_raw(ior) }) else { return };
    let Some(u) = unit_of(ior) else {
        unsafe { io.finish(IOERR_OPENFAIL, 0) };
        return;
    };
    let cmd = io.command();
    unsafe {
        if cmd == NSCMD_DEVICEQUERY {
            let (e, n) = answer_device_query(io, NSDEVTYPE_SANA2, 0, &SUPPORTED_COMMANDS[..26]);
            io.finish(e, n);
            return;
        }
        if req_len(ior) < S2REQ_SIZE {
            io.finish(IOERR_BADLENGTH, 0);
            return;
        }
        let r = S2Req::from_raw(ior).unwrap();
        let bm = r.buffer_management();
        if !u.shared.with(|s| s.opener_ok(bm)) {
            r.finish(S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
            return;
        }
        let op = bm as *mut Opener;
        match cmd {
            CMD_READ | S2_READORPHAN => {
                if !(*op).bm.complete() {
                    r.finish(S2ERR_BAD_STATE, S2WERR_FUNCTIONS_MISSING);
                    return;
                }
                io.mark_pending();
                let wake = u.shared.with(|s| {
                    if cmd == CMD_READ {
                        let t = r.packet_type();
                        if !s.read_types.iter().any(|&(x, o)| x == t && o == op) {
                            s.read_types.push((t, op));
                        }
                        s.reads.push(r);
                    } else {
                        s.orphans.push(r);
                    }
                    s.rx_held
                });
                if wake {
                    u.worker.wake();
                }
            }
            CMD_WRITE | S2_BROADCAST | S2_MULTICAST => {
                if !(*op).bm.complete() {
                    r.finish(S2ERR_BAD_STATE, S2WERR_FUNCTIONS_MISSING);
                    return;
                }
                tx_from_caller(u, r);
            }
            S2_CONFIGINTERFACE | S2_ONLINE | S2_OFFLINE | S2_ADDMULTICASTADDRESS | S2_DELMULTICASTADDRESS
            | S2_ADDMULTICASTADDRESSES | S2_DELMULTICASTADDRESSES | CMD_FLUSH | VNET_CMD_TESTFAULT => to_task(u, r),
            S2_SAMPLE_THROUGHPUT => {
                let st = r.stat_data() as *const u32;
                if st.is_null() {
                    r.finish(S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
                } else if core::ptr::read_unaligned(st) < 8 {
                    r.finish(IOERR_BADLENGTH, S2WERR_BAD_STATDATA);
                } else {
                    to_task(u, r);
                }
            }
            S2_SANA2HOOK => {
                let res = u.shared.with(|s| {
                    let _ = s;
                    (*op).bm.install_hook(r.data() as *mut Sana2Hook, r.data_length())
                });
                match res {
                    Ok(()) => {
                        serial_println!("{} unit {}: stack copy hook installed", NAME, u.index());
                        r.finish(0, 0)
                    }
                    Err((e, w)) => r.finish(e, w),
                }
            }
            S2_DEVICEQUERY => {
                let q = r.stat_data() as *mut u8;
                if q.is_null() {
                    r.finish(S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
                    return;
                }
                let avail = core::ptr::read_unaligned(q as *const u32) as usize;
                let (mtu, bps) = u.shared.with(|s| (s.mtu, s.bps));
                let mut full: Sana2DeviceQuery = core::mem::zeroed();
                full.SizeAvailable = avail as u32;
                let supply = avail.min(core::mem::size_of::<Sana2DeviceQuery>());
                full.SizeSupplied = supply as u32;
                full.AddrFieldSize = 48;
                full.MTU = mtu;
                full.BPS = bps;
                full.HardwareType = S2_WIRETYPE_ETHERNET;
                full.RawMTU = mtu + vn::ETH_HLEN as u32;
                // SizeAvailable is the caller's; write from SizeSupplied on.
                if supply >= 8 {
                    core::ptr::copy_nonoverlapping((&full as *const _ as *const u8).add(4), q.add(4), supply - 4);
                }
                r.finish(0, 0);
            }
            S2_GETSTATIONADDRESS => {
                let (st, fa, cfg) = u.shared.with(|s| (s.station, s.factory, s.configured));
                r.set_src_mac(if cfg { &st } else { &fa });
                r.set_dst_mac(&fa);
                r.finish(0, 0);
            }
            S2_GETGLOBALSTATS => {
                let p = r.stat_data() as *mut Sana2DeviceStats;
                if p.is_null() {
                    r.finish(S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
                    return;
                }
                let (st, ls) = u.shared.with(|s| (s.stats, s.last_start));
                let v = Sana2DeviceStats {
                    PacketsReceived: st.rx_packets as u32,
                    PacketsSent: st.tx_packets as u32,
                    BadData: st.bad_data as u32,
                    Overruns: st.overruns as u32,
                    Unused: 0,
                    UnknownTypesReceived: st.unknown_types as u32,
                    Reconfigurations: st.reconfigurations as u32,
                    LastStart: ls,
                };
                core::ptr::write_unaligned(p, v);
                r.finish(0, 0);
            }
            S2_GETEXTENDEDGLOBALSTATS => {
                let p = r.stat_data() as *mut u8;
                if p.is_null() {
                    r.finish(S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
                    return;
                }
                let len = core::ptr::read_unaligned(p as *const u32) as usize;
                if len < 8 {
                    r.finish(IOERR_BADLENGTH, S2WERR_BAD_STATDATA);
                    return;
                }
                let (st, ls, up) = u.shared.with(|s| (s.stats, s.last_start, s.link_up));
                let mut v: Sana2ExtDeviceStats = core::mem::zeroed();
                let actual = len.min(core::mem::size_of::<Sana2ExtDeviceStats>());
                v.s2xds_Length = len as u32;
                v.s2xds_Actual = actual as u32;
                v.s2xds_PacketsReceived = S2Quad::from(st.rx_packets);
                v.s2xds_PacketsSent = S2Quad::from(st.tx_packets);
                v.s2xds_BadData = S2Quad::from(st.bad_data);
                v.s2xds_Overruns = S2Quad::from(st.overruns);
                v.s2xds_UnknownTypesReceived = S2Quad::from(st.unknown_types);
                v.s2xds_Reconfigurations = S2Quad::from(st.reconfigurations);
                v.s2xds_LastStart = ls;
                if up {
                    // A wired link: connected since the unit went online.
                    v.s2xds_LastConnected = ls;
                    v.s2xds_TimeConnected = TimeVal { Seconds: 0, Microseconds: 1 };
                }
                core::ptr::copy_nonoverlapping(&v as *const _ as *const u8, p, actual);
                r.finish(0, 0);
            }
            S2_GETSPECIALSTATS => {
                let h = r.stat_data() as *mut Sana2SpecialStatHeader;
                if h.is_null() {
                    r.finish(S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
                    return;
                }
                let st = u.shared.with(|s| s.stats);
                let p = |i: usize| u.perf[i].load(Ordering::Relaxed) as u64;
                let counts = [
                    st.rx_no_read,
                    st.rx_filtered,
                    st.mcast_rejects,
                    st.tx_waits,
                    st.recoveries,
                    st.bad_data,
                    u.irq_claimed.load(Ordering::Relaxed) as u64,
                    p(perf::WAKES),
                    p(perf::WAKES_RX),
                    p(perf::TX_PUBLISH),
                    p(perf::TX_NOTIFY),
                    p(perf::RX_NOTIFY),
                    p(perf::TX_ACKS),
                    p(perf::RX_ACKS),
                    p(perf::TX_NOTIFY_T),
                    p(perf::ISR_T),
                    p(perf::TX_CALL_T),
                ];
                let max = core::ptr::read_unaligned(core::ptr::addr_of!((*h).RecordCountMax)) as usize;
                let n = max.min(counts.len());
                let recs = (h as *mut u8).add(8) as *mut Sana2SpecialStatRecord;
                for i in 0..n {
                    core::ptr::write_unaligned(
                        recs.add(i),
                        Sana2SpecialStatRecord {
                            Type: (S2_WIRETYPE_ETHERNET << 16) | (i as u32 + 1),
                            Count: counts[i] as u32,
                            String: SPECIAL_NAMES[i].as_ptr(),
                        },
                    );
                }
                core::ptr::write_unaligned(core::ptr::addr_of_mut!((*h).RecordCountSupplied), n as u32);
                r.finish(0, 0);
            }
            S2_TRACKTYPE | S2_UNTRACKTYPE | S2_GETTYPESTATS => {
                let t = r.packet_type();
                let op = r.buffer_management() as *mut Opener;
                let res = u.shared.with(|s| {
                    // Each opener tracks its own types and sees its own counts.
                    let pos = s.tracked.iter().position(|(x, _, o)| *x == t && *o == op);
                    match (cmd, pos) {
                        (S2_TRACKTYPE, Some(_)) => Err(S2WERR_ALREADY_TRACKED),
                        (S2_TRACKTYPE, None) => {
                            s.tracked.push((t, Sana2PacketTypeStats::default(), op));
                            Ok(None)
                        }
                        (S2_UNTRACKTYPE, Some(p)) => {
                            s.tracked.remove(p);
                            Ok(None)
                        }
                        (_, Some(p)) => Ok(Some(s.tracked[p].1)),
                        (_, None) => Err(S2WERR_NOT_TRACKED),
                    }
                });
                match res {
                    Ok(Some(st)) => {
                        let p = r.stat_data() as *mut Sana2PacketTypeStats;
                        if p.is_null() {
                            r.finish(S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
                        } else {
                            core::ptr::write_unaligned(p, st);
                            r.finish(0, 0);
                        }
                    }
                    Ok(None) => r.finish(0, 0),
                    Err(w) => r.finish(S2ERR_BAD_STATE, w),
                }
            }
            S2_ONEVENT => {
                let want = r.wire_error();
                if want == 0 || want & !ALL_EVENTS != 0 {
                    r.finish(S2ERR_NOT_SUPPORTED, S2WERR_BAD_EVENT);
                    return;
                }
                let now_hit = u.shared.with(|s| {
                    let mut hit = 0;
                    if want & S2EVENT_ONLINE != 0 && s.online {
                        hit |= S2EVENT_ONLINE;
                    }
                    if want & S2EVENT_OFFLINE != 0 && !s.online {
                        hit |= S2EVENT_OFFLINE;
                    }
                    // The link too: a client waiting for CONNECT while the
                    // link is already up would otherwise wait forever.
                    if want & S2EVENT_CONNECT != 0 && s.link_up {
                        hit |= S2EVENT_CONNECT;
                    }
                    if want & S2EVENT_DISCONNECT != 0 && !s.link_up {
                        hit |= S2EVENT_DISCONNECT;
                    }
                    if hit == 0 {
                        io.mark_pending();
                        s.events.push(r);
                    }
                    hit
                });
                if now_hit != 0 {
                    r.finish(0, now_hit);
                }
            }
            // Point-to-point commands: not for Ethernet.
            _ => r.finish(IOERR_NOCMD, 0),
        }
    }
}

/// Queue a request for the unit task.
unsafe fn to_task(u: &Unit, r: S2Req) {
    let io = r.io();
    if !u.worker.is_ready() {
        r.finish(IOERR_UNITBUSY, 0);
        return;
    }
    // The unit task replies it later, so it is not quick: with a DoIO's
    // IOF_QUICK left set, DoIO returned at once, before the command was
    // done (io_Error not yet set), and the late ReplyMsg left the request
    // on the caller's port (S2_CONFIGINTERFACE, S2_ONLINE, S2_OFFLINE, the
    // multicast commands, CMD_FLUSH, S2_SAMPLE_THROUGHPUT).
    io.mark_pending();
    u.cmdq.push(io);
    if !u.worker.wake() && u.cmdq.abort(io) == Abort::Removed {
        r.reply(IOERR_UNITBUSY, 0);
    }
}

#[no_mangle]
pub extern "C" fn rust_dev_abort_io(ior: *mut IORequest) {
    let Some(u) = unit_of(ior) else { return };
    // SAFETY: the caller's request; lists under the unit's mutex.
    unsafe {
        let Some(io) = IoReq::from_raw(ior) else { return };
        if u.cmdq.abort(io) == Abort::Removed {
            io.reply(IOERR_ABORTED, 0);
            return;
        }
        if req_len(ior) < S2REQ_SIZE {
            return;
        }
        let r = S2Req::from_raw(ior).unwrap();
        let found = u.shared.with(|s| {
            for list in [&mut s.reads, &mut s.orphans, &mut s.events, &mut s.samples] {
                if let Some(pos) = list.iter().position(|&x| x == r) {
                    list.remove(pos);
                    return true;
                }
            }
            false
        });
        let found = found
            || u.tx.with(|tx| {
                let Some(tx) = tx.as_mut() else { return false };
                match tx.pending.iter().position(|&x| x == r) {
                    Some(pos) => {
                        tx.pending.remove(pos);
                        true
                    }
                    None => false,
                }
            });
        if found {
            r.reply(IOERR_ABORTED, 0);
        }
    }
}
