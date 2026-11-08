//! rngread: a Shell tool that exercises virtiorng.device.
//!
//!     rngread [KB]        (default 256)
//!
//! Opens virtiorng.device unit 0 and runs:
//!
//! 1. NSCMD_DEVICEQUERY: the device type and its command list.
//! 2. Unsupported commands (CMD_INVALID, CMD_WRITE) answer IOERR_NOCMD.
//! 3. Throughput: KB kilobytes in 64 KB CMD_READs, timed with timer.device;
//!    the first 64 KB must hold all 256 byte values.
//! 4. AbortIO on a queued request: replied IOERR_ABORTED with nothing read.
//! 5. AbortIO on the request in progress: IOERR_ABORTED with part read.
//! 6. CMD_FLUSH with one request in progress and one queued: both aborted.
//! 7. A plain read after all that still works.
//! 8. Recovery: VRNG_CMD_TESTFAULT resets the device behind the driver's
//!    back; the next read must still succeed, through the driver's recovery.
//! 9. Edges: 300001 bytes into a buffer at an odd address, with guard bytes
//!    either side. The driver writes the aligned interior directly and
//!    bounces the edges; every byte inside must change, none outside.
//!
//! Prints a line per test and `rngread: N/9 tests passed`; returns 0 when
//! every test passed, 20 otherwise.
//!
//!     rngread TORTURE [secs] [unit] [kids] [units]   (src/torture.rs)
//!     rngread EXPUNGE                        unload the device

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::panic::PanicInfo;

use amigaos4::allocator::SharedExecAllocator;
use amigaos4::port::AmigaMsgPort;
use amigaos4::sys::{self, IORequest};
use amigaos4::timer::{AmigaTimer, TimerUnit, TimerVal};

// MEMF_SHARED: the driver's unit task writes into these buffers, and the
// torture kids are other tasks on the same heap.
#[global_allocator]
static ALLOCATOR: SharedExecAllocator = SharedExecAllocator;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    amigaos4::panic::default_panic_handler(info)
}

const DEVNAME_C: &[u8] = amigaos4::amstr!("virtiorng.device");

use amigaos4::device::cmd::{CMD_FLUSH, CMD_INVALID, CMD_READ, CMD_WRITE, NSCMD_DEVICEQUERY};
use amigaos4::device::ioerr::{IOERR_ABORTED, IOERR_NOCMD};
use amigaos4::device::{IoReq, IOSTDREQ_SIZE};
/// virtiorng.device: reset the device behind the driver's back (CMD_NONSTD).
const VRNG_CMD_TESTFAULT: u16 = amigaos4::device::cmd::CMD_NONSTD;

/// Shell output through dos.library PutStr, in NUL-terminated chunks.
struct Out;
impl Write for Out {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut buf = [0u8; 128];
        for chunk in s.as_bytes().chunks(buf.len() - 1) {
            buf[..chunk.len()].copy_from_slice(chunk);
            buf[chunk.len()] = 0;
            // SAFETY: a NUL-terminated buffer on the stack.
            unsafe { sys::dos_put_str(buf.as_ptr() as _) };
        }
        Ok(())
    }
}
macro_rules! say {
    ($($arg:tt)*) => {{ let _ = writeln!(Out, $($arg)*); }};
}

mod torture;

/// An IOStdReq opened on virtiorng.device unit 0. Closed and deleted on drop.
struct Req(*mut IORequest);

impl Req {
    fn open(port: &AmigaMsgPort, unit: u32) -> Result<Self, String> {
        // SAFETY: plain exec calls.
        unsafe {
            let ior = sys::exec_create_iorequest(port.as_ptr(), IOSTDREQ_SIZE);
            if ior.is_null() {
                return Err("CreateIORequest failed".into());
            }
            let err = sys::exec_open_device(DEVNAME_C.as_ptr() as _, unit, ior, 0);
            if err != 0 {
                sys::exec_delete_iorequest(ior);
                return Err(alloc::format!("OpenDevice(\"virtiorng.device\", {}) returned {}", unit, err));
            }
            Ok(Self(ior))
        }
    }
    fn io(&self) -> IoReq {
        // SAFETY: the IOStdReq this owns (IOSTDREQ_SIZE, non-null).
        unsafe { IoReq::from_raw(self.0) }.expect("open request")
    }
    fn set(&self, cmd: u16, data: *mut u8, len: u32) {
        let r = self.io();
        r.set_command(cmd);
        r.set_data(data);
        r.set_length(len);
        r.set_actual(0);
    }
    fn error(&self) -> i8 {
        self.io().error()
    }
    fn actual(&self) -> u32 {
        self.io().actual()
    }
    /// DoIO; returns (io_Error, io_Actual).
    fn do_io(&self, cmd: u16, data: *mut u8, len: u32) -> (i8, u32) {
        self.set(cmd, data, len);
        // SAFETY: an open request.
        unsafe { sys::exec_do_io(self.0) };
        (self.error(), self.actual())
    }
    fn send(&self, cmd: u16, data: *mut u8, len: u32) {
        self.set(cmd, data, len);
        // SAFETY: an open request.
        unsafe { sys::exec_send_io(self.0) };
    }
    fn abort(&self) {
        // SAFETY: an open request.
        unsafe { sys::exec_abort_io(self.0) };
    }
    fn wait(&self) -> (i8, u32) {
        // SAFETY: an open request that was sent.
        unsafe { sys::exec_wait_io(self.0) };
        (self.error(), self.actual())
    }
}

impl Drop for Req {
    fn drop(&mut self) {
        // SAFETY: opened in open().
        unsafe {
            sys::exec_close_device(self.0);
            sys::exec_delete_iorequest(self.0);
        }
    }
}

fn distinct(bytes: &[u8]) -> usize {
    let mut seen = [false; 256];
    for &b in bytes {
        seen[b as usize] = true;
    }
    seen.iter().filter(|&&s| s).count()
}

/// The numbers in the Shell's argument string, in order.
fn parse_numbers(args: *const u8, len: i32) -> Vec<u32> {
    let mut v = Vec::new();
    let mut cur: Option<u32> = None;
    for i in 0..len.max(0) as usize {
        // SAFETY: the Shell's argument string, `len` bytes.
        let c = unsafe { *args.add(i) };
        if c.is_ascii_digit() {
            cur = Some(cur.unwrap_or(0).saturating_mul(10).saturating_add((c - b'0') as u32));
        } else if let Some(x) = cur.take() {
            v.push(x);
        }
    }
    if let Some(x) = cur {
        v.push(x);
    }
    v
}

fn run(kb: u32, unit: u32) -> Result<u32, String> {
    let port = AmigaMsgPort::new().map_err(|e| alloc::format!("CreateMsgPort: {}", e))?;
    let a = Req::open(&port, unit)?;
    let b = Req::open(&port, unit)?;
    let c = Req::open(&port, unit)?;
    let mut timer = AmigaTimer::open(TimerUnit::MicroHz).map_err(|e| alloc::format!("timer.device: {}", e))?;
    let mut passed = 0u32;
    let mut check = |name: &str, ok: bool, detail: String| {
        say!("{} {}: {}", if ok { "PASS" } else { "FAIL" }, name, detail);
        if ok {
            passed += 1;
        }
    };

    // 1. NSCMD_DEVICEQUERY
    let mut q = [0u8; 16];
    let (e, n) = a.do_io(NSCMD_DEVICEQUERY, q.as_mut_ptr(), q.len() as u32);
    let mut cmds = Vec::new();
    let format = u32::from_be_bytes([q[0], q[1], q[2], q[3]]);
    let devtype = u16::from_be_bytes([q[8], q[9]]);
    let list = u32::from_be_bytes([q[12], q[13], q[14], q[15]]) as *const u16;
    if e == 0 && !list.is_null() {
        for i in 0..32 {
            // SAFETY: the device's static, 0-terminated command list.
            let v = unsafe { *list.add(i) };
            if v == 0 {
                break;
            }
            cmds.push(v);
        }
    }
    let mut listed = String::new();
    for v in &cmds {
        let _ = write!(listed, " 0x{:04x}", v);
    }
    check(
        "devicequery",
        e == 0 && n == 16 && format == 0 && cmds.contains(&CMD_READ) && cmds.contains(&NSCMD_DEVICEQUERY),
        alloc::format!("error {} actual {} format {} type {} commands:{}", e, n, format, devtype, listed),
    );

    // 2. Unsupported commands
    let mut scratch = [0u8; 16];
    let (e1, _) = a.do_io(CMD_INVALID, scratch.as_mut_ptr(), 16);
    let (e2, _) = a.do_io(CMD_WRITE, scratch.as_mut_ptr(), 16);
    check(
        "nocmd",
        e1 == IOERR_NOCMD && e2 == IOERR_NOCMD,
        alloc::format!("CMD_INVALID -> {}, CMD_WRITE -> {} (IOERR_NOCMD is -3)", e1, e2),
    );

    // 3. Throughput
    let chunk = 64 * 1024usize;
    let mut buf = vec![0u8; chunk];
    let total = kb as usize * 1024;
    let t0 = timer.get_sys_time();
    let mut got = 0usize;
    let mut first_distinct = 0;
    let mut err3 = 0i8;
    while got < total {
        let want = chunk.min(total - got);
        let (e, n) = a.do_io(CMD_READ, buf.as_mut_ptr(), want as u32);
        if got == 0 {
            first_distinct = distinct(&buf[..n as usize]);
        }
        if e != 0 || n as usize != want {
            err3 = if e != 0 { e } else { -100 };
            break;
        }
        got += want;
    }
    let t1 = timer.get_sys_time();
    let ms = t1.as_millis().saturating_sub(t0.as_millis()).max(1);
    check(
        "throughput",
        err3 == 0 && got == total && first_distinct == 256,
        alloc::format!(
            "{} KB in {} ms = {} KB/s; {} distinct byte values in the first 64 KB",
            got / 1024, ms, got as u64 * 1000 / 1024 / ms, first_distinct
        ),
    );

    // 4 + 5. AbortIO: queued, then in progress.
    //
    // The unit task runs at priority 5 and, under QEMU, the device answers
    // its kick at once, so it serves a whole request without sleeping. To
    // catch requests queued and in progress, this task runs above it for
    // tests 4 to 6: a request it sends stays queued until it waits, and
    // when its timer fires it preempts the unit task mid-request.
    // SAFETY: this task; the old priority is restored below.
    let me = unsafe { sys::exec_find_task(core::ptr::null_mut()) };
    let old_pri = unsafe { sys::exec_set_task_pri(me, 10) };
    // Abort and flush need the request in progress when they land: started
    // (the unit task has posted pieces) and not finished. Under load, with
    // other tasks above the unit task or another unit busy, either can fail
    // to hold for a fixed delay: the unit task may not have run yet (aborted
    // with 0 bytes), or this task's timer may wake late (the read already
    // complete). Both answers are right but prove nothing, so the read is
    // large (32 MB, about 250 ms at 130 MB/s) and an inconclusive attempt is
    // repeated, up to 3 times, with a longer delay after a 0-byte one.
    let big = 32 * 1024 * 1024usize;
    let mut abuf = vec![0u8; big];
    let mut bbuf = vec![0u8; chunk];
    a.send(CMD_READ, abuf.as_mut_ptr(), big as u32);
    b.send(CMD_READ, bbuf.as_mut_ptr(), chunk as u32); // queued behind A
    b.abort();
    let (eb, nb) = b.wait();
    check(
        "abort-queued",
        eb == IOERR_ABORTED && nb == 0,
        alloc::format!("error {} actual {} (IOERR_ABORTED is -2, nothing read)", eb, nb),
    );
    let mut tries = 0;
    let mut ms = 10u32;
    let (ea, na) = loop {
        tries += 1;
        if tries > 1 {
            a.send(CMD_READ, abuf.as_mut_ptr(), big as u32);
        }
        let _ = timer.delay(TimerVal::from_millis(ms));
        a.abort();
        let (ea, na) = a.wait();
        let partial = na > 0 && (na as usize) < big;
        if partial || tries == 3 {
            break (ea, na);
        }
        if na == 0 {
            ms *= 5;
        }
    };
    check(
        "abort-current",
        ea == IOERR_ABORTED && na > 0 && (na as usize) < big,
        alloc::format!(
            "error {} actual {} of {} (part read, then stopped; attempt {} of 3)",
            ea, na, big, tries
        ),
    );

    // 6. CMD_FLUSH with one in progress and one queued
    let mut tries = 0;
    let mut ms = 10u32;
    let (ec, ea, na, eb, nb) = loop {
        tries += 1;
        a.send(CMD_READ, abuf.as_mut_ptr(), big as u32);
        b.send(CMD_READ, bbuf.as_mut_ptr(), chunk as u32);
        let _ = timer.delay(TimerVal::from_millis(ms));
        let (ec, _) = c.do_io(CMD_FLUSH, core::ptr::null_mut(), 0);
        let (ea, na) = a.wait();
        let (eb, nb) = b.wait();
        let partial = na > 0 && (na as usize) < big;
        if partial || tries == 3 {
            break (ec, ea, na, eb, nb);
        }
        if na == 0 {
            ms *= 5;
        }
    };
    // SAFETY: this task, back to the priority it was started with.
    unsafe { sys::exec_set_task_pri(me, old_pri) };
    check(
        "flush",
        ec == 0 && ea == IOERR_ABORTED && na > 0 && eb == IOERR_ABORTED && nb == 0,
        alloc::format!(
            "CMD_FLUSH {}; in progress: error {} actual {}; queued: error {} actual {} (attempt {} of 3)",
            ec, ea, na, eb, nb, tries
        ),
    );

    // 7. The device still works
    let mut tail = [0u8; 64];
    let (e, n) = a.do_io(CMD_READ, tail.as_mut_ptr(), 64);
    let mut hex = String::new();
    for x in &tail[..16] {
        let _ = write!(hex, "{:02x}", x);
    }
    check(
        "after",
        e == 0 && n == 64 && distinct(&tail) > 8,
        alloc::format!("error {} actual {}: {}...", e, n, hex),
    );

    // 8. Recovery after the device is reset behind the driver's back
    let (ef, _) = a.do_io(VRNG_CMD_TESTFAULT, core::ptr::null_mut(), 0);
    let mut r = vec![0u8; 4096];
    let (e, n) = a.do_io(CMD_READ, r.as_mut_ptr(), 4096);
    check(
        "recover",
        ef == 0 && e == 0 && n == 4096 && distinct(&r) > 200,
        alloc::format!(
            "test fault {}; read after it: error {} actual {}, {} distinct byte values",
            ef, e, n, distinct(&r)
        ),
    );

    // 9. A misaligned buffer: direct interior, bounced edges, guards intact
    const EDGE_LEN: usize = 300_001;
    const LEAD: usize = 3;
    let mut g = vec![0xa5u8; EDGE_LEN + 64];
    let (e, n) = a.do_io(CMD_READ, g[LEAD..].as_mut_ptr(), EDGE_LEN as u32);
    let body = &g[LEAD..LEAD + EDGE_LEN];
    let guards = g[..LEAD].iter().chain(&g[LEAD + EDGE_LEN..]).all(|&x| x == 0xa5);
    // A run of 64 guard values inside would be an unwritten cache line.
    let unwritten = body.windows(64).any(|w| w.iter().all(|&x| x == 0xa5));
    check(
        "edges",
        e == 0 && n as usize == EDGE_LEN && guards && !unwritten && distinct(body) > 250,
        alloc::format!(
            "error {} actual {} of {} at an odd address; guards {}; {}; {} distinct byte values",
            e, n, EDGE_LEN,
            if guards { "intact" } else { "OVERWRITTEN" },
            if unwritten { "an unwritten 64-byte run inside" } else { "no unwritten run" },
            distinct(body)
        ),
    );

    Ok(passed)
}

/// `rngread EXPUNGE`: ask exec to expunge virtiorng.device, the classic
/// way and without Forbid: open it, RemDevice (its Expunge, seeing it open,
/// defers), close (the last close runs the deferred expunge). The driver
/// reports the outcome on serial.
fn expunge() -> i32 {
    let port = match AmigaMsgPort::new() {
        Ok(p) => p,
        Err(e) => {
            say!("rngread: CreateMsgPort: {}", e);
            return 20;
        }
    };
    let req = match Req::open(&port, 0) {
        Ok(r) => r,
        Err(e) => {
            say!("rngread: {}", e);
            return 20;
        }
    };
    let dev = req.io().device();
    // SAFETY: the device this request has open.
    unsafe { sys::exec_rem_device(dev as *mut sys::Device) };
    drop(req); // CloseDevice: the deferred expunge runs here
    say!("rngread: RemDevice and CloseDevice done; see the driver's serial lines");
    0
}

#[no_mangle]
pub extern "C" fn rust_main(args: *const u8, arglen: i32) -> i32 {
    // SAFETY: the Shell's argument string, `arglen` bytes.
    let text = unsafe { core::slice::from_raw_parts(args, arglen.max(0) as usize) };
    if text.windows(7).any(|w| w.eq_ignore_ascii_case(b"EXPUNGE")) {
        return expunge();
    }
    // rngread TORTURE [secs] [unit] [kids] [units]: see torture.rs.
    if text.windows(7).any(|w| w.eq_ignore_ascii_case(b"TORTURE")) {
        let nums = parse_numbers(args, arglen);
        let secs = nums.first().copied().filter(|&s| s > 0).unwrap_or(60).min(24 * 3600);
        let unit = nums.get(1).copied().unwrap_or(0);
        let kids = nums.get(2).copied().filter(|&k| k > 0).unwrap_or(8) as usize;
        let units = nums.get(3).copied().filter(|&k| k > 0).unwrap_or(1);
        return torture::torture(secs, unit, kids, units);
    }
    // rngread [KB] [unit]: KB 1..16384 (default 256), unit default 0.
    // rngread EXPUNGE: unload the device (see expunge()).
    let nums = parse_numbers(args, arglen);
    let kb = match nums.first() {
        Some(&k) if k > 0 => k.min(16 * 1024),
        _ => 256,
    };
    let unit = nums.get(1).copied().unwrap_or(0);
    say!("rngread: virtiorng.device unit {}, {} KB throughput run", unit, kb);
    match run(kb, unit) {
        Ok(p) => {
            say!("rngread: {}/9 tests passed", p);
            amigaos4::serial_println!("rngread: {}/9 tests passed", p);
            if p == 9 { 0 } else { 20 }
        }
        Err(e) => {
            say!("rngread: FAILED: {}", e);
            20
        }
    }
}
