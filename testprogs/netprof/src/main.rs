//! netprof: where the time goes in a network transfer on AmigaOS 4.
//!
//! It runs a controlled TCP load against `netprof-host` (tools/netprof-host)
//! and reads every counter the system keeps along the way, so the layers
//! can be told apart: the CPU (an idle meter), the program's own calls,
//! Roadshow's TCP, IP and mbuf statistics, the interface's frames, drops
//! and SANA-II request queues, and the socket's queues. Nothing in it is
//! driver-specific: it works with any SANA-II driver Roadshow runs.
//!
//!   netprof info
//!   netprof cpu
//!   netprof burn [s=N]   (spin N seconds at the caller's priority)
//!   netprof watch [iface=NAME] [secs=N]
//!   netprof recv HOST [port=N] [mb=N] [buf=N] [rcvbuf=N] [sndbuf=N] [nodelay] [iface=NAME] [quiet]
//!                [noidle] [nosample]   (also for send)
//!   netprof send HOST [port=N] [mb=N] [buf=N] [rcvbuf=N] [sndbuf=N] [nodelay] [iface=NAME] [quiet]
//!   netprof rtt HOST [port=N] [count=N] [size=N]
//!   (every mode but cpu: lib=NAME opens another stack by name, one with
//!   bsdsocket.library's interface)
//!
//! `recv` and `send` print one line a second and a report at the end:
//! the throughput, how busy the CPU was, the frames and packets per layer,
//! losses, queue depths, and what those say about where the limit is.
//! Ctrl-C stops a run early and still prints the report.

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
use amigaos4::arch::timebase;
use amigaos4::bsdsocket::*;
use amigaos4::port::AmigaMsgPort;
use amigaos4::sana2::{S2Req, S2_COPY_FROM_BUFF, S2_COPY_TO_BUFF, S2_GETGLOBALSTATS, S2_GETSPECIALSTATS};
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

const DEFAULT_PORT: u16 = 4500;
const SIGBREAKF_CTRL_C: u32 = 1 << 12;
const GCIT_NUMBER_OF_CPUS: Tag = sys::TAG_USER + 1;
const GCIT_MODEL_STRING: Tag = sys::TAG_USER + 4;
const GCIT_PROCESSOR_SPEED: Tag = sys::TAG_USER + 8;

fn break_pressed() -> bool {
    // SAFETY: reads and clears only the Ctrl-C signal of this task.
    unsafe { sys::exec_set_signal(0, SIGBREAKF_CTRL_C) & SIGBREAKF_CTRL_C != 0 }
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

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

struct Clock {
    hz: u64,
}
impl Clock {
    fn secs(&self, ticks: u64) -> f64 {
        ticks as f64 / self.hz as f64
    }
}

// ---------------------------------------------------------------------------
// The idle meter: a task at the lowest priority counts while nothing else
// wants the CPU. Its rate while the machine is quiet is the reference;
// busy = 1 - rate / reference.
// ---------------------------------------------------------------------------

static IDLE_COUNT: AtomicU32 = AtomicU32::new(0);
static IDLE_STOP: AtomicU32 = AtomicU32::new(0);
const IDLE_NAME: &[u8] = b"netprof idle meter\0";

extern "C" fn idle_task() {
    while IDLE_STOP.load(Ordering::Relaxed) == 0 {
        IDLE_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

struct IdleMeter {
    /// Counts per second with the machine quiet.
    reference: f64,
    /// False with `noidle`: no task, and busy is not measured.
    on: bool,
}

impl IdleMeter {
    /// The meter, or with `on` false a stand-in that measures nothing
    /// (`noidle`: on QEMU a guest that never halts can slow the emulated
    /// devices, so a run without it shows what the meter itself costs).
    fn start_if(on: bool, timer: &mut AmigaTimer, clock: &Clock) -> Option<IdleMeter> {
        if on { IdleMeter::start(timer, clock) } else { Some(IdleMeter { reference: 0.0, on: false }) }
    }
    fn start(timer: &mut AmigaTimer, clock: &Clock) -> Option<IdleMeter> {
        IDLE_STOP.store(0, Ordering::Release);
        let tags = [TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 }];
        // SAFETY: a plain task running idle_task; it ends when IDLE_STOP is set.
        let t = unsafe { sys::exec_create_task(IDLE_NAME.as_ptr(), -128, idle_task as *const c_void, 16384, tags.as_ptr()) };
        if t.is_null() {
            return None;
        }
        // Calibrate: one second with this task asleep.
        let _ = timer.delay(TimerVal::from_millis(100));
        let (c0, t0) = (IDLE_COUNT.load(Ordering::Relaxed), timebase());
        let _ = timer.delay(TimerVal::from_millis(1000));
        let (c1, t1) = (IDLE_COUNT.load(Ordering::Relaxed), timebase());
        let reference = c1.wrapping_sub(c0) as f64 / clock.secs(t1 - t0).max(1e-6);
        Some(IdleMeter { reference, on: true })
    }
    /// Busy fraction between two (count, timebase) readings.
    fn busy(&self, clock: &Clock, c0: u32, t0: u64, c1: u32, t1: u64) -> Option<f64> {
        if !self.on {
            return None;
        }
        let rate = c1.wrapping_sub(c0) as f64 / clock.secs(t1.saturating_sub(t0)).max(1e-6);
        Some((1.0 - rate / self.reference.max(1.0)).clamp(0.0, 1.0))
    }
    fn stop(self, timer: &mut AmigaTimer) {
        if !self.on {
            return;
        }
        IDLE_STOP.store(1, Ordering::Release);
        // The task's last instructions run after the flag: wait until it is
        // gone before this program's code can be unloaded.
        for _ in 0..500 {
            // SAFETY: FindTask by name.
            if unsafe { sys::exec_find_task(IDLE_NAME.as_ptr()) }.is_null() {
                return;
            }
            let _ = timer.delay(TimerVal::from_millis(10));
        }
    }
}

fn idle_now() -> u32 {
    IDLE_COUNT.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Counters
// ---------------------------------------------------------------------------

const IFQ_COUNTERS: [(Tag, &str); 17] = [
    (IFQ_PacketsReceived, "frames in"),
    (IFQ_PacketsSent, "frames out"),
    (IFQ_BadData, "bad data"),
    (IFQ_Overruns, "overruns"),
    (IFQ_UnknownTypes, "unknown types"),
    (IFQ_InputDrops, "input drops"),
    (IFQ_OutputDrops, "output drops"),
    (IFQ_InputErrors, "input errors"),
    (IFQ_OutputErrors, "output errors"),
    (IFQ_IPDrops, "IP drops"),
    (IFQ_ARPDrops, "ARP drops"),
    (IFQ_NumReadRequests, "read requests"),
    (IFQ_MaxReadRequests, "max read requests"),
    (IFQ_NumReadRequestsPending, "read requests pending"),
    (IFQ_NumWriteRequests, "write requests"),
    (IFQ_MaxWriteRequests, "max write requests"),
    (IFQ_NumWriteRequestsPending, "write requests pending"),
];
const Q_RX: usize = 0;
const Q_TX: usize = 1;
const Q_BAD: usize = 2;
const Q_OVERRUN: usize = 3;
const Q_INDROP: usize = 5;
const Q_OUTDROP: usize = 6;
const Q_INERR: usize = 7;
const Q_OUTERR: usize = 8;
const Q_IPDROP: usize = 9;
const Q_NREAD: usize = 11;
const Q_MAXREAD: usize = 12;
const Q_RPEND: usize = 13;
const Q_NWRITE: usize = 14;
const Q_WPEND: usize = 16;
const NIFQ: usize = IFQ_COUNTERS.len();

#[derive(Clone)]
struct Snap {
    t: u64,
    idle: u32,
    ifq: [u32; NIFQ],
    bytes_in: u64,
    bytes_out: u64,
    tcp: Vec<u32>,
    ip: Vec<u32>,
    mb: Vec<u32>,
    copy: SANA2CopyStats,
    drv: DrvSnap,
}

/// Reads the counters of one interface and the stack.
struct Probe<'a> {
    lib: &'a SocketLib,
    iface: String,
    /// Which IFQ_COUNTERS the stack answered (bit per entry).
    ifq_ok: u32,
    have_status: bool,
    /// The SANA-II device opened for its own statistics (not with `nodriver`).
    driver: Option<Driver>,
}

impl<'a> Probe<'a> {
    fn new(lib: &'a SocketLib, iface: &str, with_driver: bool) -> Probe<'a> {
        let have_status = lib.get_base_value(SBTC_HAVE_STATUS_API).map(|v| v != 0).unwrap_or(false);
        let driver = if with_driver { Driver::open(lib, iface) } else { None };
        let mut p = Probe { lib, iface: String::from(iface), ifq_ok: 0, have_status, driver };
        for (i, (tag, _)) in IFQ_COUNTERS.iter().enumerate() {
            if lib.query_u32(iface, *tag).is_ok() {
                p.ifq_ok |= 1 << i;
            }
        }
        p
    }
    fn ok(&self, i: usize) -> bool {
        self.ifq_ok & (1 << i) != 0
    }
    fn ifq(&self, out: &mut [u32; NIFQ]) {
        let mut tags: Vec<TagItem> = Vec::with_capacity(NIFQ);
        for (i, (tag, _)) in IFQ_COUNTERS.iter().enumerate() {
            if self.ok(i) {
                tags.push(TagItem { ti_Tag: *tag, ti_Data: &mut out[i] as *mut u32 as usize as u32 });
            }
        }
        // SAFETY: every tag here takes a 32-bit destination.
        let _ = unsafe { self.lib.query_interface(&self.iface, &tags) };
    }
    /// The read and write requests pending at the driver right now.
    fn pending(&self) -> (u32, u32) {
        let (mut r, mut w) = (0u32, 0u32);
        let mut tags: Vec<TagItem> = Vec::new();
        if self.ok(Q_RPEND) {
            tags.push(TagItem { ti_Tag: IFQ_NumReadRequestsPending, ti_Data: &mut r as *mut u32 as usize as u32 });
        }
        if self.ok(Q_WPEND) {
            tags.push(TagItem { ti_Tag: IFQ_NumWriteRequestsPending, ti_Data: &mut w as *mut u32 as usize as u32 });
        }
        // SAFETY: both take LONG *.
        let _ = unsafe { self.lib.query_interface(&self.iface, &tags) };
        (r, w)
    }
    fn stats(&self, kind: i32, n: usize) -> Vec<u32> {
        let mut v = vec![0u32; n];
        if self.have_status {
            let _ = self.lib.network_stats(kind, &mut v);
        }
        v
    }
    fn snap(&self) -> Snap {
        let mut ifq = [0u32; NIFQ];
        self.ifq(&mut ifq);
        Snap {
            t: timebase(),
            idle: idle_now(),
            ifq,
            bytes_in: self.lib.query_u64(&self.iface, IFQ_GetBytesIn).unwrap_or(0),
            bytes_out: self.lib.query_u64(&self.iface, IFQ_GetBytesOut).unwrap_or(0),
            tcp: self.stats(NETSTATUS_tcp, TCPSTAT_FIELDS.len()),
            ip: self.stats(NETSTATUS_ip, IPSTAT_FIELDS.len()),
            mb: self.stats(NETSTATUS_mb, MBSTAT_FIELDS.len()),
            copy: self.lib.copy_stats(&self.iface).unwrap_or_default(),
            drv: self.driver.as_ref().map(|d| d.snap()).unwrap_or_default(),
        }
    }
}

fn tcp(s: &Snap, name: &str) -> u32 {
    field_index(TCPSTAT_FIELDS, name).and_then(|i| s.tcp.get(i).copied()).unwrap_or(0)
}
fn ip(s: &Snap, name: &str) -> u32 {
    field_index(IPSTAT_FIELDS, name).and_then(|i| s.ip.get(i).copied()).unwrap_or(0)
}
fn mb(s: &Snap, name: &str) -> u32 {
    field_index(MBSTAT_FIELDS, name).and_then(|i| s.mb.get(i).copied()).unwrap_or(0)
}
fn dtcp(a: &Snap, b: &Snap, name: &str) -> u32 {
    tcp(b, name).wrapping_sub(tcp(a, name))
}
fn dq(a: &Snap, b: &Snap, i: usize) -> u32 {
    b.ifq[i].wrapping_sub(a.ifq[i])
}

/// Gauges sampled every 100 ms during a run.
#[derive(Default)]
struct Gauges {
    n: u32,
    rpend_min: u32,
    rpend_max: u32,
    rpend_sum: u64,
    rpend_zero: u32,
    wpend_max: u32,
    wpend_sum: u64,
    rq_max: u32,
    rq_sum: u64,
    sq_max: u32,
    sq_sum: u64,
    conn_seen: u32,
}

impl Gauges {
    fn sample(&mut self, p: &Probe, local_port: u16) {
        let (r, w) = p.pending();
        if self.n == 0 || r < self.rpend_min {
            self.rpend_min = r;
        }
        self.rpend_max = self.rpend_max.max(r);
        self.rpend_sum += r as u64;
        if r == 0 {
            self.rpend_zero += 1;
        }
        self.wpend_max = self.wpend_max.max(w);
        self.wpend_sum += w as u64;
        if local_port != 0 && p.have_status {
            if let Ok(conns) = p.lib.connections(NETSTATUS_tcp_sockets) {
                if let Some(c) = conns.iter().find(|c| { let lp = c.pcd_local_port; lp == local_port }) {
                    let (rq, sq) = (c.pcd_receive_queue_size, c.pcd_send_queue_size);
                    self.rq_max = self.rq_max.max(rq);
                    self.rq_sum += rq as u64;
                    self.sq_max = self.sq_max.max(sq);
                    self.sq_sum += sq as u64;
                    self.conn_seen += 1;
                }
            }
        }
        self.n += 1;
    }
}

// ---------------------------------------------------------------------------
// The driver's own statistics: netprof opens the interface's SANA-II device
// as one more opener (it never reads or writes, so its copy functions are
// never called) and asks for S2_GETGLOBALSTATS and S2_GETSPECIALSTATS.
// Roadshow cannot see a frame the driver drops; these can.
// ---------------------------------------------------------------------------

unsafe extern "C" fn no_copy(_to: *mut c_void, _from: *mut c_void, _n: u32) -> u32 {
    0
}

struct Driver {
    _port: AmigaMsgPort,
    req: S2Req,
    name: String,
}

#[derive(Clone, Default)]
struct DrvSnap {
    /// PacketsReceived, PacketsSent, BadData, Overruns, UnknownTypes, Reconfigurations.
    global: Option<[u32; 6]>,
    /// (type, count, name) for each special record.
    special: Vec<(u32, u32, String)>,
}

impl Driver {
    fn open(lib: &SocketLib, iface: &str) -> Option<Driver> {
        let (dev, unit) = lib.interface_device(iface).ok()?;
        let port = AmigaMsgPort::new().ok()?;
        let tags = [
            TagItem { ti_Tag: S2_COPY_TO_BUFF, ti_Data: no_copy as *const () as usize as u32 },
            TagItem { ti_Tag: S2_COPY_FROM_BUFF, ti_Data: no_copy as *const () as usize as u32 },
            TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
        ];
        // Exec finds a loaded device by its node name; a disk device that is
        // not loaded yet is under DEVS:Networks/.
        let mut names: Vec<String> = vec![dev.clone()];
        if !dev.contains('/') && !dev.contains(':') {
            names.push(alloc::format!("networks/{}", dev));
        }
        for n in names {
            // SAFETY: an 88-byte request on our port; the tags live through
            // OpenDevice, which copies what it keeps.
            unsafe {
                let ior = sys::exec_create_iorequest(port.as_ptr(), 88);
                let Some(req) = S2Req::from_raw(ior) else { return None };
                req.set_buffer_management(tags.as_ptr() as *mut c_void);
                let mut c = n.clone().into_bytes();
                c.push(0);
                if sys::exec_open_device(c.as_ptr(), unit, ior, 0) == 0 {
                    return Some(Driver { _port: port, req, name: alloc::format!("{} unit {}", n, unit) });
                }
                sys::exec_delete_iorequest(ior);
            }
        }
        None
    }

    fn snap(&self) -> DrvSnap {
        let mut s = DrvSnap::default();
        // SAFETY: an opened request; the stat buffers live through DoIO.
        unsafe {
            let mut g: [u32; 9] = [0; 9];
            let io = self.req.io();
            io.set_command(S2_GETGLOBALSTATS);
            io.set_flags(0);
            io.set_error(0);
            self.req.set_stat_data(g.as_mut_ptr() as *mut c_void);
            sys::exec_do_io(self.req.as_ptr());
            if io.error() == 0 {
                // Sana2DeviceStats: received, sent, bad, overruns, unused, unknown types, reconfigurations.
                s.global = Some([g[0], g[1], g[2], g[3], g[5], g[6]]);
            }
            const MAX: usize = 24;
            let mut sp = [0u32; 2 + 3 * MAX];
            sp[0] = MAX as u32;
            io.set_command(S2_GETSPECIALSTATS);
            io.set_flags(0);
            io.set_error(0);
            self.req.set_stat_data(sp.as_mut_ptr() as *mut c_void);
            sys::exec_do_io(self.req.as_ptr());
            if io.error() == 0 {
                let n = (sp[1] as usize).min(MAX);
                for i in 0..n {
                    let (ty, count, p) = (sp[2 + 3 * i], sp[3 + 3 * i], sp[4 + 3 * i] as usize as *const u8);
                    let mut name = String::new();
                    if !p.is_null() {
                        let mut l = 0;
                        while l < 120 && *p.add(l) != 0 {
                            l += 1;
                        }
                        name = String::from_utf8_lossy(core::slice::from_raw_parts(p, l)).into_owned();
                    }
                    s.special.push((ty, count, name));
                }
            }
        }
        s
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        // SAFETY: opened in `open`.
        unsafe {
            sys::exec_close_device(self.req.as_ptr());
            sys::exec_delete_iorequest(self.req.as_ptr());
        }
    }
}

/// The driver's counters that changed between two snapshots: (name, delta).
fn driver_deltas(a: &DrvSnap, b: &DrvSnap) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    if let (Some(x), Some(y)) = (a.global, b.global) {
        const N: [&str; 6] = ["frames received", "frames sent", "bad data", "overruns", "unknown types", "reconfigurations"];
        for i in 0..6 {
            out.push((String::from(N[i]), y[i].wrapping_sub(x[i])));
        }
    }
    for (ty, count, name) in &b.special {
        let before = a.special.iter().find(|r| r.0 == *ty).map(|r| r.1).unwrap_or(0);
        let label = if name.is_empty() { alloc::format!("special 0x{:x}", ty) } else { name.clone() };
        out.push((label, count.wrapping_sub(before)));
    }
    out
}

/// The cost counters per frame (virtionet.device's special records), when
/// the driver has them.
fn per_frame(dd: &[(String, u32)]) {
    let get = |n: &str| dd.iter().find(|(k, _)| k == n).map(|(_, v)| *v as f64);
    let (Some(rx), Some(tx), Some(irq)) = (get("frames received"), get("frames sent"), get("Interrupts claimed")) else { return };
    let frames = rx + tx;
    if frames == 0.0 {
        return;
    }
    let r = |a: Option<f64>, b: f64| match a {
        Some(a) if b > 0.0 => alloc::format!("{:.3}", a / b),
        _ => String::from("-"),
    };
    say!("Per frame ({} received, {} sent):", rx as u64, tx as u64);
    say!("  interrupts per frame: {}", r(Some(irq), frames));
    say!("  unit task wakes per frame: {} (wakes that received: {} per received frame)", r(get("Unit task wakes"), frames), r(get("Unit task wakes that received frames"), rx));
    say!("  transmit publishes per sent frame: {}", r(get("Transmit ring publishes"), tx));
    say!("  transmit notifications per sent frame: {}", r(get("Transmit notifications"), tx));
    say!("  receive ring notifications per received frame: {}", r(get("Receive ring notifications"), rx));
    say!("  pure ACKs sent per received frame: {}", r(get("Pure TCP ACKs sent"), rx));
    say!("  pure ACKs received per sent frame: {}", r(get("Pure TCP ACKs received"), tx));
    let us = |n: &str, per: f64| match get(n) {
        Some(t) if per > 0.0 => alloc::format!("{:.1}", t / 10.0 / per),
        _ => String::from("-"),
    };
    say!("  us in the transmit notification per notification: {}", us("Transmit notification time (0.1 us)", get("Transmit notifications").unwrap_or(0.0)));
    say!("  us in the ISR read per interrupt: {}", us("Interrupt ISR read time (0.1 us)", irq));
    say!("  us in the driver's write path per sent frame: {}", us("Write path time in the caller (0.1 us)", tx));
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_ip(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut it = s.split('.');
    for b in out.iter_mut() {
        *b = it.next()?.parse().ok()?;
    }
    if it.next().is_some() { None } else { Some(out) }
}

struct Args<'a> {
    words: Vec<&'a str>,
}
impl<'a> Args<'a> {
    fn num(&self, key: &str, d: u64) -> u64 {
        self.words.iter().find_map(|w| w.strip_prefix(key).and_then(|v| parse_size(v))).unwrap_or(d)
    }
    fn text(&self, key: &str) -> Option<&'a str> {
        self.words.iter().find_map(|w| w.strip_prefix(key))
    }
    fn flag(&self, f: &str) -> bool {
        self.words.iter().any(|w| w.eq_ignore_ascii_case(f))
    }
}

/// "65536", "64k", "2m".
fn parse_size(v: &str) -> Option<u64> {
    let v = v.trim();
    let (num, mul) = match v.as_bytes().last()? {
        b'k' | b'K' => (&v[..v.len() - 1], 1024),
        b'm' | b'M' => (&v[..v.len() - 1], 1024 * 1024),
        _ => (v, 1),
    };
    num.parse::<u64>().ok().map(|n| n * mul)
}

fn mbps(bytes: u64, secs: f64) -> f64 {
    bytes as f64 / secs.max(1e-9) / 1e6
}

fn pct(x: f64) -> f64 {
    x * 100.0
}

/// The interface to watch: `iface=NAME`, else the one on the host's
/// subnet, else the first one that is up and not loopback.
fn pick_iface(lib: &SocketLib, a: &Args, host: Option<[u8; 4]>) -> Option<String> {
    if let Some(n) = a.text("iface=") {
        return Some(String::from(n));
    }
    let names = lib.interfaces().ok()?;
    if let Some(h) = host {
        for n in &names {
            if let Ok((ip, mask)) = lib.interface_ipv4(n) {
                let same = (0..4).all(|i| ip[i] & mask[i] == h[i] & mask[i]);
                if same && mask != [0; 4] {
                    return Some(n.clone());
                }
            }
        }
    }
    names.into_iter().find(|n| {
        let up = lib.query_u32(n, IFQ_State).map(|s| s as i32 == SM_Up).unwrap_or(false);
        up && !n.eq_ignore_ascii_case("lo0")
    })
}

/// exec's CopyMemQuick speed on 1 MB blocks, as a reference for what
/// copying data costs on this machine. (Not this program's memcpy: without
/// a C runtime that one copies a byte at a time.)
fn memcpy_rate(clock: &Clock) -> f64 {
    let n = 1 << 20;
    let src = vec![0x5au32; n / 4];
    let mut dst = vec![0u32; n / 4];
    let t0 = timebase();
    let mut rounds = 0u64;
    while clock.secs(timebase() - t0) < 0.25 {
        // SAFETY: two longword-aligned 1 MB buffers.
        unsafe { sys::exec_copy_mem_quick(src.as_ptr() as _, dst.as_mut_ptr() as _, n as u32) };
        core::hint::black_box(&mut dst);
        rounds += 1;
    }
    mbps(rounds * n as u64, clock.secs(timebase() - t0))
}

// ---------------------------------------------------------------------------
// info, cpu, watch
// ---------------------------------------------------------------------------

fn cpu_line() -> String {
    let mut model: *const u8 = core::ptr::null();
    let mut mhz: u64 = 0;
    let mut ncpu: u32 = 0;
    // SAFETY: STRPTR *, uint64 *, uint32 * destinations.
    unsafe {
        let tags = [
            TagItem { ti_Tag: GCIT_MODEL_STRING, ti_Data: &mut model as *mut *const u8 as usize as u32 },
            TagItem { ti_Tag: GCIT_PROCESSOR_SPEED, ti_Data: &mut mhz as *mut u64 as usize as u32 },
            TagItem { ti_Tag: GCIT_NUMBER_OF_CPUS, ti_Data: &mut ncpu as *mut u32 as usize as u32 },
            TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
        ];
        sys::exec_get_cpuinfo(tags.as_ptr());
    }
    let mut s = String::new();
    if !model.is_null() {
        // SAFETY: exec's NUL-terminated model string.
        unsafe {
            let mut l = 0;
            while *model.add(l) != 0 {
                l += 1;
            }
            s.push_str(&String::from_utf8_lossy(core::slice::from_raw_parts(model, l)));
        }
    }
    let _ = write!(s, ", {} MHz, {} CPU(s), timebase {} Hz", mhz / 1_000_000, ncpu, timebase_hz());
    s
}

fn run_info(lib: &SocketLib) -> i32 {
    let (v, r) = lib.version();
    say!("CPU: {}", cpu_line());
    say!("bsdsocket.library {}.{}", v, r);
    let names = match lib.interfaces() {
        Ok(n) => n,
        Err(e) => {
            say!("netprof: ObtainInterfaceList failed: {}", e);
            return 20;
        }
    };
    for n in &names {
        let dev = lib.interface_device(n).map(|(d, u)| alloc::format!("{} unit {}", d, u)).unwrap_or_else(|_| String::from("-"));
        let (ip, mask) = lib.interface_ipv4(n).unwrap_or(([0; 4], [0; 4]));
        let q = |t| lib.query_u32(n, t).map(|v| v as i64).unwrap_or(-1);
        let st = q(IFQ_State) as i32;
        let state = if st == SM_Up { "up" } else if st == SM_Down { "down" } else { "?" };
        say!("{}: {}, {}, {}.{}.{}.{}/{}.{}.{}.{}, MTU {} (hardware {}), {} Mbit/s",
            n, dev, state, ip[0], ip[1], ip[2], ip[3], mask[0], mask[1], mask[2], mask[3],
            q(IFQ_MTU), q(IFQ_HardwareMTU), q(IFQ_BPS) / 1_000_000);
        say!("    read requests {} (in use at a time {}), write requests {} (in use at a time {})",
            q(IFQ_NumReadRequests), q(IFQ_MaxReadRequests), q(IFQ_NumWriteRequests), q(IFQ_MaxWriteRequests));
        say!("    frames in {}, out {}; bytes in {}, out {}",
            q(IFQ_PacketsReceived), q(IFQ_PacketsSent),
            lib.query_u64(n, IFQ_GetBytesIn).unwrap_or(0), lib.query_u64(n, IFQ_GetBytesOut).unwrap_or(0));
    }
    0
}

fn run_cpu(timer: &mut AmigaTimer, clock: &Clock) -> i32 {
    say!("CPU: {}", cpu_line());
    say!("CopyMemQuick: {:.0} MB/s (1 MB blocks)", memcpy_rate(clock));
    match IdleMeter::start(timer, clock) {
        Some(m) => {
            let (c0, t0) = (idle_now(), timebase());
            let _ = timer.delay(TimerVal::from_millis(2000));
            let (c1, t1) = (idle_now(), timebase());
            say!("idle meter: {:.0} counts/s quiet; the next 2 s busy {:.1}%", m.reference, pct(m.busy(clock, c0, t0, c1, t1).unwrap_or(0.0)));
            m.stop(timer);
            0
        }
        None => {
            say!("netprof: the idle meter task did not start");
            20
        }
    }
}

fn line_header() {
    say!("   s   MB/s  busy%  frames-in  frames-out  B/frame-in  rdpend  retx  ooo  dupack  drops");
}

fn print_line(clock: &Clock, meter: &IdleMeter, a: &Snap, b: &Snap, t_start: u64, payload: u64) {
    let dt = clock.secs(b.t - a.t);
    let fin = dq(a, b, Q_RX);
    let bin = b.bytes_in.wrapping_sub(a.bytes_in);
    let drops = dq(a, b, Q_INDROP) + dq(a, b, Q_OUTDROP) + dq(a, b, Q_OVERRUN);
    let busy = match meter.busy(clock, a.idle, a.t, b.idle, b.t) {
        Some(x) => alloc::format!("{:.1}", pct(x)),
        None => String::from("-"),
    };
    say!("{:4.0} {:6.2} {:>6} {:10} {:11} {:11} {:7} {:5} {:4} {:7} {:6}",
        clock.secs(b.t - t_start), mbps(payload, dt), busy,
        fin, dq(a, b, Q_TX), if fin > 0 { bin / fin as u64 } else { 0 }, b.ifq[Q_RPEND],
        dtcp(a, b, "tcps_sndrexmitpack"), dtcp(a, b, "tcps_rcvoopack"), dtcp(a, b, "tcps_rcvdupack"), drops);
}

fn run_watch(lib: &SocketLib, a: &Args, timer: &mut AmigaTimer, clock: &Clock) -> i32 {
    let Some(iface) = pick_iface(lib, a, None) else {
        say!("netprof: no interface to watch (give iface=NAME)");
        return 20;
    };
    let secs = a.num("secs=", 10);
    let probe = Probe::new(lib, &iface, !a.flag("nodriver"));
    let Some(meter) = IdleMeter::start_if(!a.flag("noidle"), timer, clock) else {
        say!("netprof: the idle meter task did not start");
        return 20;
    };
    say!("watching {} for {} s (Ctrl-C stops)", iface, secs);
    line_header();
    let first = probe.snap();
    let mut prev = first.clone();
    let mut g = Gauges::default();
    for _ in 0..secs {
        for _ in 0..10 {
            let _ = timer.delay(TimerVal::from_millis(100));
            g.sample(&probe, 0);
        }
        let s = probe.snap();
        let payload = s.bytes_in.wrapping_sub(prev.bytes_in);
        print_line(clock, &meter, &prev, &s, first.t, payload);
        prev = s;
        if break_pressed() {
            break;
        }
    }
    report_layers(clock, &meter, &probe, &first, &prev, &g, None);
    meter.stop(timer);
    0
}

// ---------------------------------------------------------------------------
// recv, send
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Dir {
    Recv,
    Send,
}

struct Calls {
    n: u64,
    ticks: u64,
    bytes: u64,
}

struct XferInfo {
    dir: Dir,
    bytes: u64,
    secs: f64,
    calls: Calls,
    buf: usize,
    rcvbuf: i32,
    sndbuf: i32,
    host_secs: Option<f64>,
    copy_rate: f64,
}

/// Read a line (up to '\n') from the socket.
fn read_line(lib: &SocketLib, fd: i32) -> Option<String> {
    let mut v = Vec::new();
    let mut b = [0u8; 1];
    while v.len() < 200 {
        match lib.recv(fd, &mut b, 0) {
            Ok(1) if b[0] == b'\n' => return Some(String::from_utf8_lossy(&v).into_owned()),
            Ok(1) => v.push(b[0]),
            _ => return None,
        }
    }
    None
}

fn run_xfer(lib: &SocketLib, a: &Args, dir: Dir, timer: &mut AmigaTimer, clock: &Clock) -> i32 {
    let Some(host) = a.words.get(1).and_then(|w| parse_ip(w)) else {
        say!("netprof: give the host's IPv4 address, e.g. netprof recv 10.0.3.2");
        return 20;
    };
    let port = a.num("port=", DEFAULT_PORT as u64) as u16;
    let total = a.num("mb=", 64) * 1024 * 1024;
    let buf_len = a.num("buf=", 32 * 1024).clamp(1, 16 * 1024 * 1024) as usize;
    let quiet = a.flag("quiet");
    // nosample: no 100 ms gauges and no per-second lines (what sampling costs).
    let nosample = a.flag("nosample");
    let Some(iface) = pick_iface(lib, a, Some(host)) else {
        say!("netprof: no interface found (give iface=NAME)");
        return 20;
    };
    let copy_rate = memcpy_rate(clock);
    let probe = Probe::new(lib, &iface, !a.flag("nodriver"));
    let dev = lib.interface_device(&iface).map(|(d, u)| alloc::format!("{} unit {}", d, u)).unwrap_or_default();

    let fd = match lib.socket(AF_INET, SOCK_STREAM, 0) {
        Ok(f) => f,
        Err(e) => {
            say!("netprof: socket failed: {}", e);
            return 20;
        }
    };
    if let Some(v) = a.text("rcvbuf=").and_then(parse_size) {
        if let Err(e) = lib.set_opt(fd, SOL_SOCKET, SO_RCVBUF, v as i32) {
            say!("netprof: SO_RCVBUF {}: {}", v, e);
        }
    }
    if let Some(v) = a.text("sndbuf=").and_then(parse_size) {
        if let Err(e) = lib.set_opt(fd, SOL_SOCKET, SO_SNDBUF, v as i32) {
            say!("netprof: SO_SNDBUF {}: {}", v, e);
        }
    }
    if a.flag("nodelay") {
        let _ = lib.set_opt(fd, IPPROTO_TCP, TCP_NODELAY, 1);
    }
    let Some(meter) = IdleMeter::start_if(!a.flag("noidle"), timer, clock) else {
        say!("netprof: the idle meter task did not start");
        let _ = lib.close(fd);
        return 20;
    };
    if let Err(e) = lib.connect(fd, &SockAddrIn::v4(host, port)) {
        say!("netprof: connect to {}.{}.{}.{}:{} failed: {} (is netprof-host running?)", host[0], host[1], host[2], host[3], port, e);
        meter.stop(timer);
        let _ = lib.close(fd);
        return 20;
    }
    let local_port = lib.local_addr(fd).map(|s| s.sin_port).unwrap_or(0);
    let rcvbuf = lib.get_opt(fd, SOL_SOCKET, SO_RCVBUF).unwrap_or(-1);
    let sndbuf = lib.get_opt(fd, SOL_SOCKET, SO_SNDBUF).unwrap_or(-1);
    let req = alloc::format!("NETPROF 1 {} {}\n", if dir == Dir::Recv { "SEND" } else { "RECV" }, total);
    if lib.send(fd, req.as_bytes(), 0).is_err() {
        say!("netprof: sending the request failed");
        meter.stop(timer);
        let _ = lib.close(fd);
        return 20;
    }
    say!("netprof {}: {} MB {} {}.{}.{}.{}:{} on {} ({}), buffer {}, rcvbuf {}, sndbuf {}",
        if dir == Dir::Recv { "recv" } else { "send" }, total / (1024 * 1024),
        if dir == Dir::Recv { "from" } else { "to" }, host[0], host[1], host[2], host[3], port,
        iface, dev, buf_len, rcvbuf, sndbuf);
    if !quiet {
        line_header();
    }

    // netprof-host checks that byte o of the stream is o % 251: the send
    // buffer holds that pattern with 251 bytes to spare, and each call
    // starts at the stream's offset into it.
    let mut buf = vec![0u8; buf_len + 251];
    if dir == Dir::Send {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
    }
    let mut g = Gauges::default();
    let first = probe.snap();
    let mut prev = first.clone();
    let mut done: u64 = 0;
    let mut prev_done: u64 = 0;
    let mut calls = Calls { n: 0, ticks: 0, bytes: 0 };
    let tick = clock.hz / 10;
    let mut next_tick = first.t + tick;
    let mut next_line = first.t + clock.hz;
    let mut stopped = false;
    while done < total {
        let want = ((total - done) as usize).min(buf_len);
        let t0 = timebase();
        let r = match dir {
            Dir::Recv => lib.recv(fd, &mut buf[..want], 0),
            Dir::Send => {
                let at = (done % 251) as usize;
                lib.send(fd, &buf[at..at + want], 0)
            }
        };
        let t1 = timebase();
        calls.n += 1;
        calls.ticks += t1 - t0;
        match r {
            Ok(0) => break,
            Ok(n) => {
                done += n as u64;
                calls.bytes += n as u64;
            }
            Err(e) => {
                if e == amigaos4::error::AmigaError::IoError(EINTR) {
                    stopped = true;
                } else {
                    say!("netprof: {} failed after {} bytes: {}", if dir == Dir::Recv { "recv" } else { "send" }, done, e);
                }
                break;
            }
        }
        if t1 >= next_tick && !nosample {
            g.sample(&probe, local_port);
            next_tick = t1 + tick;
        }
        if t1 >= next_line && !nosample {
            let s = probe.snap();
            if !quiet {
                print_line(clock, &meter, &prev, &s, first.t, done - prev_done);
            }
            prev = s;
            prev_done = done;
            next_line = t1 + clock.hz;
        }
        if break_pressed() {
            stopped = true;
            break;
        }
    }
    let last = probe.snap();
    let secs = clock.secs(last.t - first.t);
    let mut host_secs = None;
    if dir == Dir::Send && !stopped && done == total {
        // netprof-host answers "DONE <bytes> <microseconds>" once it has all.
        if let Some(l) = read_line(lib, fd) {
            let mut w = l.split_whitespace();
            if w.next() == Some("DONE") {
                let _bytes = w.next();
                host_secs = w.next().and_then(|u| u.parse::<u64>().ok()).map(|u| u as f64 / 1e6);
            }
        }
    }
    let _ = lib.close(fd);
    if stopped {
        say!("netprof: stopped (Ctrl-C) after {} bytes", done);
    }
    let x = XferInfo { dir, bytes: done, secs, calls, buf: buf_len, rcvbuf, sndbuf, host_secs, copy_rate };
    report_layers(clock, &meter, &probe, &first, &last, &g, Some(&x));
    meter.stop(timer);
    if done == total { 0 } else { 10 }
}

// ---------------------------------------------------------------------------
// rtt
// ---------------------------------------------------------------------------

fn run_rtt(lib: &SocketLib, a: &Args, clock: &Clock) -> i32 {
    let Some(host) = a.words.get(1).and_then(|w| parse_ip(w)) else {
        say!("netprof: give the host's IPv4 address, e.g. netprof rtt 10.0.3.2");
        return 20;
    };
    let port = a.num("port=", DEFAULT_PORT as u64) as u16;
    let count = a.num("count=", 200).max(1) as usize;
    let size = a.num("size=", 64).clamp(1, 65536) as usize;
    let fd = match lib.socket(AF_INET, SOCK_STREAM, 0) {
        Ok(f) => f,
        Err(e) => {
            say!("netprof: socket failed: {}", e);
            return 20;
        }
    };
    let _ = lib.set_opt(fd, IPPROTO_TCP, TCP_NODELAY, 1);
    if let Err(e) = lib.connect(fd, &SockAddrIn::v4(host, port)) {
        say!("netprof: connect failed: {} (is netprof-host running?)", e);
        let _ = lib.close(fd);
        return 20;
    }
    let req = alloc::format!("NETPROF 1 ECHO {}\n", size);
    let _ = lib.send(fd, req.as_bytes(), 0);
    let msg = vec![0xa5u8; size];
    let mut back = vec![0u8; size];
    let mut times: Vec<f64> = Vec::with_capacity(count);
    for _ in 0..count {
        let t0 = timebase();
        if lib.send(fd, &msg, 0).is_err() {
            break;
        }
        let mut got = 0;
        while got < size {
            match lib.recv(fd, &mut back[got..], 0) {
                Ok(0) | Err(_) => break,
                Ok(n) => got += n,
            }
        }
        if got < size {
            break;
        }
        times.push(clock.secs(timebase() - t0) * 1e3);
        if break_pressed() {
            break;
        }
    }
    let _ = lib.close(fd);
    if times.is_empty() {
        say!("netprof: no echo came back");
        return 20;
    }
    let avg = times.iter().sum::<f64>() / times.len() as f64;
    let mut sorted = times.clone();
    sorted.sort_by(|x, y| x.partial_cmp(y).unwrap_or(core::cmp::Ordering::Equal));
    let q = |p: f64| sorted[((sorted.len() - 1) as f64 * p) as usize];
    say!("netprof rtt: {} round trips of {} bytes over TCP (TCP_NODELAY) to {}.{}.{}.{}:{}", times.len(), size, host[0], host[1], host[2], host[3], port);
    say!("  min {:.3} ms, median {:.3} ms, avg {:.3} ms, 99% {:.3} ms, max {:.3} ms", sorted[0], q(0.5), avg, q(0.99), sorted[sorted.len() - 1]);
    say!("  a TCP window of W bytes can carry at most W / RTT: 32 KB at the median is {:.2} MB/s, 64 KB {:.2} MB/s",
        32768.0 / (q(0.5) / 1e3) / 1e6, 65536.0 / (q(0.5) / 1e3) / 1e6);
    0
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

fn report_layers(clock: &Clock, meter: &IdleMeter, probe: &Probe, a: &Snap, b: &Snap, g: &Gauges, x: Option<&XferInfo>) {
    let secs = clock.secs(b.t - a.t);
    let busy_m = meter.busy(clock, a.idle, a.t, b.idle, b.t);
    // Without the meter, no finding is drawn from the CPU.
    let busy = busy_m.unwrap_or(f64::NAN);
    say!("");
    say!("== netprof report: {} over {:.2} s ==", probe.iface, secs);
    if let Some(x) = x {
        say!("Throughput: {} bytes in {:.2} s = {:.2} MB/s ({} by this machine's clock)",
            x.bytes, x.secs, mbps(x.bytes, x.secs), if x.dir == Dir::Recv { "received" } else { "sent" });
        if let Some(h) = x.host_secs {
            say!("  netprof-host received them in {:.2} s = {:.2} MB/s", h, mbps(x.bytes, h));
        }
    }

    // CPU
    say!("CPU");
    match busy_m {
        Some(b) => say!("  busy {:.1}% (idle meter, against {:.0} counts/s measured quiet just before)", pct(b), meter.reference),
        None => say!("  busy: not measured (noidle)"),
    }
    if let Some(x) = x {
        let in_calls = clock.secs(x.calls.ticks) / x.secs.max(1e-9);
        let name = if x.dir == Dir::Recv { "recv()" } else { "send()" };
        say!("  inside {}: {:.1}% of the run, {} calls, {:.0} bytes per call (buffer {}); working or waiting",
            name, pct(in_calls.min(1.0)), x.calls.n, x.calls.bytes as f64 / (x.calls.n.max(1)) as f64, x.buf);
        say!("  CopyMemQuick here: {:.0} MB/s; this run moved {:.2} MB/s, {:.1}% of one copy's speed",
            x.copy_rate, mbps(x.bytes, x.secs), pct(mbps(x.bytes, x.secs) / x.copy_rate.max(1e-9)));
    }

    // Interface
    let fin = dq(a, b, Q_RX);
    let fout = dq(a, b, Q_TX);
    let bin = b.bytes_in.wrapping_sub(a.bytes_in);
    let bout = b.bytes_out.wrapping_sub(a.bytes_out);
    say!("Interface (Roadshow's counters for the SANA-II device)");
    say!("  in:  {} frames, {} bytes, {:.0} bytes/frame, {:.0} frames/s", fin, bin, bin as f64 / fin.max(1) as f64, fin as f64 / secs.max(1e-9));
    say!("  out: {} frames, {} bytes, {:.0} bytes/frame, {:.0} frames/s", fout, bout, bout as f64 / fout.max(1) as f64, fout as f64 / secs.max(1e-9));
    let losses_if = dq(a, b, Q_BAD) + dq(a, b, Q_OVERRUN) + dq(a, b, Q_INDROP) + dq(a, b, Q_OUTDROP) + dq(a, b, Q_INERR) + dq(a, b, Q_OUTERR) + dq(a, b, Q_IPDROP);
    say!("  bad data {}, overruns {}, input drops {}, output drops {}, input errors {}, output errors {}, IP drops {}",
        dq(a, b, Q_BAD), dq(a, b, Q_OVERRUN), dq(a, b, Q_INDROP), dq(a, b, Q_OUTDROP), dq(a, b, Q_INERR), dq(a, b, Q_OUTERR), dq(a, b, Q_IPDROP));
    if g.n > 0 {
        say!("  read requests: {} allocated, {} in use at a time; pending at the driver min {}, avg {:.1}, max {}; none pending in {} of {} samples",
            b.ifq[Q_NREAD], b.ifq[Q_MAXREAD], g.rpend_min, g.rpend_sum as f64 / g.n as f64, g.rpend_max, g.rpend_zero, g.n);
        say!("  write requests: {} allocated; pending avg {:.1}, max {}", b.ifq[Q_NWRITE], g.wpend_sum as f64 / g.n as f64, g.wpend_max);
    }
    let c0 = a.copy;
    let c1 = b.copy;
    say!("  SANA-II copy calls: in {} (DMA {}), out {} (DMA {}), word out {}",
        c1.s2cs_ByteIn.wrapping_sub(c0.s2cs_ByteIn), c1.s2cs_DMAIn.wrapping_sub(c0.s2cs_DMAIn),
        c1.s2cs_ByteOut.wrapping_sub(c0.s2cs_ByteOut), c1.s2cs_DMAOut.wrapping_sub(c0.s2cs_DMAOut),
        c1.s2cs_WordOut.wrapping_sub(c0.s2cs_WordOut));

    // The driver itself
    let dd = driver_deltas(&a.drv, &b.drv);
    match &probe.driver {
        Some(d) => {
            say!("Driver ({}: its own SANA-II statistics)", d.name);
            if dd.is_empty() {
                say!("  the driver answers neither S2_GETGLOBALSTATS nor S2_GETSPECIALSTATS");
            }
            for (name, v) in &dd {
                say!("  {}: {}", name, v);
            }
            per_frame(&dd);
        }
        None => say!("Driver: not opened (nodriver, or OpenDevice refused a second opener)"),
    }

    // TCP / IP / mbufs
    let rexmit = dtcp(a, b, "tcps_sndrexmitpack");
    let ooo = dtcp(a, b, "tcps_rcvoopack");
    let dup = dtcp(a, b, "tcps_rcvduppack") + dtcp(a, b, "tcps_rcvpartduppack");
    let dupack = dtcp(a, b, "tcps_rcvdupack");
    let afterwin = dtcp(a, b, "tcps_rcvpackafterwin");
    let rto = dtcp(a, b, "tcps_rexmttimeo");
    let datain = dtcp(a, b, "tcps_rcvpack");
    let dataout = dtcp(a, b, "tcps_sndpack");
    let acks = dtcp(a, b, "tcps_sndacks");
    let ackin = dtcp(a, b, "tcps_rcvackpack");
    if probe.have_status {
        say!("TCP (all connections)");
        say!("  in:  {} packets, {} data packets in order ({} bytes), {} duplicate, {} out of order, {} after the window, {} window probes",
            dtcp(a, b, "tcps_rcvtotal"), datain, dtcp(a, b, "tcps_rcvbyte"), dup, ooo, afterwin, dtcp(a, b, "tcps_rcvwinprobe"));
        say!("       {} acks, {} duplicate acks, {} window updates, {} bad checksum",
            ackin, dupack, dtcp(a, b, "tcps_rcvwinupd"), dtcp(a, b, "tcps_rcvbadsum"));
        say!("  out: {} packets, {} data packets ({} bytes), {} ack-only ({} delayed), {} window updates, {} window probes",
            dtcp(a, b, "tcps_sndtotal"), dataout, dtcp(a, b, "tcps_sndbyte"), acks, dtcp(a, b, "tcps_delack"),
            dtcp(a, b, "tcps_sndwinup"), dtcp(a, b, "tcps_sndprobe"));
        say!("       {} retransmitted ({} bytes), {} retransmit timeouts, {} persist timeouts",
            rexmit, dtcp(a, b, "tcps_sndrexmitbyte"), rto, dtcp(a, b, "tcps_persisttimeo"));
        say!("  header prediction: {} acks, {} data packets", dtcp(a, b, "tcps_predack"), dtcp(a, b, "tcps_preddat"));
        let ipd = |n: &str| ip(b, n).wrapping_sub(ip(a, n));
        let mbd = |n: &str| mb(b, n).wrapping_sub(mb(a, n));
        say!("IP: {} in, {} delivered, {} out, {} dropped on output, {} bad checksum, {} fragments",
            ipd("ips_total"), ipd("ips_delivered"), ipd("ips_localout"), ipd("ips_odropped"), ipd("ips_badsum"), ipd("ips_fragments"));
        say!("mbufs: {} drops, {} waits, {} drains; {} clusters free now",
            mbd("m_drops"), mbd("m_wait"), mbd("m_drain"), mb(b, "m_clfree"));
        if g.conn_seen > 0 {
            if let Some(x) = x {
                say!("Socket queues: receive max {} avg {:.0} bytes (rcvbuf {}); send max {} avg {:.0} bytes (sndbuf {})",
                    g.rq_max, g.rq_sum as f64 / g.conn_seen as f64, x.rcvbuf, g.sq_max, g.sq_sum as f64 / g.conn_seen as f64, x.sndbuf);
            }
        }
    } else {
        say!("TCP: the stack has no GetNetworkStatistics (SBTC_HAVE_STATUS_API)");
    }

    // What it means
    say!("Findings");
    let mut any = false;
    let mut f = |s: String| {
        say!("  - {}", s);
        any = true;
    };
    let lost = rexmit + ooo + afterwin + losses_if + rto;
    if fin + fout < 10 * secs.max(1.0) as u32 {
        f(String::from("Almost no traffic passed this interface: there is nothing to measure."));
    } else if busy >= 0.90 {
        f(alloc::format!("The CPU is the limit: it was busy {:.0}% of the run. Faster code anywhere on the path (driver, stack, program) moves the figure; a faster link does not.", pct(busy)));
        if let Some(x) = x {
            let in_calls = clock.secs(x.calls.ticks) / x.secs.max(1e-9);
            if in_calls > 0.7 {
                f(alloc::format!("{:.0}% of the time this program sat inside the socket call: the stack's protocol work and its copy run there and in Roadshow's task, and the driver and interrupts take the rest.", pct(in_calls.min(1.0))));
            }
        }
    } else if busy < 0.70 && lost == 0 {
        f(alloc::format!("The CPU had time to spare ({:.0}% busy) and nothing was lost: the limit is outside this machine's CPU: the peer, the link, or the TCP window against the round trip (see netprof rtt).", pct(busy)));
    } else if busy < 0.90 && busy_m.is_some() {
        f(alloc::format!("The CPU was {:.0}% busy: close to its limit, but waiting also played a part.", pct(busy)));
    }
    if rexmit > 0 || rto > 0 {
        f(alloc::format!("{} packets were retransmitted ({} timeouts): packets were lost on the way out or their acks were lost on the way in.", rexmit, rto));
    }
    if ooo > 0 {
        f(alloc::format!("{} packets arrived out of order: packets before them were lost (or reordered) on the way in, and the sender had to resend them.", ooo));
    }
    if dupack >= 3 {
        f(alloc::format!("{} duplicate acks came in: packets this machine sent were lost on the way out.", dupack));
    }
    if afterwin > 0 {
        f(alloc::format!("{} packets arrived after the receive window: the sender overran what this machine advertised.", afterwin));
    }
    if losses_if > 0 {
        f(alloc::format!("The interface counted {} lost or bad frames (drops, overruns, errors).", losses_if));
    }
    if g.n > 0 && g.rpend_zero > 0 {
        f(alloc::format!("The driver had no read request pending in {} of {} samples: frames arriving then had nowhere to go.", g.rpend_zero, g.n));
    }
    if let Some(x) = x {
        if x.dir == Dir::Recv && g.conn_seen > 0 && x.rcvbuf > 0 && g.rq_max as f64 >= 0.9 * x.rcvbuf as f64 {
            let in_calls = clock.secs(x.calls.ticks) / x.secs.max(1e-9);
            if in_calls > 0.9 {
                f(alloc::format!("The socket's receive queue reached {} of {} bytes while this program spent {:.0}% of the run inside recv(): the data was here, and handing it over is slow. The time goes in the stack's receive path, not on the wire; while the queue is full the window is closed and the sender waits.", g.rq_max, x.rcvbuf, pct(in_calls.min(1.0))));
            } else {
                f(alloc::format!("The socket's receive queue reached {} of {} bytes: data waited for this program to read it.", g.rq_max, x.rcvbuf));
            }
        }
        if x.dir == Dir::Send && g.conn_seen > 0 && x.sndbuf > 0 && (g.sq_sum as f64 / g.conn_seen as f64) < 0.25 * x.sndbuf as f64 && busy < 0.9 {
            f(String::from("The send queue stayed mostly empty: this program did not keep the stack fed."));
        }
        if fin > 0 && x.dir == Dir::Recv && (bin as f64 / fin as f64) < 1000.0 {
            f(alloc::format!("Frames in averaged {:.0} bytes: small segments, so the per-frame costs dominate.", bin as f64 / fin as f64));
        }
        if x.dir == Dir::Recv && datain > 0 && acks as f64 > 0.9 * datain as f64 {
            f(alloc::format!("{} acks were sent for {} data packets: about one per packet, so delayed acks are not saving work.", acks, datain));
        }
    }
    // Driver counters that mean frames went missing or were refused.
    for (name, v) in dd.iter().skip(if a.drv.global.is_some() { 2 } else { 0 }) {
        let l = name.to_ascii_lowercase();
        let bad = l.contains("drop") || l.contains("bad") || l.contains("overrun") || l.contains("error") || l.contains("lost") || l.contains("recover");
        if *v > 0 && bad {
            f(alloc::format!("The driver counted {} under \"{}\" during the run; Roadshow's counters do not include what the driver drops itself.", v, name));
        }
    }
    if !any {
        say!("  - nothing stands out");
    }
}

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

fn usage() {
    say!("netprof info | cpu | watch [iface=NAME] [secs=N]   (any mode: lib=NAME for another stack)");
    say!("        recv HOST [port=N] [mb=N] [buf=N] [rcvbuf=N] [sndbuf=N] [nodelay] [iface=NAME] [quiet]");
    say!("        send HOST [port=N] [mb=N] [buf=N] [rcvbuf=N] [sndbuf=N] [nodelay] [iface=NAME] [quiet]");
    say!("        rtt HOST [port=N] [count=N] [size=N]");
    say!("        soakrecv | soaksend | soakrtt HOST [secs=N] [mb=N] [quiet]   (every byte checked)");
    say!("        disk FILE [mb=N] [buf=N] [keep]   (write then read FILE sequentially; deleted after unless keep)");
    say!("HOST runs tools/netprof-host (default port {}). Sizes take k and m.", DEFAULT_PORT);
}

/// `disk FILE [mb=N] [buf=N] [keep]`: how fast this machine's disk writes
/// and reads one large file in big sequential DOS calls, the rate a bulk
/// network transfer would end at (default 1024 MB in 1 MB calls).
fn run_disk(a: &Args, clock: &Clock) -> i32 {
    use amigaos4::dos::fs::{self, File, OpenMode};
    let Some(path) = a.words.get(1).copied().filter(|w| !w.contains('=')) else {
        usage();
        return 5;
    };
    let mb = a.num("mb=", 1024);
    let buf = (a.num("buf=", 1 << 20) as usize).clamp(512, 16 << 20);
    let total = mb << 20;
    let mut data = vec![0u8; buf];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i as u32).wrapping_mul(2654435761).to_be_bytes()[0];
    }
    let mut f = match File::open(path, OpenMode::Create) {
        Ok(f) => f,
        Err(e) => {
            say!("netprof disk: {}: {}", path, e);
            return 20;
        }
    };
    let t0 = timebase();
    let mut done = 0u64;
    while done < total {
        let n = (total - done).min(buf as u64) as usize;
        if let Err(e) = f.write_all(&data[..n]) {
            say!("netprof disk: write at {}: {}", done, e);
            drop(f);
            let _ = fs::delete(path);
            return 20;
        }
        done += n as u64;
    }
    drop(f);
    let w = clock.secs(timebase() - t0);
    say!("disk write: {} bytes in {:.2} s = {:.2} MB/s ({} bytes per Write)", total, w, total as f64 / w / 1e6, buf);
    let mut f = match File::open(path, OpenMode::Read) {
        Ok(f) => f,
        Err(e) => {
            say!("netprof disk: reopen {}: {}", path, e);
            return 20;
        }
    };
    let t0 = timebase();
    let mut got = 0u64;
    let mut wrong = 0u64;
    loop {
        match f.read(&mut data) {
            Ok(0) => break,
            Ok(n) => {
                // Spot-check one byte per call (the pattern repeats every buf bytes).
                let at = (got % buf as u64) as usize;
                if n > 0 && data[0] != (at as u32).wrapping_mul(2654435761).to_be_bytes()[0] {
                    wrong += 1;
                }
                got += n as u64;
            }
            Err(e) => {
                say!("netprof disk: read at {}: {}", got, e);
                break;
            }
        }
    }
    drop(f);
    let r = clock.secs(timebase() - t0);
    say!("disk read: {} bytes in {:.2} s = {:.2} MB/s, {} calls with a wrong first byte", got, r, got as f64 / r / 1e6, wrong);
    if !a.flag("keep") {
        let _ = fs::delete(path);
    }
    if got == total && wrong == 0 { 0 } else { 10 }
}

#[no_mangle]
pub extern "C" fn rust_main(args: *const u8, arglen: i32) -> i32 {
    // SAFETY: the Shell's argument string.
    let text = unsafe { core::slice::from_raw_parts(args, arglen.max(0) as usize) };
    let text = core::str::from_utf8(text).unwrap_or("");
    let a = Args { words: text.split_whitespace().collect() };
    let Some(mode) = a.words.first().copied() else {
        usage();
        return 5;
    };
    let clock = Clock { hz: timebase_hz() };
    let mut timer = match AmigaTimer::open(TimerUnit::MicroHz) {
        Ok(t) => t,
        Err(_) => {
            say!("netprof: timer.device did not open");
            return 20;
        }
    };
    if mode.eq_ignore_ascii_case("cpu") {
        return run_cpu(&mut timer, &clock);
    }
    if mode.eq_ignore_ascii_case("disk") {
        return run_disk(&a, &clock);
    }
    if mode.eq_ignore_ascii_case("burn") {
        // Keep the CPU busy at this task's priority for s=N seconds (a
        // competitor for a transfer run beside it).
        let end = timebase() + a.num("s=", 20) * clock.hz;
        let mut spins = 0u64;
        while timebase() < end {
            spins = core::hint::black_box(spins + 1);
        }
        say!("netprof burn: {} spins", spins);
        return 0;
    }
    // lib=NAME: another stack, opened by name, with bsdsocket.library's
    // interface.
    let opened = match a.text("lib=") {
        Some(n) => SocketLib::open_named(n, 4),
        None => SocketLib::open(4),
    };
    let lib = match opened {
        Ok(l) => l,
        Err(e) => {
            say!("netprof: {} (is Roadshow running?)", e);
            return 20;
        }
    };
    match mode.to_ascii_lowercase().as_str() {
        "info" => run_info(&lib),
        "watch" => run_watch(&lib, &a, &mut timer, &clock),
        "recv" => run_xfer(&lib, &a, Dir::Recv, &mut timer, &clock),
        "send" => run_xfer(&lib, &a, Dir::Send, &mut timer, &clock),
        "rtt" => run_rtt(&lib, &a, &clock),
        "soakrecv" | "soaksend" | "soakrtt" => soak::run_soak(&lib, &a, &mode.to_ascii_lowercase(), &clock),
        _ => {
            usage();
            5
        }
    }
}

mod soak;
