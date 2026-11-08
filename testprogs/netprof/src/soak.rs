//! netprof's soak modes: hours of traffic with every byte checked.
//!
//!   soakrecv HOST [secs=N] [mb=N] [buf=N] [iface=NAME]
//!   soaksend HOST [secs=N] [mb=N] [buf=N] [iface=NAME]
//!   soakrtt  HOST [secs=N]
//!
//! soakrecv and soaksend repeat transfers of mb (default 8) until secs
//! (default 60) have passed. Byte o of every stream is o % 251; soakrecv
//! checks each byte it receives, netprof-host checks each byte soaksend
//! sends. soakrtt connects, does 20 echoes of 64 to 1400 bytes, checks
//! them and closes, over and over (connection churn and small frames).
//! Each round prints a line with the machine's free memory; the end prints
//! totals, the driver's counters that changed, and PASS or FAIL: any wrong
//! byte, short transfer or round that could not connect (after 3 tries)
//! fails. Run them at the same time as separate jobs; Ctrl-C stops one.

use super::*;

/// Byte `o` of a stream.
fn pat(o: u64) -> u8 {
    (o % 251) as u8
}

fn avail() -> u32 {
    // SAFETY: a plain exec query (MEMF_ANY).
    unsafe { sys::exec_avail_mem(0) }
}

enum Kind {
    Recv,
    Send,
    Rtt,
}

struct Round {
    bytes: u64,
    secs: f64,
    errors: u64,
    connect_tries: u32,
    ok: bool,
    why: String,
}

fn connect(lib: &SocketLib, host: [u8; 4], port: u16) -> Result<(i32, u32), String> {
    let mut tries = 0;
    loop {
        tries += 1;
        let fd = lib.socket(AF_INET, SOCK_STREAM, 0).map_err(|e| alloc::format!("socket: {}", e))?;
        match lib.connect(fd, &SockAddrIn::v4(host, port)) {
            Ok(()) => return Ok((fd, tries)),
            Err(e) => {
                let _ = lib.close(fd);
                if tries >= 3 || break_pressed() {
                    return Err(alloc::format!("connect: {} ({} tries)", e, tries));
                }
            }
        }
    }
}

fn round_xfer(lib: &SocketLib, clock: &Clock, recv: bool, host: [u8; 4], port: u16, total: u64, buf: &mut [u8]) -> Round {
    let mut r = Round { bytes: 0, secs: 0.0, errors: 0, connect_tries: 0, ok: false, why: String::new() };
    let (fd, tries) = match connect(lib, host, port) {
        Ok(x) => x,
        Err(e) => {
            r.why = e;
            return r;
        }
    };
    r.connect_tries = tries;
    let req = alloc::format!("NETPROF 1 {} {}\n", if recv { "SEND" } else { "RECV" }, total);
    if lib.send(fd, req.as_bytes(), 0).is_err() {
        r.why = String::from("request not sent");
        let _ = lib.close(fd);
        return r;
    }
    let t0 = timebase();
    let mut off: u64 = 0;
    while off < total {
        let want = ((total - off) as usize).min(buf.len());
        if recv {
            match lib.recv(fd, &mut buf[..want], 0) {
                Ok(0) => break,
                Ok(n) => {
                    for (i, &b) in buf[..n].iter().enumerate() {
                        if b != pat(off + i as u64) {
                            r.errors += 1;
                        }
                    }
                    off += n as u64;
                }
                Err(e) => {
                    r.why = alloc::format!("recv after {} bytes: {}", off, e);
                    break;
                }
            }
        } else {
            for (i, b) in buf[..want].iter_mut().enumerate() {
                *b = pat(off + i as u64);
            }
            // send() may take part of it: keep the pattern aligned.
            let mut sent = 0;
            while sent < want {
                match lib.send(fd, &buf[sent..want], 0) {
                    Ok(n) => sent += n,
                    Err(e) => {
                        r.why = alloc::format!("send after {} bytes: {}", off + sent as u64, e);
                        break;
                    }
                }
            }
            off += sent as u64;
            if sent < want {
                break;
            }
        }
    }
    r.bytes = off;
    if !recv && off == total {
        // netprof-host checks every byte: "DONE <bytes> <us> <wrong>".
        match read_line(lib, fd) {
            Some(l) => {
                let w: Vec<&str> = l.split_whitespace().collect();
                let got: u64 = w.get(1).and_then(|v| v.parse().ok()).unwrap_or(0);
                r.errors += w.get(3).and_then(|v| v.parse().ok()).unwrap_or(0);
                if got != total {
                    r.why = alloc::format!("netprof-host received {} of {}", got, total);
                }
                if w.get(3).is_none() {
                    r.why = String::from("netprof-host does not check bytes (update it)");
                }
            }
            None => r.why = String::from("no DONE line from netprof-host"),
        }
    }
    r.secs = clock.secs(timebase() - t0);
    let _ = lib.close(fd);
    r.ok = r.why.is_empty() && off == total && r.errors == 0;
    if r.why.is_empty() && off != total {
        r.why = alloc::format!("short: {} of {} bytes", off, total);
    }
    r
}

fn round_rtt(lib: &SocketLib, clock: &Clock, host: [u8; 4], port: u16, seq: u32) -> Round {
    let mut r = Round { bytes: 0, secs: 0.0, errors: 0, connect_tries: 0, ok: false, why: String::new() };
    // Sizes from 64 to 1400 bytes, a different one each round.
    let size = 64 + (seq as usize * 97) % 1337;
    let (fd, tries) = match connect(lib, host, port) {
        Ok(x) => x,
        Err(e) => {
            r.why = e;
            return r;
        }
    };
    r.connect_tries = tries;
    let _ = lib.set_opt(fd, IPPROTO_TCP, TCP_NODELAY, 1);
    let req = alloc::format!("NETPROF 1 ECHO {}\n", size);
    let _ = lib.send(fd, req.as_bytes(), 0);
    let mut msg = vec![0u8; size];
    let mut back = vec![0u8; size];
    let t0 = timebase();
    for k in 0..20u64 {
        for (i, b) in msg.iter_mut().enumerate() {
            *b = pat(k * 7919 + i as u64 + seq as u64);
        }
        if lib.send(fd, &msg, 0).is_err() {
            r.why = String::from("echo send failed");
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
            r.why = alloc::format!("echo {} came back short: {} of {}", k, got, size);
            break;
        }
        r.errors += msg.iter().zip(back.iter()).filter(|(a, b)| a != b).count() as u64;
        r.bytes += 2 * size as u64;
    }
    r.secs = clock.secs(timebase() - t0);
    let _ = lib.close(fd);
    r.ok = r.why.is_empty() && r.errors == 0;
    r
}

pub(crate) fn run_soak(lib: &SocketLib, a: &Args, mode: &str, clock: &Clock) -> i32 {
    let kind = match mode {
        "soakrecv" => Kind::Recv,
        "soaksend" => Kind::Send,
        _ => Kind::Rtt,
    };
    let Some(host) = a.words.get(1).and_then(|w| parse_ip(w)) else {
        say!("netprof: give the host's IPv4 address, e.g. netprof {} 10.0.3.2", mode);
        return 20;
    };
    let port = a.num("port=", DEFAULT_PORT as u64) as u16;
    let secs = a.num("secs=", 60);
    let total = a.num("mb=", 8) * 1024 * 1024;
    let buf_len = a.num("buf=", 32 * 1024).clamp(1, 1 << 20) as usize;
    let iface = pick_iface(lib, a, Some(host));
    let probe = iface.as_ref().map(|i| Probe::new(lib, i, !a.flag("nodriver")));
    let first = probe.as_ref().map(|p| p.snap());
    let mem0 = avail();
    say!("netprof {}: {}.{}.{}.{}:{} for {} s on {}; free memory {}", mode, host[0], host[1], host[2], host[3], port, secs,
        iface.as_deref().unwrap_or("?"), mem0);
    let mut buf = vec![0u8; buf_len];
    let t_start = timebase();
    let (mut rounds, mut failed, mut bytes, mut errors, mut retried) = (0u32, 0u32, 0u64, 0u64, 0u32);
    let (mut lo, mut hi) = (f64::MAX, 0.0f64);
    let mut mem_min = mem0;
    let mut stopped = false;
    let mut next_line = 30.0f64;
    while clock.secs(timebase() - t_start) < secs as f64 {
        rounds += 1;
        let r = match kind {
            Kind::Recv => round_xfer(lib, clock, true, host, port, total, &mut buf),
            Kind::Send => round_xfer(lib, clock, false, host, port, total, &mut buf),
            Kind::Rtt => round_rtt(lib, clock, host, port, rounds),
        };
        let m = avail();
        mem_min = mem_min.min(m);
        bytes += r.bytes;
        errors += r.errors;
        if r.connect_tries > 1 {
            retried += 1;
        }
        let rate = mbps(r.bytes, r.secs);
        if r.ok {
            lo = lo.min(rate);
            hi = hi.max(rate);
        } else {
            failed += 1;
        }
        let at = clock.secs(timebase() - t_start);
        // quiet: failed rounds, and one line per 30 s.
        let due = at >= next_line;
        if due {
            next_line = at + 30.0;
        }
        if !r.ok || !a.flag("quiet") || due {
            say!("{:6.0} s round {:5}: {} bytes {:.2} s {:.2} MB/s, wrong bytes {}{}{}, free {}",
                at, rounds, r.bytes, r.secs, rate, r.errors,
                if r.connect_tries > 1 { alloc::format!(", {} connect tries", r.connect_tries) } else { String::new() },
                if r.why.is_empty() { String::new() } else { alloc::format!(", FAILED: {}", r.why) }, m);
        }
        if break_pressed() {
            stopped = true;
            break;
        }
    }
    let mem1 = avail();
    say!("");
    say!("== netprof {} summary ==", mode);
    say!("rounds {}, failed {}, connect retried in {}; {} bytes, {} wrong", rounds, failed, retried, bytes, errors);
    if hi > 0.0 {
        say!("per round {:.2} to {:.2} MB/s", lo, hi);
    }
    say!("free memory: start {}, lowest {}, end {} ({} bytes less)", mem0, mem_min, mem1, mem0 as i64 - mem1 as i64);
    if let (Some(p), Some(f)) = (probe.as_ref(), first.as_ref()) {
        let l = p.snap();
        for (name, v) in driver_deltas(&f.drv, &l.drv) {
            if v != 0 {
                say!("driver: {}: {}", name, v);
            }
        }
        let d = |i: usize| l.ifq[i].wrapping_sub(f.ifq[i]);
        say!("interface: frames in {}, out {}; input drops {}, output drops {}, overruns {}", d(Q_RX), d(Q_TX), d(Q_INDROP), d(Q_OUTDROP), d(Q_OVERRUN));
        say!("tcp: retransmitted {}, out of order {}, timeouts {}", dtcp(f, &l, "tcps_sndrexmitpack"), dtcp(f, &l, "tcps_rcvoopack"), dtcp(f, &l, "tcps_rexmttimeo"));
    }
    let pass = failed == 0 && errors == 0 && rounds > 0 && !stopped;
    say!("{} {}: {} rounds, {} failed, {} wrong bytes{}", if pass { "PASS" } else { "FAIL" }, mode, rounds, failed, errors, if stopped { ", stopped early" } else { "" });
    if pass { 0 } else { 20 }
}
