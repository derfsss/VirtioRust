//! vscsitest: a Shell tool that exercises the Rust virtioscsi.device.
//!
//!     vscsitest <device> <units> [<all units> [<first>]]
//!
//! Units first..units-1 are tested (first defaults to 0). With the drop-in
//! driver unit 0 is the system disk: give first = 1.
//!
//! `device` is the name to open (vscsirs.device during development,
//! virtioscsi.device for the drop-in); `units` is how many units the test
//! machine has (the disks on the driver's controller). THE TEST WRITES TO
//! EVERY UNIT: give it scratch disks only.
//!
//! Per unit 0..units-1:
//! 1. OpenDevice succeeds, and a second open of the same unit does too.
//! 2. NSCMD_DEVICEQUERY: format 0, NSDEVTYPE_TRACKDISK, a command list with
//!    NSCMD_DEVICEQUERY and the 64-bit commands.
//! 3. CMD_INVALID answers IOERR_NOCMD.
//! 4. The block size (the smallest of 512..4096 a read at 0 accepts) and the
//!    size (bisected on IOERR_BADADDRESS). A unit that answers
//!    TDERR_DiskChanged has no medium; it gets only test 5.
//! 5. No medium: CMD_READ answers TDERR_DiskChanged (29).
//! 6. Writes read back: CMD_WRITE of 64 KiB at 0; 300 KiB (several of the
//!    driver's chunks, not a whole number of them) at 1 MiB; TD_FORMAT of 4
//!    blocks at 2 MiB; the last block with TD_WRITE64; each read back with
//!    CMD_READ or TD_READ64 and compared. The data is pattern(byte offset
//!    on the disk, unit), so the host can check the image afterwards.
//! 7. A buffer at an odd address: 4 blocks read into it at 1 MiB, compared,
//!    with guard bytes either side untouched.
//! 8. Above 4 GiB (units larger than 5 GiB): NSCMD_TD_WRITE64 at 5 GiB,
//!    read back with NSCMD_TD_READ64 and with TD_READ64.
//! 9. Rejected: an offset and a length that are not whole blocks
//!    (IOERR_BADLENGTH); a start at the end and a range crossing it
//!    (IOERR_BADADDRESS); nothing is read. A zero-length read: 0, 0 bytes.
//! 10. Speed: 4 MiB written and read at 16 MiB in 1 MiB requests (shown,
//!    not judged).
//! 11. TD_GETGEOMETRY (zeroed reserved bytes, TotalSectors clamped to 32
//!    bits, Cylinders x CylSectors = the size, MEMF_PUBLIC|MEMF_LOCAL) and
//!    NSCMD_TD_GETGEOMETRY64 (the full size); a short buffer is
//!    IOERR_BADLENGTH.
//! 12. Quick commands: TD_GETDRIVETYPE DRIVE3_5, TD_CHANGESTATE (0 with a
//!    medium), TD_PROTSTATUS 0, CMD_UPDATE 0, TD_MOTOR answers 1.
//! 13. TD_ADDCHANGEINT is held (still pending after 200 ms) until
//!    TD_REMCHANGEINT on the same request; a second one comes back
//!    IOERR_ABORTED from AbortIO.
//! 14. HD_SCSICMD: INQUIRY (36 bytes, vendor QEMU), TEST UNIT READY, READ(10)
//!    of block 0 equal to CMD_READ's, and an unknown opcode: HFERR_BadStatus,
//!    CHECK CONDITION, autosense with sense key 5. A CD with no medium: TEST
//!    UNIT READY gives sense key 2.
//! and once: open on unit `all units` (default `units`) fails, so a run on
//! the first units only (the soak: never the filesystem disk) still checks
//! the end of the unit list.
//!
//! Then the pipeline (units 0 and 1, between 8 and 10 MiB):
//! 15. 16 writes of 64 KiB sent at once to unit 0, then 16 reads at once,
//!    each compared.
//! 16. 8 requests on unit 0 and 8 on unit 1, all sent at once, interleaved.
//! 17. 20 reads of 1 MiB sent at once from above the driver task's
//!    priority; AbortIO on the last (still queued behind the driver's 16):
//!    it comes back IOERR_ABORTED, the others complete and compare.
//! 18. Development build only: VSCSI_CMD_TESTFAULT resets the controller
//!    behind the driver's back; the next read must still succeed, through
//!    the driver's recovery.
//!
//! Prints a line per test and `vscsitest: N/M tests passed`; returns 0 when
//! every test passed, 20 otherwise.
//!
//!     vscsitest TORTURE <device> <secs> <units,...> [kids] [kidpri]
//!
//! tasks doing checked random I/O on regions of the given units, with
//! resets on the development build (src/torture.rs).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::panic::PanicInfo;

use amigaos4::allocator::SharedExecAllocator;
use amigaos4::device::cmd::{CMD_INVALID, CMD_READ, CMD_WRITE, NSCMD_DEVICEQUERY};
use amigaos4::device::ioerr::{IOERR_BADADDRESS, IOERR_BADLENGTH, IOERR_NOCMD};
use amigaos4::device::cmd::CMD_UPDATE;
use amigaos4::device::td::{
    DRIVE3_5, HD_SCSICMD, NSCMD_TD_GETGEOMETRY64, NSCMD_TD_READ64, NSCMD_TD_WRITE64, TD_ADDCHANGEINT, TD_CHANGESTATE, TD_FORMAT,
    TD_GETDRIVETYPE, TD_GETGEOMETRY, TD_MOTOR, TD_PROTSTATUS, TD_READ64, TD_REMCHANGEINT, TD_WRITE64,
};
use amigaos4::device::ioerr::IOERR_ABORTED;
use amigaos4::device::tderr::{HFERR_BAD_STATUS, TDERR_DISK_CHANGED};
use amigaos4::timer::TimerVal;
use amigaos4::device::{IoReq, IOSTDREQ_SIZE, NSDEVTYPE_TRACKDISK};
use amigaos4::port::AmigaMsgPort;
use amigaos4::sys::{self, IORequest};
use amigaos4::timer::{AmigaTimer, TimerUnit};

// MEMF_SHARED: the driver's task writes into these buffers, and the
// torture kids are other tasks on the same heap.
#[global_allocator]
static ALLOCATOR: SharedExecAllocator = SharedExecAllocator;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    amigaos4::panic::default_panic_handler(info)
}

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

/// The test data: what byte `abs` of unit `unit` holds after a test write.
/// Also computed by the host check (tools/vscsicheck.py).
fn pattern(abs: u64, unit: u32) -> u8 {
    ((abs >> 9) as u8) ^ (abs as u8).wrapping_mul(31) ^ (0x5a + unit as u8)
}

fn fill(buf: &mut [u8], abs: u64, unit: u32) {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = pattern(abs + i as u64, unit);
    }
}

/// The first byte where `buf` differs from the pattern, if any.
fn mismatch(buf: &[u8], abs: u64, unit: u32) -> Option<usize> {
    buf.iter().enumerate().position(|(i, &b)| b != pattern(abs + i as u64, unit))
}

/// An IOStdReq opened on the device. Closed and deleted on drop.
struct Req(*mut IORequest);

impl Req {
    fn open(port: &AmigaMsgPort, name: &[u8], unit: u32) -> Result<Self, i8> {
        // SAFETY: plain exec calls; `name` is NUL-terminated.
        unsafe {
            let ior = sys::exec_create_iorequest(port.as_ptr(), IOSTDREQ_SIZE);
            if ior.is_null() {
                return Err(-128);
            }
            let err = sys::exec_open_device(name.as_ptr() as _, unit, ior, 0);
            if err != 0 {
                sys::exec_delete_iorequest(ior);
                return Err(err as i8);
            }
            Ok(Self(ior))
        }
    }
    fn io(&self) -> IoReq {
        // SAFETY: the IOStdReq this owns (IOSTDREQ_SIZE, non-null).
        unsafe { IoReq::from_raw(self.0) }.expect("open request")
    }
    /// DoIO at a 64-bit byte offset (io_Actual takes the high half, which
    /// the 32-bit commands ignore); returns (io_Error, io_Actual).
    fn do_io(&self, cmd: u16, data: *mut u8, len: u32, offset: u64) -> (i8, u32) {
        let r = self.io();
        r.set_command(cmd);
        r.set_data(data);
        r.set_length(len);
        r.set_offset(offset as u32);
        r.set_actual((offset >> 32) as u32);
        // SAFETY: an open request.
        unsafe { sys::exec_do_io(self.0) };
        (r.error(), r.actual())
    }
}

impl Req {
    fn send(&self, cmd: u16) {
        let r = self.io();
        r.set_command(cmd);
        r.set_data(core::ptr::null_mut());
        r.set_length(0);
        // SAFETY: an open request.
        unsafe { sys::exec_send_io(self.0) };
    }
    fn is_done(&self) -> bool {
        // SAFETY: an open request.
        unsafe { !sys::exec_check_io(self.0).is_null() }
    }
    fn abort_wait(&self) -> i8 {
        // SAFETY: an open request that was sent.
        unsafe {
            sys::exec_abort_io(self.0);
            sys::exec_wait_io(self.0);
        }
        self.io().error()
    }
    /// HD_SCSICMD: (io_Error, scsi_Status, scsi_Actual, sense key, scsi_SenseActual).
    fn scsi(&self, cdb: &[u8], data: &mut [u8], read: bool) -> (i8, u8, u32, u8, u16) {
        let mut cmd = cdb.to_vec();
        let mut sense = [0u8; 32];
        let mut sc = [0u8; 30];
        let put32 = |b: &mut [u8], off: usize, v: u32| b[off..off + 4].copy_from_slice(&v.to_be_bytes());
        put32(&mut sc, 0, if data.is_empty() { 0 } else { data.as_mut_ptr() as usize as u32 });
        put32(&mut sc, 4, data.len() as u32);
        put32(&mut sc, 12, cmd.as_mut_ptr() as usize as u32);
        sc[16..18].copy_from_slice(&(cmd.len() as u16).to_be_bytes());
        sc[20] = (read as u8) | 2; // SCSIF_READ, SCSIF_AUTOSENSE
        put32(&mut sc, 22, sense.as_mut_ptr() as usize as u32);
        sc[26..28].copy_from_slice(&(sense.len() as u16).to_be_bytes());
        let (e, _) = self.do_io(HD_SCSICMD, sc.as_mut_ptr(), sc.len() as u32, 0);
        let actual = u32::from_be_bytes([sc[8], sc[9], sc[10], sc[11]]);
        let sense_actual = u16::from_be_bytes([sc[28], sc[29]]);
        (e, sc[21], actual, sense[2] & 0x0f, sense_actual)
    }
}

impl Req {
    /// SendIO of a read or write at a 32-bit offset (no wait).
    fn send_rw(&self, cmd: u16, data: *mut u8, len: u32, offset: u32) {
        let r = self.io();
        r.set_command(cmd);
        r.set_data(data);
        r.set_length(len);
        r.set_offset(offset);
        r.set_actual(0);
        // SAFETY: an open request.
        unsafe { sys::exec_send_io(self.0) };
    }
    /// WaitIO; (io_Error, io_Actual).
    fn wait(&self) -> (i8, u32) {
        // SAFETY: an open request that was sent.
        unsafe { sys::exec_wait_io(self.0) };
        (self.io().error(), self.io().actual())
    }
    fn abort(&self) {
        // SAFETY: an open request that was sent.
        unsafe { sys::exec_abort_io(self.0) };
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

struct Tally {
    passed: u32,
    total: u32,
}

impl Tally {
    fn check(&mut self, ok: bool, what: &str, detail: String) -> bool {
        self.total += 1;
        if ok {
            self.passed += 1;
        }
        say!("  {} {}: {}", if ok { "PASS" } else { "FAIL" }, what, detail);
        amigaos4::serial_println!("vscsitest: {} {}: {}", if ok { "PASS" } else { "FAIL" }, what, detail);
        ok
    }
}

fn device_query(r: &Req) -> (i8, u32, u32, u16, Vec<u16>) {
    let mut buf = [0u8; 16];
    let (e, n) = r.do_io(NSCMD_DEVICEQUERY, buf.as_mut_ptr(), buf.len() as u32, 0);
    let format = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let dtype = u16::from_be_bytes([buf[8], buf[9]]);
    let list = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]) as usize as *const u16;
    let mut cmds = Vec::new();
    if e == 0 && !list.is_null() {
        for i in 0..64 {
            // SAFETY: the driver's 0-terminated command list.
            let c = unsafe { *list.add(i) };
            if c == 0 {
                break;
            }
            cmds.push(c);
        }
    }
    (e, n, format, dtype, cmds)
}

/// Block size and blocks, or None without a medium.
fn geometry(r: &Req) -> Result<Option<(u32, u64)>, String> {
    let mut buf = vec![0u8; 4096];
    let mut bs = 0;
    for size in [512u32, 1024, 2048, 4096] {
        let (e, _) = r.do_io(CMD_READ, buf.as_mut_ptr(), size, 0);
        if e == TDERR_DISK_CHANGED {
            return Ok(None);
        }
        if e == 0 {
            bs = size;
            break;
        }
        if e != IOERR_BADLENGTH {
            return Err(alloc::format!("a {}-byte read at 0 answered {}", size, e));
        }
    }
    if bs == 0 {
        return Err("no block size from 512 to 4096 was accepted".into());
    }
    // Bisect: lo is readable, hi is not (2^40 blocks is past any test disk).
    let (mut lo, mut hi) = (0u64, 1u64 << 40);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        let (e, _) = r.do_io(TD_READ64, buf.as_mut_ptr(), bs, mid * bs as u64);
        match e {
            0 => lo = mid,
            IOERR_BADADDRESS => hi = mid,
            e => return Err(alloc::format!("a read of block {} answered {}", mid, e)),
        }
    }
    Ok(Some((bs, lo + 1)))
}

/// Write `len` bytes of the pattern at `off` with `wcmd`, read them back
/// with `rcmd`, compare.
fn write_read(t: &mut Tally, r: &Req, unit: u32, what: &str, wcmd: u16, rcmd: u16, off: u64, len: usize) {
    let mut w = vec![0u8; len];
    fill(&mut w, off, unit);
    let (we, wn) = r.do_io(wcmd, w.as_mut_ptr(), len as u32, off);
    let mut back = vec![0u8; len];
    let (re, rn) = r.do_io(rcmd, back.as_mut_ptr(), len as u32, off);
    let bad = mismatch(&back, off, unit);
    t.check(
        we == 0 && wn == len as u32 && re == 0 && rn == len as u32 && bad.is_none(),
        what,
        alloc::format!("{} bytes at {}: write {}/{}, read {}/{}, first difference {:?}", len, off, we, wn, re, rn, bad),
    );
}

fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Tests 11-14 on a unit with a medium.
fn trackdisk_tests(t: &mut Tally, a: &Req, port: &AmigaMsgPort, name: &[u8], unit: u32, bs: u32, blocks: u64, timer: &mut Option<AmigaTimer>) {
    let mut g = [0xffu8; 32];
    let (e, n) = a.do_io(TD_GETGEOMETRY, g.as_mut_ptr(), 32, 0);
    let total = be32(&g, 4) as u64;
    let (cyl, cs) = (be32(&g, 8) as u64, be32(&g, 12) as u64);
    let chs_ok = if blocks <= u32::MAX as u64 { cyl * cs == blocks && total == blocks } else { total == u32::MAX as u64 };
    t.check(
        e == 0 && n == 32 && be32(&g, 0) == bs && chs_ok && be32(&g, 24) == 0x101 && g[28] == 0 && g[30] == 0 && g[31] == 0,
        "TD_GETGEOMETRY",
        alloc::format!("error {} actual {}: sector {} total {} cyl {} x {} heads {} track {} memtype 0x{:x} type {} flags {}",
            e, n, be32(&g, 0), total, cyl, cs, be32(&g, 16), be32(&g, 20), be32(&g, 24), g[28], g[29]),
    );
    let mut g = [0xffu8; 32];
    let (e, n) = a.do_io(NSCMD_TD_GETGEOMETRY64, g.as_mut_ptr(), 32, 0);
    let total64 = u64::from_be_bytes([g[16], g[17], g[18], g[19], g[20], g[21], g[22], g[23]]);
    t.check(e == 0 && n == 32 && be32(&g, 0) == bs && total64 == blocks && g[8..16].iter().all(|&b| b == 0),
        "NSCMD_TD_GETGEOMETRY64", alloc::format!("error {} actual {}: sector {} total {}", e, n, be32(&g, 0), total64));
    let (e, n) = a.do_io(TD_GETGEOMETRY, g.as_mut_ptr(), 16, 0);
    t.check(e == IOERR_BADLENGTH && n == 0, "TD_GETGEOMETRY, short buffer", alloc::format!("error {} actual {}", e, n));

    let q = [
        (TD_GETDRIVETYPE, DRIVE3_5),
        (TD_CHANGESTATE, 0),
        (TD_PROTSTATUS, 0),
        (CMD_UPDATE, 0),
        (TD_MOTOR, 1),
    ];
    let mut got = Vec::new();
    let mut ok = true;
    for (cmd, want) in q {
        let (e, n) = a.do_io(cmd, core::ptr::null_mut(), 0, 0);
        ok &= e == 0 && n == want;
        got.push((cmd, e, n));
    }
    t.check(ok, "quick commands", alloc::format!("(command, error, actual) {:?}", got));

    // A held change request, and its removal.
    if let (Ok(c1), Ok(c2)) = (Req::open(port, name, unit), Req::open(port, name, unit)) {
        c1.send(TD_ADDCHANGEINT);
        c2.send(TD_ADDCHANGEINT);
        if let Some(tm) = timer.as_mut() {
            let _ = tm.delay(TimerVal::from_millis(200));
        }
        let held = !c1.is_done() && !c2.is_done();
        // TD_REMCHANGEINT on the same (still pending) request.
        let (e, _) = c1.do_io(TD_REMCHANGEINT, core::ptr::null_mut(), 0, 0);
        let e2 = c2.abort_wait();
        t.check(held && e == 0 && e2 == IOERR_ABORTED, "TD_ADDCHANGEINT held; REMCHANGEINT; AbortIO",
            alloc::format!("held after 200 ms: {}; REMCHANGEINT error {}; AbortIO error {} (want {})", held, e, e2, IOERR_ABORTED));
    } else {
        t.check(false, "TD_ADDCHANGEINT held; REMCHANGEINT; AbortIO", "open failed".into());
    }

    // HD_SCSICMD.
    let mut inq = [0u8; 36];
    let (e, st, n, _, _) = a.scsi(&[0x12, 0, 0, 0, 36, 0], &mut inq, true);
    t.check(e == 0 && st == 0 && n == 36 && &inq[8..12] == b"QEMU", "HD_SCSICMD INQUIRY",
        alloc::format!("error {} status {} actual {} vendor {:?}", e, st, n, core::str::from_utf8(&inq[8..16]).unwrap_or("?")));
    let (e, st, n, _, _) = a.scsi(&[0, 0, 0, 0, 0, 0], &mut [], true);
    t.check(e == 0 && st == 0 && n == 0, "HD_SCSICMD TEST UNIT READY", alloc::format!("error {} status {} actual {}", e, st, n));
    let mut via_scsi = vec![0u8; bs as usize];
    let (e, st, n, _, _) = a.scsi(&[0x28, 0, 0, 0, 0, 0, 0, 0, 1, 0], &mut via_scsi, true);
    let mut via_read = vec![0u8; bs as usize];
    let (re, _) = a.do_io(CMD_READ, via_read.as_mut_ptr(), bs, 0);
    t.check(e == 0 && st == 0 && n == bs && re == 0 && via_scsi == via_read, "HD_SCSICMD READ(10) of block 0",
        alloc::format!("error {} status {} actual {}; equal to CMD_READ: {}", e, st, n, via_scsi == via_read));
    let (e, st, n, key, sa) = a.scsi(&[0xc7, 0, 0, 0, 0, 0], &mut [], true);
    t.check(e == HFERR_BAD_STATUS && st == 2 && key == 5 && sa > 0 && n == 0, "HD_SCSICMD unknown opcode",
        alloc::format!("error {} status {} actual {} sense key {} sense bytes {} (want {}, 2, 0, 5, >0)", e, st, n, key, sa, HFERR_BAD_STATUS));
}

fn now_us(timer: &mut AmigaTimer) -> u64 {
    let v = timer.get_sys_time();
    v.secs as u64 * 1_000_000 + v.micros as u64
}

fn unit_tests(t: &mut Tally, port: &AmigaMsgPort, name: &[u8], unit: u32, timer: &mut Option<AmigaTimer>) {
    say!("unit {}:", unit);
    let a = Req::open(port, name, unit);
    let b = Req::open(port, name, unit);
    t.check(a.is_ok() && b.is_ok(), "open twice", alloc::format!("{:?} / {:?}", a.as_ref().err(), b.as_ref().err()));
    let Ok(a) = a else { return };
    drop(b);
    let (e, n, format, dtype, cmds) = device_query(&a);
    t.check(
        e == 0 && format == 0 && dtype == NSDEVTYPE_TRACKDISK && [NSCMD_DEVICEQUERY, NSCMD_TD_READ64, NSCMD_TD_WRITE64, TD_READ64, TD_WRITE64].iter().all(|c| cmds.contains(c)),
        "NSCMD_DEVICEQUERY",
        alloc::format!("error {} actual {} format {} type {} commands {:x?}", e, n, format, dtype, cmds),
    );
    let (e, _) = a.do_io(CMD_INVALID, core::ptr::null_mut(), 0, 0);
    t.check(e == IOERR_NOCMD, "CMD_INVALID", alloc::format!("error {} (want {})", e, IOERR_NOCMD));

    let geo = geometry(&a);
    let (bs, blocks) = match geo {
        Ok(Some(g)) => g,
        Ok(None) => {
            let mut buf = [0u8; 2048];
            let (e, n) = a.do_io(CMD_READ, buf.as_mut_ptr(), 2048, 0);
            t.check(e == TDERR_DISK_CHANGED && n == 0, "no medium", alloc::format!("CMD_READ: error {} actual {}", e, n));
            let (e, n) = a.do_io(TD_CHANGESTATE, core::ptr::null_mut(), 0, 0);
            t.check(e == 0 && n != 0, "no medium: TD_CHANGESTATE", alloc::format!("error {} actual {} (want 0, not 0)", e, n));
            let (e, st, _, key, _) = a.scsi(&[0, 0, 0, 0, 0, 0], &mut [], true);
            t.check(e == HFERR_BAD_STATUS && st == 2 && key == 2, "no medium: HD_SCSICMD TEST UNIT READY",
                alloc::format!("error {} status {} sense key {} (want {}, 2, 2)", e, st, key, HFERR_BAD_STATUS));
            return;
        }
        Err(why) => {
            t.check(false, "geometry", why);
            return;
        }
    };
    let size = blocks * bs as u64;
    t.check(true, "geometry", alloc::format!("{} blocks of {} bytes ({} MB)", blocks, bs, size >> 20));
    trackdisk_tests(t, &a, port, name, unit, bs, blocks, timer);

    write_read(t, &a, unit, "CMD_WRITE 64 KiB at 0", CMD_WRITE, CMD_READ, 0, 65536);
    write_read(t, &a, unit, "CMD_WRITE 300 KiB at 1 MiB", CMD_WRITE, CMD_READ, 1 << 20, 300 * 1024);
    write_read(t, &a, unit, "TD_FORMAT 4 blocks at 2 MiB", TD_FORMAT, CMD_READ, 2 << 20, 4 * bs as usize);
    write_read(t, &a, unit, "TD_WRITE64 the last block", TD_WRITE64, TD_READ64, size - bs as u64, bs as usize);

    // An odd buffer address, with guards.
    {
        let len = 4 * bs as usize;
        let mut buf = vec![0xa5u8; len + 64];
        let off = 1u64 << 20;
        let (e, n) = a.do_io(CMD_READ, unsafe { buf.as_mut_ptr().add(33) }, len as u32, off);
        let bad = mismatch(&buf[33..33 + len], off, unit);
        let guards = buf[..33].iter().chain(buf[33 + len..].iter()).all(|&g| g == 0xa5);
        t.check(e == 0 && n == len as u32 && bad.is_none() && guards, "read into an odd address",
            alloc::format!("error {} actual {}, first difference {:?}, guards {}", e, n, bad, if guards { "intact" } else { "CHANGED" }));
    }

    if size > 5u64 << 30 {
        let off = 5u64 << 30;
        write_read(t, &a, unit, "NSCMD_TD_WRITE64 at 5 GiB", NSCMD_TD_WRITE64, NSCMD_TD_READ64, off, 8192.max(bs as usize));
        let len = 8192.max(bs as usize);
        let mut back = vec![0u8; len];
        let (e, n) = a.do_io(TD_READ64, back.as_mut_ptr(), len as u32, off);
        let bad = mismatch(&back, off, unit);
        t.check(e == 0 && n == len as u32 && bad.is_none(), "TD_READ64 at 5 GiB", alloc::format!("error {} actual {}, first difference {:?}", e, n, bad));
    }

    // Rejected requests: nothing is read, nothing changes.
    let mut buf = vec![0u8; 2 * bs as usize];
    let cases: [(&str, u16, u32, u64, i8); 4] = [
        ("offset not a whole block", CMD_READ, bs, (bs / 2) as u64, IOERR_BADLENGTH),
        ("length not a whole block", CMD_READ, bs + bs / 2, 0, IOERR_BADLENGTH),
        ("start at the end", TD_READ64, bs, size, IOERR_BADADDRESS),
        ("range across the end", TD_READ64, 2 * bs, size - bs as u64, IOERR_BADADDRESS),
    ];
    for (what, cmd, len, off, want) in cases {
        let (e, n) = a.do_io(cmd, buf.as_mut_ptr(), len, off);
        t.check(e == want && n == 0, what, alloc::format!("error {} actual {} (want {}, 0)", e, n, want));
    }
    let (e, n) = a.do_io(CMD_READ, buf.as_mut_ptr(), 0, 0);
    t.check(e == 0 && n == 0, "zero-length read", alloc::format!("error {} actual {}", e, n));

    // Speed, shown only.
    if let Some(tm) = timer.as_mut() {
        if size >= 32 << 20 {
            let mut big = vec![0u8; 1 << 20];
            let base = 16u64 << 20;
            let t0 = now_us(tm);
            let mut ok = true;
            for i in 0..4u64 {
                fill(&mut big, base + (i << 20), unit);
                ok &= a.do_io(CMD_WRITE, big.as_mut_ptr(), 1 << 20, base + (i << 20)).0 == 0;
            }
            let t1 = now_us(tm);
            for i in 0..4u64 {
                ok &= a.do_io(CMD_READ, big.as_mut_ptr(), 1 << 20, base + (i << 20)).0 == 0;
                ok &= mismatch(&big, base + (i << 20), unit).is_none();
            }
            let t2 = now_us(tm);
            let kbs = |us: u64| if us == 0 { 0 } else { 4 * 1024 * 1_000_000 / us };
            t.check(ok, "4 MiB write + read at 16 MiB",
                alloc::format!("write {} KB/s, read {} KB/s (the read also compares)", kbs(t1 - t0), kbs(t2 - t1)));
        }
    }
}

/// VSCSI_CMD_TESTFAULT (the development build's private command).
const VSCSI_CMD_TESTFAULT: u16 = 0x7ff0;

/// `n` requests, each `len` bytes of the pattern, written at once and read
/// back at once: (unit, offset) per request. True when all are right.
fn burst(t: &mut Tally, port: &AmigaMsgPort, name: &[u8], what: &str, places: &[(u32, u32)], len: usize) {
    let reqs: Vec<Result<Req, i8>> = places.iter().map(|&(u, _)| Req::open(port, name, u)).collect();
    if reqs.iter().any(|r| r.is_err()) {
        t.check(false, what, "open failed".into());
        return;
    }
    let reqs: Vec<Req> = reqs.into_iter().map(|r| r.ok().unwrap()).collect();
    let mut bufs: Vec<Vec<u8>> = places.iter().map(|&(u, off)| {
        let mut b = vec![0u8; len];
        fill(&mut b, off as u64, u);
        b
    }).collect();
    for (i, r) in reqs.iter().enumerate() {
        r.send_rw(CMD_WRITE, bufs[i].as_mut_ptr(), len as u32, places[i].1);
    }
    let writes: Vec<(i8, u32)> = reqs.iter().map(|r| r.wait()).collect();
    for b in bufs.iter_mut() {
        b.iter_mut().for_each(|x| *x = 0);
    }
    for (i, r) in reqs.iter().enumerate() {
        r.send_rw(CMD_READ, bufs[i].as_mut_ptr(), len as u32, places[i].1);
    }
    let reads: Vec<(i8, u32)> = reqs.iter().map(|r| r.wait()).collect();
    let bad: Vec<usize> = (0..places.len())
        .filter(|&i| writes[i] != (0, len as u32) || reads[i] != (0, len as u32) || mismatch(&bufs[i], places[i].1 as u64, places[i].0).is_some())
        .collect();
    t.check(bad.is_empty(), what, alloc::format!("{} requests of {} bytes; wrong: {:?}", places.len(), len, bad));
}

fn pipeline_tests(t: &mut Tally, port: &AmigaMsgPort, name: &[u8], first: u32, units: u32, timer: &mut Option<AmigaTimer>) {
    say!("pipeline:");
    let base = 8u32 << 20;
    let k64 = 65536u32;
    let one: Vec<(u32, u32)> = (0..16).map(|i| (first, base + i * k64)).collect();
    burst(t, port, name, "16 at once on the first unit", &one, k64 as usize);
    if units >= first + 2 {
        let two: Vec<(u32, u32)> = (0..16).map(|i| (first + i % 2, base + (i / 2) * k64 + 4096)).collect();
        burst(t, port, name, "8 + 8 at once on the first two units", &two, 16384);
    }

    // 20 reads of 1 MiB, the last aborted.
    let mib = 1u32 << 20;
    let reqs: Vec<Req> = (0..20).filter_map(|_| Req::open(port, name, first).ok()).collect();
    if reqs.len() == 20 {
        // The data there: 1 MiB of pattern at 8 MiB.
        let mut w = vec![0u8; mib as usize];
        fill(&mut w, base as u64, first);
        let _ = reqs[0].do_io(CMD_WRITE, w.as_mut_ptr(), mib, base as u64);
        let mut bufs: Vec<Vec<u8>> = (0..20).map(|_| vec![0u8; mib as usize]).collect();
        // Above the driver's task (priority 5) while sending, so all 20 are
        // queued before it runs: the last is still waiting when aborted.
        // SAFETY: our own task.
        let old = unsafe { sys::exec_set_task_pri(sys::exec_find_task(core::ptr::null_mut()), 10) };
        for (i, r) in reqs.iter().enumerate() {
            r.send_rw(CMD_READ, bufs[i].as_mut_ptr(), mib, base);
        }
        reqs[19].abort();
        unsafe { sys::exec_set_task_pri(sys::exec_find_task(core::ptr::null_mut()), old) };
        let res: Vec<(i8, u32)> = reqs.iter().map(|r| r.wait()).collect();
        let others_ok = (0..19).all(|i| res[i] == (0, mib) && mismatch(&bufs[i], base as u64, first).is_none());
        let last = res[19];
        let last_ok = last == (IOERR_ABORTED, 0);
        t.check(others_ok && last_ok, "20 x 1 MiB at once, AbortIO on the last",
            alloc::format!("19 others right: {}; the last: error {} actual {} ({})", others_ok, last.0, last.1,
                if last.0 == IOERR_ABORTED { "aborted" } else { "finished first" }));
    } else {
        t.check(false, "20 x 1 MiB at once, AbortIO on the last", "open failed".into());
    }

    // Recovery.
    if let Ok(r) = Req::open(port, name, first) {
        let (e, _) = r.do_io(VSCSI_CMD_TESTFAULT, core::ptr::null_mut(), 0, 0);
        if e == IOERR_NOCMD {
            t.check(true, "recovery", "not a development build (no VSCSI_CMD_TESTFAULT); skipped".into());
            return;
        }
        let t0 = timer.as_mut().map(|tm| now_us(tm)).unwrap_or(0);
        let mut b = vec![0u8; 65536];
        let (re, rn) = r.do_io(CMD_READ, b.as_mut_ptr(), 65536, base as u64);
        let t1 = timer.as_mut().map(|tm| now_us(tm)).unwrap_or(0);
        let bad = mismatch(&b, base as u64, first);
        t.check(e == 0 && re == 0 && rn == 65536 && bad.is_none(), "recovery after a reset behind the driver's back",
            alloc::format!("fault {}; read error {} actual {} first difference {:?}, {} ms", e, re, rn, bad, (t1.saturating_sub(t0)) / 1000));
    }
}

fn run(name: &[u8], first: u32, units: u32, total: u32) -> Tally {
    let mut t = Tally { passed: 0, total: 0 };
    let port = match AmigaMsgPort::new() {
        Ok(p) => p,
        Err(_) => {
            t.check(false, "setup", String::from("CreateMsgPort failed"));
            return t;
        }
    };
    let mut timer = AmigaTimer::open(TimerUnit::MicroHz).ok();
    for unit in first..units {
        unit_tests(&mut t, &port, name, unit, &mut timer);
    }
    let r = Req::open(&port, name, total);
    t.check(r.is_err(), "open past the last unit", alloc::format!("unit {}: {:?}", total, r.as_ref().err()));
    if units > first {
        pipeline_tests(&mut t, &port, name, first, units, &mut timer);
    }
    t
}

#[no_mangle]
pub extern "C" fn rust_main(args: *const u8, arglen: i32) -> i32 {
    // SAFETY: the Shell's argument string, `arglen` bytes.
    let text = unsafe { core::slice::from_raw_parts(args, arglen.max(0) as usize) };
    let mut words = text.split(|c| c.is_ascii_whitespace()).filter(|w| !w.is_empty());
    let first_word = words.clone().next();
    if first_word.map(|w| w.eq_ignore_ascii_case(b"TORTURE")).unwrap_or(false) {
        let _ = words.next();
        let num = |w: Option<&[u8]>| w.and_then(|w| core::str::from_utf8(w).ok()).and_then(|s| s.parse::<i64>().ok());
        let (Some(dev), secs, Some(list)) = (words.next(), num(words.next()), words.next()) else {
            say!("usage: vscsitest TORTURE <device> <secs> <units,...> [kids] [kidpri]   (WRITES TO THOSE UNITS)");
            return 20;
        };
        let units: Vec<u32> = list
            .split(|&c| c == b',')
            .filter_map(|u| core::str::from_utf8(u).ok().and_then(|s| s.parse::<u32>().ok()))
            .collect();
        let kids = num(words.next()).unwrap_or(8).clamp(1, 8) as usize;
        let kidpri = num(words.next()).unwrap_or(0).clamp(-20, 4) as i32;
        let secs = secs.unwrap_or(60).clamp(1, 24 * 3600) as u32;
        if units.is_empty() {
            say!("vscsitest TORTURE: no units given");
            return 20;
        }
        let mut name: Vec<u8> = dev.to_vec();
        name.push(0);
        return torture::torture(&name, secs, &units, kids, kidpri);
    }
    let (Some(dev), Some(n)) = (words.next(), words.next()) else {
        say!("usage: vscsitest <device> <units>   (WRITES TO EVERY UNIT)");
        return 20;
    };
    let units = core::str::from_utf8(n).ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let total = words.next().and_then(|w| core::str::from_utf8(w).ok()).and_then(|s| s.parse::<u32>().ok()).unwrap_or(units).max(units);
    let first = words.next().and_then(|w| core::str::from_utf8(w).ok()).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0).min(units);
    let mut name: Vec<u8> = dev.to_vec();
    name.push(0);
    let shown = core::str::from_utf8(dev).unwrap_or("?");
    say!("vscsitest: {} with {} unit(s)", shown, units);
    say!("vscsitest: testing units {} to {}", first, units.saturating_sub(1));
    let t = run(&name, first, units, total);
    say!("vscsitest: {}/{} tests passed", t.passed, t.total);
    amigaos4::serial_println!("vscsitest: {}/{} tests passed", t.passed, t.total);
    if t.passed == t.total { 0 } else { 20 }
}
