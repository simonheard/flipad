//! flipad host: turns a handheld on the LAN into a local Xbox 360 controller.

mod pad;
mod stats;

use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flipad_proto::{Msg, PORT, Pad, TOKEN_LEN, parse_token, seq_newer};
use pad::VirtualPad;

/// No packets for this long: release everything so nothing stays held.
const NEUTRAL_AFTER: Duration = Duration::from_millis(250);
/// No packets for this long: the handheld is gone, unplug the controller.
const DROP_AFTER: Duration = Duration::from_secs(10);

struct Session {
    addr: SocketAddr,
    id: u32,
    seq: Option<u32>,
    last_seen: Instant,
    name: String,
    pad: VirtualPad,
}

fn config_dir() -> std::path::PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(Into::into)
        .unwrap_or_else(|_| std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"));
    base.join("flipad")
}

/// Loads the shared token, creating one on first run.
fn load_token() -> std::io::Result<[u8; TOKEN_LEN]> {
    let dir = config_dir();
    let path = dir.join("token");
    if let Some(t) = std::fs::read_to_string(&path).ok().and_then(|s| parse_token(&s)) {
        return Ok(t);
    }
    let mut t = [0u8; TOKEN_LEN];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut t)?;
    std::fs::create_dir_all(&dir)?;
    let hex: String = t.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&path, format!("{hex}\n"))?;
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    eprintln!("created token at {}", path.display());
    Ok(t)
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "host".into())
}

fn random_u32() -> u32 {
    let mut b = [0u8; 4];
    let _ = std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom").unwrap(), &mut b);
    u32::from_le_bytes(b)
}

fn send(sock: &UdpSocket, to: SocketAddr, msg: &Msg) {
    let _ = sock.send_to(&msg.encode(), to);
}

fn main() -> std::io::Result<()> {
    let token = load_token()?;
    let name = hostname();
    let sock = UdpSocket::bind(("0.0.0.0", PORT))?;
    sock.set_nonblocking(true)?;
    // mark replies (rumble) as voice traffic so Wi-Fi queues them first
    unsafe {
        let tos: libc::c_int = 0xb8;
        libc::setsockopt(sock.as_raw_fd(), libc::IPPROTO_IP, libc::IP_TOS, (&tos as *const libc::c_int).cast(), 4);
    }
    eprintln!("flipad host {name:?} listening on udp/{PORT}");

    let peer: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
    {
        let (sock, peer) = (sock.try_clone()?, peer.clone());
        std::thread::spawn(move || {
            let mut stats = stats::Stats::new();
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let status = stats.sample();
                if let Some(to) = *peer.lock().unwrap() {
                    send(&sock, to, &Msg::Status(status));
                }
            }
        });
    }

    let mut session: Option<Session> = None;
    let mut buf = [0u8; 512];
    loop {
        let mut fds = vec![libc::pollfd { fd: sock.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
        if let Some(s) = &session {
            fds.push(libc::pollfd { fd: s.pad.fd(), events: libc::POLLIN, revents: 0 });
        }
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 50) };
        let now = Instant::now();

        if fds[0].revents & libc::POLLIN != 0 {
            while let Ok((n, from)) = sock.recv_from(&mut buf) {
                let Some(msg) = Msg::decode(&buf[..n]) else { continue };
                handle(&sock, &mut session, &peer, &token, &name, msg, from, now);
            }
        }

        if let Some(s) = &mut session {
            if fds.get(1).is_some_and(|f| f.revents & libc::POLLIN != 0) {
                for r in s.pad.poll_ff() {
                    send(&sock, s.addr, &Msg::Rumble { strong: r.strong, weak: r.weak, ms: r.ms });
                }
            }
            let silent = now.duration_since(s.last_seen);
            if silent > DROP_AFTER {
                eprintln!("{} timed out", s.name);
                session = None;
                *peer.lock().unwrap() = None;
            } else if silent > NEUTRAL_AFTER && s.pad.state() != Pad::default() {
                let _ = s.pad.apply(Pad::default());
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle(
    sock: &UdpSocket,
    session: &mut Option<Session>,
    peer: &Mutex<Option<SocketAddr>>,
    token: &[u8; TOKEN_LEN],
    name: &str,
    msg: Msg,
    from: SocketAddr,
    now: Instant,
) {
    let ours = |s: &Session, id: u32| s.addr == from && s.id == id;
    match msg {
        Msg::Discover => send(sock, from, &Msg::Offer { name: name.into() }),
        Msg::Hello { token: t, name: client } => {
            if t != *token {
                send(sock, from, &Msg::Reject { reason: "token mismatch".into() });
                return;
            }
            if let Some(s) = session.as_ref()
                && s.addr != from
                && now.duration_since(s.last_seen) < Duration::from_secs(2)
            {
                send(sock, from, &Msg::Reject { reason: format!("{} is connected", s.name) });
                return;
            }
            // reuse the controller across reconnects so games keep their binding
            let pad = match session.take() {
                Some(s) => s.pad,
                None => match VirtualPad::new() {
                    Ok(p) => p,
                    Err(e) => {
                        send(sock, from, &Msg::Reject { reason: format!("uinput: {e}") });
                        return;
                    }
                },
            };
            let id = random_u32();
            eprintln!("{client} connected from {from}");
            *session = Some(Session { addr: from, id, seq: None, last_seen: now, name: client, pad });
            *peer.lock().unwrap() = Some(from);
            send(sock, from, &Msg::Welcome { session: id, name: name.into() });
        }
        Msg::State { session: id, seq, pad } => {
            if let Some(s) = session.as_mut().filter(|s| ours(s, id)) {
                s.last_seen = now;
                if s.seq.is_none_or(|last| seq_newer(seq, last)) {
                    s.seq = Some(seq);
                    let _ = s.pad.apply(pad);
                }
            }
        }
        Msg::Ping { session: id, id: ping, micros } => {
            if let Some(s) = session.as_mut().filter(|s| ours(s, id)) {
                s.last_seen = now;
                send(sock, from, &Msg::Pong { id: ping, micros });
            }
        }
        Msg::Bye { session: id } => {
            if session.as_ref().is_some_and(|s| ours(s, id)) {
                eprintln!("{} disconnected", session.as_ref().unwrap().name);
                *session = None;
                *peer.lock().unwrap() = None;
            }
        }
        _ => {}
    }
}
