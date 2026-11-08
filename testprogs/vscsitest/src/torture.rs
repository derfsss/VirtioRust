//! vscsitest's torture mode: try to break virtioscsi.device under load.
//!
//!     vscsitest TORTURE <device> <secs> <units> [kids] [kidpri]
//!
//! `units` is a comma-separated list (e.g. 1,2,3); never the system disk or
//! a unit with a mounted filesystem: THE TORTURE WRITES TO ITS REGIONS.
//! `kids` tasks (at most 8, default 8) take the units in turn (kid i: units[i
//! % n]) at priority `kidpri` (default 0; below -2 lets a remote-control
//! shell run beside it, as the soak needs). Each kid owns an 8 MiB region of its unit,
//! from 16 MiB, or from 5 GiB on a unit larger than 6 GiB (so the 64-bit
//! commands are used); regions on one unit do not overlap.
//!
//! The oracle: every block a kid writes holds a 16-byte header (magic, unit,
//! kid, the block's number on the disk, a generation) and bytes derived from
//! it, and the kid keeps each block's generation. Every read checks every
//! block whose generation is known; at the end of a round the kid re-reads
//! its whole region. A block whose write failed (only expected while the
//! main task resets the controller) is unknown until written again.
//!
//! The work, chosen at random: writes of 1 to 256 blocks (CMD_WRITE,
//! TD_WRITE64, NSCMD_TD_WRITE64) with DoIO; reads (CMD_READ, TD_READ64,
//! NSCMD_TD_READ64) with DoIO, some into a misaligned buffer with guard
//! bytes; a SendIO read aborted at a random moment (writes are never
//! aborted: an aborted write leaves a block in one of two states); two reads
//! in flight on two requests; the quick commands the filesystems issue
//! (TD_GETGEOMETRY, TD_CHANGESTATE, TD_PROTSTATUS, NSCMD_DEVICEQUERY) and
//! an HD_SCSICMD INQUIRY; a TD_ADDCHANGEINT held across several operations
//! and then removed with TD_REMCHANGEINT; closing and opening again.
//!
//! The main task runs at priority 2, above the kids. On the development
//! build it resets each unit's controller behind the driver's back
//! (VSCSI_CMD_TESTFAULT) every 2 s; the drop-in has no such command (it
//! answers IOERR_NOCMD) and is not reset. A kid whose request has not come
//! back for 20 s is a lost request. Two rounds; free memory after the second
//! is compared with after the first, except with kidpri below 0 (other jobs
//! share the machine, as in the soak: the difference is shown, not judged).

use super::*;

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use amigaos4::sys::TagItem;

const MAX_KIDS: usize = 8;
const REGION: u64 = 8 << 20;
const MAX_RUN: u64 = 256;
const STALL_MS: u32 = 20_000;
const RESET_EVERY_MS: u32 = 2_000;
const MEMF_ANY: u32 = 0;
const GUARD: usize = 64;
const FILLB: u8 = 0xa5;
const MAGIC: u32 = 0x5653_5442; // "VSTB"
/// Generation 0: never written in this round (the old contents are unknown).
const UNKNOWN: u32 = 0;

static KID_NAMES: [&[u8]; MAX_KIDS] = [
    b"vscsitest kid 0\0", b"vscsitest kid 1\0", b"vscsitest kid 2\0", b"vscsitest kid 3\0",
    b"vscsitest kid 4\0", b"vscsitest kid 5\0", b"vscsitest kid 6\0", b"vscsitest kid 7\0",
];

static OPS: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static BAD: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static ERRS: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static BLOCKS_W: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static BLOCKS_R: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static CHECKED: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static ABORTED: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static FINAL_OK: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static DONE: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static FIRST_BAD: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static STOP: AtomicU32 = AtomicU32::new(0);
/// Resets on: an I/O error is tolerated (counted), a wrong block never is.
static RESETTING: AtomicU32 = AtomicU32::new(0);

/// Per kid, set by the main task before the kid starts: unit, block size,
/// first block of its region, blocks in it.
struct KidCfg {
    name: Vec<u8>,
    unit: u32,
    bs: u32,
    first: u64,
    blocks: u64,
}
static mut CFG: [Option<KidCfg>; MAX_KIDS] = [const { None }; MAX_KIDS];

// First-bad codes: kind * 1_000_000 + |io_Error| * 1000 + op.
const K_DATA: u32 = 1; // a block read back wrong
const K_ERROR: u32 = 2; // an io_Error not allowed
const K_GUARD: u32 = 3; // a guard byte overwritten
const K_ACTUAL: u32 = 4; // io_Actual wrong
const K_OPEN: u32 = 5; // OpenDevice failed
const K_SETUP: u32 = 6; // no port, timer, task or memory

fn bad_text(code: u32) -> String {
    let what = match code / 1_000_000 {
        K_DATA => "a block read back wrong",
        K_ERROR => "an io_Error not allowed",
        K_GUARD => "a guard byte overwritten",
        K_ACTUAL => "io_Actual wrong",
        K_OPEN => "OpenDevice failed",
        K_SETUP => "setup failed",
        _ => "?",
    };
    alloc::format!("{} (io_Error -{}, op {})", what, (code / 1000) % 1000, code % 1000)
}

fn note_bad(i: usize, kind: u32, err: i8, op: u32) {
    BAD[i].fetch_add(1, Ordering::Relaxed);
    let code = kind * 1_000_000 + (err as i32).unsigned_abs().min(999) * 1000 + op.min(999);
    let _ = FIRST_BAD[i].compare_exchange(0, code, Ordering::Relaxed, Ordering::Relaxed);
}

/// An I/O error: tolerated (counted) while the controller is being reset,
/// a failure otherwise.
fn io_error(i: usize, e: i8, op: u32) {
    ERRS[i].fetch_add(1, Ordering::Relaxed);
    if RESETTING.load(Ordering::Relaxed) == 0 {
        note_bad(i, K_ERROR, e, op);
    }
}

fn avail() -> u32 {
    // SAFETY: a plain exec query.
    unsafe { sys::exec_avail_mem(MEMF_ANY) }
}

struct Rand(u32);
impl Rand {
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() as u64 % n.max(1)
    }
}

/// The contents of disk block `abs` of `unit` at generation `gen`, written by
/// kid `kid`: a header, then bytes derived from it.
fn fill_block(b: &mut [u8], unit: u32, kid: u32, abs: u64, gen: u32) {
    b[0..4].copy_from_slice(&MAGIC.to_be_bytes());
    b[4] = unit as u8;
    b[5] = kid as u8;
    b[6..8].copy_from_slice(&0u16.to_be_bytes());
    b[8..12].copy_from_slice(&(abs as u32).to_be_bytes());
    b[12..16].copy_from_slice(&gen.to_be_bytes());
    let mut x = (abs as u32).wrapping_mul(0x9e37_79b1) ^ gen.wrapping_mul(0x85eb_ca6b) ^ (unit << 24) ^ 1;
    for chunk in b[16..].chunks_mut(4) {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let w = x.to_be_bytes();
        chunk.copy_from_slice(&w[..chunk.len()]);
    }
}

fn block_ok(b: &[u8], unit: u32, kid: u32, abs: u64, gen: u32, scratch: &mut [u8]) -> bool {
    fill_block(scratch, unit, kid, abs, gen);
    b == &scratch[..b.len()]
}

/// A buffer with guard bytes either side, at a chosen misalignment.
struct Guarded {
    v: Vec<u8>,
    off: usize,
    len: usize,
}
impl Guarded {
    fn new(len: usize, skew: usize) -> Self {
        Self { v: vec![FILLB; len + 2 * GUARD + 8], off: GUARD + skew, len }
    }
    fn ptr(&mut self) -> *mut u8 {
        self.v[self.off..].as_mut_ptr()
    }
    fn data(&self) -> &[u8] {
        &self.v[self.off..self.off + self.len]
    }
    fn guards_ok(&self) -> bool {
        self.v[..self.off].iter().all(|&x| x == FILLB) && self.v[self.off + self.len..].iter().all(|&x| x == FILLB)
    }
}

/// SendIO at a 64-bit byte offset with the 64-bit command.
fn send64(r: &Req, cmd: u16, data: *mut u8, len: u32, offset: u64) {
    let q = r.io();
    q.set_command(cmd);
    q.set_data(data);
    q.set_length(len);
    q.set_offset(offset as u32);
    q.set_actual((offset >> 32) as u32);
    // SAFETY: an open request.
    unsafe { sys::exec_send_io(r.0) };
}

struct Kid {
    i: usize,
    unit: u32,
    bs: u32,
    first: u64,
    blocks: u64,
    gens: Vec<u32>,
    r: Rand,
    scratch: Vec<u8>,
}

impl Kid {
    fn abs(&self, b: u64) -> u64 {
        self.first + b
    }
    /// A command for a read or write at block `b`: the 64-bit ones are
    /// needed above 4 GiB, and used at random below it.
    fn pick(&mut self, b: u64, write: bool) -> u16 {
        let above = self.abs(b) * self.bs as u64 >= 1 << 32;
        let k = self.r.below(3);
        match (write, above || k == 0, k) {
            (true, true, 1) | (true, true, 0) => NSCMD_TD_WRITE64,
            (true, true, _) => TD_WRITE64,
            (true, false, _) => CMD_WRITE,
            (false, true, 1) | (false, true, 0) => NSCMD_TD_READ64,
            (false, true, _) => TD_READ64,
            (false, false, _) => CMD_READ,
        }
    }

    /// Check `n` blocks from region block `b` in `data` against their
    /// generations; counts and notes a wrong one.
    fn verify(&mut self, data: &[u8], b: u64, n: u64, op: u32) -> bool {
        let bs = self.bs as usize;
        let mut ok = true;
        for k in 0..n {
            let gen = self.gens[(b + k) as usize];
            if gen == UNKNOWN {
                continue;
            }
            let abs = self.abs(b + k);
            let blk = &data[k as usize * bs..(k as usize + 1) * bs];
            let mut s = core::mem::take(&mut self.scratch);
            let good = block_ok(blk, self.unit, self.i as u32, abs, gen, &mut s);
            self.scratch = s;
            CHECKED[self.i].fetch_add(1, Ordering::Relaxed);
            if !good {
                if ok {
                    note_bad(self.i, K_DATA, 0, op);
                }
                ok = false;
            }
        }
        ok
    }

    fn write_run(&mut self, a: &Req, op: u32) {
        let n = 1 + self.r.below(MAX_RUN.min(self.blocks));
        let b = self.r.below(self.blocks - n + 1);
        let bs = self.bs as usize;
        let mut buf = vec![0u8; n as usize * bs];
        let mut newgens = Vec::with_capacity(n as usize);
        for k in 0..n {
            let gen = self.gens[(b + k) as usize].wrapping_add(1).max(1);
            fill_block(&mut buf[k as usize * bs..(k as usize + 1) * bs], self.unit, self.i as u32, self.abs(b + k), gen);
            newgens.push(gen);
        }
        let cmd = self.pick(b, true);
        let (e, got) = a.do_io(cmd, buf.as_mut_ptr(), buf.len() as u32, self.abs(b) * self.bs as u64);
        if e == 0 && got == buf.len() as u32 {
            for k in 0..n {
                self.gens[(b + k) as usize] = newgens[k as usize];
            }
            BLOCKS_W[self.i].fetch_add(n as u32, Ordering::Relaxed);
        } else {
            // What reached the disk is not known: these blocks are unknown
            // until written again.
            for k in 0..n {
                self.gens[(b + k) as usize] = UNKNOWN;
            }
            if e == 0 {
                note_bad(self.i, K_ACTUAL, 0, op);
            } else {
                io_error(self.i, e, op);
            }
        }
    }

    fn read_run(&mut self, a: &Req, op: u32, skew: usize) {
        let n = 1 + self.r.below(MAX_RUN.min(self.blocks));
        let b = self.r.below(self.blocks - n + 1);
        let len = n as usize * self.bs as usize;
        let mut g = Guarded::new(len, skew);
        let cmd = self.pick(b, false);
        let (e, got) = a.do_io(cmd, g.ptr(), len as u32, self.abs(b) * self.bs as u64);
        if !g.guards_ok() {
            note_bad(self.i, K_GUARD, e, op);
        }
        if e != 0 {
            io_error(self.i, e, op);
            return;
        }
        if got != len as u32 {
            note_bad(self.i, K_ACTUAL, 0, op);
            return;
        }
        BLOCKS_R[self.i].fetch_add(n as u32, Ordering::Relaxed);
        let data = g.data().to_vec();
        self.verify(&data, b, n, op);
    }

    /// A SendIO read, aborted now, after a moment, or not at all.
    fn abort_read(&mut self, a: &Req, timer: &mut AmigaTimer, op: u32) {
        let n = 1 + self.r.below(MAX_RUN.min(self.blocks));
        let b = self.r.below(self.blocks - n + 1);
        let len = n as usize * self.bs as usize;
        let mut g = Guarded::new(len, self.r.below(4) as usize);
        send64(a, TD_READ64, g.ptr(), len as u32, self.abs(b) * self.bs as u64);
        match self.r.below(3) {
            0 => a.abort(),
            1 => {
                let _ = timer.delay(TimerVal::from_millis(self.r.below(3) as u32));
                a.abort();
            }
            _ => {}
        }
        let (e, got) = a.wait();
        if !g.guards_ok() {
            note_bad(self.i, K_GUARD, e, op);
        }
        match e {
            0 => {
                if got != len as u32 {
                    note_bad(self.i, K_ACTUAL, 0, op);
                    return;
                }
                let data = g.data().to_vec();
                self.verify(&data, b, n, op);
            }
            IOERR_ABORTED => {
                ABORTED[self.i].fetch_add(1, Ordering::Relaxed);
            }
            e => io_error(self.i, e, op),
        }
    }

    /// Two reads in flight on two requests, then both checked.
    fn two_reads(&mut self, a: &Req, b2: &Req, op: u32) {
        let mut parts = Vec::new();
        for _ in 0..2 {
            let n = 1 + self.r.below(64.min(self.blocks));
            let b = self.r.below(self.blocks - n + 1);
            parts.push((b, n, Guarded::new(n as usize * self.bs as usize, 0)));
        }
        let bs = self.bs as u64;
        let first = self.first;
        {
            let (x, y) = parts.split_at_mut(1);
            send64(a, TD_READ64, x[0].2.ptr(), x[0].2.len as u32, (first + x[0].0) * bs);
            send64(b2, TD_READ64, y[0].2.ptr(), y[0].2.len as u32, (first + y[0].0) * bs);
        }
        let res = [a.wait(), b2.wait()];
        for (k, (b, n, g)) in parts.iter().enumerate() {
            let (e, got) = res[k];
            if !g.guards_ok() {
                note_bad(self.i, K_GUARD, e, op);
            }
            if e != 0 {
                io_error(self.i, e, op);
                continue;
            }
            if got != g.len as u32 {
                note_bad(self.i, K_ACTUAL, 0, op);
                continue;
            }
            let data = g.data().to_vec();
            self.verify(&data, *b, *n, op);
        }
    }

    fn quick(&mut self, a: &Req, op: u32) {
        let mut g = [0u8; 32];
        let checks: [(u16, u32); 3] = [(TD_GETGEOMETRY, 32), (TD_CHANGESTATE, 0), (TD_PROTSTATUS, 0)];
        let (cmd, len) = checks[self.r.below(3) as usize];
        let (e, n) = a.do_io(cmd, if len > 0 { g.as_mut_ptr() } else { core::ptr::null_mut() }, len, 0);
        if e != 0 {
            io_error(self.i, e, op);
        } else if (cmd == TD_GETGEOMETRY && (n != 32 || be32(&g, 0) != self.bs)) || (cmd == TD_CHANGESTATE && n != 0) {
            note_bad(self.i, K_ACTUAL, 0, op);
        }
        let (e, _, _, _, _) = device_query(a);
        if e != 0 {
            io_error(self.i, e, op);
        }
        let mut inq = [0u8; 36];
        let (e, st, n, _, _) = a.scsi(&[0x12, 0, 0, 0, 36, 0], &mut inq, true);
        if e != 0 || st != 0 {
            io_error(self.i, if e != 0 { e } else { -1 }, op);
        } else if n != 36 || &inq[8..12] != b"QEMU" {
            note_bad(self.i, K_ACTUAL, 0, op);
        }
    }

    /// Re-read the whole region and check every known block.
    fn final_check(&mut self, a: &Req) -> bool {
        let per = 128u64.min(self.blocks);
        let mut b = 0;
        let mut ok = true;
        while b < self.blocks {
            let n = per.min(self.blocks - b);
            let len = n as usize * self.bs as usize;
            let mut buf = vec![0u8; len];
            let (e, got) = a.do_io(TD_READ64, buf.as_mut_ptr(), len as u32, self.abs(b) * self.bs as u64);
            if e != 0 || got != len as u32 {
                // One retry: a reset may have been in flight.
                let (e2, got2) = a.do_io(TD_READ64, buf.as_mut_ptr(), len as u32, self.abs(b) * self.bs as u64);
                if e2 != 0 || got2 != len as u32 {
                    note_bad(self.i, K_ERROR, e2, 900);
                    return false;
                }
            }
            ok &= self.verify(&buf, b, n, 900);
            b += n;
        }
        ok
    }
}

extern "C" fn kid_entry(i: u32) {
    let i = i as usize;
    kid_body(i);
    DONE[i].store(1, Ordering::Release);
}

fn kid_body(i: usize) {
    // SAFETY: set by the main task before this kid was created, then only read.
    let Some(cfg) = (unsafe { (*core::ptr::addr_of!(CFG))[i].as_ref() }) else {
        note_bad(i, K_SETUP, 0, 0);
        return;
    };
    let (Ok(port), Ok(mut timer)) = (AmigaMsgPort::new(), AmigaTimer::open(TimerUnit::MicroHz)) else {
        note_bad(i, K_SETUP, 0, 0);
        return;
    };
    let (Ok(mut a), Ok(b2), Ok(chg)) = (
        Req::open(&port, &cfg.name, cfg.unit),
        Req::open(&port, &cfg.name, cfg.unit),
        Req::open(&port, &cfg.name, cfg.unit),
    ) else {
        note_bad(i, K_OPEN, 0, 0);
        return;
    };
    let mut kid = Kid {
        i,
        unit: cfg.unit,
        bs: cfg.bs,
        first: cfg.first,
        blocks: cfg.blocks,
        gens: vec![UNKNOWN; cfg.blocks as usize],
        r: Rand(0x2545_f491 ^ ((i as u32 + 1).wrapping_mul(0x9e37_79b9)) ^ amigaos4::arch::timebase() as u32),
        scratch: vec![0u8; cfg.bs as usize],
    };
    let mut held = false;
    while STOP.load(Ordering::Acquire) == 0 {
        let op = kid.r.below(20) as u32;
        match op {
            0..=6 => kid.write_run(&a, op),
            7..=10 => kid.read_run(&a, op, 0),
            11 => {
                let skew = 1 + kid.r.below(3) as usize;
                kid.read_run(&a, op, skew)
            }
            12..=13 => kid.abort_read(&a, &mut timer, op),
            14 => kid.two_reads(&a, &b2, op),
            15..=16 => kid.quick(&a, op),
            17 => {
                // A change request held across the next operations, removed later.
                if !held {
                    chg.send(TD_ADDCHANGEINT);
                    held = true;
                } else {
                    let (e, _) = chg.do_io(TD_REMCHANGEINT, core::ptr::null_mut(), 0, 0);
                    if e != 0 {
                        io_error(i, e, op);
                    }
                    held = false;
                }
            }
            18 => {
                // Close and open again (the held change request is on another opener).
                drop(a);
                match Req::open(&port, &cfg.name, cfg.unit) {
                    Ok(x) => a = x,
                    Err(e) => {
                        note_bad(i, K_OPEN, e, op);
                        if held {
                            let _ = chg.do_io(TD_REMCHANGEINT, core::ptr::null_mut(), 0, 0);
                        }
                        return;
                    }
                }
            }
            _ => kid.read_run(&a, op, 0),
        }
        OPS[i].fetch_add(1, Ordering::Relaxed);
    }
    if held {
        let _ = chg.do_io(TD_REMCHANGEINT, core::ptr::null_mut(), 0, 0);
    }
    if kid.final_check(&a) {
        FINAL_OK[i].store(1, Ordering::Relaxed);
    }
}

/// One round. Returns (kids that stalled, kids still there, resets sent, resets done).
fn round(timer: &mut AmigaTimer, ctls: &[Req], resets: bool, secs: u32, kids: usize, kidpri: i32) -> (u32, u32, u32, u32) {
    STOP.store(0, Ordering::Release);
    RESETTING.store(resets as u32, Ordering::Release);
    for i in 0..kids {
        for x in [&OPS[i], &BAD[i], &ERRS[i], &BLOCKS_W[i], &BLOCKS_R[i], &CHECKED[i], &ABORTED[i], &FINAL_OK[i], &DONE[i], &FIRST_BAD[i]] {
            x.store(0, Ordering::Relaxed);
        }
        let tags = [
            TagItem { ti_Tag: sys::AT_PARAM1, ti_Data: i as u32 },
            TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
        ];
        // SAFETY: an exec task (not a DOS process); it ends when STOP is set.
        let t = unsafe { sys::exec_create_task(KID_NAMES[i].as_ptr() as _, kidpri, kid_entry as *const c_void, 65536, tags.as_ptr()) };
        if t.is_null() {
            note_bad(i, K_SETUP, 0, 0);
            DONE[i].store(1, Ordering::Release);
        }
    }
    let mut last = [0u32; MAX_KIDS];
    let mut still = [0u32; MAX_KIDS];
    let mut stalled = [false; MAX_KIDS];
    let (mut sent, mut done) = (0u32, 0u32);
    let step = 250u32;
    let (mut t, mut since) = (0u32, 0u32);
    while t < secs * 1000 {
        let _ = timer.delay(TimerVal::from_millis(step));
        t += step;
        since += step;
        if resets && since >= RESET_EVERY_MS {
            since = 0;
            for c in ctls {
                sent += 1;
                if c.do_io(VSCSI_CMD_TESTFAULT, core::ptr::null_mut(), 0, 0).0 == 0 {
                    done += 1;
                }
            }
        }
        for i in 0..kids {
            let o = OPS[i].load(Ordering::Relaxed);
            if o == last[i] && DONE[i].load(Ordering::Acquire) == 0 {
                still[i] += step;
                if still[i] >= STALL_MS && !stalled[i] {
                    stalled[i] = true;
                    say!("  FAIL watchdog: kid {} has had a request out for {} s (operations so far {})", i, still[i] / 1000, o);
                }
            } else {
                still[i] = 0;
                last[i] = o;
            }
        }
    }
    STOP.store(1, Ordering::Release);
    // The kids finish their operation and re-read their regions.
    let mut waited = 0;
    while (0..kids).any(|i| DONE[i].load(Ordering::Acquire) == 0) && waited < 120_000 {
        let _ = timer.delay(TimerVal::from_millis(100));
        waited += 100;
    }
    RESETTING.store(0, Ordering::Release);
    let mut left = 0;
    for i in 0..kids {
        let mut gone = false;
        for _ in 0..300 {
            // SAFETY: FindTask by name.
            if unsafe { sys::exec_find_task(KID_NAMES[i].as_ptr() as _) }.is_null() {
                gone = true;
                break;
            }
            let _ = timer.delay(TimerVal::from_millis(10));
        }
        if !gone {
            left += 1;
        }
    }
    (stalled.iter().filter(|&&s| s).count() as u32, left, sent, done)
}

pub fn torture(name: &[u8], secs: u32, units: &[u32], kids: usize, kidpri: i32) -> i32 {
    let kids = kids.clamp(1, MAX_KIDS);
    let shown = core::str::from_utf8(&name[..name.len() - 1]).unwrap_or("?");
    say!("vscsitest torture: {} units {:?}, {} kids at priority {}, {} s per round, two rounds", shown, units, kids, kidpri, secs);
    let (Ok(port), Ok(mut timer)) = (AmigaMsgPort::new(), AmigaTimer::open(TimerUnit::MicroHz)) else {
        say!("vscsitest torture: no message port or timer");
        return 20;
    };
    // One control opener per unit: geometry now, resets and the after-read
    // later; it keeps the units open across both rounds.
    let mut ctls = Vec::new();
    let mut geo = Vec::new();
    for &u in units {
        let r = match Req::open(&port, name, u) {
            Ok(r) => r,
            Err(e) => {
                say!("vscsitest torture: unit {}: OpenDevice answered {}", u, e);
                return 20;
            }
        };
        match geometry(&r) {
            Ok(Some((bs, blocks))) => geo.push((bs, blocks)),
            Ok(None) => {
                say!("vscsitest torture: unit {} has no medium", u);
                return 20;
            }
            Err(why) => {
                say!("vscsitest torture: unit {}: {}", u, why);
                return 20;
            }
        }
        ctls.push(r);
    }
    // The regions: kid i on units[i % n], the (i / n)th region there.
    for i in 0..kids {
        let k = i % units.len();
        let (bs, blocks) = geo[k];
        let size = blocks * bs as u64;
        let base = if size > 6 << 30 { 5u64 << 30 } else { 16u64 << 20 };
        let first = (base + (i / units.len()) as u64 * REGION) / bs as u64;
        let n = REGION / bs as u64;
        if (first + n) * bs as u64 > size {
            say!("vscsitest torture: unit {} is too small for kid {}'s region", units[k], i);
            return 20;
        }
        // SAFETY: no kid runs yet.
        unsafe {
            (*core::ptr::addr_of_mut!(CFG))[i] = Some(KidCfg { name: name.to_vec(), unit: units[k], bs, first, blocks: n });
        }
        say!("  kid {}: unit {}, {}-byte blocks {}..{} (byte {} on)", i, units[k], bs, first, first + n - 1, first * bs as u64);
    }
    // Resets only where the development build answers its private command.
    let resets = ctls[0].do_io(VSCSI_CMD_TESTFAULT, core::ptr::null_mut(), 0, 0).0 == 0;
    say!("  controller resets every {} s: {}", RESET_EVERY_MS / 1000,
        if resets { "on (development build)" } else { "off (no VSCSI_CMD_TESTFAULT: the drop-in)" });
    if resets {
        // Let the probe's recovery finish before the kids start.
        let _ = timer.delay(TimerVal::from_millis(1500));
    }
    // Above the kids, so the watchdog and the resets run.
    // SAFETY: this task; the old priority is restored before returning.
    let me = unsafe { sys::exec_find_task(core::ptr::null_mut()) };
    let old_pri = unsafe { sys::exec_set_task_pri(me, 2) };

    let mut t = Tally { passed: 0, total: 0 };
    let mut mem = [0u32; 2];
    for rd in 0..2 {
        let (lost, left, sent, done) = round(&mut timer, &ctls, resets, secs, kids, kidpri);
        let sum = |a: &[AtomicU32; MAX_KIDS]| (0..kids).map(|i| a[i].load(Ordering::Relaxed) as u64).sum::<u64>();
        let (ops, bad, errs) = (sum(&OPS), sum(&BAD), sum(&ERRS));
        let (bw, br, chk, ab) = (sum(&BLOCKS_W), sum(&BLOCKS_R), sum(&CHECKED), sum(&ABORTED));
        let finals = (0..kids).filter(|&i| FINAL_OK[i].load(Ordering::Relaxed) != 0).count();
        let mut first = String::new();
        for i in 0..kids {
            let c = FIRST_BAD[i].load(Ordering::Relaxed);
            if c != 0 {
                let _ = write!(first, "; kid {}: {}", i, bad_text(c));
            }
        }
        t.check(bad == 0 && ops > 0, &alloc::format!("round{}-answers", rd + 1), alloc::format!(
            "{} operations, {} blocks written, {} read, {} blocks checked, {} aborted reads, {} I/O errors{}, {} bad{}",
            ops, bw, br, chk, ab, errs, if resets { " (tolerated during resets)" } else { "" }, bad, first));
        t.check(finals == kids, &alloc::format!("round{}-regions", rd + 1),
            alloc::format!("{} of {} kids' regions re-read and right after the round", finals, kids));
        t.check(lost == 0, &alloc::format!("round{}-no-lost-request", rd + 1),
            alloc::format!("{} kids had a request out for {} s or more", lost, STALL_MS / 1000));
        t.check(left == 0, &alloc::format!("round{}-kids-gone", rd + 1), alloc::format!("{} of {} kid tasks still there", left, kids));
        if resets {
            t.check(sent > 0 && done == sent, &alloc::format!("round{}-resets", rd + 1),
                alloc::format!("{} controller resets sent, {} done", sent, done));
        }
        for (k, c) in ctls.iter().enumerate() {
            let bs = geo[k].0 as usize;
            let mut b = vec![0u8; 8 * bs];
            let (e, n) = c.do_io(CMD_READ, b.as_mut_ptr(), b.len() as u32, 0);
            t.check(e == 0 && n == b.len() as u32, &alloc::format!("round{}-after-unit{}", rd + 1, units[k]),
                alloc::format!("a read at 0 after the round: error {} actual {}", e, n));
        }
        if resets {
            let _ = timer.delay(TimerVal::from_millis(1500));
        }
        let _ = timer.delay(TimerVal::from_millis(1000));
        mem[rd] = avail();
    }
    let less = mem[0] as i64 - mem[1] as i64;
    if kidpri < 0 {
        // Kids below 0 mean other jobs share the machine (the soak's
        // filesystems): their allocations are in these figures too, so the
        // difference is shown, not judged; measure memory around the soak.
        say!("  memory (not judged: other jobs share the machine): free after round 1 {}, after round 2 {} ({} bytes less)", mem[0], mem[1], less);
    } else {
        t.check(less < 128 * 1024, "memory", alloc::format!("free memory after round 1 {}, after round 2 {} ({} bytes less)", mem[0], mem[1], less));
    }
    drop(ctls);
    // SAFETY: this task, back to the priority it was started with.
    unsafe { sys::exec_set_task_pri(me, old_pri) };
    say!("vscsitest torture: {} passed, {} failed", t.passed, t.total - t.passed);
    amigaos4::serial_println!("vscsitest torture: {} passed, {} failed", t.passed, t.total - t.passed);
    if t.passed == t.total { 0 } else { 20 }
}
