//! s2test: drive a SANA-II Ethernet device directly, without a TCP/IP stack,
//! and check what it does. `s2test [unit] [device]` (default unit 0 of
//! virtionet.device). Made for QEMU user networking, where 10.0.x.2 is the
//! host side and answers ARP and ICMP echo:
//!
//!   s2test 0 virtionet.device 10.0.3.15 10.0.3.2
//!
//! Two openers at once: A gives its copy functions as OpenDevice tags
//! (native code), B installs a copy hook (S2_SANA2HOOK). Each check prints
//! PASS or FAIL; the tool returns 0 when all pass. Modes: events, prom,
//! stats, burst, and the torture modes in torture.rs (torture, hold, closepend).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::fmt::{self, Write};
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU32, Ordering};

use amigaos4::allocator::ExecAllocator;
use amigaos4::device::cmd::{CMD_READ, CMD_WRITE, NSCMD_DEVICEQUERY};
use amigaos4::device::ioerr::{IOERR_ABORTED, IOERR_UNITBUSY};
use amigaos4::port::AmigaMsgPort;
use amigaos4::sana2::*;
use amigaos4::sys::{self, Tag, TagItem};
use amigaos4::timer::{AmigaTimer, TimerUnit, TimerVal};

#[global_allocator]
static ALLOCATOR: ExecAllocator = ExecAllocator;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    amigaos4::panic::default_panic_handler(info)
}

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

// ---- The stack side of buffer management ----
//
// A "buffer" cookie is a pointer to a Buf: the bytes and their length.

const BUF_CAP: usize = 2048;
#[repr(C)]
struct Buf {
    len: u32,
    data: [u8; BUF_CAP],
}

static TAG_COPIES: AtomicU32 = AtomicU32::new(0);
static HOOK_COPIES: AtomicU32 = AtomicU32::new(0);

unsafe fn copy_to(to: *mut c_void, from: *const u8, n: u32) -> u32 {
    let b = to as *mut Buf;
    if b.is_null() || n as usize > BUF_CAP {
        return 0;
    }
    core::ptr::copy_nonoverlapping(from, (*b).data.as_mut_ptr(), n as usize);
    (*b).len = n;
    1
}
unsafe fn copy_from(to: *mut u8, from: *const c_void, n: u32) -> u32 {
    let b = from as *const Buf;
    if b.is_null() || n > (*b).len {
        return 0;
    }
    core::ptr::copy_nonoverlapping((*b).data.as_ptr(), to, n as usize);
    1
}

// OpenDevice tag functions, native: (to, from, n) -> BOOL.
unsafe extern "C" fn tag_copy_to(to: *mut c_void, from: *mut c_void, n: u32) -> u32 {
    TAG_COPIES.fetch_add(1, Ordering::Relaxed);
    copy_to(to, from as *const u8, n)
}
unsafe extern "C" fn tag_copy_from(to: *mut c_void, from: *mut c_void, n: u32) -> u32 {
    TAG_COPIES.fetch_add(1, Ordering::Relaxed);
    copy_from(to as *mut u8, from, n)
}

// The S2_SANA2HOOK hook: (hook, request, SANA2CopyHookMsg) -> BOOL.
unsafe extern "C" fn hook_entry(_h: *mut Hook, _ior: *mut c_void, msg: *mut c_void) -> u32 {
    let m = msg as *mut SANA2CopyHookMsg;
    let size = core::ptr::addr_of!((*m).schm_MsgSize).read_unaligned();
    if size < 20 {
        return 0;
    }
    HOOK_COPIES.fetch_add(1, Ordering::Relaxed);
    let to = core::ptr::addr_of!((*m).schm_To).read_unaligned();
    let from = core::ptr::addr_of!((*m).schm_From).read_unaligned();
    let n = core::ptr::addr_of!((*m).schm_Size).read_unaligned();
    match core::ptr::addr_of!((*m).schm_Method).read_unaligned() {
        S2_COPY_TO_BUFF => copy_to(to, from as *const u8, n),
        S2_COPY_FROM_BUFF => copy_from(to as *mut u8, from, n),
        _ => 0,
    }
}

static HOOK_METHODS: [Tag; 3] = [S2_COPY_TO_BUFF, S2_COPY_FROM_BUFF, sys::TAG_DONE];

// ---- Requests ----

struct Req {
    r: S2Req,
    buf: alloc::boxed::Box<Buf>,
}

impl Req {
    fn data_mut(&mut self) -> &mut [u8] {
        &mut self.buf.data
    }
    fn cookie(&mut self) -> *mut c_void {
        &mut *self.buf as *mut Buf as *mut c_void
    }
    fn received(&self) -> &[u8] {
        &self.buf.data[..self.buf.len as usize]
    }
    fn set(&mut self, cmd: u16, flags: u8) {
        let c = self.cookie();
        let io = self.r.io();
        io.set_command(cmd);
        io.set_flags(flags);
        io.set_error(0);
        self.r.set_wire_error(0);
        self.r.set_data(c);
    }
}

struct Ctx {
    port: AmigaMsgPort,
    timer: AmigaTimer,
    devname: Vec<u8>,
    unit: u32,
    passed: u32,
    failed: u32,
}

impl Ctx {
    fn check(&mut self, name: &str, ok: bool, detail: String) {
        if ok {
            self.passed += 1;
            say!("PASS {}: {}", name, detail);
        } else {
            self.failed += 1;
            say!("FAIL {}: {}", name, detail);
        }
        amigaos4::serial_println!("s2test {} {}: {}", if ok { "PASS" } else { "FAIL" }, name, detail);
    }

    /// A new request on our port, unopened.
    fn new_req(&self) -> Option<Req> {
        // SAFETY: plain exec calls.
        unsafe {
            let ior = sys::exec_create_iorequest(self.port.as_ptr(), 88);
            let r = S2Req::from_raw(ior)?;
            Some(Req { r, buf: alloc::boxed::Box::new(Buf { len: 0, data: [0; BUF_CAP] }) })
        }
    }

    /// A copy of an opened request (device, unit and buffer management).
    fn clone_req(&self, base: &Req) -> Option<Req> {
        let n = self.new_req()?;
        // SAFETY: both are 88-byte requests; the reply port is ours in both.
        unsafe { core::ptr::copy_nonoverlapping(base.r.raw() as *const u8, n.r.raw() as *mut u8, 88) };
        Some(n)
    }

    fn open(&self, tags: &[TagItem], flags: u32) -> Result<Req, i8> {
        let req = self.new_req().ok_or(-1i8)?;
        req.r.set_buffer_management(tags.as_ptr() as *mut c_void);
        // SAFETY: an 88-byte request and a NUL-terminated name.
        let e = unsafe { sys::exec_open_device(self.devname.as_ptr() as _, self.unit, req.r.as_ptr(), flags) };
        if e != 0 { Err(e as i8) } else { Ok(req) }
    }

    fn close(&self, req: Req) {
        // SAFETY: an opened request.
        unsafe {
            sys::exec_close_device(req.r.as_ptr());
            sys::exec_delete_iorequest(req.r.as_ptr());
        }
    }

    fn do_io(&self, req: &Req) -> (i8, u32) {
        // SAFETY: an opened request.
        unsafe { sys::exec_do_io(req.r.as_ptr()) };
        (req.r.io().error(), req.r.wire_error())
    }

    /// Start a request with its io_Flags as set (BeginIO: SendIO would
    /// clear them, and SANA2IOF_RAW with them).
    fn send(&self, req: &Req) {
        // SAFETY: an opened request.
        unsafe { sys::exec_begin_io(req.r.as_ptr()) };
    }

    /// Wait for a sent request, at most `ms`. None: it did not come back.
    fn wait(&mut self, req: &Req, ms: u32) -> Option<(i8, u32)> {
        // SAFETY: a sent request.
        unsafe {
            self.timer.start(TimerVal::from_millis(ms));
            loop {
                if !sys::exec_check_io(req.r.as_ptr()).is_null() {
                    self.timer.abort();
                    sys::exec_wait_io(req.r.as_ptr());
                    return Some((req.r.io().error(), req.r.wire_error()));
                }
                sys::exec_wait(self.port.signal_mask() | self.timer.signal_mask());
                if self.timer.is_done() && sys::exec_check_io(req.r.as_ptr()).is_null() {
                    self.timer.finish();
                    return None;
                }
            }
        }
    }

    fn abort_wait(&mut self, req: &Req) -> (i8, u32) {
        // SAFETY: a sent request.
        unsafe { sys::exec_abort_io(req.r.as_ptr()) };
        self.wait(req, 2000).unwrap_or((99, 0))
    }

    /// Abort a sent request; true if it came back within 2 s.
    fn wait_abort_ok(&mut self, req: &Req) -> bool {
        // SAFETY: a sent request.
        unsafe { sys::exec_abort_io(req.r.as_ptr()) };
        self.wait(req, 2000).is_some()
    }

    /// OpenDevice on another unit of the same device.
    fn open_unit(&self, tags: &[TagItem], flags: u32, unit: u32) -> Result<Req, i8> {
        let req = self.new_req().ok_or(-1i8)?;
        req.r.set_buffer_management(tags.as_ptr() as *mut c_void);
        // SAFETY: an 88-byte request and a NUL-terminated name.
        let e = unsafe { sys::exec_open_device(self.devname.as_ptr() as _, unit, req.r.as_ptr(), flags) };
        if e != 0 {
            // SAFETY: never opened.
            unsafe { sys::exec_delete_iorequest(req.r.as_ptr()) };
            Err(e as i8)
        } else {
            Ok(req)
        }
    }
}

fn mac(m: &[u8]) -> String {
    alloc::format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    for c in data.chunks(2) {
        sum += (c[0] as u32) << 8 | *c.get(1).unwrap_or(&0) as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn arp_request(src_mac: &[u8; 6], src_ip: [u8; 4], dst_ip: [u8; 4]) -> [u8; 28] {
    let mut a = [0u8; 28];
    a[0..2].copy_from_slice(&1u16.to_be_bytes()); // Ethernet
    a[2..4].copy_from_slice(&0x0800u16.to_be_bytes()); // IPv4
    a[4] = 6;
    a[5] = 4;
    a[6..8].copy_from_slice(&1u16.to_be_bytes()); // request
    a[8..14].copy_from_slice(src_mac);
    a[14..18].copy_from_slice(&src_ip);
    a[24..28].copy_from_slice(&dst_ip);
    a
}

fn icmp_echo(src: [u8; 4], dst: [u8; 4], id: u16, seq: u16, payload: &[u8]) -> Vec<u8> {
    let mut icmp = vec![8u8, 0, 0, 0];
    icmp.extend_from_slice(&id.to_be_bytes());
    icmp.extend_from_slice(&seq.to_be_bytes());
    icmp.extend_from_slice(payload);
    let c = checksum(&icmp);
    icmp[2..4].copy_from_slice(&c.to_be_bytes());
    let total = 20 + icmp.len();
    let mut ip = vec![0x45u8, 0];
    ip.extend_from_slice(&(total as u16).to_be_bytes());
    ip.extend_from_slice(&[0x12, 0x34, 0, 0, 64, 1, 0, 0]);
    ip.extend_from_slice(&src);
    ip.extend_from_slice(&dst);
    let c = checksum(&ip);
    ip[10..12].copy_from_slice(&c.to_be_bytes());
    ip.extend_from_slice(&icmp);
    ip
}

fn parse_ip(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut it = s.split('.');
    for o in out.iter_mut() {
        *o = it.next()?.parse().ok()?;
    }
    if it.next().is_some() { None } else { Some(out) }
}

fn run(ctx: &mut Ctx, my_ip: [u8; 4], gw_ip: [u8; 4], want_mtu: u32, want_bps: u32) {
    // ---- Openers ----
    let tags_a = [
        TagItem { ti_Tag: S2_COPY_TO_BUFF, ti_Data: tag_copy_to as *const () as usize as u32 },
        TagItem { ti_Tag: S2_COPY_FROM_BUFF, ti_Data: tag_copy_from as *const () as usize as u32 },
        TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
    ];
    let mut a = match ctx.open(&tags_a, 0) {
        Ok(r) => r,
        Err(e) => {
            ctx.check("open", false, alloc::format!("OpenDevice returned {}", e));
            return;
        }
    };
    let mut b = match ctx.open(&tags_a, 0) {
        Ok(r) => r,
        Err(e) => {
            ctx.check("open-second", false, alloc::format!("OpenDevice returned {}", e));
            ctx.close(a);
            return;
        }
    };
    ctx.check("open", true, String::from("two openers on the unit"));

    // B installs a hook.
    let mut hook = alloc::boxed::Box::new(Sana2Hook {
        s2h_Hook: Hook {
            h_MinNode: MinNode { mln_Succ: core::ptr::null_mut(), mln_Pred: core::ptr::null_mut() },
            h_Entry: Some(hook_entry),
            h_SubEntry: None,
            h_Data: core::ptr::null_mut(),
        },
        s2h_Methods: HOOK_METHODS.as_ptr(),
    });
    b.set(S2_SANA2HOOK, 0);
    b.r.set_data(&mut *hook as *mut Sana2Hook as *mut c_void);
    b.r.set_data_length(24);
    let (e, w) = ctx.do_io(&b);
    ctx.check("sana2hook", e == 0, alloc::format!("error {} wire {}", e, w));
    b.set(S2_SANA2HOOK, 0);
    b.r.set_data(&mut *hook as *mut Sana2Hook as *mut c_void);
    b.r.set_data_length(24);
    let (e, _) = ctx.do_io(&b);
    ctx.check("sana2hook-twice", e == IOERR_UNITBUSY, alloc::format!("second hook: error {} (IOERR_UNITBUSY is -6)", e));

    // Exclusive open while others are open.
    match ctx.open(&tags_a, SANA2OPF_MINE) {
        Ok(r) => {
            ctx.check("exclusive-refused", false, String::from("an exclusive open succeeded while others were open"));
            ctx.close(r);
        }
        Err(e) => ctx.check("exclusive-refused", e == IOERR_UNITBUSY, alloc::format!("OpenDevice returned {}", e)),
    }

    // ---- Queries ----
    let mut q = [0u8; 34];
    q[0..4].copy_from_slice(&34u32.to_ne_bytes());
    a.set(S2_DEVICEQUERY, 0);
    a.r.set_stat_data(q.as_mut_ptr() as *mut c_void);
    let (e, _) = ctx.do_io(&a);
    let rd = |o: usize| u32::from_be_bytes([q[o], q[o + 1], q[o + 2], q[o + 3]]);
    let addr_bits = u16::from_be_bytes([q[16], q[17]]);
    ctx.check(
        "devicequery",
        e == 0 && rd(4) == 34 && addr_bits == 48 && rd(18) == want_mtu && rd(22) == want_bps && rd(26) == 1 && rd(30) == want_mtu + 14,
        alloc::format!("error {} supplied {} addr {} bits MTU {} {} bit/s type {} raw MTU {}", e, rd(4), addr_bits, rd(18), rd(22), rd(26), rd(30)),
    );
    let mut q30 = [0u8; 30];
    q30[0..4].copy_from_slice(&30u32.to_ne_bytes());
    a.set(S2_DEVICEQUERY, 0);
    a.r.set_stat_data(q30.as_mut_ptr() as *mut c_void);
    let (e, _) = ctx.do_io(&a);
    let sup = u32::from_be_bytes([q30[4], q30[5], q30[6], q30[7]]);
    ctx.check("devicequery-short", e == 0 && sup == 30, alloc::format!("30-byte query: error {} supplied {}", e, sup));

    let mut nsq = [0u8; 16];
    a.set(NSCMD_DEVICEQUERY, 0);
    a.r.io().set_data(nsq.as_mut_ptr());
    a.r.io().set_length(16);
    // SAFETY: the same request as an IOStdReq for the NSD query.
    unsafe { sys::exec_do_io(a.r.as_ptr()) };
    let devtype = u16::from_be_bytes([nsq[8], nsq[9]]);
    ctx.check("nsd-devicequery", a.r.io().error() == 0 && devtype == NSDEVTYPE_SANA2, alloc::format!("type {} (SANA-II is 7)", devtype));

    a.set(S2_GETSTATIONADDRESS, 0);
    let (e, _) = ctx.do_io(&a);
    let factory = a.r.dst_mac();
    let current = a.r.src_mac();
    ctx.check("stationaddress", e == 0 && factory != [0; 6] && factory[0] & 1 == 0, alloc::format!("current {} factory {}", mac(&current), mac(&factory)));

    // ---- Before configuration ----
    let payload = arp_request(&factory, my_ip, gw_ip);
    a.set(S2_BROADCAST, 0);
    a.r.set_packet_type(0x0806);
    a.data_mut()[..28].copy_from_slice(&payload);
    a.buf.len = 28;
    a.r.set_data(a.cookie());
    a.r.set_data_length(28);
    let (e, w) = ctx.do_io(&a);
    ctx.check("write-unconfigured", e == S2ERR_BAD_STATE && w == S2WERR_NOT_CONFIGURED, alloc::format!("error {} wire {}", e, w));

    // ---- Configure, with an ONLINE event pending ----
    let ev = ctx.clone_req(&a).unwrap();
    ev.r.io().set_command(S2_ONEVENT);
    ev.r.set_wire_error(S2EVENT_ONLINE);
    ctx.send(&ev);
    a.set(S2_CONFIGINTERFACE, 0);
    a.r.set_src_mac(&factory);
    let (e, w) = ctx.do_io(&a);
    ctx.check("configinterface", e == 0, alloc::format!("error {} wire {}", e, w));
    let evr = ctx.wait(&ev, 2000);
    ctx.check("event-online", evr == Some((0, S2EVENT_ONLINE)), alloc::format!("{:?}", evr));
    a.set(S2_CONFIGINTERFACE, 0);
    a.r.set_src_mac(&factory);
    let (e, w) = ctx.do_io(&a);
    ctx.check("configure-twice", e == S2ERR_BAD_STATE && w == S2WERR_IS_CONFIGURED, alloc::format!("error {} wire {}", e, w));
    let ev2 = ctx.clone_req(&a).unwrap();
    ev2.r.io().set_command(S2_ONEVENT);
    ev2.r.set_wire_error(S2EVENT_ONLINE);
    let (e, w) = ctx.do_io(&ev2);
    ctx.check("event-online-immediate", e == 0 && w == S2EVENT_ONLINE, alloc::format!("already online: error {} wire 0x{:x}", e, w));

    // ---- Track ARP ----
    a.set(S2_TRACKTYPE, 0);
    a.r.set_packet_type(0x0806);
    let (e, _) = ctx.do_io(&a);
    a.set(S2_TRACKTYPE, 0);
    a.r.set_packet_type(0x0806);
    let (e2, w2) = ctx.do_io(&a);
    ctx.check("tracktype", e == 0 && w2 == S2WERR_ALREADY_TRACKED && e2 == S2ERR_BAD_STATE, alloc::format!("track {} again {}/{}", e, e2, w2));

    // ---- ARP: one read on each opener, a broadcast request ----
    let mut ra = ctx.clone_req(&a).unwrap();
    ra.set(CMD_READ, 0);
    ra.r.set_packet_type(0x0806);
    let mut rb = ctx.clone_req(&b).unwrap();
    rb.set(CMD_READ, 0);
    rb.r.set_packet_type(0x0806);
    ctx.send(&ra);
    ctx.send(&rb);
    let tags_before = TAG_COPIES.load(Ordering::Relaxed);
    let hook_before = HOOK_COPIES.load(Ordering::Relaxed);
    a.set(S2_BROADCAST, 0);
    a.r.set_packet_type(0x0806);
    a.data_mut()[..28].copy_from_slice(&payload);
    a.buf.len = 28;
    a.r.set_data(a.cookie());
    a.r.set_data_length(28);
    let (e, w) = ctx.do_io(&a);
    ctx.check("broadcast", e == 0, alloc::format!("ARP request: error {} wire {}", e, w));
    let wa = ctx.wait(&ra, 3000);
    let wb = ctx.wait(&rb, 3000);
    let reply_a = ra.received().to_vec();
    let gw_mac = ra.r.src_mac();
    // Padded to the 60-byte Ethernet minimum: 28 bytes of ARP, then padding.
    let ok_a = wa == Some((0, 0)) && reply_a.len() >= 28 && reply_a[7] == 2 && reply_a[14..18] == gw_ip;
    ctx.check(
        "arp-reply",
        ok_a,
        alloc::format!("{:?} {} bytes from {}, opcode {}, flags 0x{:02x}", wa, reply_a.len(), mac(&gw_mac), reply_a.get(7).copied().unwrap_or(0), ra.r.io().flags()),
    );
    ctx.check(
        "arp-both-openers",
        wb == Some((0, 0)) && rb.received() == &reply_a[..],
        alloc::format!("opener B (hook): {:?}, same bytes: {}", wb, rb.received() == &reply_a[..]),
    );
    let tags_used = TAG_COPIES.load(Ordering::Relaxed) - tags_before;
    let hook_used = HOOK_COPIES.load(Ordering::Relaxed) - hook_before;
    ctx.check("copy-paths", tags_used >= 2 && hook_used >= 1, alloc::format!("tag copies {} (native functions), hook copies {}", tags_used, hook_used));

    // ---- Raw read ----
    let mut rb2 = ctx.clone_req(&b).unwrap();
    rb2.set(CMD_READ, SANA2IOF_RAW);
    rb2.r.set_packet_type(0x0806);
    ctx.send(&rb2);
    a.set(S2_BROADCAST, 0);
    a.r.set_packet_type(0x0806);
    a.data_mut()[..28].copy_from_slice(&payload);
    a.buf.len = 28;
    a.r.set_data(a.cookie());
    a.r.set_data_length(28);
    ctx.do_io(&a);
    let wr = ctx.wait(&rb2, 3000);
    let raw = rb2.received();
    ctx.check(
        "raw-read",
        wr == Some((0, 0)) && raw.len() >= 42 && raw[12..14] == [0x08, 0x06] && raw[0..6] == factory,
        alloc::format!("{:?} {} bytes, type {:02x}{:02x}", wr, raw.len(), raw.get(12).copied().unwrap_or(0), raw.get(13).copied().unwrap_or(0)),
    );

    // ---- Orphan read: no CMD_READ of the reply's type pending anywhere ----
    let mut orph = ctx.clone_req(&a).unwrap();
    orph.set(S2_READORPHAN, 0);
    ctx.send(&orph);
    a.set(S2_BROADCAST, 0);
    a.r.set_packet_type(0x0806);
    a.data_mut()[..28].copy_from_slice(&payload);
    a.buf.len = 28;
    a.r.set_data(a.cookie());
    a.r.set_data_length(28);
    ctx.do_io(&a);
    let wo = ctx.wait(&orph, 3000);
    ctx.check(
        "orphan-read",
        wo == Some((0, 0)) && orph.r.packet_type() == 0x0806 && orph.received().len() >= 28,
        alloc::format!("{:?} type 0x{:04x} {} bytes", wo, orph.r.packet_type(), orph.received().len()),
    );

    // ---- ICMP echo to the gateway ----
    let mut ri = ctx.clone_req(&b).unwrap();
    ri.set(CMD_READ, 0);
    ri.r.set_packet_type(0x0800);
    ctx.send(&ri);
    let pay: Vec<u8> = (0..56u8).collect();
    let ip = icmp_echo(my_ip, gw_ip, 0x4242, 1, &pay);
    a.set(CMD_WRITE, 0);
    a.r.set_packet_type(0x0800);
    a.r.set_dst_mac(&gw_mac);
    a.data_mut()[..ip.len()].copy_from_slice(&ip);
    a.buf.len = ip.len() as u32;
    a.r.set_data(a.cookie());
    a.r.set_data_length(ip.len() as u32);
    let (e, _) = ctx.do_io(&a);
    let wi = ctx.wait(&ri, 3000);
    let got = ri.received();
    let echo_ok = wi == Some((0, 0)) && got.len() == ip.len() && got[9] == 1 && got[20] == 0 && got[28..] == pay[..];
    ctx.check("icmp-echo", e == 0 && echo_ok, alloc::format!("write {} read {:?}, {} bytes, ICMP type {}", e, wi, got.len(), got.get(20).copied().unwrap_or(255)));

    // ---- Statistics ----
    let mut ts = Sana2PacketTypeStats::default();
    a.set(S2_GETTYPESTATS, 0);
    a.r.set_packet_type(0x0806);
    a.r.set_stat_data(&mut ts as *mut _ as *mut c_void);
    let (e, _) = ctx.do_io(&a);
    let (ps, pr) = (ts.PacketsSent, ts.PacketsReceived);
    ctx.check("typestats", e == 0 && ps >= 2 && pr >= 2, alloc::format!("ARP sent {} received {}", ps, pr));
    a.set(S2_UNTRACKTYPE, 0);
    a.r.set_packet_type(0x0806);
    let (e, _) = ctx.do_io(&a);
    a.set(S2_GETTYPESTATS, 0);
    a.r.set_packet_type(0x0806);
    a.r.set_stat_data(&mut ts as *mut _ as *mut c_void);
    let (e2, w2) = ctx.do_io(&a);
    ctx.check("untracktype", e == 0 && w2 == S2WERR_NOT_TRACKED && e2 != 0, alloc::format!("untrack {}, then stats {}/{}", e, e2, w2));

    let mut gs: Sana2DeviceStats = unsafe { core::mem::zeroed() };
    a.set(S2_GETGLOBALSTATS, 0);
    a.r.set_stat_data(&mut gs as *mut _ as *mut c_void);
    let (e, _) = ctx.do_io(&a);
    let (gr, gsent, ls) = (gs.PacketsReceived, gs.PacketsSent, gs.LastStart);
    let lss = ls.Seconds;
    ctx.check("globalstats", e == 0 && gr >= 3 && gsent >= 3 && lss > 0, alloc::format!("received {} sent {} last start {} s", gr, gsent, lss));

    let mut xs: Sana2ExtDeviceStats = unsafe { core::mem::zeroed() };
    xs.s2xds_Length = core::mem::size_of::<Sana2ExtDeviceStats>() as u32;
    a.set(S2_GETEXTENDEDGLOBALSTATS, 0);
    a.r.set_stat_data(&mut xs as *mut _ as *mut c_void);
    let (e, _) = ctx.do_io(&a);
    let (act, xr) = (xs.s2xds_Actual, xs.s2xds_PacketsReceived);
    let xrl = xr.s2q_Low;
    ctx.check("extendedstats", e == 0 && act == 88 && xrl >= 3, alloc::format!("actual {} received {}", act, xrl));

    let mut sp = [0u8; 8 + 12 * 8];
    sp[0..4].copy_from_slice(&8u32.to_ne_bytes());
    a.set(S2_GETSPECIALSTATS, 0);
    a.r.set_stat_data(sp.as_mut_ptr() as *mut c_void);
    let (e, _) = ctx.do_io(&a);
    let nrec = u32::from_ne_bytes([sp[4], sp[5], sp[6], sp[7]]);
    ctx.check("specialstats", e == 0 && nrec >= 4, alloc::format!("{} records", nrec));

    // ---- Multicast ----
    let group = [0x01u8, 0x00, 0x5e, 0x00, 0x00, 0xfb];
    let mut errs = Vec::new();
    for cmd in [S2_ADDMULTICASTADDRESS, S2_ADDMULTICASTADDRESS, S2_DELMULTICASTADDRESS, S2_DELMULTICASTADDRESS] {
        a.set(cmd, 0);
        a.r.set_src_mac(&group);
        errs.push(ctx.do_io(&a).0);
    }
    a.set(S2_ADDMULTICASTADDRESS, 0);
    a.r.set_src_mac(&factory);
    let (eb, wb2) = ctx.do_io(&a);
    a.set(S2_ADDMULTICASTADDRESSES, 0);
    a.r.set_src_mac(&[0x01, 0x00, 0x5e, 0, 0, 0]);
    a.r.set_dst_mac(&[0x01, 0x00, 0x5e, 0x7f, 0xff, 0xff]);
    let (er, _) = ctx.do_io(&a);
    a.set(S2_DELMULTICASTADDRESSES, 0);
    a.r.set_src_mac(&[0x01, 0x00, 0x5e, 0, 0, 0]);
    a.r.set_dst_mac(&[0x01, 0x00, 0x5e, 0x7f, 0xff, 0xff]);
    let (er2, _) = ctx.do_io(&a);
    ctx.check(
        "multicast",
        errs.iter().all(|&e| e == 0) && eb == S2ERR_BAD_ADDRESS && wb2 == S2WERR_BAD_MULTICAST && er == 0 && er2 == 0,
        alloc::format!("add/add/del/del {:?}, unicast {}/{}, range add {} del {}", errs, eb, wb2, er, er2),
    );

    // ---- Throughput sample ----
    let mut st: Sana2ThroughputStats = unsafe { core::mem::zeroed() };
    st.s2ts_Length = core::mem::size_of::<Sana2ThroughputStats>() as u32;
    let sample = ctx.clone_req(&a).unwrap();
    sample.r.io().set_command(S2_SAMPLE_THROUGHPUT);
    sample.r.set_stat_data(&mut st as *mut _ as *mut c_void);
    ctx.send(&sample);
    let mut ri2 = ctx.clone_req(&b).unwrap();
    ri2.set(CMD_READ, 0);
    ri2.r.set_packet_type(0x0800);
    ctx.send(&ri2);
    let ip2 = icmp_echo(my_ip, gw_ip, 0x4242, 2, &pay);
    a.set(CMD_WRITE, 0);
    a.r.set_packet_type(0x0800);
    a.r.set_dst_mac(&gw_mac);
    a.data_mut()[..ip2.len()].copy_from_slice(&ip2);
    a.buf.len = ip2.len() as u32;
    a.r.set_data(a.cookie());
    a.r.set_data_length(ip2.len() as u32);
    ctx.do_io(&a);
    let _ = ctx.wait(&ri2, 3000);
    let sres = ctx.abort_wait(&sample);
    let (bs, br, up) = (st.s2ts_BytesSent, st.s2ts_BytesReceived, st.s2ts_Updates);
    let (bsl, brl, upl) = (bs.s2q_Low, br.s2q_Low, up.s2q_Low);
    ctx.check("sample-throughput", sres.0 == IOERR_ABORTED && bsl >= 98 && brl >= 98 && upl >= 2, alloc::format!("sent {} received {} updates {}, then AbortIO {:?}", bsl, brl, upl, sres));

    // ---- AbortIO of a pending read ----
    let mut rx = ctx.clone_req(&a).unwrap();
    rx.set(CMD_READ, 0);
    rx.r.set_packet_type(0x88b5); // a type nobody sends
    ctx.send(&rx);
    let ab = ctx.abort_wait(&rx);
    ctx.check("abort-read", ab.0 == IOERR_ABORTED, alloc::format!("{:?}", ab));

    // ---- Offline and online ----
    let mut rp = ctx.clone_req(&a).unwrap();
    rp.set(CMD_READ, 0);
    rp.r.set_packet_type(0x88b5);
    ctx.send(&rp);
    let evo = ctx.clone_req(&a).unwrap();
    evo.r.io().set_command(S2_ONEVENT);
    evo.r.set_wire_error(S2EVENT_OFFLINE);
    ctx.send(&evo);
    a.set(S2_OFFLINE, 0);
    let (e, _) = ctx.do_io(&a);
    let rpr = ctx.wait(&rp, 2000);
    let evr = ctx.wait(&evo, 2000);
    a.set(S2_BROADCAST, 0);
    a.r.set_packet_type(0x0806);
    a.data_mut()[..28].copy_from_slice(&payload);
    a.buf.len = 28;
    a.r.set_data(a.cookie());
    a.r.set_data_length(28);
    let (ew, ww) = ctx.do_io(&a);
    ctx.check(
        "offline",
        e == 0 && rpr == Some((S2ERR_OUTOFSERVICE, S2WERR_UNIT_OFFLINE)) && evr == Some((0, S2EVENT_OFFLINE)) && ew == S2ERR_OUTOFSERVICE && ww == S2WERR_UNIT_OFFLINE,
        alloc::format!("offline {}; pending read {:?}; event {:?}; write {}/{}", e, rpr, evr, ew, ww),
    );
    a.set(S2_ONLINE, 0);
    let (e, _) = ctx.do_io(&a);
    let mut ra3 = ctx.clone_req(&a).unwrap();
    ra3.set(CMD_READ, 0);
    ra3.r.set_packet_type(0x0806);
    ctx.send(&ra3);
    a.set(S2_BROADCAST, 0);
    a.r.set_packet_type(0x0806);
    a.data_mut()[..28].copy_from_slice(&payload);
    a.buf.len = 28;
    a.r.set_data(a.cookie());
    a.r.set_data_length(28);
    ctx.do_io(&a);
    let w3 = ctx.wait(&ra3, 3000);
    ctx.check("online-again", e == 0 && w3 == Some((0, 0)), alloc::format!("online {}, ARP after it {:?}", e, w3));

    // ---- Recovery: the device reset behind the driver's back ----
    a.set(0x8001, 0);
    let (e, _) = ctx.do_io(&a);
    let mut ra4 = ctx.clone_req(&a).unwrap();
    ra4.set(CMD_READ, 0);
    ra4.r.set_packet_type(0x0806);
    ctx.send(&ra4);
    // Kick the unit task so it notices, then send.
    let _ = ctx.timer.delay(TimerVal::from_millis(50));
    a.set(S2_BROADCAST, 0);
    a.r.set_packet_type(0x0806);
    a.data_mut()[..28].copy_from_slice(&payload);
    a.buf.len = 28;
    a.r.set_data(a.cookie());
    a.r.set_data_length(28);
    ctx.do_io(&a);
    let w4 = ctx.wait(&ra4, 3000);
    let w4 = if w4.is_none() {
        // The first broadcast may have gone during the recovery; once more.
        ctx.do_io(&a);
        ctx.wait(&ra4, 3000)
    } else {
        w4
    };
    ctx.check("recovery", e == 0 && w4 == Some((0, 0)), alloc::format!("test fault {}, ARP afterwards {:?}", e, w4));
    if w4.is_none() {
        ctx.abort_wait(&ra4);
    }

    for r in [ev, ev2, ra, rb, orph, rb2, ri, sample, rx, rp, evo, ra3, ri2, ra4] {
        // SAFETY: requests that came back.
        unsafe { sys::exec_delete_iorequest(r.r.as_ptr()) };
    }
    ctx.close(b);
    ctx.close(a);
    drop(hook);
}

/// `events` mode: configure the unit, then report each link event within
/// 60 seconds (QMP set_link from outside drives them; `events announce`
/// also wants CONFIGCHANGED, from QMP announce-self). Passes when it saw
/// DISCONNECT, then CONNECT (and CONFIGCHANGED with `announce`).
///
/// CONNECT is asked for only after a DISCONNECT: with the link up, a driver
/// answers a CONNECT request at once (the link is already connected), so
/// asking for it first would end the wait straight away. The 60 seconds
/// are measured, not counted per request.
fn run_events(ctx: &mut Ctx, announce: bool) {
    let tags = [
        TagItem { ti_Tag: S2_COPY_TO_BUFF, ti_Data: tag_copy_to as *const () as usize as u32 },
        TagItem { ti_Tag: S2_COPY_FROM_BUFF, ti_Data: tag_copy_from as *const () as usize as u32 },
        TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
    ];
    let mut a = match ctx.open(&tags, 0) {
        Ok(r) => r,
        Err(e) => {
            ctx.check("events-open", false, alloc::format!("OpenDevice returned {}", e));
            return;
        }
    };
    a.set(S2_GETSTATIONADDRESS, 0);
    ctx.do_io(&a);
    let factory = a.r.dst_mac();
    a.set(S2_CONFIGINTERFACE, 0);
    a.r.set_src_mac(&factory);
    ctx.do_io(&a);
    say!("s2test: waiting 60 s for link{} events", if announce { " and configuration" } else { "" });
    amigaos4::serial_println!("s2test: events armed");
    let want = S2EVENT_DISCONNECT | S2EVENT_CONNECT | if announce { S2EVENT_CONFIGCHANGED } else { 0 };
    let mut seen = 0u32;
    let mut log = Vec::new();
    let limit_ms = 60_000u64;
    let t0 = ctx.timer.up_time();
    let elapsed_ms = |ctx: &Ctx| {
        let t = ctx.timer.up_time();
        (t.secs as u64 * 1000 + t.micros as u64 / 1000).saturating_sub(t0.secs as u64 * 1000 + t0.micros as u64 / 1000)
    };
    while seen & want != want && elapsed_ms(ctx) < limit_ms {
        let mut ask = want & !seen;
        if seen & S2EVENT_DISCONNECT == 0 {
            // Not until the link has gone down (see the doc comment).
            ask = (ask & !S2EVENT_CONNECT) | S2EVENT_DISCONNECT;
        }
        let ev = ctx.clone_req(&a).unwrap();
        ev.r.io().set_command(S2_ONEVENT);
        ev.r.set_wire_error(ask);
        ctx.send(&ev);
        let step = (limit_ms - elapsed_ms(ctx).min(limit_ms)).min(5000) as u32;
        match ctx.wait(&ev, step.max(1)) {
            Some((0, got)) => {
                seen |= got;
                log.push(got);
                amigaos4::serial_println!("s2test: event 0x{:x}", got);
            }
            Some(other) => log.push(0x8000_0000 | other.1),
            None => {
                ctx.abort_wait(&ev);
            }
        }
        // SAFETY: a request that came back.
        unsafe { sys::exec_delete_iorequest(ev.r.as_ptr()) };
    }
    ctx.check(
        "link-events",
        seen & want == want,
        alloc::format!(
            "events in order {:x?} after {} ms (DISCONNECT 0x400, CONNECT 0x200, CONFIGCHANGED 0x100)",
            log,
            elapsed_ms(ctx)
        ),
    );
    ctx.close(a);
}

/// The driver's special statistics, as (name index, count): virtionet's
/// fourth record is the writes that waited for a transmit slot.
fn special_stats(ctx: &mut Ctx, a: &mut Req) -> Vec<u32> {
    let mut sp = [0u8; 8 + 12 * 8];
    sp[0..4].copy_from_slice(&8u32.to_ne_bytes());
    a.set(S2_GETSPECIALSTATS, 0);
    a.r.set_stat_data(sp.as_mut_ptr() as *mut c_void);
    let (e, _) = ctx.do_io(a);
    if e != 0 {
        return Vec::new();
    }
    let n = u32::from_ne_bytes([sp[4], sp[5], sp[6], sp[7]]) as usize;
    (0..n.min(8))
        .map(|i| {
            let o = 8 + i * 12 + 4;
            u32::from_ne_bytes([sp[o], sp[o + 1], sp[o + 2], sp[o + 3]])
        })
        .collect()
}

/// `stats`: print the special statistics (works while a stack has the unit).
fn run_stats(ctx: &mut Ctx) {
    let tags = [
        TagItem { ti_Tag: S2_COPY_TO_BUFF, ti_Data: tag_copy_to as *const () as usize as u32 },
        TagItem { ti_Tag: S2_COPY_FROM_BUFF, ti_Data: tag_copy_from as *const () as usize as u32 },
        TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
    ];
    let Ok(mut a) = ctx.open(&tags, 0) else {
        ctx.check("stats-open", false, String::from("OpenDevice failed"));
        return;
    };
    let st = special_stats(ctx, &mut a);
    ctx.check("stats", !st.is_empty(), alloc::format!("special counts {:?} (no read, filtered, mcast rejected, tx waits, recoveries, bad data)", st));
    ctx.close(a);
}

/// `burst`: 32 broadcasts started at once, the last 8 aborted at once.
/// Every request must come back, sent or aborted; with fewer transmit
/// slots than 32 some wait for one (the special statistics show how many).
fn run_burst(ctx: &mut Ctx, my_ip: [u8; 4], gw_ip: [u8; 4]) {
    let tags = [
        TagItem { ti_Tag: S2_COPY_TO_BUFF, ti_Data: tag_copy_to as *const () as usize as u32 },
        TagItem { ti_Tag: S2_COPY_FROM_BUFF, ti_Data: tag_copy_from as *const () as usize as u32 },
        TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
    ];
    let Ok(mut a) = ctx.open(&tags, 0) else {
        ctx.check("burst-open", false, String::from("OpenDevice failed"));
        return;
    };
    a.set(S2_GETSTATIONADDRESS, 0);
    ctx.do_io(&a);
    let factory = a.r.dst_mac();
    a.set(S2_CONFIGINTERFACE, 0);
    a.r.set_src_mac(&factory);
    ctx.do_io(&a); // already configured is fine
    let before = special_stats(ctx, &mut a);
    let payload = arp_request(&factory, my_ip, gw_ip);
    a.set(S2_BROADCAST, 0);
    a.r.set_packet_type(0x0806);
    a.data_mut()[..28].copy_from_slice(&payload);
    a.buf.len = 28;
    a.r.set_data(a.cookie());
    a.r.set_data_length(28);
    let reqs: Vec<Req> = (0..32).map(|_| ctx.clone_req(&a).unwrap()).collect();
    // Above the unit task (5) while sending and aborting, so writes that
    // wait for a slot are still waiting when AbortIO comes.
    // SAFETY: our own task.
    let me = unsafe { sys::exec_find_task(core::ptr::null_mut()) };
    let old = unsafe { sys::exec_set_task_pri(me, 10) };
    for r in &reqs {
        ctx.send(r);
    }
    for r in &reqs[24..] {
        // SAFETY: a sent request; AbortIO on one already done is harmless.
        unsafe { sys::exec_abort_io(r.r.as_ptr()) };
    }
    // SAFETY: as above.
    unsafe { sys::exec_set_task_pri(me, old) };
    let (mut sent, mut aborted, mut other, mut lost) = (0, 0, Vec::new(), 0);
    for r in &reqs {
        match ctx.wait(r, 3000) {
            Some((0, _)) => sent += 1,
            Some((IOERR_ABORTED, _)) => aborted += 1,
            Some(x) => other.push(x),
            None => lost += 1,
        }
    }
    let after = special_stats(ctx, &mut a);
    let waits = |v: &Vec<u32>| v.get(3).copied().unwrap_or(0);
    ctx.check(
        "burst",
        lost == 0 && other.is_empty() && sent + aborted == 32 && sent >= 24,
        alloc::format!("sent {} aborted {} other {:?} lost {}; writes that waited for a slot {}", sent, aborted, other, lost, waits(&after) - waits(&before)),
    );
    for r in reqs {
        // SAFETY: every request came back (or the check above failed).
        if lost == 0 {
            unsafe { sys::exec_delete_iorequest(r.r.as_ptr()) };
        }
    }
    ctx.close(a);
}

/// `prom`: open the unit promiscuous (SANA2OPF_PROM) next to an ordinary
/// opener, hold it 10 s, close it, hold 10 s more. The device's own
/// receive mode is checked from the host meanwhile (QEMU's
/// query-rx-filter); the serial lines mark when to look.
fn run_prom(ctx: &mut Ctx) {
    let tags = [
        TagItem { ti_Tag: S2_COPY_TO_BUFF, ti_Data: tag_copy_to as *const () as usize as u32 },
        TagItem { ti_Tag: S2_COPY_FROM_BUFF, ti_Data: tag_copy_from as *const () as usize as u32 },
        TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
    ];
    let a = match ctx.open(&tags, 0) {
        Ok(r) => r,
        Err(e) => {
            ctx.check("prom-first-open", false, alloc::format!("OpenDevice returned {}", e));
            return;
        }
    };
    amigaos4::serial_println!("s2test: ordinary open held");
    say!("s2test: ordinary open held 10 s");
    let _ = ctx.timer.delay(TimerVal::from_millis(10_000));
    match ctx.open(&tags, SANA2OPF_PROM) {
        Ok(b) => {
            ctx.check("prom-open", true, String::from("SANA2OPF_PROM open next to an ordinary one"));
            amigaos4::serial_println!("s2test: promiscuous open held");
            say!("s2test: promiscuous open held 10 s");
            let _ = ctx.timer.delay(TimerVal::from_millis(10_000));
            // A second promiscuous opener, then close both: the mode
            // must stay on until the last one goes.
            match ctx.open(&tags, SANA2OPF_PROM) {
                Ok(c) => {
                    ctx.close(b);
                    amigaos4::serial_println!("s2test: one of two promiscuous openers closed");
                    say!("s2test: one of two promiscuous openers closed, 10 s");
                    let _ = ctx.timer.delay(TimerVal::from_millis(10_000));
                    ctx.close(c);
                    ctx.check("prom-second-open", true, String::from("two promiscuous openers"));
                }
                Err(e) => {
                    ctx.close(b);
                    ctx.check("prom-second-open", false, alloc::format!("OpenDevice returned {}", e));
                }
            }
            amigaos4::serial_println!("s2test: promiscuous openers closed");
            say!("s2test: promiscuous openers closed, 10 s");
            let _ = ctx.timer.delay(TimerVal::from_millis(10_000));
        }
        Err(e) => ctx.check("prom-open", false, alloc::format!("OpenDevice returned {}", e)),
    }
    ctx.close(a);
}

#[no_mangle]
pub extern "C" fn rust_main(args: *const u8, arglen: i32) -> i32 {
    // SAFETY: the Shell's argument string.
    let text = unsafe { core::slice::from_raw_parts(args, arglen.max(0) as usize) };
    let text = core::str::from_utf8(text).unwrap_or("");
    let words: Vec<&str> = text.split_whitespace().collect();
    let unit = words.first().and_then(|w| w.parse().ok()).unwrap_or(0);
    let name = words.get(1).copied().unwrap_or("virtionet.device");
    let my_ip = words.get(2).and_then(|w| parse_ip(w)).unwrap_or([10, 0, 3, 15]);
    let gw_ip = words.get(3).and_then(|w| parse_ip(w)).unwrap_or([10, 0, 3, 2]);
    // mtu=N and mbit=N: what the device should report (default 1500, 1000).
    let opt = |k: &str, d: u32| -> u32 {
        words.iter().find_map(|w| w.strip_prefix(k).and_then(|v| v.parse().ok())).unwrap_or(d)
    };
    let want_mtu = opt("mtu=", 1500);
    let want_bps = opt("mbit=", 1000).saturating_mul(1_000_000);
    let mut devname: Vec<u8> = name.as_bytes().to_vec();
    devname.push(0);
    let port = match AmigaMsgPort::new() {
        Ok(p) => p,
        Err(_) => return 20,
    };
    let timer = match AmigaTimer::open(TimerUnit::MicroHz) {
        Ok(t) => t,
        Err(_) => return 20,
    };
    let mut ctx = Ctx { port, timer, devname, unit, passed: 0, failed: 0 };
    say!("s2test: {} unit {}, {}.{}.{}.{} -> {}.{}.{}.{}", name, unit, my_ip[0], my_ip[1], my_ip[2], my_ip[3], gw_ip[0], gw_ip[1], gw_ip[2], gw_ip[3]);
    if words.iter().any(|w| *w == "events") {
        run_events(&mut ctx, words.iter().any(|w| *w == "announce"));
    } else if words.iter().any(|w| *w == "prom") {
        run_prom(&mut ctx);
    } else if words.iter().any(|w| *w == "stats") {
        run_stats(&mut ctx);
    } else if words.iter().any(|w| *w == "burst") {
        run_burst(&mut ctx, my_ip, gw_ip);
    } else if words.iter().any(|w| *w == "torture") {
        torture::run_torture(&mut ctx, my_ip, gw_ip, opt("secs=", 30), opt("kids=", 4), want_mtu as usize);
    } else if words.iter().any(|w| *w == "hold") {
        torture::run_hold(&mut ctx, my_ip, gw_ip);
    } else if words.iter().any(|w| *w == "faults") {
        torture::run_faults(&mut ctx, opt("secs=", 60), opt("period=", 20));
    } else if words.iter().any(|w| *w == "leak") {
        torture::run_leak(&mut ctx, my_ip, gw_ip);
    } else if words.iter().any(|w| *w == "closepend") {
        torture::run_closepend(&mut ctx, my_ip, gw_ip);
    } else {
        run(&mut ctx, my_ip, gw_ip, want_mtu, want_bps);
    }
    say!("s2test: {} passed, {} failed", ctx.passed, ctx.failed);
    amigaos4::serial_println!("s2test: {} passed, {} failed", ctx.passed, ctx.failed);
    if ctx.failed == 0 { 0 } else { 20 }
}

mod torture;
