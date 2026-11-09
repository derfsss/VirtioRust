//! netprof's serve mode: netprof-host's side of the protocol on an AmigaOS
//! machine, so two Amigas can measure the network between them.
//!
//!   serve [port=N] [count=N]
//!
//! Listens on port N (default 4500) on every interface and answers each
//! connection as netprof-host does (one at a time):
//!   NETPROF 1 SEND <bytes>   send that many bytes (byte o of the stream is
//!                            o % 251), then close
//!   NETPROF 1 RECV <bytes>   read them, check the pattern, answer
//!                            "DONE <bytes> <microseconds> <bytes wrong>"
//!   NETPROF 1 ECHO <size>    echo blocks of <size> bytes until the peer closes
//! and prints a line per session. count=N stops after N sessions (default:
//! until Ctrl-C). On the other machine, `netprof recv|send|rtt <this
//! machine's address>` measures as usual.

use super::*;

/// 64 KB of the stream pattern (byte o is o % 251), plus 251 so any
/// offset's next 64 KB is one slice.
fn pattern_block() -> Vec<u8> {
    (0..(1usize << 16) + 251).map(|i| (i % 251) as u8).collect()
}

pub fn run_serve(lib: &SocketLib, a: &Args, clock: &Clock) -> i32 {
    let port = a.num("port=", DEFAULT_PORT as u64) as u16;
    let count = a.num("count=", 0);
    let fd = match lib.socket(AF_INET, SOCK_STREAM, 0) {
        Ok(f) => f,
        Err(e) => {
            say!("netprof: socket failed: {}", e);
            return 20;
        }
    };
    let _ = lib.set_opt(fd, SOL_SOCKET, SO_REUSEADDR, 1);
    if let Err(e) = lib.bind(fd, &SockAddrIn::v4([0, 0, 0, 0], port)) {
        say!("netprof: bind to port {} failed: {}", port, e);
        let _ = lib.close(fd);
        return 20;
    }
    if let Err(e) = lib.listen(fd, 4) {
        say!("netprof: listen failed: {}", e);
        let _ = lib.close(fd);
        return 20;
    }
    say!("netprof serve: listening on port {} (Ctrl-C stops)", port);
    let pat = pattern_block();
    let mut buf = vec![0u8; 1 << 16];
    let mut sessions = 0u64;
    let mut rc = 0;
    loop {
        if count != 0 && sessions >= count {
            break;
        }
        let (c, peer) = match lib.accept(fd) {
            Ok(x) => x,
            Err(e) => {
                if e != amigaos4::error::AmigaError::IoError(EINTR) {
                    say!("netprof: accept failed: {}", e);
                    rc = 10;
                }
                break;
            }
        };
        sessions += 1;
        let ip = peer.sin_addr.to_be_bytes();
        let who = alloc::format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], peer.sin_port);
        match session(lib, c, &pat, &mut buf, clock) {
            Ok(line) => say!("{}: {}", who, line),
            Err(e) => say!("{}: {}", who, e),
        }
        let _ = lib.close(c);
        if break_pressed() {
            break;
        }
    }
    let _ = lib.close(fd);
    say!("netprof serve: {} session(s)", sessions);
    rc
}

fn session(lib: &SocketLib, c: i32, pat: &[u8], buf: &mut [u8], clock: &Clock) -> Result<String, String> {
    let line = read_line(lib, c).ok_or("no request line")?;
    let w: Vec<&str> = line.split_whitespace().collect();
    if w.len() != 4 || w[0] != "NETPROF" || w[1] != "1" {
        return Err(alloc::format!("not a netprof request: {:?}", line.trim()));
    }
    let n: u64 = w[3].parse().map_err(|_| String::from("bad number"))?;
    match w[2] {
        "SEND" => {
            let t0 = timebase();
            let mut off: u64 = 0;
            while off < n {
                let k = (n - off).min(1 << 16) as usize;
                let start = (off % 251) as usize;
                let sent = lib.send(c, &pat[start..start + k], 0).map_err(|e| alloc::format!("send failed after {} bytes: {}", off, e))?;
                off += sent as u64;
            }
            let _ = lib.shutdown(c, 1);
            // Wait for the peer to close, so the time covers delivery.
            while matches!(lib.recv(c, &mut buf[..256], 0), Ok(k) if k > 0) {}
            let s = clock.secs(timebase() - t0);
            Ok(alloc::format!("sent {} bytes in {:.2} s = {:.2} MB/s", n, s, n as f64 / s.max(1e-9) / 1e6))
        }
        "RECV" => {
            let mut got: u64 = 0;
            let mut errors: u64 = 0;
            let mut t0: Option<u64> = None;
            while got < n {
                let want = ((n - got) as usize).min(buf.len());
                let k = match lib.recv(c, &mut buf[..want], 0) {
                    Ok(0) => break,
                    Ok(k) => k,
                    Err(e) => return Err(alloc::format!("recv failed after {} bytes: {}", got, e)),
                };
                t0.get_or_insert_with(timebase);
                for (i, &b) in buf[..k].iter().enumerate() {
                    if b != ((got + i as u64) % 251) as u8 {
                        errors += 1;
                    }
                }
                got += k as u64;
            }
            let us = t0.map(|t| (clock.secs(timebase() - t) * 1e6) as u64).unwrap_or(0);
            let done = alloc::format!("DONE {} {} {}\n", got, us, errors);
            let _ = lib.send(c, done.as_bytes(), 0);
            let s = us as f64 / 1e6;
            Ok(alloc::format!("received {} bytes in {:.2} s = {:.2} MB/s, {} bytes wrong", got, s, got as f64 / s.max(1e-9) / 1e6, errors))
        }
        "ECHO" => {
            let _ = lib.set_opt(c, IPPROTO_TCP, TCP_NODELAY, 1);
            let size = n.clamp(1, 1 << 16) as usize;
            let mut blocks = 0u64;
            'outer: loop {
                let mut have = 0;
                while have < size {
                    match lib.recv(c, &mut buf[have..size], 0) {
                        Ok(k) if k > 0 => have += k,
                        _ => break 'outer,
                    }
                }
                let mut off = 0;
                while off < size {
                    match lib.send(c, &buf[off..size], 0) {
                        Ok(k) if k > 0 => off += k,
                        _ => break 'outer,
                    }
                }
                blocks += 1;
            }
            Ok(alloc::format!("echoed {} blocks of {} bytes", blocks, size))
        }
        other => Err(alloc::format!("unknown request {}", other)),
    }
}
