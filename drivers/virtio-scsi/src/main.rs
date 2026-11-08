//! The Rust virtioscsi.device: a trackdisk-style block device for QEMU's
//! virtio-scsi disks (`-device virtio-scsi-pci` / `-non-transitional`).
//!
//! - Init finds and locks the controllers, sets each one up (modern
//!   interface; legacy for a transitional controller whose modern probe
//!   fails; indirect descriptors required), scans targets 0-7 x LUNs 0-7
//!   with polled INQUIRY and READ CAPACITY, and makes a unit of every disk
//!   and CD-ROM found, numbered in the order found (target, then LUN), as
//!   the C driver does.
//! - The first open on a controller starts its task, which puts an
//!   interrupt server on the controller's chain. In the disk build the last
//!   close stops it (Expunge cannot wait for a task, so the task cannot live
//!   from Init to Expunge there). The drop-in (Kickstart) build is never
//!   expunged and never stops a task: diskboot and Workbench open and close
//!   units all the time, and during boot an exited task stays in exec's
//!   lists for seconds. Requests of every unit on the
//!   controller go through the controller's pipe to its task.
//! - The pipeline: PIPE_SLOTS commands in flight, from up to MAX_ACTIVE
//!   requests. Slot i owns ring descriptor i, which points at the slot's
//!   indirect table [req][data out...][resp][data in...]; the used entry's
//!   id is the slot. A read or write is checked against the unit, its
//!   io_Data mapped for DMA once, and split by `plan_commands` into
//!   READ(16)/WRITE(16) commands of at most seg_max segments; it is replied
//!   when its last command is back.
//! - The task wakes on the interrupt, on new work, and on a tick (TICK_BUSY_MS
//!   with commands out, TICK_IDLE_MS otherwise), and takes completions on
//!   every wake: a lost interrupt costs time, not the request. The tick also
//!   reads the device status: NEEDS_RESET, a lost DRIVER_OK (a reset behind
//!   the driver's back) or a command older than CMD_TIMEOUT_MS starts a
//!   recovery: reset, set up again, and the interrupted requests are sent
//!   once more. MAX_RECOVERIES in a row without a completed command leave
//!   the controller failing every request.
//! - AbortIO: a queued request is replied IOERR_ABORTED at once; one in
//!   progress sends no more commands and is replied IOERR_ABORTED when the
//!   ones out are back. No task management function is sent.
//! - A reset callback resets every running controller on reboot.
//!
//! The default build is vscsirs.device and claims modern-only
//! controllers (1af4:1048) only: the C virtioscsi.device that runs the
//! system disk drives the first transitional one (1af4:1004) without
//! locking it, so taking a 1004 here would mean two drivers on one device.
//! The `dropin` build (virtioscsi.device) takes transitional controllers too.

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
use amigaos4::device::cmd::{CMD_CLEAR, CMD_FLUSH, CMD_READ, CMD_START, CMD_STOP, CMD_UPDATE, CMD_WRITE, NSCMD_DEVICEQUERY};
use amigaos4::device::ioerr::{IOERR_ABORTED, IOERR_BADADDRESS, IOERR_BADLENGTH, IOERR_NOCMD, IOERR_OPENFAIL, IOERR_UNITBUSY};
use amigaos4::device::td::*;
use amigaos4::device::tderr::*;
use amigaos4::device::{answer_device_query, IoReq, Worker, NSDEVTYPE_TRACKDISK};
use amigaos4::interrupt::{IntServer, ResetCallback};
use amigaos4::mem::{DmaBuffer, DmaDirection, DmaMapping};
use amigaos4::serial_println;
use amigaos4::sys::{self, IORequest, PCIDevice, TagItem, Task};
use amigaos4::timer::{AmigaTimer, TimerUnit, TimerVal};
use amigaos4::virtio::ring::{write_indirect, DESC_F_INDIRECT, DESC_F_NEXT, DESC_F_WRITE};
use amigaos4::virtio::scsi::{self as vs, plan_commands, PlannedCmd};
use amigaos4::virtio::{self, Endian, PciTransport, Regs, Transport, Virtqueue};

#[global_allocator]
// MEMF_SHARED: the heap is used by the controller tasks and by every caller
// (BeginIO, Open, Close run in the caller's task), never one task's alone.
static ALLOCATOR: SharedExecAllocator = SharedExecAllocator;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    amigaos4::panic::default_panic_handler(info)
}

#[cfg(feature = "dropin")]
const NAME: &str = "virtioscsi.device";
#[cfg(not(feature = "dropin"))]
const NAME: &str = "vscsirs.device";
#[cfg(feature = "dropin")]
const DEVNAME_C: &[u8] = amigaos4::amstr!("virtioscsi.device");
#[cfg(not(feature = "dropin"))]
const DEVNAME_C: &[u8] = amigaos4::amstr!("vscsirs.device");
#[cfg(feature = "dropin")]
const TASK_NAMES: [&[u8]; MAX_CONTROLLERS] = [
    amigaos4::amstr!("virtioscsi.device controller 0"),
    amigaos4::amstr!("virtioscsi.device controller 1"),
    amigaos4::amstr!("virtioscsi.device controller 2"),
    amigaos4::amstr!("virtioscsi.device controller 3"),
];
#[cfg(not(feature = "dropin"))]
const TASK_NAMES: [&[u8]; MAX_CONTROLLERS] = [
    amigaos4::amstr!("vscsirs.device controller 0"),
    amigaos4::amstr!("vscsirs.device controller 1"),
    amigaos4::amstr!("vscsirs.device controller 2"),
    amigaos4::amstr!("vscsirs.device controller 3"),
];

/// The controllers this build claims, in order.
#[cfg(feature = "dropin")]
const CLAIM_IDS: &[u32] = &[vs::PCI_ID_TRANSITIONAL, vs::PCI_ID_MODERN];
#[cfg(not(feature = "dropin"))]
const CLAIM_IDS: &[u32] = &[vs::PCI_ID_MODERN];

const MAX_CONTROLLERS: usize = 4;
/// Units, over all controllers (the C driver's limit).
const MAX_UNITS: usize = 8;
/// Targets and LUNs scanned (the C driver's range).
const SCAN_TARGETS: u16 = 8;
const SCAN_LUNS: u16 = 8;
/// Queue entries asked for (bounded by what the device offers).
const QUEUE_ENTRIES: usize = 128;
/// Features: the modern interface and indirect descriptors (required).
/// EVENT_IDX is not taken until it has been measured to help.
const WANTED: u64 = virtio::F_VERSION_1 | virtio::F_INDIRECT_DESC;
/// Commands in flight per controller, and the internal slot (Init's scan).
const PIPE_SLOTS: usize = 16;
const INTERNAL_SLOT: usize = PIPE_SLOTS;
const NSLOTS: usize = PIPE_SLOTS + 1;
/// Requests the task works on at once (the rest wait in the pipe).
const MAX_ACTIVE: usize = 16;
/// A slot: request header, response header, indirect table, small data.
const SLOT_STRIDE: usize = 8192;
const SLOT_REQ: usize = 0;
const SLOT_RESP: usize = 64;
const SLOT_TABLE: usize = 256;
const TABLE_ENTRIES: usize = 256;
const SLOT_DATA: usize = SLOT_TABLE + 16 * TABLE_ENTRIES;
const SLOT_DATA_MAX: usize = SLOT_STRIDE - SLOT_DATA;
/// Data segments a command can carry (the table also holds req and resp).
const MAX_SEGS: usize = TABLE_ENTRIES - 2;
/// The most one command moves (bounded by max_sectors too).
const MAX_CMD_BYTES: usize = 4 << 20;
/// A polled command (Init's scan) is given up after this long.
const POLL_TIMEOUT_MS: u64 = 2000;
/// A command older than this starts a recovery.
const CMD_TIMEOUT_MS: u64 = 15_000;
const TICK_BUSY_MS: u32 = 100;
const TICK_IDLE_MS: u32 = 1000;
/// Recoveries in a row (no command completed between) before giving up.
const MAX_RECOVERIES: u32 = 8;
/// A request interrupted by a recovery is sent again this many times.
const MAX_RETRIES: u32 = 1;
/// A command answered UNIT ATTENTION (after a reset, or a medium change)
/// is sent again this many times.
const MAX_UA_RETRIES: u32 = 3;
const TASK_PRI: i32 = 5;
const TASK_STACK: u32 = 65536;
/// How long a closer waits for the controller task to stop, then to be gone.
const STOP_BOUND_MS: u32 = 5000;
/// How long a stopping task lets commands already out finish.
const STOP_DRAIN_MS: u64 = 2000;
const GONE_POLL_MS: u32 = 5;
const PCI_INTERRUPT_LINE: u32 = 0x3c;
/// Development build only: reset the controller behind the driver's back
/// (a private command, far from the trackdisk range).
#[cfg(not(feature = "dropin"))]
const VSCSI_CMD_TESTFAULT: u16 = 0x7ff0;

static SUPPORTED_COMMANDS: [u16; 43] = [
    CMD_READ, CMD_WRITE, CMD_UPDATE, CMD_CLEAR, CMD_STOP, CMD_START, CMD_FLUSH,
    TD_MOTOR, TD_SEEK, TD_FORMAT, TD_REMOVE, TD_CHANGENUM, TD_CHANGESTATE, TD_PROTSTATUS,
    TD_GETDRIVETYPE, TD_GETNUMTRACKS, TD_ADDCHANGEINT, TD_REMCHANGEINT, TD_GETGEOMETRY, TD_EJECT,
    TD_READ64, TD_WRITE64, TD_SEEK64, TD_FORMAT64,
    ETD_READ, ETD_WRITE, ETD_MOTOR, ETD_SEEK, ETD_FORMAT, ETD_UPDATE, ETD_CLEAR,
    NSCMD_TD_READ64, NSCMD_TD_WRITE64, NSCMD_TD_SEEK64, NSCMD_TD_FORMAT64,
    NSCMD_ETD_READ64, NSCMD_ETD_WRITE64, NSCMD_ETD_SEEK64, NSCMD_ETD_FORMAT64,
    NSCMD_TD_GETGEOMETRY64, HD_SCSICMD,
    NSCMD_DEVICEQUERY,
    0,
];
/// The commands that go to the controller task.
const READS: [u16; 5] = [CMD_READ, ETD_READ, TD_READ64, NSCMD_TD_READ64, NSCMD_ETD_READ64];
const WRITES: [u16; 10] = [CMD_WRITE, TD_FORMAT, ETD_WRITE, ETD_FORMAT, TD_WRITE64, TD_FORMAT64, NSCMD_TD_WRITE64, NSCMD_TD_FORMAT64, NSCMD_ETD_WRITE64, NSCMD_ETD_FORMAT64];
const WIDE: [u16; 9] = [TD_READ64, TD_WRITE64, TD_FORMAT64, NSCMD_TD_READ64, NSCMD_TD_WRITE64, NSCMD_TD_FORMAT64, NSCMD_ETD_READ64, NSCMD_ETD_WRITE64, NSCMD_ETD_FORMAT64];
/// sizeof(struct SCSICmd) (devices/scsidisk.h, pack(2)).
const SCSICMD_SIZE: u32 = 30;
/// scsi_Flags.
const SCSIF_READ: u8 = 1;
const SCSIF_AUTOSENSE: u8 = 2;
/// sizeof(struct DriveGeometry) (devices/trackdisk.h, pack(2)) and
/// sizeof(struct DriveGeometry64) (libraries/mounter.h, natural alignment:
/// dg_TotalSectors at 16).
const DG_SIZE: u32 = 32;
const DG64_SIZE: u32 = 32;
/// dg_BufMemType: MEMF_PUBLIC | MEMF_LOCAL (the C driver's; a DOSEnvVec
/// BufMemType must be BPTR-safe).
const BUF_MEM_TYPE: u32 = 0x0001 | 0x0100;
const DGF_REMOVABLE: u8 = 1;

macro_rules! pci {
    ($dev:expr, $method:ident $(, $arg:expr)*) => {
        ((*$dev).$method)($dev $(, $arg)*)
    };
}

// ---------------------------------------------------------------------------
// Controllers: the hardware
// ---------------------------------------------------------------------------

/// What the device's configuration says (section 5.6.4).
#[derive(Clone, Copy, Default, Debug)]
struct Config {
    num_queues: u32,
    seg_max: u32,
    max_sectors: u32,
    cmd_per_lun: u32,
    event_info_size: u32,
    sense_size: u32,
    cdb_size: u32,
    max_channel: u16,
    max_target: u16,
    max_lun: u32,
}

/// One command's data, as the device sees it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    /// The device writes into these blocks (a READ).
    In,
    /// The device reads these blocks (a WRITE).
    Out,
}

/// The outcome of one SCSI command.
#[derive(Clone, Copy, Debug)]
struct Outcome {
    /// virtio-scsi response (`S_OK` ...).
    response: u8,
    /// SCSI status (0 GOOD, 2 CHECK CONDITION).
    status: u8,
    /// Sense key, ASC, ASCQ (fixed-format sense), when there is sense data.
    sense: (u8, u8, u8),
    /// Sense bytes the device returned.
    sense_len: usize,
    /// Data bytes the device did not use.
    residual: u32,
}

impl Outcome {
    fn good(&self) -> bool {
        self.response == vs::S_OK && self.status == 0
    }

    /// The io_Error for a failed command (the C driver's mapping,
    /// virtio_scsi_io.c:18-40).
    fn io_error(&self) -> i8 {
        if self.response != vs::S_OK || self.status != 2 {
            return TDERR_NOT_SPECIFIED;
        }
        match self.sense.0 {
            2 | 4 => TDERR_BAD_DRIVE_TYPE,
            3 => TDERR_BAD_SEC_HDR,
            5 => IOERR_NOCMD,
            6 => TDERR_DISK_CHANGED,
            7 => TDERR_WRITE_PROT,
            _ => TDERR_NOT_SPECIFIED,
        }
    }
}

/// What a slot is doing.
#[derive(Clone, Copy)]
struct SlotUse {
    /// The request (an index into the task's active list) and its command.
    req: usize,
    cmd: usize,
    /// When it was sent (timebase).
    sent: u64,
}

/// One controller, set up.
struct Controller {
    dev: *mut PCIDevice,
    t: PciTransport,
    modern: bool,
    cfg: Config,
    _ctrlq: Virtqueue,
    _eventq: Virtqueue,
    reqq: Virtqueue,
    // Dropped before slots (field order): EndDMA before the memory goes.
    _slots_map: DmaMapping<'static>,
    slots: DmaBuffer,
    slots_phys: u64,
    using: [Option<SlotUse>; NSLOTS],
    /// Recoveries since the last completed command.
    recoveries_in_row: u32,
    /// Gave up after MAX_RECOVERIES: every request fails.
    dead: bool,
}

impl Controller {
    fn slot_ptr(&mut self, slot: usize, off: usize) -> *mut u8 {
        // SAFETY: every slot and offset used is inside the block.
        unsafe { self.slots.as_mut_ptr().add(slot * SLOT_STRIDE + off) }
    }
    fn slot_phys(&self, slot: usize, off: usize) -> u64 {
        self.slots_phys + (slot * SLOT_STRIDE + off) as u64
    }
    fn endian(&self) -> Endian {
        self.t.ring_endian()
    }
    fn in_flight(&self) -> usize {
        self.using[..PIPE_SLOTS].iter().filter(|s| s.is_some()).count()
    }
    fn free_slot(&self) -> Option<usize> {
        self.using[..PIPE_SLOTS].iter().position(|s| s.is_none())
    }

    /// Put one command on the ring in `slot` (its descriptor is the slot's
    /// own). `segs` must hold at most MAX_SEGS entries.
    unsafe fn issue(&mut self, slot: usize, target: u8, lun: u16, cdb: &[u8], dir: Dir, segs: &[(u64, u32)]) {
        let mut req = vs::ReqCmd::zeroed();
        req.lun = vs::lun_address(target, lun);
        req.id = slot as u64;
        req.cdb[..cdb.len()].copy_from_slice(cdb);
        core::ptr::write_unaligned(self.slot_ptr(slot, SLOT_REQ) as *mut vs::ReqCmd, req);
        core::ptr::write_bytes(self.slot_ptr(slot, SLOT_RESP), 0, core::mem::size_of::<vs::RespCmd>());
        // Device-readable first, then device-writable (section 2.7.4):
        // READ [req][resp][data in], WRITE [req][data out][resp].
        let mut parts: Vec<(u64, u32, u16)> = Vec::with_capacity(segs.len() + 2);
        parts.push((self.slot_phys(slot, SLOT_REQ), core::mem::size_of::<vs::ReqCmd>() as u32, 0));
        if dir == Dir::Out {
            parts.extend(segs.iter().map(|&(a, l)| (a, l, 0)));
        }
        parts.push((self.slot_phys(slot, SLOT_RESP), core::mem::size_of::<vs::RespCmd>() as u32, DESC_F_WRITE));
        if dir == Dir::In {
            parts.extend(segs.iter().map(|&(a, l)| (a, l, DESC_F_WRITE)));
        }
        let n = parts.len();
        let table = self.slot_ptr(slot, SLOT_TABLE);
        let e = self.endian();
        for (i, &(addr, len, flags)) in parts.iter().enumerate() {
            let last = i + 1 == n;
            write_indirect(table, e, i as u16, addr, len, flags | if last { 0 } else { DESC_F_NEXT }, if last { 0 } else { i as u16 + 1 });
        }
        let table_phys = self.slot_phys(slot, SLOT_TABLE);
        let ring = self.reqq.ring();
        ring.set_desc(slot as u16, table_phys, (16 * n) as u32, DESC_F_INDIRECT, 0);
        ring.push_avail(slot as u16);
        ring.publish();
        self.t.notify(vs::REQUEST_QUEUE);
    }

    /// The outcome in `slot`'s response header.
    unsafe fn outcome(&mut self, slot: usize) -> Outcome {
        let resp = core::ptr::read_unaligned(self.slot_ptr(slot, SLOT_RESP) as *const vs::RespCmd);
        let le = |v: u32| if self.modern { u32::from_le(v) } else { v };
        let sense_len = (le(resp.sense_len) as usize).min(vs::SENSE_SIZE);
        let sense = if sense_len >= 14 { (resp.sense[2] & 0x0f, resp.sense[12], resp.sense[13]) } else { (0, 0, 0) };
        Outcome { response: resp.response, status: resp.status, sense, sense_len, residual: le(resp.residual) }
    }

    /// Every used entry's slot.
    unsafe fn drain(&mut self) -> Vec<usize> {
        let mut done = Vec::new();
        while let Some((id, _len)) = self.reqq.ring().take_used() {
            let slot = id as usize;
            if slot < NSLOTS {
                done.push(slot);
            } else {
                serial_println!("{}: used entry {} is no slot of ours", NAME, id);
            }
        }
        done
    }

    /// A command with up to SLOT_DATA_MAX bytes read into the internal
    /// slot, polled (Init: no task, interrupts off), retried while the
    /// target reports UNIT ATTENTION. None: no answer in time; the
    /// controller is then not used.
    unsafe fn small_in(&mut self, target: u8, lun: u16, cdb: &[u8], len: usize) -> Option<(Outcome, Vec<u8>)> {
        let len = len.min(SLOT_DATA_MAX);
        let s = INTERNAL_SLOT;
        for _ in 0..3 {
            core::ptr::write_bytes(self.slot_ptr(s, SLOT_DATA), 0, len);
            let data = [(self.slot_phys(s, SLOT_DATA), len as u32)];
            self.issue(s, target, lun, cdb, Dir::In, if len == 0 { &[] } else { &data });
            let deadline = arch::timebase() + timebase_hz() * POLL_TIMEOUT_MS / 1000;
            loop {
                if self.drain().contains(&s) {
                    break;
                }
                if arch::timebase() > deadline {
                    return None;
                }
            }
            let o = self.outcome(s);
            if o.status == 2 && o.sense.0 == 6 {
                continue;
            }
            let got = len.saturating_sub(o.residual as usize);
            return Some((o, core::slice::from_raw_parts(self.slot_ptr(s, SLOT_DATA), got).to_vec()));
        }
        None
    }

    /// The most one command of a unit with `block`-byte blocks moves.
    fn max_cmd_bytes(&self, block: usize) -> usize {
        let m = (self.cfg.max_sectors.max(1) as usize).saturating_mul(block).min(MAX_CMD_BYTES).max(block);
        m - m % block
    }

    /// Reset the device and set it up again on fresh queues.
    unsafe fn reinit(&mut self) -> Result<(), String> {
        let e = |x: amigaos4::AmigaError| alloc::format!("{}", x);
        virtio::init_device(&mut self.t, WANTED).map_err(e)?;
        let (c, ev, r) = make_queues(&mut self.t)?;
        self._ctrlq = c;
        self._eventq = ev;
        self.reqq = r;
        self.reqq.ring().set_interrupts(true);
        virtio::driver_ok(&mut self.t).map_err(e)?;
        self.using = [None; NSLOTS];
        Ok(())
    }
}

/// Every controller this build claims, locked for it, in order.
unsafe fn find_controllers() -> Vec<*mut PCIDevice> {
    let mut found = Vec::new();
    for &id in CLAIM_IDS {
        for index in 0.. {
            if found.len() == MAX_CONTROLLERS {
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

unsafe fn read_config(t: &PciTransport) -> Config {
    Config {
        num_queues: t.config_u32(vs::CFG_NUM_QUEUES),
        seg_max: t.config_u32(vs::CFG_SEG_MAX),
        max_sectors: t.config_u32(vs::CFG_MAX_SECTORS),
        cmd_per_lun: t.config_u32(vs::CFG_CMD_PER_LUN),
        event_info_size: t.config_u32(vs::CFG_EVENT_INFO_SIZE),
        sense_size: t.config_u32(vs::CFG_SENSE_SIZE),
        cdb_size: t.config_u32(vs::CFG_CDB_SIZE),
        max_channel: t.config_u16(vs::CFG_MAX_CHANNEL),
        max_target: t.config_u16(vs::CFG_MAX_TARGET),
        max_lun: t.config_u32(vs::CFG_MAX_LUN),
    }
}

/// The control, event and request queues, set up on the device.
unsafe fn make_queues(t: &mut PciTransport) -> Result<(Virtqueue, Virtqueue, Virtqueue), String> {
    let e = |x: amigaos4::AmigaError| alloc::format!("{}", x);
    let modern = t.is_modern();
    let endian = t.ring_endian();
    let mut queue = |q: u16| -> Result<Virtqueue, String> {
        let max = t.queue_max(q);
        if max == 0 {
            return Err(alloc::format!("queue {} is missing", q));
        }
        let size = if modern { (max as usize).min(QUEUE_ENTRIES).max(1) as u16 } else { max };
        if q == vs::REQUEST_QUEUE && (size as usize) < NSLOTS {
            return Err(alloc::format!("the request queue has {} entries, needs {}", size, NSLOTS));
        }
        let mut vq = Virtqueue::new(size, endian).map_err(e)?;
        vq.ring().set_interrupts(false);
        t.setup_queue(q, &vq).map_err(e)?;
        Ok(vq)
    };
    Ok((queue(vs::CONTROL_QUEUE)?, queue(vs::EVENT_QUEUE)?, queue(vs::REQUEST_QUEUE)?))
}

/// Set one controller up: features, the three queues, the slots,
/// DRIVER_OK. Interrupts stay off until its task runs.
unsafe fn setup(n: usize, dev: *mut PCIDevice) -> Result<Controller, String> {
    let e = |x: amigaos4::AmigaError| alloc::format!("{}", x);
    let device = pci!(dev, ReadConfigWord, 2);
    let irq = pci!(dev, ReadConfigByte, PCI_INTERRUPT_LINE);
    virtio::enable_pci(dev, false);
    let (mut t, why_legacy) = PciTransport::open(dev).map_err(e)?;
    let modern = t.is_modern();
    let offered = t.device_features();
    let features = virtio::init_device(&mut t, WANTED).map_err(e)?;
    if features & virtio::F_INDIRECT_DESC == 0 {
        let _ = t.reset();
        return Err("the device offers no indirect descriptors".into());
    }
    let cfg = read_config(&t);
    if cfg.cdb_size as usize != vs::CDB_SIZE || cfg.sense_size as usize != vs::SENSE_SIZE {
        // The headers are laid out for the default sizes.
        serial_println!("{} controller {}: device uses cdb {} sense {}; headers are {}/{}", NAME, n, cfg.cdb_size, cfg.sense_size, vs::CDB_SIZE, vs::SENSE_SIZE);
    }
    let (ctrlq, eventq, reqq) = make_queues(&mut t)?;
    let sizes = (ctrlq.layout().size, eventq.layout().size, reqq.layout().size);
    let total = NSLOTS * SLOT_STRIDE;
    let mut slots = DmaBuffer::alloc_contiguous(total, 4096).map_err(e)?;
    let map = DmaMapping::new(slots.as_mut_ptr(), total, DmaDirection::FromDevice).map_err(e)?;
    let ent = map.entries();
    if ent.len() != 1 {
        return Err(alloc::format!("the slots are {} DMA blocks, need 1", ent.len()));
    }
    let slots_phys = ent[0].PhysicalAddress as usize as u64;
    serial_println!(
        "{} controller {}: DMA memory: rings at 0x{:08x} 0x{:08x} 0x{:08x} (+{}), slots 0x{:08x}-0x{:08x} (virtual 0x{:08x})",
        NAME, n, ctrlq.desc_phys(), eventq.desc_phys(), reqq.desc_phys(), reqq.layout().total, slots_phys, slots_phys + total as u64,
        slots.as_mut_ptr() as usize
    );
    virtio::driver_ok(&mut t).map_err(e)?;
    match why_legacy {
        None => serial_println!("{} controller {}: 1af4:{:04x}, modern interface, IRQ line {}", NAME, n, device, irq),
        Some(why) => serial_println!("{} controller {}: 1af4:{:04x}, legacy interface ({}), IRQ line {}", NAME, n, device, why, irq),
    }
    serial_println!(
        "{} controller {}: features offered 0x{:x}, accepted 0x{:x}; queues control {}, event {}, request {}; {} slots",
        NAME, n, offered, features, sizes.0, sizes.1, sizes.2, PIPE_SLOTS
    );
    serial_println!(
        "{} controller {}: config: {} request queue(s), seg_max {}, max_sectors {}, cmd_per_lun {}, event_info {}, sense {}, cdb {}, max_channel {}, max_target {}, max_lun {}",
        NAME, n, cfg.num_queues, cfg.seg_max, cfg.max_sectors, cfg.cmd_per_lun, cfg.event_info_size,
        cfg.sense_size, cfg.cdb_size, cfg.max_channel, cfg.max_target, cfg.max_lun
    );
    Ok(Controller {
        dev, t, modern, cfg, _ctrlq: ctrlq, _eventq: eventq, reqq,
        _slots_map: map, slots, slots_phys, using: [None; NSLOTS], recoveries_in_row: 0, dead: false,
    })
}

unsafe fn release(dev: *mut PCIDevice) {
    pci!(dev, Unlock);
    sys::wrappers::pci::pci_free_device(dev);
}

/// The timebase frequency (exec GetCPUInfo, GCIT_TimeBaseSpeed).
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
// The pipe: requests waiting for the task, and the ones it works on
// ---------------------------------------------------------------------------

struct PipeState {
    pending: VecDeque<IoReq>,
    /// Taken by the task: (request, AbortIO asked for it).
    active: Vec<(IoReq, bool)>,
}

/// What AbortIO found.
#[derive(PartialEq, Eq)]
enum Found {
    /// Queued and removed: the caller replies it IOERR_ABORTED.
    Removed,
    /// In progress: flagged, the task replies it.
    Flagged,
    NotFound,
}

/// A controller's requests, under an exec mutex (BeginIO, AbortIO and the
/// task; never an interrupt).
struct Pipe {
    state: UnsafeCell<PipeState>,
    mutex: AtomicPtr<c_void>,
}

impl Pipe {
    const fn new() -> Self {
        Pipe { state: UnsafeCell::new(PipeState { pending: VecDeque::new(), active: Vec::new() }), mutex: AtomicPtr::new(null_mut()) }
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
        *self.state.get() = PipeState { pending: VecDeque::new(), active: Vec::new() };
    }
    fn with<R>(&self, f: impl FnOnce(&mut PipeState) -> R) -> R {
        let m = self.mutex.load(Ordering::Acquire);
        // SAFETY: the mutex guards the state.
        unsafe {
            if !m.is_null() {
                sys::exec_mutex_obtain(m as sys::APTR);
            }
            let r = f(&mut *self.state.get());
            if !m.is_null() {
                sys::exec_mutex_release(m as sys::APTR);
            }
            r
        }
    }
    fn push(&self, io: IoReq) {
        io.mark_pending();
        self.with(|s| s.pending.push_back(io));
    }
    fn take(&self) -> Option<IoReq> {
        self.with(|s| {
            let io = s.pending.pop_front()?;
            s.active.push((io, false));
            Some(io)
        })
    }
    fn aborted(&self, io: IoReq) -> bool {
        self.with(|s| s.active.iter().any(|&(a, f)| a == io && f))
    }
    /// The task is done with `io`: call before the reply.
    fn finish(&self, io: IoReq) {
        self.with(|s| s.active.retain(|&(a, _)| a != io));
    }
    fn abort(&self, io: IoReq) -> Found {
        self.with(|s| {
            if let Some(i) = s.pending.iter().position(|&p| p == io) {
                s.pending.remove(i);
                Found::Removed
            } else if let Some(a) = s.active.iter_mut().find(|(a, _)| *a == io) {
                a.1 = true;
                Found::Flagged
            } else {
                Found::NotFound
            }
        })
    }
    fn take_all_pending(&self) -> Vec<IoReq> {
        self.with(|s| s.pending.drain(..).collect())
    }
}

// ---------------------------------------------------------------------------
// Controllers: the software side (pipe, task, interrupt)
// ---------------------------------------------------------------------------

/// Everything about a controller that its task, its interrupt server and
/// the device entry points share. A fixed array, so the server's is_Data
/// never moves.
struct Ctl {
    /// The hardware. Init and Expunge (nothing else runs), otherwise the
    /// controller task only.
    hw: UnsafeCell<Option<Controller>>,
    /// The server's copy of the ISR and status registers.
    regs: UnsafeCell<Option<Regs>>,
    regs_ready: AtomicU32,
    pipe: Pipe,
    worker: Worker,
    /// The task's interrupt signal (0 while there is no server).
    irq_task: AtomicPtr<Task>,
    irq_mask: AtomicU32,
    /// Opens of all units on this controller (under OPEN_LOCK).
    opens: AtomicU32,
    running: AtomicU32,
    stop_ack_task: AtomicPtr<Task>,
    stop_ack_mask: AtomicU32,
    /// A stopped task that exec had not yet removed: Expunge must wait for
    /// it (its last instructions are this segment's).
    lingering: AtomicU32,
    // Counters, since the device was loaded.
    irq_claimed: AtomicU32,
    irq_passed_on: AtomicU32,
    requests: AtomicU32,
    commands: AtomicU32,
    /// Wakes that found completions, by what woke the task.
    found_by_irq: AtomicU32,
    found_by_tick: AtomicU32,
    found_by_work: AtomicU32,
    most_in_flight: AtomicU32,
    recoveries: AtomicU32,
    retried: AtomicU32,
    unplannable: AtomicU32,
}

// SAFETY: see the field comments; the atomics and the pipe's mutex cover
// the rest.
unsafe impl Sync for Ctl {}

impl Ctl {
    const NEW: Ctl = Ctl {
        hw: UnsafeCell::new(None),
        regs: UnsafeCell::new(None),
        regs_ready: AtomicU32::new(0),
        pipe: Pipe::new(),
        worker: Worker::new(),
        irq_task: AtomicPtr::new(null_mut()),
        irq_mask: AtomicU32::new(0),
        opens: AtomicU32::new(0),
        running: AtomicU32::new(0),
        stop_ack_task: AtomicPtr::new(null_mut()),
        stop_ack_mask: AtomicU32::new(0),
        lingering: AtomicU32::new(0),
        irq_claimed: AtomicU32::new(0),
        irq_passed_on: AtomicU32::new(0),
        requests: AtomicU32::new(0),
        commands: AtomicU32::new(0),
        found_by_irq: AtomicU32::new(0),
        found_by_tick: AtomicU32::new(0),
        found_by_work: AtomicU32::new(0),
        most_in_flight: AtomicU32::new(0),
        recoveries: AtomicU32::new(0),
        retried: AtomicU32::new(0),
        unplannable: AtomicU32::new(0),
    };
}

static CTLS: [Ctl; MAX_CONTROLLERS] = [Ctl::NEW, Ctl::NEW, Ctl::NEW, Ctl::NEW];
/// How many of CTLS hold a controller (set by Init, cleared by Expunge).
static NUM_CTLS: AtomicU32 = AtomicU32::new(0);
/// Serialises Open and Close (controller task start and stop).
static OPEN_LOCK: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
/// Controller tasks running (under OPEN_LOCK): the reset callback is on
/// while it is non-zero.
static RUNNING_TASKS: AtomicU32 = AtomicU32::new(0);
struct ResetCell(UnsafeCell<Option<ResetCallback>>);
// SAFETY: changed only under OPEN_LOCK.
unsafe impl Sync for ResetCell {}
static RESET_CB: ResetCell = ResetCell(UnsafeCell::new(None));

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

/// The interrupt server. A shared INTx line: the ISR register says whether
/// it is ours (reading it clears it and drops the line); zero is someone
/// else's, passed on.
unsafe extern "C" fn scsi_irq(_ctx: *mut c_void, _sysbase: *mut c_void, data: *mut c_void) -> u32 {
    let c = &*(data as *const Ctl);
    let r = if c.regs_ready.load(Ordering::Acquire) != 0 { *c.regs.get() } else { None };
    let claimed = match r {
        Some(r) => {
            let isr = r.read_isr();
            if isr == 0 {
                c.irq_passed_on.fetch_add(1, Ordering::Relaxed);
                0
            } else {
                c.irq_claimed.fetch_add(1, Ordering::Relaxed);
                let task = c.irq_task.load(Ordering::Acquire);
                let mask = c.irq_mask.load(Ordering::Relaxed);
                if !task.is_null() && mask != 0 {
                    sys::exec_signal(task, mask);
                }
                1
            }
        }
        None => 0,
    };
    arch::clear_reservation();
    claimed
}

/// The reset callback (exec.doc AddResetCallback): on reboot, reset every
/// running controller so none goes on with DMA into memory the next boot
/// hands out again. Status 0 is the virtio reset; reading the ISR drops the
/// line. No lock, one walk.
unsafe extern "C" fn scsi_reset(_ctx: *mut c_void, _sysbase: *mut c_void, _data: *mut c_void) -> u32 {
    for c in &CTLS {
        if c.regs_ready.load(Ordering::Acquire) != 0 {
            if let Some(r) = *c.regs.get() {
                r.write_status(0);
                let _ = r.read_isr();
            }
        }
    }
    0
}

/// Start controller `ci`'s task, and wait until it takes requests.
unsafe fn start_controller(ci: usize) -> Result<(), String> {
    let c = &CTLS[ci];
    if c.running.load(Ordering::Acquire) != 0 {
        return Ok(());
    }
    if !c.pipe.init() {
        return Err("mutex allocation failed".into());
    }
    let tags = [
        TagItem { ti_Tag: sys::AT_PARAM1, ti_Data: ci as u32 },
        TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
    ];
    c.running.store(1, Ordering::Release);
    let task = sys::exec_create_task(TASK_NAMES[ci].as_ptr() as _, TASK_PRI, controller_task as *const c_void, TASK_STACK, tags.as_ptr());
    if task.is_null() {
        c.running.store(0, Ordering::Release);
        c.pipe.destroy();
        return Err("CreateTask failed".into());
    }
    let mut timer = AmigaTimer::open(TimerUnit::MicroHz).map_err(|e| alloc::format!("timer.device: {}", e))?;
    let mut waited = 0;
    while !c.worker.is_ready() {
        if c.running.load(Ordering::Acquire) == 0 {
            return Err("the controller task ended at once".into());
        }
        if waited >= STOP_BOUND_MS {
            return Err(alloc::format!("the controller task was not ready within {} ms", STOP_BOUND_MS));
        }
        let _ = timer.delay(TimerVal::from_millis(GONE_POLL_MS));
        waited += GONE_POLL_MS;
    }
    if RUNNING_TASKS.fetch_add(1, Ordering::AcqRel) == 0 {
        match ResetCallback::add(127, DEVNAME_C, scsi_reset, null_mut()) {
            Ok(cb) => *RESET_CB.0.get() = Some(cb),
            Err(e) => serial_println!("{}: AddResetCallback failed: {}; controllers are not reset on reboot", NAME, e),
        }
    }
    Ok(())
}

/// Stop controller `ci`'s task: ask, wait for its answer, then until exec
/// has removed it (its last instructions are this segment's).
unsafe fn stop_controller(ci: usize) -> Result<(), String> {
    let c = &CTLS[ci];
    if c.running.load(Ordering::Acquire) == 0 {
        return Ok(());
    }
    let mut timer = AmigaTimer::open(TimerUnit::MicroHz).map_err(|e| alloc::format!("timer.device: {}", e))?;
    let sig = sys::exec_alloc_signal(-1);
    if sig < 0 {
        return Err("no free signal".into());
    }
    let mask = 1u32 << sig;
    c.stop_ack_task.store(sys::exec_find_task(null_mut()), Ordering::Release);
    c.stop_ack_mask.store(mask, Ordering::Release);
    c.worker.request_stop();
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
    c.stop_ack_mask.store(0, Ordering::Release);
    c.stop_ack_task.store(null_mut(), Ordering::Release);
    if !answered {
        return Err(alloc::format!("the controller task did not stop within {} ms", STOP_BOUND_MS));
    }
    // The task has answered: it is done with the controller, the pipe and
    // the Worker (only exec's exit code is left). Ready for the next start.
    c.pipe.destroy();
    // A fresh Worker for the next start (its stop flag stays set otherwise).
    core::ptr::write(&c.worker as *const Worker as *mut Worker, Worker::new());
    if RUNNING_TASKS.fetch_sub(1, Ordering::AcqRel) == 1 {
        drop((*RESET_CB.0.get()).take()); // RemResetCallback
    }
    // Gone from exec's lists? Until it is, Expunge refuses.
    let mut waited = 0;
    while !sys::exec_find_task(TASK_NAMES[ci].as_ptr() as _).is_null() {
        if waited >= STOP_BOUND_MS {
            c.lingering.store(1, Ordering::Release);
            serial_println!("{} controller {}: the stopped task was still in exec's lists after {} ms; expunge waits for it", NAME, ci, STOP_BOUND_MS);
            break;
        }
        let _ = timer.delay(TimerVal::from_millis(GONE_POLL_MS));
        waited += GONE_POLL_MS;
    }
    let n = |a: &AtomicU32| a.load(Ordering::Relaxed);
    serial_println!(
        "{} controller {}: task stopped. Since load: {} requests, {} commands, most {} in flight; completions found by interrupt {}, tick {}, new work {}; interrupts {} ours, {} passed on; recoveries {}, requests retried {}, unplannable {}",
        NAME, ci, n(&c.requests), n(&c.commands), n(&c.most_in_flight), n(&c.found_by_irq), n(&c.found_by_tick), n(&c.found_by_work),
        n(&c.irq_claimed), n(&c.irq_passed_on), n(&c.recoveries), n(&c.retried), n(&c.unplannable)
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Requests in the task
// ---------------------------------------------------------------------------

enum Kind {
    /// A read or write: the unit's first block.
    Rw { write: bool, lba: u64 },
    /// HD_SCSICMD: the caller's SCSICmd and command.
    Scsi { sc: *mut u8, cdb: Vec<u8>, read: bool, len: usize },
}

/// A request the task works on.
struct Active {
    io: IoReq,
    unit: &'static DevUnit,
    kind: Kind,
    /// io_Data (or scsi_Data) mapped for DMA, until the reply.
    map: Option<DmaMapping<'static>>,
    plan: Vec<PlannedCmd>,
    /// The next command to send; commands out; the first error.
    next: usize,
    out: usize,
    error: i8,
    aborted: bool,
    retries: u32,
    /// Commands to send again (UNIT ATTENTION), before `next`.
    redo: Vec<usize>,
    ua_retries: u32,
    /// HD_SCSICMD: the outcome and sense, kept for the reply.
    scsi_outcome: Option<(Outcome, [u8; vs::SENSE_SIZE])>,
}

impl Active {
    fn finished(&self) -> bool {
        self.out == 0 && ((self.next == self.plan.len() && self.redo.is_empty()) || self.error != 0 || self.aborted)
    }
}

/// Start a request: check it, map it and plan its commands. Err: reply it
/// at once with this (io_Error, io_Actual).
unsafe fn prepare(hw: &mut Controller, ctl: &Ctl, io: IoReq) -> Result<Active, (i8, u32)> {
    let u = unit_of(io).ok_or((IOERR_OPENFAIL, 0))?;
    let cmd = io.command();
    if hw.dead {
        return Err((TDERR_NOT_SPECIFIED, 0));
    }
    let new = |kind: Kind, map: Option<DmaMapping<'static>>, plan: Vec<PlannedCmd>| Active {
        io, unit: u, kind, map, plan, next: 0, out: 0, error: 0, aborted: false, retries: 0, redo: Vec::new(), ua_retries: 0, scsi_outcome: None,
    };
    if cmd == HD_SCSICMD {
        let sc = io.data();
        if sc.is_null() || io.length() < SCSICMD_SIZE {
            return Err((IOERR_BADLENGTH, 0));
        }
        let rd32 = |off: usize| u32::from_be(core::ptr::read_unaligned(sc.add(off) as *const u32));
        let rd16 = |off: usize| u16::from_be(core::ptr::read_unaligned(sc.add(off) as *const u16));
        let wr32 = |off: usize, v: u32| core::ptr::write_unaligned(sc.add(off) as *mut u32, v.to_be());
        let wr16 = |off: usize, v: u16| core::ptr::write_unaligned(sc.add(off) as *mut u16, v.to_be());
        let data = rd32(0) as usize as *mut u8;
        let len = rd32(4) as usize;
        let cmdp = rd32(12) as usize as *const u8;
        let cmd_len = rd16(16) as usize;
        let read = *sc.add(20) & SCSIF_READ != 0;
        wr32(8, 0); // scsi_Actual
        wr16(18, 0); // scsi_CmdActual
        *sc.add(21) = 0; // scsi_Status
        wr16(28, 0); // scsi_SenseActual
        if cmdp.is_null() || cmd_len == 0 || cmd_len > vs::CDB_SIZE || (len > 0 && data.is_null()) {
            return Err((IOERR_BADLENGTH, 0));
        }
        let cdb = core::slice::from_raw_parts(cmdp, cmd_len).to_vec();
        let (map, segs) = if len > 0 {
            let m = DmaMapping::new(data, len, if read { DmaDirection::FromDevice } else { DmaDirection::ToDevice }).map_err(|_| (HFERR_DMA, 0))?;
            let segs: Vec<(u64, u32)> = m.entries().iter().map(|e| (e.PhysicalAddress as usize as u64, e.BlockLength)).collect();
            (Some(m), segs)
        } else {
            (None, Vec::new())
        };
        if segs.len() > MAX_SEGS.min(hw.cfg.seg_max.max(1) as usize) {
            ctl.unplannable.fetch_add(1, Ordering::Relaxed);
            return Err((HFERR_DMA, 0));
        }
        let plan = alloc::vec![PlannedCmd { offset: 0, len, segs }];
        return Ok(new(Kind::Scsi { sc, cdb, read, len }, map, plan));
    }
    // A read or write.
    let write = WRITES.contains(&cmd);
    let offset = if WIDE.contains(&cmd) { ((io.actual() as u64) << 32) | io.offset() as u64 } else { io.offset() as u64 };
    #[cfg(feature = "trace")]
    serial_println!("{} unit {}: cmd 0x{:04x} offset {} (block {}) length {} data {:p}", NAME, u.index, cmd, offset, offset / u.block_size.max(1) as u64, io.length(), io.data());
    let len = io.length() as u64;
    let bs = u.block_size as u64;
    if u.blocks == 0 || bs == 0 {
        return Err((TDERR_DISK_CHANGED, 0)); // no medium
    }
    if offset % bs != 0 || len % bs != 0 {
        return Err((IOERR_BADLENGTH, 0));
    }
    let lba = offset / bs;
    if lba >= u.blocks || len / bs > u.blocks - lba {
        return Err((IOERR_BADADDRESS, 0));
    }
    if len == 0 {
        return Err((0, 0)); // nothing to do: replied at once, successfully
    }
    let data = io.data();
    if data.is_null() {
        return Err((IOERR_BADADDRESS, 0));
    }
    let m = DmaMapping::new(data, len as usize, if write { DmaDirection::ToDevice } else { DmaDirection::FromDevice }).map_err(|_| (HFERR_DMA, 0))?;
    let segs: Vec<(u64, u32)> = m.entries().iter().map(|e| (e.PhysicalAddress as usize as u64, e.BlockLength)).collect();
    let max_segs = MAX_SEGS.min(hw.cfg.seg_max.max(1) as usize);
    let Some(plan) = plan_commands(&segs, bs as usize, max_segs, hw.max_cmd_bytes(bs as usize)) else {
        ctl.unplannable.fetch_add(1, Ordering::Relaxed);
        serial_println!("{} unit {}: {} bytes at {:p} are in {} DMA blocks that cannot be planned", NAME, u.index, len, data, segs.len());
        return Err((HFERR_DMA, 0));
    };
    Ok(new(Kind::Rw { write, lba }, Some(m), plan))
}

/// Send command `a.next` of request `ri` in a free slot.
unsafe fn send_next(hw: &mut Controller, ctl: &Ctl, actives: &mut [Active], ri: usize, slot: usize) {
    let a = &mut actives[ri];
    let ci = if let Some(c) = a.redo.pop() { c } else { a.next };
    let pc = &a.plan[ci];
    let (cdb, dir): (Vec<u8>, Dir) = match &a.kind {
        Kind::Rw { write, lba } => {
            let bs = a.unit.block_size as usize;
            let first = lba + (pc.offset / bs) as u64;
            let nblk = (pc.len / bs) as u32;
            let mut cdb = alloc::vec![0u8; 16];
            cdb[0] = if *write { 0x8a } else { 0x88 }; // WRITE(16) / READ(16)
            cdb[2..10].copy_from_slice(&first.to_be_bytes());
            cdb[10..14].copy_from_slice(&nblk.to_be_bytes());
            (cdb, if *write { Dir::Out } else { Dir::In })
        }
        Kind::Scsi { cdb, read, .. } => (cdb.clone(), if *read { Dir::In } else { Dir::Out }),
    };
    let segs = pc.segs.clone();
    let (target, lun) = (a.unit.target, a.unit.lun);
    if ci == a.next {
        a.next += 1;
    }
    a.out += 1;
    hw.using[slot] = Some(SlotUse { req: ri, cmd: ci, sent: arch::timebase() });
    hw.issue(slot, target, lun, &cdb, dir, &segs);
    ctl.commands.fetch_add(1, Ordering::Relaxed);
    let n = hw.in_flight() as u32;
    if n > ctl.most_in_flight.load(Ordering::Relaxed) {
        ctl.most_in_flight.store(n, Ordering::Relaxed);
    }
}

/// A command came back in `slot`.
unsafe fn complete(hw: &mut Controller, actives: &mut [Active], slot: usize) {
    let Some(su) = hw.using[slot].take() else {
        serial_println!("{}: slot {} came back but was not in use", NAME, slot);
        return;
    };
    let o = hw.outcome(slot);
    let Some(a) = actives.get_mut(su.req) else { return };
    a.out -= 1;
    hw.recoveries_in_row = 0;
    // UNIT ATTENTION: the target was reset (a recovery) or its medium
    // changed; the command was not carried out. Send it again.
    if o.response == vs::S_OK && o.status == 2 && o.sense.0 == 6 && a.ua_retries < MAX_UA_RETRIES {
        a.ua_retries += 1;
        a.redo.push(su.cmd);
        return;
    }
    match &a.kind {
        Kind::Rw { write, .. } => {
            if !o.good() || o.residual != 0 {
                if a.error == 0 {
                    a.error = if o.good() { TDERR_NOT_SPECIFIED } else { o.io_error() };
                }
                let pc = &a.plan[su.cmd];
                serial_println!(
                    "{} unit {}: {} of {} bytes at request offset {} failed: response {} status {} sense {:x}/{:x}/{:x} residual {}",
                    NAME, a.unit.index, if *write { "write" } else { "read" }, pc.len, pc.offset,
                    vs::response_name(o.response), o.status, o.sense.0, o.sense.1, o.sense.2, o.residual
                );
            }
        }
        Kind::Scsi { .. } => {
            let resp = core::ptr::read_unaligned(hw.slot_ptr(slot, SLOT_RESP) as *const vs::RespCmd);
            a.scsi_outcome = Some((o, resp.sense));
        }
    }
}

/// Reply a finished request.
unsafe fn reply(ctl: &Ctl, mut a: Active) {
    drop(a.map.take()); // EndDMA before the caller sees its buffer again
    let (error, actual) = if a.aborted && a.error == 0 && a.next < a.plan.len() {
        (IOERR_ABORTED, 0)
    } else if a.error != 0 {
        (a.error, 0)
    } else {
        match &a.kind {
            Kind::Rw { .. } => (0, a.io.length()),
            Kind::Scsi { sc, len, cdb, .. } => scsi_reply(*sc, *len, cdb.len(), a.scsi_outcome),
        }
    };
    ctl.pipe.finish(a.io);
    ctl.requests.fetch_add(1, Ordering::Relaxed);
    checked_reply(a.io, error, actual, "a.io@1");
}

/// Reply a request that was made pending, after checking it still is: a
/// pending request is NT_MESSAGE (IoReq::mark_pending) with a reply port.
/// Anything else would be a second reply (ReplyMsg would link the message
/// into its port twice, or into a port already freed): logged, not sent.
/// `site` says which reply it was.
unsafe fn checked_reply(io: IoReq, error: i8, actual: u32, site: &str) {
    let base = io.as_ptr() as *const u8;
    let ln_type = *base.add(8);
    let reply_port = core::ptr::read_unaligned(base.add(14) as *const u32);
    if ln_type != NT_MESSAGE || reply_port == 0 {
        GUARD_TRIPS.fetch_add(1, Ordering::Relaxed);
        serial_println!(
            "{}: NOT replying request {:p} ({}): node type {} (want {}), reply port 0x{:08x}, command 0x{:04x}",
            NAME, base, site, ln_type, NT_MESSAGE, reply_port, io.command()
        );
        return;
    }
    io.reply(error, actual);
}

/// How often checked_reply refused.
static GUARD_TRIPS: AtomicU32 = AtomicU32::new(0);

/// Fill a SCSICmd from its command's outcome: (io_Error, io_Actual).
unsafe fn scsi_reply(sc: *mut u8, len: usize, cmd_len: usize, out: Option<(Outcome, [u8; vs::SENSE_SIZE])>) -> (i8, u32) {
    let Some((o, sense)) = out else { return (TDERR_NOT_SPECIFIED, 0) };
    if o.response != vs::S_OK {
        return (if o.response == vs::S_BAD_TARGET { HFERR_SEL_TIMEOUT } else { TDERR_NOT_SPECIFIED }, 0);
    }
    let rd32 = |off: usize| u32::from_be(core::ptr::read_unaligned(sc.add(off) as *const u32));
    let rd16 = |off: usize| u16::from_be(core::ptr::read_unaligned(sc.add(off) as *const u16));
    let wr32 = |off: usize, v: u32| core::ptr::write_unaligned(sc.add(off) as *mut u32, v.to_be());
    let wr16 = |off: usize, v: u16| core::ptr::write_unaligned(sc.add(off) as *mut u16, v.to_be());
    wr32(8, (len as u32).saturating_sub(o.residual));
    wr16(18, cmd_len as u16);
    *sc.add(21) = o.status;
    let flags = *sc.add(20);
    let sense_buf = rd32(22) as usize as *mut u8;
    let sense_len = rd16(26) as usize;
    if o.status == 2 && flags & SCSIF_AUTOSENSE != 0 && !sense_buf.is_null() && sense_len > 0 {
        let n = o.sense_len.min(sense_len);
        core::ptr::copy_nonoverlapping(sense.as_ptr(), sense_buf, n);
        wr16(28, n as u16);
    }
    if o.status != 0 {
        return (HFERR_BAD_STATUS, 0);
    }
    (0, SCSICMD_SIZE)
}

/// Reset and set the controller up again. Requests that were in progress
/// start over (MAX_RETRIES) or fail.
unsafe fn recover(hw: &mut Controller, ctl: &Ctl, ci: usize, actives: &mut Vec<Active>, why: &str) {
    ctl.recoveries.fetch_add(1, Ordering::Relaxed);
    hw.recoveries_in_row += 1;
    serial_println!("{} controller {}: recovery ({}), {} in flight, {} requests in progress, {} in a row", NAME, ci, why, hw.in_flight(), actives.len(), hw.recoveries_in_row);
    let _ = hw.t.reset();
    let _ = hw.t.read_isr();
    let ok = if hw.recoveries_in_row > MAX_RECOVERIES {
        serial_println!("{} controller {}: {} recoveries in a row: giving up; every request fails", NAME, ci, MAX_RECOVERIES);
        false
    } else {
        match hw.reinit() {
            Ok(()) => true,
            Err(e) => {
                serial_println!("{} controller {}: set-up after reset failed: {}; every request fails", NAME, ci, e);
                false
            }
        }
    };
    if !ok {
        hw.dead = true;
        hw.using = [None; NSLOTS];
    }
    // Start the interrupted requests over, or fail them.
    let mut keep = Vec::new();
    for mut a in actives.drain(..) {
        a.out = 0;
        if ok && a.retries < MAX_RETRIES && !a.aborted {
            a.retries += 1;
            a.next = 0;
            a.error = 0;
            a.redo.clear();
            a.scsi_outcome = None;
            ctl.retried.fetch_add(1, Ordering::Relaxed);
            keep.push(a);
        } else {
            if a.error == 0 {
                a.error = TDERR_NOT_SPECIFIED;
            }
            reply(ctl, a);
        }
    }
    *actives = keep;
}

/// A controller task's entry (exec.doc CreateTask): AT_Param1, the
/// controller's index, arrives as the first argument.
extern "C" fn controller_task(ci: u32) {
    // SAFETY: from here on this task is the controller's only user.
    unsafe {
        let ci = ci as usize;
        let c = &CTLS[ci];
        let finish = |c: &Ctl| {
            c.running.store(0, Ordering::Release);
            let t = c.stop_ack_task.load(Ordering::Acquire);
            let m = c.stop_ack_mask.load(Ordering::Acquire);
            if !t.is_null() && m != 0 {
                sys::exec_signal(t, m);
            }
        };
        let Some(hw) = (*c.hw.get()).as_mut() else { return finish(c) };
        let Ok(mut timer) = AmigaTimer::open(TimerUnit::MicroHz) else {
            serial_println!("{} controller {}: task: no timer.device", NAME, ci);
            return finish(c);
        };
        let irq_sig = sys::exec_alloc_signal(-1);
        let work_sig = sys::exec_alloc_signal(-1);
        if irq_sig < 0 || work_sig < 0 {
            serial_println!("{} controller {}: task: no free signal", NAME, ci);
            return finish(c);
        }
        let me = sys::exec_find_task(null_mut());
        // Where this task's stack is (exec/tasks.h, pack(2): tc_SPLower at 58,
        // tc_SPUpper at 62), and who created it, for the logs.
        {
            let tb = me as *const u8;
            let lo = core::ptr::read_unaligned(tb.add(58) as *const u32);
            let hi = core::ptr::read_unaligned(tb.add(62) as *const u32);
            serial_println!("{} controller {}: task 0x{:08x}, stack 0x{:08x}-0x{:08x}", NAME, ci, me as usize, lo, hi);
        }
        let irq_mask = 1u32 << irq_sig;
        let work_mask = 1u32 << work_sig;

        // Everything the server reads is in place before AddIntServer.
        *c.regs.get() = Some(hw.t.regs());
        c.regs_ready.store(1, Ordering::Release);
        c.irq_mask.store(irq_mask, Ordering::Relaxed);
        c.irq_task.store(me, Ordering::Release);
        let chain = pci!(hw.dev, MapInterrupt);
        let server = match IntServer::add(chain, 0, TASK_NAMES[ci], scsi_irq, c as *const Ctl as *mut c_void) {
            Ok(s) => Some(s),
            Err(e) => {
                serial_println!("{} controller {}: AddIntServer({}) failed: {}; completions are found by the tick", NAME, ci, chain, e);
                None
            }
        };
        hw.reqq.ring().set_interrupts(server.is_some());
        serial_println!(
            "{} controller {}: task ready: interrupt chain {} (PCI line {}), {}",
            NAME, ci, chain, pci!(hw.dev, ReadConfigByte, PCI_INTERRUPT_LINE),
            if server.is_some() { "server added" } else { "no server" }
        );
        c.worker.publish(me, work_mask);

        let hz = timebase_hz();
        let mut actives: Vec<Active> = Vec::new();
        let mut stop_since: Option<u64> = None;
        loop {
            if timer.is_done() {
                timer.finish();
                timer.start(TimerVal::from_millis(if hw.in_flight() > 0 { TICK_BUSY_MS } else { TICK_IDLE_MS }));
            }
            let got = sys::exec_wait(work_mask | irq_mask | timer.signal_mask());
            let ticked = got & timer.signal_mask() != 0 && timer.is_done();

            // Completions, whatever woke the task.
            let done = hw.drain();
            if !done.is_empty() {
                let by = if got & irq_mask != 0 { &c.found_by_irq } else if ticked { &c.found_by_tick } else { &c.found_by_work };
                by.fetch_add(1, Ordering::Relaxed);
            }
            for slot in done {
                if slot < PIPE_SLOTS {
                    complete(hw, &mut actives, slot);
                }
            }

            // The heartbeat.
            if ticked && !hw.dead {
                let st = hw.t.status();
                let now = arch::timebase();
                let oldest = hw.using[..PIPE_SLOTS].iter().flatten().map(|s| s.sent).min();
                if st & virtio::STATUS_DEVICE_NEEDS_RESET != 0 {
                    recover(hw, c, ci, &mut actives, "the device needs a reset");
                } else if st & virtio::STATUS_DRIVER_OK == 0 {
                    recover(hw, c, ci, &mut actives, "the device was reset (DRIVER_OK lost)");
                } else if let Some(t0) = oldest {
                    if now.wrapping_sub(t0) > hz * CMD_TIMEOUT_MS / 1000 {
                        recover(hw, c, ci, &mut actives, "a command timed out");
                    }
                }
            }

            // AbortIO's flags.
            for a in actives.iter_mut() {
                if !a.aborted && c.pipe.aborted(a.io) {
                    a.aborted = true;
                }
            }

            // STOP (the last close): nothing new; let what is out finish.
            if c.worker.stop_requested() {
                for io in c.pipe.take_all_pending() {
                    checked_reply(io, IOERR_ABORTED, 0, "io@3");
                }
                let since = *stop_since.get_or_insert(arch::timebase());
                if hw.in_flight() > 0 && arch::timebase().wrapping_sub(since) > hz * STOP_DRAIN_MS / 1000 {
                    for a in actives.iter_mut() {
                        a.aborted = true;
                        a.retries = MAX_RETRIES;
                    }
                    recover(hw, c, ci, &mut actives, "stopping with commands out");
                }
                for a in actives.iter_mut() {
                    a.aborted = true;
                }
            } else {
                // New requests.
                while actives.len() < MAX_ACTIVE {
                    let Some(io) = c.pipe.take() else { break };
                    #[cfg(not(feature = "dropin"))]
                    if io.command() == VSCSI_CMD_TESTFAULT {
                        // Behind the driver's back: the next tick must notice.
                        hw.t.set_status(0);
                        c.pipe.finish(io);
                        checked_reply(io, 0, 0, "io@4");
                        continue;
                    }
                    match prepare(hw, c, io) {
                        Ok(a) => actives.push(a),
                        Err((e, n)) => {
                            c.pipe.finish(io);
                            c.requests.fetch_add(1, Ordering::Relaxed);
                            checked_reply(io, e, n, "io@5");
                        }
                    }
                }
            }

            // Send commands while there are free slots, oldest request first.
            if !hw.dead {
                'send: for ri in 0..actives.len() {
                    while !actives[ri].aborted && actives[ri].error == 0 && (!actives[ri].redo.is_empty() || actives[ri].next < actives[ri].plan.len()) {
                        let Some(slot) = hw.free_slot() else { break 'send };
                        send_next(hw, c, &mut actives, ri, slot);
                    }
                }
            } else {
                for a in actives.iter_mut() {
                    if a.error == 0 {
                        a.error = TDERR_NOT_SPECIFIED;
                    }
                }
            }

            // Reply the finished ones. Slot indices name requests by
            // position, so remove from the back and renumber the slots.
            let mut ri = actives.len();
            while ri > 0 {
                ri -= 1;
                if actives[ri].finished() {
                    let a = actives.remove(ri);
                    for s in hw.using.iter_mut().flatten() {
                        if s.req > ri {
                            s.req -= 1;
                        }
                    }
                    reply(c, a);
                }
            }

            if c.worker.stop_requested() && actives.is_empty() && hw.in_flight() == 0 {
                break;
            }
        }

        // Interrupts off and the line dropped BEFORE the server goes; the
        // device stays set up (and idle) for the next start.
        c.worker.withdraw();
        hw.reqq.ring().set_interrupts(false);
        let _ = hw.t.read_isr();
        drop(server); // RemIntServer
        c.irq_task.store(null_mut(), Ordering::Release);
        c.irq_mask.store(0, Ordering::Release);
        c.regs_ready.store(0, Ordering::Release);
        timer.abort();
        drop(timer);
        sys::exec_free_signal(irq_sig);
        sys::exec_free_signal(work_sig);
        finish(c);
    }
}

// ---------------------------------------------------------------------------
// Units
// ---------------------------------------------------------------------------

/// exec's `struct Unit` (exec/devices.h): a MsgPort, flags, a pad byte and
/// the open count, 38 bytes. SFS checks that it is set up as a port.
#[repr(C, packed(2))]
struct ExecUnit {
    // struct Node
    ln_succ: u32,
    ln_pred: u32,
    ln_type: u8,
    ln_pri: i8,
    ln_name: u32,
    // struct MsgPort
    mp_flags: u8,
    mp_sigbit: u8,
    mp_sigtask: u32,
    // struct List mp_MsgList (at 20)
    lh_head: u32,
    lh_tail: u32,
    lh_tailpred: u32,
    lh_type: u8,
    l_pad: u8,
    unit_flags: u8,
    unit_pad: u8,
    unit_opencnt: u16,
}
const _: () = assert!(core::mem::size_of::<ExecUnit>() == 38);
const NT_MESSAGE: u8 = 5;
const NT_MSGPORT: u8 = 4;
const PA_IGNORE: u8 = 2;
const UNITF_ACTIVE: u8 = 1;

const TYPE_DISK: u8 = 0;
const TYPE_CDROM: u8 = 5;

/// One disk or CD-ROM. `unit` must stay first: io_Unit points here.
#[repr(C)]
struct DevUnit {
    unit: ExecUnit,
    index: usize,
    controller: usize,
    target: u8,
    lun: u16,
    dev_type: u8,
    removable: bool,
    vendor: [u8; 8],
    product: [u8; 16],
    revision: [u8; 4],
    /// Blocks and block size (0 when there is no medium).
    blocks: u64,
    block_size: u32,
    opens: AtomicU32,
    /// TD_ADDCHANGEINT / TD_REMOVE requests, held (never replied) until
    /// TD_REMCHANGEINT or AbortIO. Under CHANGE_LOCK.
    held: UnsafeCell<Vec<IoReq>>,
    change_count: AtomicU32,
}

impl DevUnit {
    fn new(index: usize, controller: usize, target: u8, lun: u16) -> Box<DevUnit> {
        let mut u = Box::new(DevUnit {
            unit: ExecUnit {
                ln_succ: 0, ln_pred: 0, ln_type: NT_MSGPORT, ln_pri: 0, ln_name: 0,
                mp_flags: PA_IGNORE, mp_sigbit: 0, mp_sigtask: 0,
                lh_head: 0, lh_tail: 0, lh_tailpred: 0, lh_type: NT_MESSAGE, l_pad: 0,
                unit_flags: UNITF_ACTIVE, unit_pad: 0, unit_opencnt: 0,
            },
            index, controller, target, lun,
            dev_type: 0, removable: false,
            vendor: [b' '; 8], product: [b' '; 16], revision: [b' '; 4],
            blocks: 0, block_size: 0,
            opens: AtomicU32::new(0),
            held: UnsafeCell::new(Vec::new()),
            change_count: AtomicU32::new(0),
        });
        // An empty list: lh_Head points at lh_Tail, lh_TailPred at lh_Head.
        let list = &mut u.unit as *mut ExecUnit as usize + 20;
        u.unit.lh_head = (list + 4) as u32;
        u.unit.lh_tailpred = list as u32;
        u
    }

    fn text(b: &[u8]) -> &str {
        core::str::from_utf8(b).unwrap_or("?").trim_end()
    }

    /// TD_GETGEOMETRY's CHS (the C driver's, cmd_td_getgeometry.c:60-95):
    /// CylSectors is the largest power of two dividing the block count, at
    /// most 256, so Cylinders x CylSectors is exactly the block count;
    /// Heads = CylSectors, TrackSectors = 1. (cylinders, cyl_sectors)
    fn chs(&self) -> (u64, u32) {
        if self.blocks == 0 {
            return (0, 1);
        }
        let mut cs = 1u32;
        while cs < 256 && self.blocks % (cs as u64 * 2) == 0 {
            cs *= 2;
        }
        (self.blocks / cs as u64, cs)
    }
}

/// Serialises the units' held change requests.
static CHANGE_LOCK: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

unsafe fn with_change_lock<R>(f: impl FnOnce() -> R) -> R {
    let m = CHANGE_LOCK.load(Ordering::Acquire);
    if !m.is_null() {
        sys::exec_mutex_obtain(m as sys::APTR);
    }
    let r = f();
    if !m.is_null() {
        sys::exec_mutex_release(m as sys::APTR);
    }
    r
}

/// Answer TD_GETGEOMETRY into io_Data: (io_Error, io_Actual).
unsafe fn geometry(u: &DevUnit, io: IoReq) -> (i8, u32) {
    let d = io.data();
    if d.is_null() || io.length() < DG_SIZE {
        return (IOERR_BADLENGTH, 0);
    }
    core::ptr::write_bytes(d, 0, DG_SIZE as usize); // SFS checks dg_Reserved
    let (cyl, cs) = u.chs();
    let put = |off: usize, v: u32| core::ptr::write_unaligned(d.add(off) as *mut u32, v.to_be());
    put(0, u.block_size);
    put(4, u.blocks.min(u32::MAX as u64) as u32); // diskboot skips 0: clamp, never wrap
    put(8, cyl.min(u32::MAX as u64) as u32);
    put(12, cs);
    put(16, cs);
    put(20, 1);
    put(24, BUF_MEM_TYPE);
    *d.add(28) = u.dev_type;
    *d.add(29) = if u.removable { DGF_REMOVABLE } else { 0 };
    (0, DG_SIZE)
}

/// Answer NSCMD_TD_GETGEOMETRY64 into io_Data.
unsafe fn geometry64(u: &DevUnit, io: IoReq) -> (i8, u32) {
    let d = io.data();
    if d.is_null() || io.length() < DG64_SIZE {
        return (IOERR_BADLENGTH, 0);
    }
    core::ptr::write_bytes(d, 0, DG64_SIZE as usize);
    core::ptr::write_unaligned(d as *mut u32, u.block_size.to_be());
    core::ptr::write_unaligned(d.add(16) as *mut u64, u.blocks.to_be());
    *d.add(28) = u.dev_type;
    *d.add(29) = if u.removable { DGF_REMOVABLE } else { 0 };
    (0, DG64_SIZE)
}

struct Units(UnsafeCell<Vec<Box<DevUnit>>>);
// SAFETY: filled by Init before any Open, emptied by Expunge after the last Close.
unsafe impl Sync for Units {}
static UNITS: Units = Units(UnsafeCell::new(Vec::new()));

fn units() -> &'static Vec<Box<DevUnit>> {
    // SAFETY: see Units.
    unsafe { &*UNITS.0.get() }
}

fn unit_of(ior: IoReq) -> Option<&'static DevUnit> {
    let p = ior.unit() as *const DevUnit;
    units().iter().map(|u| &**u).find(|u| core::ptr::eq(*u, p))
}

/// INQUIRY every target and LUN of one controller; READ CAPACITY each one
/// found. False when the controller stopped answering.
unsafe fn scan(ci: usize, c: &mut Controller, out: &mut Vec<Box<DevUnit>>) -> bool {
    let targets = SCAN_TARGETS.min(c.cfg.max_target.saturating_add(1));
    let luns = SCAN_LUNS.min(c.cfg.max_lun.saturating_add(1).min(u16::MAX as u32) as u16);
    for target in 0..targets {
        for lun in 0..luns {
            if out.len() == MAX_UNITS {
                serial_println!("{} controller {}: {} units already; scan stopped", NAME, ci, MAX_UNITS);
                return true;
            }
            let inquiry = [0x12u8, 0, 0, 0, 36, 0];
            let Some((o, d)) = c.small_in(target as u8, lun, &inquiry, 36) else {
                serial_println!("{} controller {}: target {} lun {}: INQUIRY timed out", NAME, ci, target, lun);
                return false;
            };
            if o.response == vs::S_BAD_TARGET {
                break; // no such target: no LUNs either
            }
            if !o.good() || d.len() < 36 {
                continue;
            }
            let qualifier = d[0] >> 5;
            let dev_type = d[0] & 0x1f;
            if qualifier != 0 || (dev_type != TYPE_DISK && dev_type != TYPE_CDROM) {
                continue;
            }
            let mut u = DevUnit::new(out.len(), ci, target as u8, lun);
            u.dev_type = dev_type;
            u.removable = d[1] & 0x80 != 0;
            u.vendor.copy_from_slice(&d[8..16]);
            u.product.copy_from_slice(&d[16..32]);
            u.revision.copy_from_slice(&d[32..36]);
            let (blocks, bs) = capacity(c, target as u8, lun);
            u.blocks = blocks;
            u.block_size = bs;
            serial_println!(
                "{} unit {}: controller {} target {} lun {}: {}{} {} {} {}, {}",
                NAME, u.index, ci, target, lun,
                if dev_type == TYPE_DISK { "disk" } else { "CD-ROM" },
                if u.removable { " (removable)" } else { "" },
                DevUnit::text(&u.vendor), DevUnit::text(&u.product), DevUnit::text(&u.revision),
                if blocks == 0 {
                    String::from("no medium")
                } else {
                    alloc::format!("{} blocks of {} bytes ({} MB)", blocks, bs, blocks * bs as u64 / (1024 * 1024))
                }
            );
            out.push(u);
        }
    }
    true
}

/// (blocks, block size), or (0, 0) without a medium. READ CAPACITY(10);
/// when its last LBA is 0xFFFFFFFF (2 TiB or more), READ CAPACITY(16).
unsafe fn capacity(c: &mut Controller, target: u8, lun: u16) -> (u64, u32) {
    let rc10 = [0x25u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let Some((o, d)) = c.small_in(target, lun, &rc10, 8) else { return (0, 0) };
    if !o.good() || d.len() < 8 {
        return (0, 0);
    }
    let last = u32::from_be_bytes([d[0], d[1], d[2], d[3]]);
    let bs = u32::from_be_bytes([d[4], d[5], d[6], d[7]]);
    if last != u32::MAX {
        return (last as u64 + 1, bs);
    }
    // SERVICE ACTION IN(16) / READ CAPACITY(16), allocation length 32.
    let mut rc16 = [0u8; 16];
    rc16[0] = 0x9e;
    rc16[1] = 0x10;
    rc16[13] = 32;
    let Some((o, d)) = c.small_in(target, lun, &rc16, 32) else { return (0, 0) };
    if !o.good() || d.len() < 12 {
        return (0, 0);
    }
    let last = u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]);
    let bs = u32::from_be_bytes([d[8], d[9], d[10], d[11]]);
    (last + 1, bs)
}

/// The quick commands, answered in BeginIO: Some((io_Error, io_Actual)).
unsafe fn quick(u: &DevUnit, io: IoReq) -> Option<(i8, u32)> {
    Some(match io.command() {
        // No write cache to flush: QEMU writes through (the C driver sends
        // no SYNCHRONIZE CACHE either).
        CMD_UPDATE | ETD_UPDATE | CMD_FLUSH => (0, 0),
        CMD_CLEAR | ETD_CLEAR | TD_SEEK | ETD_SEEK | TD_SEEK64 | NSCMD_TD_SEEK64 | NSCMD_ETD_SEEK64 | TD_EJECT => (0, 0),
        // "The motor was on."
        CMD_START | CMD_STOP | TD_MOTOR | ETD_MOTOR => (0, 1),
        TD_CHANGENUM => (0, u.change_count.load(Ordering::Relaxed)),
        // 0: a disk is in the drive.
        TD_CHANGESTATE => (0, (u.blocks == 0) as u32),
        TD_PROTSTATUS => (0, 0),
        TD_GETDRIVETYPE => (0, DRIVE3_5),
        TD_GETNUMTRACKS => (0, u.chs().0.min(u32::MAX as u64) as u32),
        TD_GETGEOMETRY => geometry(u, io),
        NSCMD_TD_GETGEOMETRY64 => geometry64(u, io),
        NSCMD_DEVICEQUERY => answer_device_query(io, NSDEVTYPE_TRACKDISK, 0, &SUPPORTED_COMMANDS),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Device entry points
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn rust_scsi_init() -> i32 {
    // SAFETY: the init runs once per load, before any open.
    unsafe {
        let devs = find_controllers();
        if devs.is_empty() {
            serial_println!("{}: no virtio-scsi controller to claim (1af4:{:04x}); device not added", NAME, CLAIM_IDS[0]);
            return 1;
        }
        let mk = || sys::exec_alloc_sys_object(sys::ASOT_MUTEX, [TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 }].as_ptr());
        let (m1, m2) = (mk(), mk());
        if m1.is_null() || m2.is_null() {
            for m in [m1, m2] {
                if !m.is_null() {
                    sys::exec_free_sys_object(sys::ASOT_MUTEX, m);
                }
            }
            for dev in devs {
                release(dev);
            }
            return 1;
        }
        OPEN_LOCK.store(m1 as *mut c_void, Ordering::Release);
        CHANGE_LOCK.store(m2 as *mut c_void, Ordering::Release);
        let list = &mut *UNITS.0.get();
        let mut n = 0;
        for dev in devs {
            match setup(n, dev) {
                Ok(mut hw) => {
                    if !scan(n, &mut hw, list) {
                        serial_println!("{} controller {}: not answering; units found so far are kept", NAME, n);
                    }
                    // The polled commands ran with interrupts off; drop
                    // the line in case it was raised anyway.
                    let _ = hw.t.read_isr();
                    *CTLS[n].hw.get() = Some(hw);
                    n += 1;
                }
                Err(why) => {
                    serial_println!("{} controller {}: not set up: {}", NAME, n, why);
                    release(dev);
                }
            }
        }
        NUM_CTLS.store(n as u32, Ordering::Release);
        if n == 0 {
            for lock in [&OPEN_LOCK, &CHANGE_LOCK] {
                sys::exec_free_sys_object(sys::ASOT_MUTEX, lock.swap(null_mut(), Ordering::AcqRel) as sys::APTR);
            }
            return 1;
        }
        serial_println!("{}: {} controller(s), {} unit(s)", NAME, n, list.len());
    }
    0
}

#[no_mangle]
pub extern "C" fn rust_scsi_expunge() -> i32 {
    // SAFETY: exec calls Expunge with no other user of the device.
    unsafe {
        let n = NUM_CTLS.load(Ordering::Acquire) as usize;
        if units().iter().any(|u| u.opens.load(Ordering::Acquire) != 0 || !(*u.held.get()).is_empty())
            || CTLS[..n].iter().any(|c| c.running.load(Ordering::Acquire) != 0)
        {
            return 0;
        }
        for (ci, c) in CTLS[..n].iter().enumerate() {
            if c.lingering.load(Ordering::Acquire) != 0 {
                if !sys::exec_find_task(TASK_NAMES[ci].as_ptr() as _).is_null() {
                    return 0; // a stopped task's exit code still runs in this segment
                }
                c.lingering.store(0, Ordering::Release);
            }
        }
        (*UNITS.0.get()).clear();
        for c in &CTLS[..n] {
            if let Some(mut hw) = (*c.hw.get()).take() {
                // Reset before the queue memory goes: the device must not touch it.
                hw.t.set_status(0);
                let dev = hw.dev;
                drop(hw);
                release(dev);
            }
        }
        NUM_CTLS.store(0, Ordering::Release);
        for lock in [&OPEN_LOCK, &CHANGE_LOCK] {
            let m = lock.swap(null_mut(), Ordering::AcqRel);
            if !m.is_null() {
                sys::exec_free_sys_object(sys::ASOT_MUTEX, m as sys::APTR);
            }
        }
    }
    1
}

#[no_mangle]
pub extern "C" fn rust_dev_open(ior: *mut IORequest, unit: u32, _flags: u32) -> i8 {
    // SAFETY: the caller's request.
    let Some(io) = (unsafe { IoReq::from_raw(ior) }) else { return IOERR_OPENFAIL };
    let Some(u) = units().get(unit as usize) else {
        io.set_error(IOERR_OPENFAIL);
        return IOERR_OPENFAIL;
    };
    let c = &CTLS[u.controller];
    // SAFETY: start and stop are serialised by OPEN_LOCK.
    let started = unsafe {
        with_open_lock(|| {
            if c.opens.load(Ordering::Acquire) == 0 {
                if let Err(e) = start_controller(u.controller) {
                    serial_println!("{} controller {}: start FAILED: {}", NAME, u.controller, e);
                    return false;
                }
            }
            c.opens.fetch_add(1, Ordering::AcqRel);
            u.opens.fetch_add(1, Ordering::AcqRel);
            true
        })
    };
    if !started {
        io.set_error(IOERR_OPENFAIL);
        return IOERR_OPENFAIL;
    }
    let p = &**u as *const DevUnit as *mut DevUnit;
    // SAFETY: the unit lives until Expunge, which refuses while it is open.
    unsafe {
        let cnt = core::ptr::addr_of_mut!((*p).unit.unit_opencnt);
        cnt.write_unaligned(cnt.read_unaligned().wrapping_add(1));
    }
    io.set_unit(p as *mut c_void);
    io.set_error(0);
    0
}

#[no_mangle]
pub extern "C" fn rust_dev_close(ior: *mut IORequest) {
    // SAFETY: the caller's request, opened on this device.
    let Some(io) = (unsafe { IoReq::from_raw(ior) }) else { return };
    let Some(u) = unit_of(io) else { return };
    let p = u as *const DevUnit as *mut DevUnit;
    // SAFETY: as in open.
    unsafe {
        let cnt = core::ptr::addr_of_mut!((*p).unit.unit_opencnt);
        cnt.write_unaligned(cnt.read_unaligned().wrapping_sub(1));
        let c = &CTLS[u.controller];
        with_open_lock(|| {
            u.opens.fetch_sub(1, Ordering::AcqRel);
            // The drop-in (Kickstart) build keeps its tasks (see the top).
            if c.opens.fetch_sub(1, Ordering::AcqRel) == 1 && !cfg!(feature = "dropin") && !cfg!(feature = "keep-tasks") {
                if let Err(e) = stop_controller(u.controller) {
                    serial_println!("{} controller {}: stop FAILED: {}", NAME, u.controller, e);
                }
            }
        });
    }
    io.set_unit(null_mut());
}

#[no_mangle]
pub extern "C" fn rust_dev_begin_io(ior: *mut IORequest) {
    // SAFETY: the caller's request, opened on this device.
    let Some(io) = (unsafe { IoReq::from_raw(ior) }) else { return };
    unsafe {
        let Some(u) = unit_of(io) else {
            io.finish(IOERR_OPENFAIL, 0);
            return;
        };
        #[cfg(feature = "trace")]
        if !READS.contains(&io.command()) && !WRITES.contains(&io.command()) {
            serial_println!("{} unit {}: BeginIO cmd 0x{:04x} length {}", NAME, u.index, io.command(), io.length());
        }
        if let Some((e, n)) = quick(u, io) {
            io.finish(e, n);
            return;
        }
        let cmd = io.command();
        #[cfg(not(feature = "dropin"))]
        let fault = cmd == VSCSI_CMD_TESTFAULT;
        #[cfg(feature = "dropin")]
        let fault = false;
        match cmd {
            TD_ADDCHANGEINT | TD_REMOVE => {
                // Held, never replied, until TD_REMCHANGEINT or AbortIO.
                io.mark_pending();
                with_change_lock(|| (*u.held.get()).push(io));
            }
            TD_REMCHANGEINT => {
                // The same request that was added (the usual way): it comes
                // back as this command's reply. Another one: the first held
                // request is handed back too (the C driver's behaviour).
                let other = with_change_lock(|| {
                    let held = &mut *u.held.get();
                    if let Some(i) = held.iter().position(|&h| h == io) {
                        held.remove(i);
                        None
                    } else if !held.is_empty() {
                        Some(held.remove(0))
                    } else {
                        None
                    }
                });
                if let Some(h) = other {
                    checked_reply(h, 0, 0, "h@6");
                }
                io.finish(0, 0);
            }
            _ if READS.contains(&cmd) || WRITES.contains(&cmd) || cmd == HD_SCSICMD || fault => {
                // Not quick: the controller task replies. push() marks it pending.
                let c = &CTLS[u.controller];
                if !c.worker.is_ready() {
                    io.finish(IOERR_UNITBUSY, 0);
                    return;
                }
                c.pipe.push(io);
                if !c.worker.wake() && c.pipe.abort(io) == Found::Removed {
                    // Stopped between the check and the push: take it back.
                    checked_reply(io, IOERR_UNITBUSY, 0, "io@7");
                }
            }
            _ => io.finish(IOERR_NOCMD, 0),
        }
    }
}

/// AbortIO: a held change request or a queued request is replied
/// IOERR_ABORTED at once; one in progress is flagged, sends no more
/// commands, and the task replies it IOERR_ABORTED when the ones out are
/// back.
#[no_mangle]
pub extern "C" fn rust_dev_abort_io(ior: *mut IORequest) {
    // SAFETY: the caller's request; the lists take their own mutexes, and a
    // removed request is ours to reply.
    let Some(io) = (unsafe { IoReq::from_raw(ior) }) else { return };
    let Some(u) = unit_of(io) else { return };
    let was_held = unsafe {
        with_change_lock(|| {
            let held = &mut *u.held.get();
            held.iter().position(|&h| h == io).map(|i| held.remove(i)).is_some()
        })
    };
    let c = &CTLS[u.controller];
    if was_held {
        unsafe { checked_reply(io, IOERR_ABORTED, 0, "io@8") };
        return;
    }
    match c.pipe.abort(io) {
        Found::Removed => unsafe { checked_reply(io, IOERR_ABORTED, 0, "io@9") },
        Found::Flagged => {
            let _ = c.worker.wake();
        }
        Found::NotFound => {}
    }
}
