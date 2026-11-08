//! rngread's torture mode: try to break virtiorng.device.
//!
//!     rngread TORTURE [secs] [unit] [kids] [units]
//!                         (default 60 s, unit 0, 8 kids, 1 unit)
//!
//! With `units` above 1 the kids take units `unit`, `unit`+1, ... in turn
//! (kid i: unit + i % units), so one run loads several cards at once (two
//! separate runs cannot: the second one's shell, below the first one's
//! CPU-bound kids, never runs).
//!
//! `kids` tasks (at most 8) each open the unit and loop over random work:
//! reads of random sizes (1 byte to 1 MB) into buffers at random offsets
//! with guard bytes either side, SendIO + AbortIO at a random moment, two
//! requests in flight at once, CMD_FLUSH, a NULL buffer with a length, and
//! closing and opening again. Every answer is checked:
//! - io_Error is 0, IOERR_ABORTED, IOERR_UNITBUSY (a read caught by a
//!   device reset) or, for the NULL buffer, IOERR_BADLENGTH (or
//!   IOERR_ABORTED: a flush reached it first); nothing else;
//! - io_Error 0 means io_Actual == io_Length; an abort, io_Actual <= it;
//! - the guards are untouched, and a completed read of 64 bytes or more
//!   has no 64-byte run still holding the fill value.
//! Meanwhile the main task resets the device behind the driver's back
//! (VRNG_CMD_TESTFAULT) every 1.5 s and watches every kid: a kid whose
//! request has not come back for 10 s is a lost request.
//! At the end: every kid gone, a plain read still works, and free memory
//! after a second, identical round is within 64 KB of after the first
//! (the first round warms the pools).

use super::*;

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use amigaos4::device::ioerr::{IOERR_BADLENGTH, IOERR_UNITBUSY};
use amigaos4::sys::TagItem;

const MAX_KIDS: usize = 8;
const MEMF_ANY: u32 = 0;
const FILL: u8 = 0xa5;
const GUARD: usize = 64;
/// A kid whose op counter has not moved for this long holds a lost request.
const STALL_MS: u32 = 10_000;
const RESET_EVERY_MS: u32 = 1500;

static KID_NAMES: [&[u8]; MAX_KIDS] = [
    b"rngread kid 0\0", b"rngread kid 1\0", b"rngread kid 2\0", b"rngread kid 3\0",
    b"rngread kid 4\0", b"rngread kid 5\0", b"rngread kid 6\0", b"rngread kid 7\0",
];

/// Per kid: operations finished, bad answers, bytes read, aborted answers,
/// UNITBUSY answers, and done (1 when its task body has returned).
static OPS: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static BAD: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static KB: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static ABORTED: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static BUSY: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static DONE: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
/// The first bad answer of each kid, as a code (see bad_text).
static FIRST_BAD: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static STOP: AtomicU32 = AtomicU32::new(0);
static UNIT: AtomicU32 = AtomicU32::new(0);
static UNITS: AtomicU32 = AtomicU32::new(1);

fn avail() -> u32 {
    // SAFETY: a plain exec query.
    unsafe { sys::exec_avail_mem(MEMF_ANY) }
}

/// xorshift32: the kids' random numbers.
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
    fn below(&mut self, n: u32) -> u32 {
        self.next() % n.max(1)
    }
}

// Codes for a kid's first bad answer: kind * 1_000_000 + |io_Error| * 1000 + op.
const K_ERROR: u32 = 1; // an io_Error that is not allowed
const K_ACTUAL: u32 = 2; // io_Actual wrong for the io_Error
const K_GUARD: u32 = 3; // a guard byte overwritten
const K_UNWRITTEN: u32 = 4; // a completed read left a 64-byte run unwritten
const K_OPEN: u32 = 5; // OpenDevice failed
const K_SETUP: u32 = 6; // no port or timer in the kid

fn bad_text(code: u32) -> String {
    let kind = code / 1_000_000;
    let err = (code / 1000) % 1000;
    let op = code % 1000;
    let what = match kind {
        K_ERROR => "an io_Error not allowed",
        K_ACTUAL => "io_Actual wrong for its io_Error",
        K_GUARD => "a guard byte overwritten",
        K_UNWRITTEN => "a 64-byte run left unwritten in a completed read",
        K_OPEN => "OpenDevice failed",
        K_SETUP => "no message port or timer",
        _ => "?",
    };
    alloc::format!("{} (io_Error -{}, op {})", what, err, op)
}

fn note_bad(i: usize, kind: u32, err: i8, op: u32) {
    BAD[i].fetch_add(1, Ordering::Relaxed);
    let code = kind * 1_000_000 + (err as i32).unsigned_abs().min(999) * 1000 + op.min(999);
    let _ = FIRST_BAD[i].compare_exchange(0, code, Ordering::Relaxed, Ordering::Relaxed);
}

/// A buffer with GUARD fill bytes either side of `len` bytes at offset
/// GUARD + `skew` (skew 0..3 makes it misaligned).
struct Guarded {
    v: Vec<u8>,
    off: usize,
    len: usize,
}
impl Guarded {
    fn new(len: usize, skew: usize) -> Self {
        Self { v: vec![FILL; len + 2 * GUARD + 4], off: GUARD + skew, len }
    }
    fn ptr(&mut self) -> *mut u8 {
        self.v[self.off..].as_mut_ptr()
    }
    fn guards_ok(&self) -> bool {
        self.v[..self.off].iter().all(|&x| x == FILL) && self.v[self.off + self.len..].iter().all(|&x| x == FILL)
    }
    /// A run of 64 fill bytes inside the first `n` bytes.
    fn unwritten(&self, n: usize) -> bool {
        n >= 64 && self.v[self.off..self.off + n].windows(64).any(|w| w.iter().all(|&x| x == FILL))
    }
}

/// Check one finished read; returns false (and notes it) when it is bad.
fn check_read(i: usize, op: u32, g: &Guarded, e: i8, n: u32) -> bool {
    let ok_err = e == 0 || e == IOERR_ABORTED || e == IOERR_UNITBUSY;
    if !ok_err {
        note_bad(i, K_ERROR, e, op);
        return false;
    }
    let len = g.len as u32;
    let ok_actual = match e {
        0 => n == len,
        IOERR_ABORTED => n <= len,
        _ => n == 0,
    };
    if !ok_actual {
        note_bad(i, K_ACTUAL, e, op);
        return false;
    }
    if !g.guards_ok() {
        note_bad(i, K_GUARD, e, op);
        return false;
    }
    if e == 0 && g.unwritten(n as usize) {
        note_bad(i, K_UNWRITTEN, e, op);
        return false;
    }
    match e {
        IOERR_ABORTED => {
            ABORTED[i].fetch_add(1, Ordering::Relaxed);
        }
        IOERR_UNITBUSY => {
            BUSY[i].fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
    KB[i].fetch_add(n / 1024, Ordering::Relaxed);
    true
}

/// A random read length: mostly small and middling, sometimes large.
fn rand_len(r: &mut Rand) -> usize {
    match r.below(10) {
        0..=3 => 1 + r.below(64) as usize,
        4..=6 => 65 + r.below(8 * 1024) as usize,
        7..=8 => 8 * 1024 + r.below(256 * 1024) as usize,
        _ => 256 * 1024 + r.below(768 * 1024) as usize,
    }
}

extern "C" fn kid_entry(i: u32) {
    let i = i as usize;
    kid_body(i);
    DONE[i].store(1, Ordering::Release);
}

fn kid_body(i: usize) {
    let unit = UNIT.load(Ordering::Acquire) + i as u32 % UNITS.load(Ordering::Acquire).max(1);
    let (Ok(port), Ok(mut timer)) = (AmigaMsgPort::new(), AmigaTimer::open(TimerUnit::MicroHz)) else {
        note_bad(i, K_SETUP, 0, 0);
        return;
    };
    let mut r = Rand(0x9e37_79b9 ^ ((i as u32 + 1).wrapping_mul(0x85eb_ca6b)) ^ amigaos4::arch::timebase() as u32);
    let (Ok(mut a), Ok(b)) = (Req::open(&port, unit), Req::open(&port, unit)) else {
        note_bad(i, K_OPEN, 0, 0);
        return;
    };
    while STOP.load(Ordering::Acquire) == 0 {
        let op = r.below(16);
        match op {
            // A plain read.
            0..=7 => {
                let mut g = Guarded::new(rand_len(&mut r), r.below(4) as usize);
                let (e, n) = a.do_io(CMD_READ, g.ptr(), g.len as u32);
                check_read(i, op, &g, e, n);
            }
            // SendIO, then AbortIO now, after a moment, or not at all.
            8..=10 => {
                let mut g = Guarded::new(64 * 1024 + r.below(960 * 1024) as usize, r.below(4) as usize);
                a.send(CMD_READ, g.ptr(), g.len as u32);
                match r.below(3) {
                    0 => a.abort(),
                    1 => {
                        let _ = timer.delay(TimerVal::from_millis(r.below(4)));
                        a.abort();
                    }
                    _ => {}
                }
                let (e, n) = a.wait();
                check_read(i, op, &g, e, n);
            }
            // Two in flight on one port.
            11..=12 => {
                let mut g1 = Guarded::new(rand_len(&mut r), r.below(4) as usize);
                let mut g2 = Guarded::new(rand_len(&mut r), r.below(4) as usize);
                a.send(CMD_READ, g1.ptr(), g1.len as u32);
                b.send(CMD_READ, g2.ptr(), g2.len as u32);
                let (e1, n1) = a.wait();
                let (e2, n2) = b.wait();
                check_read(i, op, &g1, e1, n1);
                check_read(i, op, &g2, e2, n2);
            }
            // CMD_FLUSH (it aborts other kids' queued reads too: they accept that).
            13 => {
                let (e, _) = b.do_io(CMD_FLUSH, core::ptr::null_mut(), 0);
                if e != 0 && e != IOERR_UNITBUSY {
                    note_bad(i, K_ERROR, e, op);
                }
            }
            // A NULL buffer with a length: IOERR_BADLENGTH, nothing read. The
            // unit task checks it, so while it is still queued another kid's
            // CMD_FLUSH may abort it first: IOERR_ABORTED, nothing read.
            14 => {
                let (e, n) = a.do_io(CMD_READ, core::ptr::null_mut(), 512);
                if (e != IOERR_BADLENGTH && e != IOERR_ABORTED) || n != 0 {
                    note_bad(i, K_ERROR, e, op);
                }
            }
            // Close and open again.
            _ => {
                drop(a);
                match Req::open(&port, unit) {
                    Ok(x) => a = x,
                    Err(_) => {
                        note_bad(i, K_OPEN, 0, op);
                        return;
                    }
                }
            }
        }
        OPS[i].fetch_add(1, Ordering::Relaxed);
    }
}

/// One round: kids for `secs` seconds, resets every RESET_EVERY_MS, the
/// watchdog. Returns (lost: kids that stalled, kids still there at the end,
/// resets sent, resets done, resets aborted by a kid's CMD_FLUSH).
fn round(timer: &mut AmigaTimer, ctls: &[Req], secs: u32, kids: usize) -> (u32, u32, u32, u32, u32) {
    STOP.store(0, Ordering::Release);
    for i in 0..kids {
        for x in [&OPS[i], &BAD[i], &KB[i], &ABORTED[i], &BUSY[i], &DONE[i], &FIRST_BAD[i]] {
            x.store(0, Ordering::Relaxed);
        }
        let tags = [
            TagItem { ti_Tag: sys::AT_PARAM1, ti_Data: i as u32 },
            TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
        ];
        // SAFETY: a plain task at priority 0; it ends when STOP is set.
        let t = unsafe { sys::exec_create_task(KID_NAMES[i].as_ptr(), 0, kid_entry as *const c_void, 65536, tags.as_ptr()) };
        if t.is_null() {
            note_bad(i, K_SETUP, 0, 0);
            DONE[i].store(1, Ordering::Release);
        }
    }
    let mut last = [0u32; MAX_KIDS];
    let mut still = [0u32; MAX_KIDS];
    let mut stalled = [false; MAX_KIDS];
    let (mut sent, mut resets, mut flushed) = (0u32, 0u32, 0u32);
    let step = 250u32;
    let mut t = 0u32;
    let mut since_reset = 0u32;
    while t < secs * 1000 {
        let _ = timer.delay(TimerVal::from_millis(step));
        t += step;
        since_reset += step;
        if since_reset >= RESET_EVERY_MS {
            since_reset = 0;
            for ctl in ctls {
                sent += 1;
                // Queued behind the kids' reads, a reset can meet a kid's
                // CMD_FLUSH first: IOERR_ABORTED, which is right.
                match ctl.do_io(VRNG_CMD_TESTFAULT, core::ptr::null_mut(), 0).0 {
                    0 => resets += 1,
                    IOERR_ABORTED => flushed += 1,
                    _ => {}
                }
            }
        }
        for i in 0..kids {
            let o = OPS[i].load(Ordering::Relaxed);
            if o == last[i] && DONE[i].load(Ordering::Acquire) == 0 {
                still[i] += step;
                if still[i] >= STALL_MS && !stalled[i] {
                    stalled[i] = true;
                    say!("FAIL watchdog: kid {} has had a request out for {} s (ops so far {})", i, still[i] / 1000, o);
                }
            } else {
                still[i] = 0;
                last[i] = o;
            }
        }
    }
    STOP.store(1, Ordering::Release);
    // Wait for the kids to finish their last operation and leave.
    let mut waited = 0;
    while (0..kids).any(|i| DONE[i].load(Ordering::Acquire) == 0) && waited < 20_000 {
        let _ = timer.delay(TimerVal::from_millis(100));
        waited += 100;
    }
    let mut left = 0u32;
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
    (stalled.iter().filter(|&&s| s).count() as u32, left, sent, resets, flushed)
}

pub fn torture(secs: u32, unit: u32, kids: usize, units: u32) -> i32 {
    let kids = kids.clamp(1, MAX_KIDS);
    let units = units.clamp(1, kids as u32);
    UNIT.store(unit, Ordering::Release);
    UNITS.store(units, Ordering::Release);
    say!("rngread torture: virtiorng.device units {} to {}, {} kids, {} s per round, two rounds", unit, unit + units - 1, kids, secs);
    let port = match AmigaMsgPort::new() {
        Ok(p) => p,
        Err(e) => {
            say!("rngread: CreateMsgPort: {}", e);
            return 20;
        }
    };
    let mut timer = match AmigaTimer::open(TimerUnit::MicroHz) {
        Ok(t) => t,
        Err(e) => {
            say!("rngread: timer.device: {}", e);
            return 20;
        }
    };
    // The control opener stays open across both rounds, so the unit (and,
    // on the disk build, the device) stays up and the memory readings
    // compare like with like.
    let mut ctls = Vec::new();
    for u in unit..unit + units {
        match Req::open(&port, u) {
            Ok(r) => ctls.push(r),
            Err(e) => {
                say!("rngread: {}", e);
                return 20;
            }
        }
    }
    // The kids are CPU-bound under QEMU (the device answers at once), so a
    // main task at or below their priority 0 would never run its watchdog
    // or its resets (the remote-control daemon's shell runs at -2). It sleeps on its timer almost
    // all the time, so above them costs nothing.
    // SAFETY: this task; the old priority is restored before returning.
    let me = unsafe { sys::exec_find_task(core::ptr::null_mut()) };
    let old_pri = unsafe { sys::exec_set_task_pri(me, 2) };
    let mut passed = 0u32;
    let mut failed = 0u32;
    let mut check = |name: &str, ok: bool, detail: String| {
        say!("{} {}: {}", if ok { "PASS" } else { "FAIL" }, name, detail);
        if ok {
            passed += 1;
        } else {
            failed += 1;
        }
    };
    let mut mem = [0u32; 2];
    for rd in 0..2 {
        let (lost, left, sent, resets, flushed) = round(&mut timer, &ctls, secs, kids);
        let ops: u32 = (0..kids).map(|i| OPS[i].load(Ordering::Relaxed)).sum();
        let bad: u32 = (0..kids).map(|i| BAD[i].load(Ordering::Relaxed)).sum();
        let kb: u32 = (0..kids).map(|i| KB[i].load(Ordering::Relaxed)).sum();
        let ab: u32 = (0..kids).map(|i| ABORTED[i].load(Ordering::Relaxed)).sum();
        let busy: u32 = (0..kids).map(|i| BUSY[i].load(Ordering::Relaxed)).sum();
        let mut first = String::new();
        for i in 0..kids {
            let c = FIRST_BAD[i].load(Ordering::Relaxed);
            if c != 0 {
                let _ = write!(first, "; kid {}: {}", i, bad_text(c));
            }
        }
        check(
            &alloc::format!("round{}-answers", rd + 1),
            bad == 0 && ops > 0,
            alloc::format!("{} operations, {} bad answers, {} KB read, {} aborted, {} UNITBUSY{}", ops, bad, kb, ab, busy, first),
        );
        check(
            &alloc::format!("round{}-no-lost-request", rd + 1),
            lost == 0,
            alloc::format!("{} kids stalled for {} s or more", lost, STALL_MS / 1000),
        );
        check(
            &alloc::format!("round{}-kids-gone", rd + 1),
            left == 0,
            alloc::format!("{} of {} kid tasks still there after stop", left, kids),
        );
        check(
            &alloc::format!("round{}-resets", rd + 1),
            sent >= (secs * 1000 / RESET_EVERY_MS).saturating_sub(1) * units && resets + flushed == sent && resets * 10 >= sent * 9,
            alloc::format!("{} device resets behind the driver's back sent: {} done, {} aborted by a kid's CMD_FLUSH, {} refused", sent, resets, flushed, sent - resets - flushed),
        );
        // Every unit still serves a plain read after the round.
        for (k, ctl) in ctls.iter().enumerate() {
            let mut g = Guarded::new(4096, 1);
            let (e, n) = ctl.do_io(CMD_READ, g.ptr(), 4096);
            let fine = e == 0 && n == 4096 && g.guards_ok() && !g.unwritten(4096);
            check(
                &alloc::format!("round{}-after-unit{}", rd + 1, unit + k as u32),
                fine,
                alloc::format!("read after the round: error {} actual {}", e, n),
            );
        }
        // Let the last reset's recovery and the reaper settle.
        let _ = timer.delay(TimerVal::from_millis(1000));
        mem[rd] = avail();
    }
    let less = mem[0] as i64 - mem[1] as i64;
    check(
        "memory",
        less < 64 * 1024,
        alloc::format!("free memory after round 1 {}, after round 2 {} ({} bytes less)", mem[0], mem[1], less),
    );
    drop(ctls);
    // SAFETY: this task, back to the priority it was started with.
    unsafe { sys::exec_set_task_pri(me, old_pri) };
    say!("rngread torture: {} passed, {} failed", passed, failed);
    amigaos4::serial_println!("rngread torture: {} passed, {} failed", passed, failed);
    if failed == 0 { 0 } else { 20 }
}
