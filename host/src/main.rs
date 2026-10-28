//! flipad host: turns a handheld on the LAN into a local Xbox 360 controller.

mod audio;
mod btaudio;
mod keys;
mod mouse;
mod pad;
mod plugins;
mod stats;

use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
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
    jack: (bool, bool),
    /// Recent action ids; the client sends each action a few times.
    actions: Vec<u32>,
    pointer_seq: Option<u32>,
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

    let quit = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT, signal_hook::consts::SIGHUP] {
        signal_hook::flag::register(sig, quit.clone())?;
    }
    let audio = audio::Audio::start();
    let (keys_tx, keys_rx) = std::sync::mpsc::channel::<keys::KeyJob>();
    std::thread::spawn(move || {
        let mut keys = keys::Keys::new();
        while let Ok(job) = keys_rx.recv() {
            keys.run(job);
        }
    });
    std::thread::Builder::new().name("btaudio".into()).spawn(btaudio::run_forever)?;
    let (plugin_tx, plugin_rx) = std::sync::mpsc::channel::<(String, u16)>();
    {
        let (sock, peer, keys) = (sock.try_clone()?, peer.clone(), keys_tx.clone());
        std::thread::Builder::new().name("plugins".into()).spawn(move || plugins::run(sock, peer, keys, plugin_rx))?;
    }

    let mut mouse = mouse::Mouse::new().map_err(|e| eprintln!("virtual mouse: {e}")).ok();
    let mut session: Option<Session> = None;
    let mut buf = [0u8; 512];
    while !quit.load(Ordering::Relaxed) {
        let mut fds = vec![libc::pollfd { fd: sock.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
        if let Some(s) = &session {
            fds.push(libc::pollfd { fd: s.pad.fd(), events: libc::POLLIN, revents: 0 });
        }
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 50) };
        let now = Instant::now();

        if fds[0].revents & libc::POLLIN != 0 {
            while let Ok((n, from)) = sock.recv_from(&mut buf) {
                let Some(msg) = Msg::decode(&buf[..n]) else { continue };
                let had_session = session.is_some();
                handle(&sock, &mut session, &peer, &audio, &keys_tx, &plugin_tx, mouse.as_mut(), &token, &name, msg, from, now);
                if had_session && session.is_none() && let Some(m) = &mut mouse {
                    m.release();
                }
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
                audio.set(None);
                if let Some(m) = &mut mouse {
                    m.release();
                }
            } else if silent > NEUTRAL_AFTER && s.pad.state() != Pad::default() {
                let _ = s.pad.apply(Pad::default());
            }
        }
    }
    eprintln!("stopping");
    audio.shutdown();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle(
    sock: &UdpSocket,
    session: &mut Option<Session>,
    peer: &Mutex<Option<SocketAddr>>,
    audio: &audio::Audio,
    keys: &std::sync::mpsc::Sender<keys::KeyJob>,
    plugins: &std::sync::mpsc::Sender<(String, u16)>,
    mouse: Option<&mut mouse::Mouse>,
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
            *session = Some(Session { addr: from, id, seq: None, last_seen: now, name: client, pad, jack: (false, false), actions: Vec::new(), pointer_seq: None });
            audio.set(None);
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
                audio.set(None);
            }
        }
        Msg::Pointer { session: id, seq, dx, dy, wheel, hwheel, buttons } => {
            if let Some(s) = session.as_mut().filter(|s| ours(s, id)) {
                s.last_seen = now;
                if s.pointer_seq.is_none_or(|last| seq_newer(seq, last)) {
                    s.pointer_seq = Some(seq);
                    if let Some(m) = mouse {
                        m.apply(dx, dy, wheel, hwheel, buttons);
                    }
                }
            }
        }
        Msg::PluginButton { session: id, plugin, id: button } => {
            if let Some(s) = session.as_mut().filter(|s| ours(s, id)) {
                s.last_seen = now;
                let _ = plugins.send((plugin, button));
            }
        }
        Msg::Action { session: id, id: action_id, action } => {
            if let Some(s) = session.as_mut().filter(|s| ours(s, id)) {
                s.last_seen = now;
                if !s.actions.contains(&action_id) {
                    if s.actions.len() >= 16 {
                        s.actions.remove(0);
                    }
                    s.actions.push(action_id);
                    eprintln!("{}: {action:?}", s.name);
                    let _ = keys.send(keys::KeyJob::Action(action));
                }
            }
        }
        Msg::Jack { session: id, headphones, mic } => {
            if let Some(s) = session.as_mut().filter(|s| ours(s, id)) {
                s.last_seen = now;
                if s.jack != (headphones, mic) {
                    s.jack = (headphones, mic);
                    eprintln!("{}: headphones={headphones} mic={mic}", s.name);
                    audio.set(Some(audio::Want { ip: from.ip(), headphones, mic }));
                }
            }
        }
        _ => {}
    }
}
