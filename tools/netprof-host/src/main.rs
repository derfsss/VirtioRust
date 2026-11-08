//! netprof-host: the peer for testprogs/netprof on an AmigaOS machine.
//!
//!   netprof-host [PORT] [ADDRESS]   (default 4500, all addresses; 127.0.0.1
//!                                   is enough for QEMU guests: slirp hands
//!                                   their connections to the PC's loopback)
//!
//! Each connection starts with one line from netprof:
//!   NETPROF 1 SEND <bytes>   send that many bytes (byte o of the stream is o % 251), then close
//!   NETPROF 1 RECV <bytes>   read that many bytes, check the same pattern, answer
//!                            "DONE <bytes> <microseconds> <bytes wrong>"
//!   NETPROF 1 ECHO <size>    echo blocks of <size> bytes until the peer closes
//! and every session prints one line with what this side saw. The host
//! side should never be the limit: it sends from memory and discards what
//! it reads.
//!
//! On QEMU's user network the guest reaches the host at the network's .2
//! address (10.0.3.2 for the virtio-net test machine); on a LAN, give
//! netprof this machine's address.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Instant;

fn main() {
    let port: u16 = std::env::args().nth(1).and_then(|p| p.parse().ok()).unwrap_or(4500);
    let addr = std::env::args().nth(2).unwrap_or_else(|| String::from("0.0.0.0"));
    let listener = match TcpListener::bind((addr.as_str(), port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("netprof-host: cannot listen on port {}: {}", port, e);
            std::process::exit(1);
        }
    };
    println!("netprof-host: listening on {} port {}", addr, port);
    // Connection times are logged from here, so overlapping transfers can be told apart.
    let up = Instant::now();
    for conn in listener.incoming() {
        match conn {
            Ok(s) => {
                let at = up.elapsed().as_secs_f64();
                thread::spawn(move || {
                    let peer = s.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                    println!("{}: connected at {:.2} s", peer, at);
                    if let Err(e) = session(s, &peer) {
                        println!("{}: {}", peer, e);
                    }
                });
            }
            Err(e) => eprintln!("netprof-host: accept: {}", e),
        }
    }
}

fn session(s: TcpStream, peer: &str) -> std::io::Result<()> {
    let mut reader = BufReader::with_capacity(1 << 16, s.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.starts_with("GET ") {
        return http(s, reader, &line, peer);
    }
    let w: Vec<&str> = line.split_whitespace().collect();
    if w.len() != 4 || w[0] != "NETPROF" || w[1] != "1" {
        return Err(std::io::Error::other(format!("not a netprof request: {:?}", line.trim())));
    }
    let n: u64 = w[3].parse().map_err(|_| std::io::Error::other("bad number"))?;
    let mut s = s;
    match w[2] {
        "SEND" => {
            // The stream's byte at offset o is o % 251 (PATTERN), so the
            // receiver can check every byte.
            let pat = pattern_block();
            let t0 = Instant::now();
            let mut off: u64 = 0;
            while off < n {
                let k = (n - off).min(1 << 16) as usize;
                let start = (off % 251) as usize;
                s.write_all(&pat[start..start + k])?;
                off += k as u64;
            }
            s.flush()?;
            s.shutdown(std::net::Shutdown::Write)?;
            // Wait for the peer to close, so the time covers delivery.
            let mut sink = [0u8; 256];
            while reader.read(&mut sink).unwrap_or(0) > 0 {}
            let dt = t0.elapsed().as_secs_f64();
            println!("{}: sent {} bytes in {:.2} s = {:.2} MB/s", peer, n, dt, n as f64 / dt / 1e6);
        }
        "RECV" => {
            let mut buf = vec![0u8; 1 << 16];
            let mut got: u64 = 0;
            let mut t0: Option<Instant> = None;
            let mut errors: u64 = 0;
            while got < n {
                let k = reader.read(&mut buf)?;
                if k == 0 {
                    break;
                }
                t0.get_or_insert_with(Instant::now);
                for (i, &b) in buf[..k].iter().enumerate() {
                    if b != ((got + i as u64) % 251) as u8 {
                        errors += 1;
                    }
                }
                got += k as u64;
            }
            let us = t0.map(|t| t.elapsed().as_micros()).unwrap_or(0);
            // "DONE <bytes> <microseconds> <bytes that broke the pattern>"
            s.write_all(format!("DONE {} {} {}\n", got, us, errors).as_bytes())?;
            let dt = us as f64 / 1e6;
            println!("{}: received {} bytes in {:.2} s = {:.2} MB/s, {} bytes wrong", peer, got, dt, got as f64 / dt.max(1e-9) / 1e6, errors);
        }
        "ECHO" => {
            // Small blocks must go at once; bulk transfers keep Nagle on.
            s.set_nodelay(true)?;
            let size = n.clamp(1, 1 << 16) as usize;
            let mut buf = vec![0u8; size];
            let mut count: u64 = 0;
            loop {
                if reader.read_exact(&mut buf).is_err() {
                    break;
                }
                s.write_all(&buf)?;
                count += 1;
            }
            println!("{}: echoed {} blocks of {} bytes", peer, count, size);
        }
        other => return Err(std::io::Error::other(format!("unknown request {}", other))),
    }
    Ok(())
}

/// 64 KB of the stream pattern (byte o is o % 251), plus 251 so any
/// offset's next 64 KB is one slice.
fn pattern_block() -> Vec<u8> {
    (0..(1usize << 16) + 251).map(|i| (i % 251) as u8).collect()
}

/// `GET /bytes/<n>`: an HTTP/1.0 answer of n bytes, so an ordinary client
/// on the Amiga (wget, curl, a browser) can be measured against the same
/// server as netprof.
fn http(mut s: TcpStream, mut reader: BufReader<TcpStream>, first: &str, peer: &str) -> std::io::Result<()> {
    // Skip the request headers.
    let mut l = String::new();
    loop {
        l.clear();
        if reader.read_line(&mut l)? == 0 || l == "\r\n" || l == "\n" {
            break;
        }
    }
    let path = first.split_whitespace().nth(1).unwrap_or("");
    let Some(n) = path.strip_prefix("/bytes/").and_then(|v| v.parse::<u64>().ok()) else {
        s.write_all(b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n")?;
        return Ok(());
    };
    s.write_all(format!("HTTP/1.0 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n", n).as_bytes())?;
    let block = vec![0x5au8; 1 << 16];
    let t0 = Instant::now();
    let mut left = n;
    while left > 0 {
        let k = left.min(block.len() as u64) as usize;
        s.write_all(&block[..k])?;
        left -= k as u64;
    }
    s.flush()?;
    s.shutdown(std::net::Shutdown::Write)?;
    let mut sink = [0u8; 256];
    while reader.read(&mut sink).unwrap_or(0) > 0 {}
    let dt = t0.elapsed().as_secs_f64();
    println!("{}: HTTP {} bytes in {:.2} s = {:.2} MB/s", peer, n, dt, n as f64 / dt / 1e6);
    Ok(())
}
