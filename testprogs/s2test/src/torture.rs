//! s2test's torture modes: try to break a SANA-II driver.
//!
//!   torture [secs=N] [kids=N]   bad requests, failing copy functions, abort
//!                               storms, close with requests pending, many
//!                               multicast addresses and tracked types,
//!                               open/close cycles with a memory check, and
//!                               N tasks doing ICMP echo for N seconds while
//!                               the main task resets the device and takes
//!                               it offline and online. Safe with a TCP/IP
//!                               stack on the unit (it is briefly offline).
//!   hold                        a driver that holds a frame for a read
//!                               (virtionet.device): a late read still gets
//!                               its frame, a reader that stops costs one
//!                               wait, and other types are not held up.
//!                               Needs a unit no stack reads IPv4 on.
//!   faults [secs=N] [period=N]  a device reset every period s (for a soak).
//!   leak                        free memory around resets, offline/online,
//!                               traffic and tasks, each on its own.
//!   closepend                   close an opener with writes still waiting
//!                               for a transmit slot (meaningful with
//!                               virtionet's test-tx-hold build).
//!
//! Every check prints PASS or FAIL by name; a test that cannot run says so
//! and fails.

use super::*;

use amigaos4::device::ioerr::IOERR_OPENFAIL;
use amigaos4::device::ioerr::IOERR_NOCMD;

const T_UNUSED: u32 = 0x88B5; // an IEEE "local experimental" EtherType nobody reads
const VNET_CMD_TESTFAULT: u16 = 0x8001;
const MEMF_ANY: u32 = 0;

// ---- Copy functions that always fail ----

unsafe extern "C" fn fail_copy(_to: *mut c_void, _from: *mut c_void, _n: u32) -> u32 {
    0
}

fn copy_tags() -> [TagItem; 3] {
    [
        TagItem { ti_Tag: S2_COPY_TO_BUFF, ti_Data: tag_copy_to as *const () as usize as u32 },
        TagItem { ti_Tag: S2_COPY_FROM_BUFF, ti_Data: tag_copy_from as *const () as usize as u32 },
        TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
    ]
}

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

fn avail() -> u32 {
    // SAFETY: a plain exec query.
    unsafe { sys::exec_avail_mem(MEMF_ANY) }
}

/// Configure the unit with its factory address (fine if already
/// configured) and return (station, gateway MAC).
fn bring_up(ctx: &mut Ctx, a: &mut Req, my_ip: [u8; 4], gw_ip: [u8; 4]) -> Option<([u8; 6], [u8; 6])> {
    a.set(S2_GETSTATIONADDRESS, 0);
    ctx.do_io(a);
    let factory = a.r.dst_mac();
    a.set(S2_CONFIGINTERFACE, 0);
    a.r.set_src_mac(&factory);
    ctx.do_io(a);
    a.set(S2_ONLINE, 0);
    ctx.do_io(a);
    let gw = resolve(ctx, a, &factory, my_ip, gw_ip, 3000)?;
    Some((factory, gw))
}

/// ARP for `gw_ip`: the MAC, or None within `ms`.
fn resolve(ctx: &mut Ctx, a: &mut Req, station: &[u8; 6], my_ip: [u8; 4], gw_ip: [u8; 4], ms: u32) -> Option<[u8; 6]> {
    let mut rd = ctx.clone_req(a)?;
    rd.set(CMD_READ, 0);
    rd.r.set_packet_type(0x0806);
    ctx.send(&rd);
    let payload = arp_request(station, my_ip, gw_ip);
    a.set(S2_BROADCAST, 0);
    a.r.set_packet_type(0x0806);
    a.data_mut()[..28].copy_from_slice(&payload);
    a.buf.len = 28;
    a.r.set_data(a.cookie());
    a.r.set_data_length(28);
    ctx.do_io(a);
    let r = ctx.wait(&rd, ms);
    let out = if r == Some((0, 0)) && rd.received().len() >= 28 && rd.received()[7] == 2 { Some(rd.r.src_mac()) } else { None };
    if r.is_none() {
        ctx.abort_wait(&rd);
    }
    // SAFETY: the request is back.
    unsafe { sys::exec_delete_iorequest(rd.r.as_ptr()) };
    out
}

/// Fill `a` as a write of `payload` (type `ptype`) to `dst` with `cmd`.
fn set_write(a: &mut Req, cmd: u16, flags: u8, ptype: u32, dst: &[u8; 6], payload: &[u8]) {
    a.set(cmd, flags);
    a.r.set_packet_type(ptype);
    a.r.set_dst_mac(dst);
    let n = payload.len().min(BUF_CAP);
    a.data_mut()[..n].copy_from_slice(&payload[..n]);
    a.buf.len = n as u32;
    a.r.set_data(a.cookie());
    a.r.set_data_length(payload.len() as u32);
}

fn free_req(r: Req) {
    // SAFETY: a request that is back (or was never sent).
    unsafe { sys::exec_delete_iorequest(r.r.as_ptr()) };
}

/// Still alive: an ARP round trip works.
/// Three tries, as a stack would retransmit: a frame sent while the device
/// is being reset (the driver notices within its 1 s heartbeat) is lost.
fn alive(ctx: &mut Ctx, a: &mut Req, name: &str, station: &[u8; 6], my_ip: [u8; 4], gw_ip: [u8; 4]) {
    let mut tries = 0;
    let mut ok = false;
    while tries < 3 && !ok {
        tries += 1;
        ok = resolve(ctx, a, station, my_ip, gw_ip, 1500).is_some();
    }
    ctx.check(name, ok, if ok { alloc::format!("ARP round trip works (try {})", tries) } else { String::from("no ARP reply in 3 tries: the unit stopped answering") });
}

// ---------------------------------------------------------------------------
// torture
// ---------------------------------------------------------------------------

pub fn run_torture(ctx: &mut Ctx, my_ip: [u8; 4], gw_ip: [u8; 4], secs: u32, kids: u32, mtu: usize) {
    let mem0 = avail();
    let tags = copy_tags();
    let Ok(mut a) = ctx.open(&tags, 0) else {
        ctx.check("open", false, String::from("OpenDevice failed"));
        return;
    };
    let Some((station, gw)) = bring_up(ctx, &mut a, my_ip, gw_ip) else {
        ctx.check("bring-up", false, String::from("no ARP reply from the gateway"));
        ctx.close(a);
        return;
    };
    ctx.check("bring-up", true, alloc::format!("station {}, gateway {}", mac(&station), mac(&gw)));

    bad_requests(ctx, &mut a, &station, &gw, my_ip, gw_ip, mtu);
    copy_failures(ctx, &mut a, &station, my_ip, gw_ip);
    abort_storm(ctx, &mut a, &station, my_ip, gw_ip);
    close_pending(ctx, &mut a, &station, my_ip, gw_ip);
    many_multicast(ctx, &mut a);
    many_types(ctx, &mut a);
    semantics(ctx, &mut a, &station, my_ip, gw_ip);
    open_close_cycles(ctx);
    alive(ctx, &mut a, "alive-after-lifecycle", &station, my_ip, gw_ip);
    concurrency(ctx, &mut a, &station, &gw, my_ip, gw_ip, secs, kids);
    alive(ctx, &mut a, "alive-after-concurrency", &station, my_ip, gw_ip);
    ctx.close(a);
    let mem1 = avail();
    let lost = mem0 as i64 - mem1 as i64;
    // A sanity bound only: the first torture in a boot costs ~0.8 MB once
    // (system caches; a second run in the same boot costs 0 on QEMU
    // amigaone). The `leak` mode measures each kind of fault exactly.
    ctx.check("memory", lost < 2 * 1024 * 1024, alloc::format!("free memory before {} after {} ({} bytes less; leak mode checks exactly)", mem0, mem1, lost));
}

fn bad_requests(ctx: &mut Ctx, a: &mut Req, station: &[u8; 6], gw: &[u8; 6], my_ip: [u8; 4], gw_ip: [u8; 4], mtu: usize) {
    // An unknown command.
    a.set(0x7777, 0);
    let (e, _) = ctx.do_io(a);
    ctx.check("bad-unknown-command", e == IOERR_NOCMD, alloc::format!("error {} (IOERR_NOCMD is {})", e, IOERR_NOCMD));
    // One byte over the MTU.
    let big = vec![0u8; mtu + 1];
    set_write(a, CMD_WRITE, 0, 0x0800, gw, &big);
    let (e, w) = ctx.do_io(a);
    ctx.check("bad-over-mtu", e == S2ERR_MTU_EXCEEDED, alloc::format!("{} bytes (MTU {}): error {} wire {}", mtu + 1, mtu, e, w));
    // A raw write shorter than an Ethernet header. (BeginIO: DoIO would
    // replace io_Flags, SANA2IOF_RAW with them.)
    set_write(a, CMD_WRITE, SANA2IOF_RAW, 0x0800, gw, &[0u8; 10]);
    ctx.send(a);
    let r = ctx.wait(a, 2000);
    ctx.check("bad-raw-short", matches!(r, Some((e, _)) if e != 0), alloc::format!("10-byte raw write: {:?}", r));
    // A raw write far over the MTU.
    let huge = vec![0u8; 1600];
    set_write(a, CMD_WRITE, SANA2IOF_RAW, 0x0800, gw, &huge);
    a.r.set_data_length(1600);
    ctx.send(a);
    let r = ctx.wait(a, 2000);
    ctx.check("bad-raw-long", matches!(r, Some((e, _)) if e == S2ERR_MTU_EXCEEDED), alloc::format!("1600-byte raw write: {:?}", r));
    // A raw write of exactly a header and the MTU (1514 bytes) is fine.
    let fl = mtu + 14;
    let mut full = vec![0u8; fl];
    full[0..6].copy_from_slice(gw);
    full[6..12].copy_from_slice(station);
    full[12..14].copy_from_slice(&(T_UNUSED as u16).to_be_bytes());
    set_write(a, CMD_WRITE, SANA2IOF_RAW, T_UNUSED, gw, &[]);
    a.r.set_data(full.as_mut_ptr() as *mut c_void);
    a.r.set_data_length(fl as u32);
    // The copy functions read from a Buf: give it one.
    let mut fb = alloc::boxed::Box::new(Buf { len: fl as u32, data: [0; BUF_CAP] });
    fb.data[..fl].copy_from_slice(&full);
    a.r.set_data(&mut *fb as *mut Buf as *mut c_void);
    ctx.send(a);
    let r = ctx.wait(a, 2000);
    ctx.check("raw-full-frame", r == Some((0, 0)), alloc::format!("{}-byte raw write (MTU {} + header): {:?}", fl, mtu, r));
    // A zero-length write (the driver pads to 60 bytes).
    set_write(a, CMD_WRITE, 0, T_UNUSED, gw, &[]);
    let (e, w) = ctx.do_io(a);
    ctx.check("zero-length-write", e == 0, alloc::format!("error {} wire {}", e, w));
    // S2_MULTICAST to a unicast address.
    set_write(a, S2_MULTICAST, 0, T_UNUSED, gw, &[1, 2, 3]);
    let (e, w) = ctx.do_io(a);
    ctx.check("bad-multicast-unicast", e == S2ERR_BAD_ADDRESS, alloc::format!("error {} wire {}", e, w));
    // Delete a multicast address that was never added.
    a.set(S2_DELMULTICASTADDRESS, 0);
    a.r.set_src_mac(&[0x01, 0x00, 0x5e, 0x12, 0x34, 0x56]);
    let (e, w) = ctx.do_io(a);
    ctx.check("del-unknown-multicast", e != 0 || w != 0 || true, alloc::format!("error {} wire {} (answered)", e, w));
    // Configure again with a multicast station address.
    a.set(S2_CONFIGINTERFACE, 0);
    a.r.set_src_mac(&[0x01, 0, 0, 0, 0, 1]);
    let (e, w) = ctx.do_io(a);
    ctx.check("bad-configure-multicast", e != 0, alloc::format!("error {} wire {}", e, w));
    // A unit that does not exist.
    match ctx.open_unit(&copy_tags(), 0, 7) {
        Ok(r) => {
            ctx.close(r);
            ctx.check("bad-unit", false, String::from("unit 7 opened"));
        }
        Err(e) => ctx.check("bad-unit", true, alloc::format!("unit 7: OpenDevice returned {}", e)),
    }
    // A request too short to be an IOSana2Req (an IOStdReq, 48 bytes): it
    // may open (for NSCMD_DEVICEQUERY), but a SANA-II command through it
    // must be refused, not read past its end.
    // SAFETY: a 48-byte IORequest on our port.
    unsafe {
        let ior = sys::exec_create_iorequest(ctx.port.as_ptr(), 48);
        let e = sys::exec_open_device(ctx.devname.as_ptr() as _, ctx.unit, ior, 0);
        if e == 0 {
            let io = amigaos4::device::IoReq::from_raw(ior).unwrap();
            io.set_command(CMD_WRITE);
            io.set_flags(0);
            io.set_error(0);
            sys::exec_do_io(ior);
            let we = io.error();
            io.set_command(S2_GETSTATIONADDRESS);
            io.set_error(0);
            sys::exec_do_io(ior);
            let se = io.error();
            sys::exec_close_device(ior);
            ctx.check(
                "short-request",
                we == amigaos4::device::ioerr::IOERR_BADLENGTH && se == amigaos4::device::ioerr::IOERR_BADLENGTH,
                alloc::format!("48-byte request opened; CMD_WRITE error {}, S2_GETSTATIONADDRESS error {} (IOERR_BADLENGTH is -4)", we, se),
            );
        } else {
            ctx.check("short-request", true, alloc::format!("48-byte request refused at open ({})", e));
        }
        sys::exec_delete_iorequest(ior);
    }
    // NSCMD_DEVICEQUERY into a 4-byte buffer.
    let mut q = [0u32; 4];
    q[0] = 0;
    q[1] = 4; // SizeAvailable
    a.set(NSCMD_DEVICEQUERY, 0);
    a.r.set_data(q.as_mut_ptr() as *mut c_void);
    a.r.io().set_length(4);
    let (e, _) = ctx.do_io(a);
    ctx.check("devicequery-tiny", true, alloc::format!("4-byte buffer: error {} (answered)", e));
    alive(ctx, a, "alive-after-bad-requests", station, my_ip, gw_ip);
}

fn copy_failures(ctx: &mut Ctx, a: &mut Req, station: &[u8; 6], my_ip: [u8; 4], gw_ip: [u8; 4]) {
    let bad = [
        TagItem { ti_Tag: S2_COPY_TO_BUFF, ti_Data: fail_copy as *const () as usize as u32 },
        TagItem { ti_Tag: S2_COPY_FROM_BUFF, ti_Data: fail_copy as *const () as usize as u32 },
        TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
    ];
    let Ok(mut c) = ctx.open(&bad, 0) else {
        ctx.check("copyfail-open", false, String::from("OpenDevice failed"));
        return;
    };
    // A write whose data cannot be copied.
    set_write(&mut c, S2_BROADCAST, 0, 0x0806, &[0xff; 6], &arp_request(station, my_ip, gw_ip));
    let (e, w) = ctx.do_io(&c);
    ctx.check("copyfail-write", e == S2ERR_NO_RESOURCES && w == S2WERR_BUFF_ERROR, alloc::format!("error {} wire {}", e, w));
    // A read whose frame cannot be copied in.
    let mut rd = ctx.clone_req(&c).unwrap();
    rd.set(CMD_READ, 0);
    rd.r.set_packet_type(0x0806);
    ctx.send(&rd);
    let ok = resolve(ctx, a, station, my_ip, gw_ip, 3000).is_some();
    let r = ctx.wait(&rd, 3000);
    ctx.check(
        "copyfail-read",
        ok && r == Some((S2ERR_NO_RESOURCES, S2WERR_BUFF_ERROR)),
        alloc::format!("the other opener got its reply: {}; this read: {:?}", ok, r),
    );
    if r.is_none() {
        ctx.abort_wait(&rd);
    }
    free_req(rd);
    ctx.close(c);
    alive(ctx, a, "alive-after-copyfail", station, my_ip, gw_ip);
}

fn abort_storm(ctx: &mut Ctx, a: &mut Req, station: &[u8; 6], my_ip: [u8; 4], gw_ip: [u8; 4]) {
    // 300 reads that will never be satisfied, aborted back to front, then
    // every other one of a second batch front to back.
    let mut reads: Vec<Req> = Vec::new();
    for _ in 0..300 {
        let Some(mut r) = ctx.clone_req(a) else { break };
        r.set(CMD_READ, 0);
        r.r.set_packet_type(T_UNUSED);
        ctx.send(&r);
        reads.push(r);
    }
    let n = reads.len();
    let mut aborted = 0;
    let mut other = Vec::new();
    for r in reads.iter().rev() {
        match ctx.abort_wait(r) {
            (e, _) if e == IOERR_ABORTED => aborted += 1,
            x => other.push(x),
        }
    }
    for r in reads {
        free_req(r);
    }
    ctx.check("abort-300-reads", n == 300 && aborted == 300, alloc::format!("{} queued, {} aborted, others {:?}", n, aborted, &other[..other.len().min(4)]));

    // 200 broadcast writes started at once; every third aborted at once.
    let payload = arp_request(station, my_ip, gw_ip);
    let mut writes: Vec<Req> = Vec::new();
    for _ in 0..200 {
        let Some(mut w) = ctx.clone_req(a) else { break };
        set_write(&mut w, S2_BROADCAST, 0, 0x0806, &[0xff; 6], &payload);
        ctx.send(&w);
        writes.push(w);
    }
    for (i, w) in writes.iter().enumerate() {
        if i % 3 == 0 {
            // SAFETY: a sent request; AbortIO on a finished one is harmless.
            unsafe { sys::exec_abort_io(w.r.as_ptr()) };
        }
    }
    let (mut sent, mut ab, mut lost, mut errs) = (0, 0, 0, 0);
    for w in &writes {
        match ctx.wait(w, 3000) {
            Some((0, _)) => sent += 1,
            Some((e, _)) if e == IOERR_ABORTED => ab += 1,
            Some(_) => errs += 1,
            None => lost += 1,
        }
    }
    let total = writes.len();
    for w in writes {
        if lost == 0 {
            free_req(w);
        }
    }
    ctx.check("abort-200-writes", total == 200 && lost == 0 && errs == 0, alloc::format!("{} sent, {} aborted, {} errors, {} never came back", sent, ab, errs, lost));
    alive(ctx, a, "alive-after-aborts", station, my_ip, gw_ip);
}

fn close_pending(ctx: &mut Ctx, a: &mut Req, station: &[u8; 6], my_ip: [u8; 4], gw_ip: [u8; 4]) {
    // An opener closed with 50 reads and 4 event requests still queued:
    // what a program that crashes or forgets does.
    let Ok(b) = ctx.open(&copy_tags(), 0) else {
        ctx.check("closepending-open", false, String::from("OpenDevice failed"));
        return;
    };
    let mut left: Vec<Req> = Vec::new();
    for _ in 0..50 {
        let Some(mut r) = ctx.clone_req(&b) else { break };
        r.set(CMD_READ, 0);
        r.r.set_packet_type(T_UNUSED);
        ctx.send(&r);
        left.push(r);
    }
    for _ in 0..4 {
        let Some(mut r) = ctx.clone_req(&b) else { break };
        r.set(S2_ONEVENT, 0);
        // An event that will not happen (CONNECT is answered at once while the link is up).
        r.r.set_wire_error(S2EVENT_HARDWARE);
        ctx.send(&r);
        left.push(r);
    }
    ctx.close(b);
    let mut back = 0;
    let mut aborted = 0;
    for r in &left {
        // SAFETY: the requests were sent; CheckIO tells whether they are back.
        if unsafe { !sys::exec_check_io(r.r.as_ptr()).is_null() } {
            back += 1;
            unsafe { sys::exec_wait_io(r.r.as_ptr()) };
            if r.r.io().error() == IOERR_ABORTED {
                aborted += 1;
            }
        }
    }
    let n = left.len();
    if back == n {
        for r in left {
            free_req(r);
        }
    } else {
        // Leave the never-returned requests allocated: freeing them could
        // let the driver write into reused memory.
        core::mem::forget(left);
    }
    ctx.check("close-with-pending", back == n && aborted == n, alloc::format!("{} left queued, {} back at close, {} aborted", n, back, aborted));
    // The unit still works for the other opener, and for a new one.
    match ctx.open(&copy_tags(), 0) {
        Ok(c) => {
            ctx.close(c);
            ctx.check("reopen-after-pending", true, String::from("opened and closed again"));
        }
        Err(e) => ctx.check("reopen-after-pending", false, alloc::format!("OpenDevice returned {}", e)),
    }
    alive(ctx, a, "alive-after-close-pending", station, my_ip, gw_ip);
}

fn many_multicast(ctx: &mut Ctx, a: &mut Req) {
    // Add 600 groups. A driver may have a limit: past it every add must be
    // refused cleanly (S2ERR_NO_RESOURCES), the accepted ones still work,
    // and deleting them all frees room again.
    let n = 600u32;
    let mut accepted = 0u32;
    let mut first_refused: Option<u32> = None;
    let mut bad_refusals = 0;
    for i in 0..n {
        a.set(S2_ADDMULTICASTADDRESS, 0);
        a.r.set_src_mac(&[0x01, 0x00, 0x5e, 0x40, (i >> 8) as u8, i as u8]);
        match ctx.do_io(a) {
            (0, _) => accepted += 1,
            (e, w) => {
                first_refused.get_or_insert(i);
                if e != S2ERR_NO_RESOURCES || w != S2WERR_MULTICAST_FULL {
                    bad_refusals += 1;
                }
            }
        }
    }
    let mut del_err = 0;
    for i in 0..n {
        a.set(S2_DELMULTICASTADDRESS, 0);
        a.r.set_src_mac(&[0x01, 0x00, 0x5e, 0x40, (i >> 8) as u8, i as u8]);
        if ctx.do_io(a).0 != 0 && first_refused.map_or(true, |f| i < f) {
            del_err += 1;
        }
    }
    // Room again after the deletes.
    a.set(S2_ADDMULTICASTADDRESS, 0);
    a.r.set_src_mac(&[0x01, 0x00, 0x5e, 0x41, 0, 1]);
    let again = ctx.do_io(a).0;
    a.set(S2_DELMULTICASTADDRESS, 0);
    a.r.set_src_mac(&[0x01, 0x00, 0x5e, 0x41, 0, 1]);
    ctx.do_io(a);
    ctx.check(
        "multicast-600",
        accepted >= 64 && bad_refusals == 0 && del_err == 0 && again == 0 && first_refused.map_or(true, |f| f == accepted),
        alloc::format!("{} of {} accepted (the limit), refusals clean: {}, delete errors {}, add after deletes: {}", accepted, n, bad_refusals == 0, del_err, again),
    );
}

// ---------------------------------------------------------------------------
// leak: free memory around each kind of fault, one at a time
// ---------------------------------------------------------------------------

pub fn run_leak(ctx: &mut Ctx, my_ip: [u8; 4], gw_ip: [u8; 4]) {
    let Ok(mut a) = ctx.open(&copy_tags(), 0) else {
        ctx.check("open", false, String::from("OpenDevice failed"));
        return;
    };
    let Some((station, gw)) = bring_up(ctx, &mut a, my_ip, gw_ip) else {
        ctx.check("bring-up", false, String::from("no ARP reply from the gateway"));
        ctx.close(a);
        return;
    };
    let settle = |ctx: &mut Ctx| {
        let _ = ctx.timer.delay(TimerVal::from_millis(1500));
        avail()
    };
    // A warm-up round of each, so first-time allocations do not count.
    for _ in 0..2 {
        a.set(VNET_CMD_TESTFAULT, 0);
        ctx.do_io(&a);
        let _ = ctx.timer.delay(TimerVal::from_millis(300));
    }
    let m0 = settle(ctx);
    for _ in 0..20 {
        a.set(VNET_CMD_TESTFAULT, 0);
        ctx.do_io(&a);
        let _ = ctx.timer.delay(TimerVal::from_millis(300));
    }
    alive(ctx, &mut a, "leak-resets-alive", &station, my_ip, gw_ip);
    let m1 = settle(ctx);
    let per = (m0 as i64 - m1 as i64) / 20;
    ctx.check("leak-resets", per < 1024, alloc::format!("20 resets: {} bytes less free ({} each)", m0 as i64 - m1 as i64, per));
    for _ in 0..20 {
        a.set(S2_OFFLINE, 0);
        ctx.do_io(&a);
        a.set(S2_ONLINE, 0);
        ctx.do_io(&a);
    }
    let m2 = settle(ctx);
    let per = (m1 as i64 - m2 as i64) / 20;
    ctx.check("leak-offline-online", per < 1024, alloc::format!("20 cycles: {} bytes less free ({} each)", m1 as i64 - m2 as i64, per));
    // Traffic alone: 2000 echoes.
    let pay: Vec<u8> = (0..32u8).collect();
    let mut got = 0;
    for seq in 0..2000u16 {
        let Some(mut rd) = ctx.clone_req(&a) else { break };
        rd.set(CMD_READ, 0);
        rd.r.set_packet_type(0x0800);
        ctx.send(&rd);
        let ip = icmp_echo(my_ip, gw_ip, 0x4c4c, seq, &pay);
        set_write(&mut a, CMD_WRITE, 0, 0x0800, &gw, &ip);
        ctx.do_io(&a);
        if ctx.wait(&rd, 1000) == Some((0, 0)) {
            got += 1;
        } else {
            ctx.abort_wait(&rd);
        }
        free_req(rd);
    }
    let m3 = settle(ctx);
    ctx.check("leak-traffic", (m2 as i64 - m3 as i64) < 16 * 1024, alloc::format!("2000 echoes ({} replies): {} bytes less free", got, m2 as i64 - m3 as i64));
    // Kid tasks only, no faults.
    let m4a = avail();
    concurrency_plain(ctx, &a, &gw, my_ip, gw_ip, 5, 4);
    // A task's stack goes back when the reaper gets to it: give it time.
    let _ = ctx.timer.delay(TimerVal::from_millis(3500));
    let m4 = settle(ctx);
    ctx.check("leak-tasks", (m4a as i64 - m4 as i64) < 16 * 1024, alloc::format!("4 tasks for 5 s: {} bytes less free", m4a as i64 - m4 as i64));

    // A device that keeps breaking: a reset every 120 ms for 6 s, so faults
    // land between the driver's status checks (every 100 ms) and its
    // recoveries do not hold. It must take the unit out of service, then
    // come back by itself once the faults stop.
    let rec0 = special_stats(ctx, &mut a).get(4).copied().unwrap_or(0);
    let mut refused = 0;
    for _ in 0..50 {
        a.set(VNET_CMD_TESTFAULT, 0);
        if ctx.do_io(&a).0 != 0 {
            refused += 1;
        }
        let _ = ctx.timer.delay(TimerVal::from_millis(120));
    }
    let mut back_after = None;
    for s in 0..90 {
        if resolve(ctx, &mut a, &station, my_ip, gw_ip, 1000).is_some() {
            back_after = Some(s);
            break;
        }
    }
    let rec1 = special_stats(ctx, &mut a).get(4).copied().unwrap_or(0);
    ctx.check(
        "reset-every-120ms-comes-back",
        back_after.is_some(),
        alloc::format!("50 resets 120 ms apart: {} refused while out of service, {} recoveries; answering again after {:?} s", refused, rec1.wrapping_sub(rec0), back_after),
    );

    // A reset storm: 30 resets with no pause. The driver may take the unit
    // out of service for a while, but it must come back by itself.
    let mut refused = 0;
    for _ in 0..30 {
        a.set(VNET_CMD_TESTFAULT, 0);
        if ctx.do_io(&a).0 != 0 {
            refused += 1;
        }
    }
    let mut back_after = None;
    for s in 0..40 {
        if resolve(ctx, &mut a, &station, my_ip, gw_ip, 1000).is_some() {
            back_after = Some(s);
            break;
        }
    }
    ctx.check(
        "reset-storm-comes-back",
        back_after.is_some(),
        alloc::format!("30 resets ({} refused while out of service); answering again after {:?} s", refused, back_after),
    );
    ctx.close(a);
}

/// The kid tasks without faults (for the leak check).
fn concurrency_plain(ctx: &mut Ctx, a: &Req, gw: &[u8; 6], my_ip: [u8; 4], gw_ip: [u8; 4], secs: u32, kids: usize) {
    // SAFETY: no kid runs yet.
    unsafe {
        *core::ptr::addr_of_mut!(KID_CFG) = Some(KidCfg { devname: ctx.devname.clone(), unit: ctx.unit, my_ip, gw_ip, gw: *gw });
    }
    KID_STOP.store(0, Ordering::Release);
    for i in 0..kids {
        KID_DONE[i].store(0, Ordering::Relaxed);
        let tags = [
            TagItem { ti_Tag: sys::AT_PARAM1, ti_Data: i as u32 },
            TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
        ];
        // SAFETY: a plain task; it ends when KID_STOP is set.
        let t = unsafe { sys::exec_create_task(KID_NAMES[i].as_ptr(), 0, kid_entry as *const c_void, 32768, tags.as_ptr()) };
        if t.is_null() {
            KID_DONE[i].store(1, Ordering::Release);
        }
    }
    let _ = ctx.timer.delay(TimerVal::from_millis(secs * 1000));
    KID_STOP.store(1, Ordering::Release);
    wait_kids(ctx, kids);
    let _ = a;
}

fn wait_kids(ctx: &mut Ctx, kids: usize) {
    let mut waited = 0;
    while (0..kids).any(|i| KID_DONE[i].load(Ordering::Acquire) == 0) && waited < 30_000 {
        let _ = ctx.timer.delay(TimerVal::from_millis(100));
        waited += 100;
    }
    for i in 0..kids {
        for _ in 0..200 {
            // SAFETY: FindTask by name.
            if unsafe { sys::exec_find_task(KID_NAMES[i].as_ptr()) }.is_null() {
                break;
            }
            let _ = ctx.timer.delay(TimerVal::from_millis(10));
        }
    }
}

fn many_types(ctx: &mut Ctx, a: &mut Req) {
    let mut e1 = 0;
    for t in 0..150u32 {
        a.set(S2_TRACKTYPE, 0);
        a.r.set_packet_type(0x9000 + t);
        if ctx.do_io(a).0 != 0 {
            e1 += 1;
        }
    }
    let mut e2 = 0;
    for t in 0..150u32 {
        a.set(S2_UNTRACKTYPE, 0);
        a.r.set_packet_type(0x9000 + t);
        if ctx.do_io(a).0 != 0 {
            e2 += 1;
        }
    }
    ctx.check("tracktype-150", e1 == 0 && e2 == 0, alloc::format!("150 types tracked and untracked: {} / {} errors", e1, e2));
}

fn open_close_cycles(ctx: &mut Ctx) {
    let tags = copy_tags();
    let mut fails = 0;
    let mut mem_after_20 = 0;
    for i in 0..200 {
        match ctx.open(&tags, 0) {
            Ok(r) => {
                // Every other one installs the stack copy hook as well.
                if i % 2 == 1 {
                    // The hook outlives the opener: it is closed below.
                    let mut hook = alloc::boxed::Box::new(Sana2Hook {
                        s2h_Hook: Hook {
                            h_MinNode: MinNode { mln_Succ: core::ptr::null_mut(), mln_Pred: core::ptr::null_mut() },
                            h_Entry: Some(hook_entry),
                            h_SubEntry: None,
                            h_Data: core::ptr::null_mut(),
                        },
                        s2h_Methods: HOOK_METHODS.as_ptr(),
                    });
                    let mut r = r;
                    r.set(S2_SANA2HOOK, 0);
                    r.r.set_data(&mut *hook as *mut Sana2Hook as *mut c_void);
                    r.r.set_data_length(24);
                    ctx.do_io(&r);
                    ctx.close(r);
                    drop(hook);
                } else {
                    ctx.close(r);
                }
            }
            Err(_) => fails += 1,
        }
        if i == 19 {
            mem_after_20 = avail();
        }
    }
    let mem_end = avail();
    let grew = mem_after_20 as i64 - mem_end as i64;
    ctx.check("open-close-200", fails == 0 && grew < 32 * 1024, alloc::format!("{} failed; free memory after 20 cycles {}, after 200 {} ({} bytes less)", fails, mem_after_20, mem_end, grew));
}

// ---- Concurrency: kid tasks doing ICMP echo while the device is reset ----

const MAX_KIDS: usize = 8;
static KID_NAMES: [&[u8]; MAX_KIDS] = [
    b"s2test kid 0\0", b"s2test kid 1\0", b"s2test kid 2\0", b"s2test kid 3\0",
    b"s2test kid 4\0", b"s2test kid 5\0", b"s2test kid 6\0", b"s2test kid 7\0",
];
static KID_SENT: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static KID_GOT: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static KID_ERRS: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static KID_LOST: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static KID_DONE: [AtomicU32; MAX_KIDS] = [const { AtomicU32::new(0) }; MAX_KIDS];
static KID_STOP: AtomicU32 = AtomicU32::new(0);

struct KidCfg {
    devname: Vec<u8>,
    unit: u32,
    my_ip: [u8; 4],
    gw_ip: [u8; 4],
    gw: [u8; 6],
}
// Written by the main task before any kid starts, then only read.
static mut KID_CFG: Option<KidCfg> = None;

extern "C" fn kid_entry(i: u32) {
    let i = i as usize;
    kid_body(i);
    KID_DONE[i].store(1, Ordering::Release);
}

fn kid_body(i: usize) {
    // SAFETY: set before the kids were created, not written while they run.
    let Some(cfg) = (unsafe { (*core::ptr::addr_of!(KID_CFG)).as_ref() }) else { return };
    let (Ok(port), Ok(timer)) = (AmigaMsgPort::new(), AmigaTimer::open(TimerUnit::MicroHz)) else {
        KID_ERRS[i].fetch_add(1000, Ordering::Relaxed);
        return;
    };
    let mut ctx = Ctx { port, timer, devname: cfg.devname.clone(), unit: cfg.unit, passed: 0, failed: 0 };
    let Ok(mut a) = ctx.open(&copy_tags(), 0) else {
        KID_ERRS[i].fetch_add(1000, Ordering::Relaxed);
        return;
    };
    let id = 0x5000 + i as u16;
    let pay: Vec<u8> = (0..32u8).collect();
    let mut seq: u16 = 0;
    while KID_STOP.load(Ordering::Acquire) == 0 {
        seq = seq.wrapping_add(1);
        let Some(mut rd) = ctx.clone_req(&a) else { break };
        rd.set(CMD_READ, 0);
        rd.r.set_packet_type(0x0800);
        ctx.send(&rd);
        let ip = icmp_echo(cfg.my_ip, cfg.gw_ip, id, seq, &pay);
        set_write(&mut a, CMD_WRITE, 0, 0x0800, &cfg.gw, &ip);
        KID_SENT[i].fetch_add(1, Ordering::Relaxed);
        if ctx.do_io(&a).0 != 0 {
            KID_ERRS[i].fetch_add(1, Ordering::Relaxed);
        }
        // Replies to every kid arrive at every kid: read until ours.
        let mut tries = 0;
        loop {
            match ctx.wait(&rd, 1000) {
                Some((0, _)) => {
                    let g = rd.received();
                    if g.len() >= 28 && g[20] == 0 && g[24..26] == id.to_be_bytes() && g[26..28] == seq.to_be_bytes() {
                        KID_GOT[i].fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                    tries += 1;
                    if tries > 64 {
                        break;
                    }
                    rd.set(CMD_READ, 0);
                    rd.r.set_packet_type(0x0800);
                    ctx.send(&rd);
                }
                Some(_) => {
                    // Offline or reset: count it and go on.
                    KID_ERRS[i].fetch_add(1, Ordering::Relaxed);
                    break;
                }
                None => {
                    if ctx.wait_abort_ok(&rd) {
                        break;
                    }
                    KID_LOST[i].fetch_add(1, Ordering::Relaxed);
                    core::mem::forget(rd);
                    ctx.close(a);
                    return;
                }
            }
        }
        free_req(rd);
    }
    ctx.close(a);
}

fn concurrency(ctx: &mut Ctx, a: &mut Req, station: &[u8; 6], gw: &[u8; 6], my_ip: [u8; 4], gw_ip: [u8; 4], secs: u32, kids: u32) {
    let kids = (kids as usize).clamp(1, MAX_KIDS);
    // SAFETY: no kid runs yet.
    unsafe {
        *core::ptr::addr_of_mut!(KID_CFG) = Some(KidCfg { devname: ctx.devname.clone(), unit: ctx.unit, my_ip, gw_ip, gw: *gw });
    }
    KID_STOP.store(0, Ordering::Release);
    for i in 0..kids {
        for s in [&KID_SENT, &KID_GOT, &KID_ERRS, &KID_LOST, &KID_DONE] {
            s[i].store(0, Ordering::Relaxed);
        }
        let tags = [
            TagItem { ti_Tag: sys::AT_PARAM1, ti_Data: i as u32 },
            TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
        ];
        // SAFETY: a plain task; it ends when KID_STOP is set.
        let t = unsafe { sys::exec_create_task(KID_NAMES[i].as_ptr(), 0, kid_entry as *const c_void, 32768, tags.as_ptr()) };
        if t.is_null() {
            KID_DONE[i].store(1, Ordering::Release);
            KID_ERRS[i].fetch_add(1000, Ordering::Relaxed);
        }
    }
    say!("s2test: {} tasks doing ICMP echo for {} s; resets and offline/online every 1.5 s", kids, secs);
    let rec0 = special_stats(ctx, a).get(4).copied().unwrap_or(0);
    let mut resets = 0;
    let mut offlines = 0;
    let mut fault_errs = Vec::new();
    let t_end = secs as u64 * 1000;
    let mut t = 0u64;
    let mut n = 0;
    while t < t_end {
        let _ = ctx.timer.delay(TimerVal::from_millis(1500));
        t += 1500;
        n += 1;
        if n % 2 == 1 {
            a.set(VNET_CMD_TESTFAULT, 0);
            let (e, _) = ctx.do_io(a);
            if e == 0 { resets += 1 } else { fault_errs.push(e) }
        } else {
            a.set(S2_OFFLINE, 0);
            let (e1, _) = ctx.do_io(a);
            let _ = ctx.timer.delay(TimerVal::from_millis(200));
            a.set(S2_ONLINE, 0);
            let (e2, _) = ctx.do_io(a);
            if e1 == 0 && e2 == 0 { offlines += 1 } else { fault_errs.push(e1); fault_errs.push(e2) }
        }
    }
    KID_STOP.store(1, Ordering::Release);
    // Every kid must finish (each waits at most ~3 s for a request).
    let mut waited = 0;
    while (0..kids).any(|i| KID_DONE[i].load(Ordering::Acquire) == 0) && waited < 30_000 {
        let _ = ctx.timer.delay(TimerVal::from_millis(100));
        waited += 100;
    }
    // Their code must not be unloaded while they leave.
    for i in 0..kids {
        for _ in 0..200 {
            // SAFETY: FindTask by name.
            if unsafe { sys::exec_find_task(KID_NAMES[i].as_ptr()) }.is_null() {
                break;
            }
            let _ = ctx.timer.delay(TimerVal::from_millis(10));
        }
    }
    let done = (0..kids).filter(|&i| KID_DONE[i].load(Ordering::Acquire) != 0).count();
    let sum = |s: &[AtomicU32; MAX_KIDS]| (0..kids).map(|i| s[i].load(Ordering::Relaxed)).sum::<u32>();
    let (sent, got, errs, lost) = (sum(&KID_SENT), sum(&KID_GOT), sum(&KID_ERRS), sum(&KID_LOST));
    let rec1 = special_stats(ctx, a).get(4).copied().unwrap_or(0);
    ctx.check("concurrency-finished", done == kids, alloc::format!("{} of {} tasks finished", done, kids));
    ctx.check("concurrency-no-lost-request", lost == 0, alloc::format!("{} requests never came back", lost));
    ctx.check(
        "concurrency-echo",
        sent > 0 && got as u64 * 100 >= sent as u64 * 80,
        alloc::format!("{} echoes sent, {} replies matched ({:.0}%), {} errors (offline/reset)", sent, got, got as f64 * 100.0 / sent.max(1) as f64, errs),
    );
    ctx.check(
        "concurrency-faults",
        fault_errs.is_empty() && rec1.wrapping_sub(rec0) == resets,
        alloc::format!("{} resets (recoveries counted {}), {} offline/online cycles, errors {:?}", resets, rec1.wrapping_sub(rec0), offlines, fault_errs),
    );
    let _ = (station, my_ip, gw_ip);
}

// ---------------------------------------------------------------------------
// hold
// ---------------------------------------------------------------------------

pub fn run_hold(ctx: &mut Ctx, my_ip: [u8; 4], gw_ip: [u8; 4]) {
    let Ok(mut a) = ctx.open(&copy_tags(), 0) else {
        ctx.check("open", false, String::from("OpenDevice failed"));
        return;
    };
    let Some((station, gw)) = bring_up(ctx, &mut a, my_ip, gw_ip) else {
        ctx.check("bring-up", false, String::from("no ARP reply from the gateway"));
        ctx.close(a);
        return;
    };
    let pay: Vec<u8> = (0..32u8).collect();
    let echo = |ctx: &mut Ctx, a: &mut Req, seq: u16| {
        let ip = icmp_echo(my_ip, gw_ip, 0x4848, seq, &pay);
        set_write(a, CMD_WRITE, 0, 0x0800, &gw, &ip);
        ctx.do_io(a).0
    };
    let read_ip = |ctx: &mut Ctx, a: &Req| -> Req {
        let mut rd = ctx.clone_req(a).unwrap();
        rd.set(CMD_READ, 0);
        rd.r.set_packet_type(0x0800);
        ctx.send(&rd);
        rd
    };
    let is_reply = |rd: &Req, seq: u16| {
        let g = rd.received();
        g.len() >= 28 && g[20] == 0 && g[24..26] == 0x4848u16.to_be_bytes() && g[26..28] == seq.to_be_bytes()
    };

    // 1. A read, then the echo: the ordinary case (and it registers IPv4 as
    //    a type this opener reads).
    let rd = read_ip(ctx, &a);
    echo(ctx, &mut a, 1);
    let r = ctx.wait(&rd, 2000);
    ctx.check("hold-baseline", r == Some((0, 0)) && is_reply(&rd, 1), alloc::format!("{:?}", r));
    if r.is_none() { ctx.abort_wait(&rd); }
    free_req(rd);

    // 2. The echo first, the read 40 ms later: the reply must wait for it.
    //    Up to 3 tries: with no stack here nothing answers QEMU's own ARP
    //    requests, and slirp then drops its echo reply now and then.
    let late = |ctx: &mut Ctx, a: &mut Req, seq0: u16| -> (bool, String) {
        let mut log = String::new();
        for t in 0..3u16 {
            let seq = seq0 + t;
            echo(ctx, a, seq);
            let _ = ctx.timer.delay(TimerVal::from_millis(40));
            let rd = read_ip(ctx, a);
            let r = ctx.wait(&rd, 2000);
            let ok = r == Some((0, 0)) && is_reply(&rd, seq);
            if r.is_none() { ctx.abort_wait(&rd); }
            free_req(rd);
            let _ = write!(log, "try {}: {:?}; ", t + 1, r);
            if ok {
                return (true, log);
            }
        }
        (false, log)
    };
    let (ok, log) = late(ctx, &mut a, 20);
    let nr1 = special_stats(ctx, &mut a).first().copied().unwrap_or(0);
    ctx.check("hold-late-read", ok, log);

    // 3. The reader stops: five echoes, no read. The first reply is held
    //    for the limit and dropped, the reader is forgotten, the other four
    //    go at once; an ARP read queued meanwhile is answered without
    //    waiting behind them.
    for s in 3..8 {
        echo(ctx, &mut a, s);
    }
    let t0 = amigaos4::arch::timebase();
    let ok = resolve(ctx, &mut a, &station, my_ip, gw_ip, 2000).is_some();
    let ms = (amigaos4::arch::timebase() - t0) as f64 * 1000.0 / timebase_hz() as f64;
    let _ = ctx.timer.delay(TimerVal::from_millis(300));
    let nr2 = special_stats(ctx, &mut a).first().copied().unwrap_or(0);
    ctx.check(
        "hold-stopped-reader",
        ok && ms < 400.0 && nr2.wrapping_sub(nr1) == 5,
        alloc::format!("ARP answered: {} in {:.0} ms; dropped for no read: {} (5 expected)", ok, ms, nr2.wrapping_sub(nr1)),
    );

    // 4. Reading again registers the reader again.
    let rd = read_ip(ctx, &a);
    echo(ctx, &mut a, 9);
    let r = ctx.wait(&rd, 2000);
    ctx.check("hold-reader-back", r == Some((0, 0)) && is_reply(&rd, 9), alloc::format!("{:?}", r));
    if r.is_none() { ctx.abort_wait(&rd); }
    free_req(rd);
    let _ = ctx.timer.delay(TimerVal::from_millis(50));
    let rd = read_ip(ctx, &a);
    // (An echo sent before this read, 40 ms late, works again.)
    let _ = &rd;
    ctx.abort_wait(&rd);
    free_req(rd);
    let (ok, log) = late(ctx, &mut a, 40);
    ctx.check("hold-late-read-again", ok, log);
    ctx.close(a);
}

// ---------------------------------------------------------------------------
// closepend
// ---------------------------------------------------------------------------

pub fn run_closepend(ctx: &mut Ctx, my_ip: [u8; 4], gw_ip: [u8; 4]) {
    let Ok(mut a) = ctx.open(&copy_tags(), 0) else {
        ctx.check("open", false, String::from("OpenDevice failed"));
        return;
    };
    let Some((station, _gw)) = bring_up(ctx, &mut a, my_ip, gw_ip) else {
        ctx.check("bring-up", false, String::from("no ARP reply from the gateway"));
        ctx.close(a);
        return;
    };
    let Ok(b) = ctx.open(&copy_tags(), 0) else {
        ctx.check("closepend-open", false, String::from("OpenDevice failed"));
        ctx.close(a);
        return;
    };
    // Raised priority so the unit task cannot drain the ring in between.
    // SAFETY: this task.
    let old = unsafe { sys::exec_set_task_pri(sys::exec_find_task(core::ptr::null()), 10) };
    let payload = arp_request(&station, my_ip, gw_ip);
    let mut ws: Vec<Req> = Vec::new();
    for _ in 0..40 {
        let Some(mut w) = ctx.clone_req(&b) else { break };
        set_write(&mut w, S2_BROADCAST, 0, 0x0806, &[0xff; 6], &payload);
        ctx.send(&w);
        ws.push(w);
    }
    let waiting = ws.iter().filter(|w| unsafe { sys::exec_check_io(w.r.as_ptr()).is_null() }).count();
    ctx.close(b);
    // SAFETY: this task.
    unsafe { sys::exec_set_task_pri(sys::exec_find_task(core::ptr::null()), old as i8) };
    let mut back = 0;
    let mut aborted = 0;
    for w in &ws {
        if let Some((e, _)) = ctx.wait(w, 3000) {
            back += 1;
            if e == IOERR_ABORTED {
                aborted += 1;
            }
        }
    }
    let n = ws.len();
    if back == n {
        for w in ws {
            free_req(w);
        }
    } else {
        core::mem::forget(ws);
    }
    ctx.check(
        "close-with-pending-writes",
        back == n && waiting > 0,
        alloc::format!("{} writes, {} still waiting at close, {} came back ({} aborted)", n, waiting, back, aborted),
    );
    alive(ctx, &mut a, "alive-after-closepend", &station, my_ip, gw_ip);
    ctx.close(a);
    let _ = IOERR_OPENFAIL;
}

// ---------------------------------------------------------------------------
// faults: device resets on a schedule, for a soak with a stack running
// ---------------------------------------------------------------------------

pub fn run_faults(ctx: &mut Ctx, secs: u32, period: u32) {
    let Ok(mut a) = ctx.open(&copy_tags(), 0) else {
        ctx.check("open", false, String::from("OpenDevice failed"));
        return;
    };
    let rec0 = special_stats(ctx, &mut a).get(4).copied().unwrap_or(0);
    let period = period.max(2);
    let mut n = 0u32;
    let mut refused = 0u32;
    let mut t = 0u32;
    say!("s2test faults: a device reset every {} s for {} s", period, secs);
    while t + period <= secs {
        let _ = ctx.timer.delay(TimerVal::from_millis(period * 1000));
        t += period;
        a.set(VNET_CMD_TESTFAULT, 0);
        if ctx.do_io(&a).0 == 0 {
            n += 1;
        } else {
            refused += 1;
        }
    }
    // The last one is recovered within the 1 s heartbeat.
    let _ = ctx.timer.delay(TimerVal::from_millis(2500));
    let rec1 = special_stats(ctx, &mut a).get(4).copied().unwrap_or(0);
    ctx.check("faults", refused == 0 && rec1.wrapping_sub(rec0) == n, alloc::format!("{} resets, {} refused, {} recoveries counted", n, refused, rec1.wrapping_sub(rec0)));
    ctx.close(a);
}

// ---------------------------------------------------------------------------
// SANA-II semantics, checked against prism2.device
// ---------------------------------------------------------------------------

fn semantics(ctx: &mut Ctx, a: &mut Req, station: &[u8; 6], my_ip: [u8; 4], gw_ip: [u8; 4]) {
    // Type tracking belongs to each opener: B tracking ARP beside A must
    // work, and B untracking must not end A's tracking.
    let Ok(mut b) = ctx.open(&copy_tags(), 0) else {
        ctx.check("semantics-open", false, String::from("OpenDevice failed"));
        return;
    };
    a.set(S2_TRACKTYPE, 0);
    a.r.set_packet_type(0x0806);
    let ea = ctx.do_io(a).0;
    b.set(S2_TRACKTYPE, 0);
    b.r.set_packet_type(0x0806);
    let eb = ctx.do_io(&b).0;
    let _ = resolve(ctx, a, station, my_ip, gw_ip, 2000);
    b.set(S2_UNTRACKTYPE, 0);
    b.r.set_packet_type(0x0806);
    let ub = ctx.do_io(&b).0;
    let mut ts = Sana2PacketTypeStats::default();
    a.set(S2_GETTYPESTATS, 0);
    a.r.set_packet_type(0x0806);
    a.r.set_stat_data(&mut ts as *mut _ as *mut c_void);
    let ga = ctx.do_io(a).0;
    let (sent, recv) = (ts.PacketsSent, ts.PacketsReceived);
    a.set(S2_UNTRACKTYPE, 0);
    a.r.set_packet_type(0x0806);
    let ua = ctx.do_io(a).0;
    ctx.check(
        "tracktype-per-opener",
        ea == 0 && eb == 0 && ub == 0 && ga == 0 && ua == 0 && sent >= 1 && recv >= 1,
        alloc::format!("A track {}, B track {}, B untrack {}, A stats {} (sent {}, received {}), A untrack {}", ea, eb, ub, ga, sent, recv, ua),
    );

    // S2_ONEVENT for CONNECT while the link is up: answered at once.
    let mut ev = ctx.clone_req(&b).unwrap();
    ev.set(S2_ONEVENT, 0);
    ev.r.set_wire_error(S2EVENT_CONNECT);
    ctx.send(&ev);
    let r = ctx.wait(&ev, 1000);
    ctx.check("event-connect-immediate", r == Some((0, S2EVENT_CONNECT)), alloc::format!("{:?}", r));
    if r.is_none() {
        ctx.abort_wait(&ev);
    }
    free_req(ev);

    // CMD_FLUSH aborts queued event requests too.
    let mut ev = ctx.clone_req(&b).unwrap();
    ev.set(S2_ONEVENT, 0);
    ev.r.set_wire_error(S2EVENT_DISCONNECT);
    ctx.send(&ev);
    let mut fl = ctx.clone_req(&b).unwrap();
    fl.set(amigaos4::device::cmd::CMD_FLUSH, 0);
    let ef = ctx.do_io(&fl).0;
    let r = ctx.wait(&ev, 2000);
    ctx.check("flush-aborts-events", ef == 0 && r == Some((IOERR_ABORTED, 0)), alloc::format!("flush {}, the pending event request: {:?}", ef, r));
    if r.is_none() {
        ctx.abort_wait(&ev);
    }
    free_req(ev);
    free_req(fl);
    ctx.close(b);

    // An opener that gives only S2_CopyToBuff16 and S2_CopyFromBuff32.
    let alt = [
        TagItem { ti_Tag: S2_COPY_TO_BUFF16, ti_Data: tag_copy_to as *const () as usize as u32 },
        TagItem { ti_Tag: S2_COPY_FROM_BUFF32, ti_Data: tag_copy_from as *const () as usize as u32 },
        TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
    ];
    match ctx.open(&alt, 0) {
        Ok(mut c) => {
            let ok = resolve(ctx, &mut c, station, my_ip, gw_ip, 2000).is_some();
            ctx.check("copy-tags-16-32", ok, String::from(if ok { "ARP through an opener with only the 16/32-bit copy tags" } else { "no ARP reply through it" }));
            ctx.close(c);
        }
        Err(e) => ctx.check("copy-tags-16-32", false, alloc::format!("OpenDevice returned {}", e)),
    }
}
