//! The forwarding loop: reads the borrowed controller, streams its state to
//! the host, plays rumble the host sends back, and tracks link quality.

use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use evdev::raw_stream::RawDevice;
use evdev::{
    AbsoluteAxisCode as Abs, EventType, FFEffect, FFEffectData, FFEffectKind, FFReplay, FFTrigger, InputEvent,
    KeyCode, SynchronizationCode,
};
use flipad_proto::{Msg, PORT, Pad, Status, TOKEN_LEN, button};

use crate::audio::{Audio, Jack, Want};

/// Re-send the current state this often even when nothing changes, so a lost
/// packet is repaired quickly and the host knows we are alive.
const KEEPALIVE: Duration = Duration::from_millis(20);
const PING_EVERY: Duration = Duration::from_millis(250);
const LINK_LOST: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_millis(700);
/// Hold Home + Select this long to quit.
pub const EXIT_HOLD: Duration = Duration::from_millis(1500);
const EXIT_COMBO: u32 = button::MODE | button::SELECT;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Phase {
    #[default]
    Searching,
    Connecting,
    Connected,
    Rejected(String),
    Lost,
}

/// Everything the UI shows.
#[derive(Clone, Debug, Default)]
pub struct View {
    pub phase: Phase,
    pub host: String,
    pub rtt_ms: Option<f32>,
    pub status: Status,
    pub pad: Pad,
    /// 0..1 progress of the exit combo.
    pub exit_hold: f32,
    pub rumble: bool,
    /// Headset state, and whether audio is being routed (needs a connection).
    pub jack: Jack,
    pub audio_live: bool,
}

pub struct Shared {
    pub view: Mutex<View>,
    pub quit: Arc<AtomicBool>,
    pub jack: Arc<Mutex<Jack>>,
    /// Called whenever `view` changed.
    pub notify: Box<dyn Fn() + Send + Sync>,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut View)) {
        f(&mut self.view.lock().unwrap());
        (self.notify)();
    }
}

pub struct Config {
    pub token: [u8; TOKEN_LEN],
    pub host: Option<SocketAddr>,
    pub name: String,
}

fn key_bit(code: u16) -> Option<u32> {
    Some(match KeyCode(code) {
        KeyCode::BTN_SOUTH => button::SOUTH,
        KeyCode::BTN_EAST => button::EAST,
        KeyCode::BTN_NORTH => button::NORTH,
        KeyCode::BTN_WEST => button::WEST,
        KeyCode::BTN_TL => button::TL,
        KeyCode::BTN_TR => button::TR,
        KeyCode::BTN_SELECT => button::SELECT,
        KeyCode::BTN_START => button::START,
        KeyCode::BTN_MODE => button::MODE,
        KeyCode::BTN_THUMBL => button::THUMBL,
        KeyCode::BTN_THUMBR => button::THUMBR,
        // xpad reports the d-pad as these when it is not a hat
        KeyCode::BTN_TRIGGER_HAPPY1 => button::DPAD_LEFT,
        KeyCode::BTN_TRIGGER_HAPPY2 => button::DPAD_RIGHT,
        KeyCode::BTN_TRIGGER_HAPPY3 => button::DPAD_UP,
        KeyCode::BTN_TRIGGER_HAPPY4 => button::DPAD_DOWN,
        _ => return None,
    })
}

fn set_bit(buttons: &mut u32, bit: u32, on: bool) {
    if on { *buttons |= bit } else { *buttons &= !bit }
}

/// Applies one evdev event to the pad state.
pub fn apply_event(pad: &mut Pad, ev: &InputEvent) {
    let (code, v) = (ev.code(), ev.value());
    match ev.event_type() {
        EventType::KEY => {
            if let Some(bit) = key_bit(code) {
                set_bit(&mut pad.buttons, bit, v != 0);
            }
        }
        EventType::ABSOLUTE => match Abs(code) {
            Abs::ABS_X => pad.lx = v.clamp(-32768, 32767) as i16,
            Abs::ABS_Y => pad.ly = v.clamp(-32768, 32767) as i16,
            Abs::ABS_RX => pad.rx = v.clamp(-32768, 32767) as i16,
            Abs::ABS_RY => pad.ry = v.clamp(-32768, 32767) as i16,
            Abs::ABS_Z => pad.lt = v.clamp(0, 255) as u8,
            Abs::ABS_RZ => pad.rt = v.clamp(0, 255) as u8,
            Abs::ABS_HAT0X => {
                set_bit(&mut pad.buttons, button::DPAD_LEFT, v < 0);
                set_bit(&mut pad.buttons, button::DPAD_RIGHT, v > 0);
            }
            Abs::ABS_HAT0Y => {
                set_bit(&mut pad.buttons, button::DPAD_UP, v < 0);
                set_bit(&mut pad.buttons, button::DPAD_DOWN, v > 0);
            }
            _ => {}
        },
        _ => {}
    }
}

struct Motor {
    effect: Option<FFEffect>,
}

impl Motor {
    fn play(&mut self, dev: &mut RawDevice, strong: u16, weak: u16, ms: u16) {
        if strong == 0 && weak == 0 {
            if let Some(e) = &mut self.effect {
                let _ = e.stop();
            }
            return;
        }
        let data = FFEffectData {
            direction: 0,
            trigger: FFTrigger { button: 0, interval: 0 },
            replay: FFReplay { length: if ms == 0 { u16::MAX } else { ms }, delay: 0 },
            kind: FFEffectKind::Rumble { strong_magnitude: strong, weak_magnitude: weak },
        };
        let ok = match &mut self.effect {
            Some(e) => e.update(data).is_ok(),
            None => match dev.upload_ff_effect(data) {
                Ok(e) => {
                    self.effect = Some(e);
                    true
                }
                Err(_) => false,
            },
        };
        if ok && let Some(e) = &mut self.effect {
            let _ = e.play(1);
        }
    }
}

fn micros() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}

fn open_socket() -> std::io::Result<UdpSocket> {
    let sock = UdpSocket::bind(("0.0.0.0", 0))?;
    sock.set_broadcast(true)?;
    sock.set_nonblocking(true)?;
    // WMM voice queue: the radio sends these ahead of bulk traffic
    unsafe {
        let tos: libc::c_int = 0xb8;
        let prio: libc::c_int = 6;
        libc::setsockopt(sock.as_raw_fd(), libc::IPPROTO_IP, libc::IP_TOS, (&tos as *const libc::c_int).cast(), 4);
        libc::setsockopt(sock.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PRIORITY, (&prio as *const libc::c_int).cast(), 4);
    }
    Ok(sock)
}

/// Runs until `shared.quit` is set. `dev` must already be grabbed.
pub fn run(dev: RawDevice, cfg: Config, shared: Arc<Shared>) -> std::io::Result<()> {
    let sock = open_socket()?;
    let mut ended = Ended { sock: &sock, to: None };
    serve(dev, cfg, shared, &sock, &mut ended)
}

/// Says goodbye however the loop ends, so the host unplugs the controller now
/// instead of after its timeout.
struct Ended<'a> {
    sock: &'a UdpSocket,
    to: Option<(SocketAddr, u32)>,
}

impl Drop for Ended<'_> {
    fn drop(&mut self) {
        if let Some((to, id)) = self.to {
            for _ in 0..3 {
                let _ = self.sock.send_to(&Msg::Bye { session: id }.encode(), to);
            }
        }
    }
}

fn serve(mut dev: RawDevice, cfg: Config, shared: Arc<Shared>, sock: &UdpSocket, ended: &mut Ended) -> std::io::Result<()> {
    let broadcast: SocketAddr = ([255, 255, 255, 255], PORT).into();
    let mut host = cfg.host;
    let mut session: Option<u32> = None;
    let mut pad = Pad::default();
    let mut sent = Pad::default();
    let mut seq: u32 = 0;
    let mut last_send = Instant::now() - KEEPALIVE;
    let mut last_ping = Instant::now() - PING_EVERY;
    let mut last_pong = Instant::now();
    let mut last_retry = Instant::now() - RETRY;
    let mut ping_id: u32 = 0;
    let mut exit_since: Option<Instant> = None;
    let mut motor = Motor { effect: None };
    let mut buf = [0u8; 512];
    let latency = std::env::var("FLIPAD_AUDIO_LATENCY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(30);
    let audio = Audio::start(latency);
    let mut routed: Option<Want> = None;
    let mut jack_sent: Option<Jack> = None;
    let mut last_jack = Instant::now();
    shared.update(|v| v.phase = if host.is_some() { Phase::Connecting } else { Phase::Searching });

    let send = |to: SocketAddr, m: &Msg| {
        let _ = sock.send_to(&m.encode(), to);
    };

    while !shared.quit.load(Ordering::Relaxed) {
        ended.to = host.zip(session);
        let mut fds = [
            libc::pollfd { fd: dev.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: sock.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        let wait = KEEPALIVE.saturating_sub(last_send.elapsed()).as_millis().max(1) as i32;
        unsafe { libc::poll(fds.as_mut_ptr(), 2, wait) };
        let now = Instant::now();

        if fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(std::io::Error::other("controller went away"));
        }
        if fds[0].revents & libc::POLLIN != 0 {
            let events: Vec<InputEvent> = dev.fetch_events()?.collect();
            for ev in &events {
                if ev.event_type() == EventType::SYNCHRONIZATION && ev.code() == SynchronizationCode::SYN_REPORT.0 {
                    if pad != sent && let (Some(to), Some(id)) = (host, session) {
                        seq = seq.wrapping_add(1);
                        send(to, &Msg::State { session: id, seq, pad });
                        sent = pad;
                        last_send = now;
                    }
                } else {
                    apply_event(&mut pad, ev);
                }
            }
            shared.update(|v| v.pad = pad);
        }

        // exit combo
        if pad.buttons & EXIT_COMBO == EXIT_COMBO {
            let since = *exit_since.get_or_insert(now);
            let p = (now.duration_since(since).as_secs_f32() / EXIT_HOLD.as_secs_f32()).min(1.0);
            shared.update(|v| v.exit_hold = p);
            if p >= 1.0 {
                shared.quit.store(true, Ordering::Relaxed);
            }
        } else if exit_since.take().is_some() {
            shared.update(|v| v.exit_hold = 0.0);
        }

        if fds[1].revents & libc::POLLIN != 0 {
            while let Ok((n, from)) = sock.recv_from(&mut buf) {
                match Msg::decode(&buf[..n]) {
                    Some(Msg::Offer { name }) if host.is_none() => {
                        host = Some(from);
                        last_retry = now - RETRY;
                        shared.update(|v| {
                            v.host = name;
                            v.phase = Phase::Connecting;
                        });
                    }
                    Some(Msg::Welcome { session: id, name }) if Some(from) == host => {
                        session = Some(id);
                        last_pong = now;
                        sent = Pad { buttons: !pad.buttons, ..pad }; // force a full send
                        shared.update(|v| {
                            v.host = name;
                            v.phase = Phase::Connected;
                        });
                    }
                    Some(Msg::Reject { reason }) if Some(from) == host => {
                        session = None;
                        shared.update(|v| v.phase = Phase::Rejected(reason));
                    }
                    Some(Msg::Pong { micros: t, .. }) if Some(from) == host => {
                        last_pong = now;
                        let rtt = micros().saturating_sub(t) as f32 / 1000.0;
                        shared.update(|v| {
                            v.rtt_ms = Some(v.rtt_ms.map_or(rtt, |old| old * 0.8 + rtt * 0.2));
                            if v.phase == Phase::Lost {
                                v.phase = Phase::Connected;
                            }
                        });
                    }
                    Some(Msg::Rumble { strong, weak, ms }) if Some(from) == host => {
                        motor.play(&mut dev, strong, weak, ms);
                        shared.update(|v| v.rumble = strong > 0 || weak > 0);
                    }
                    Some(Msg::Status(s)) if Some(from) == host => shared.update(|v| v.status = s),
                    _ => {}
                }
            }
        }

        match (host, session) {
            (None, _) if now.duration_since(last_retry) >= RETRY => {
                send(broadcast, &Msg::Discover);
                last_retry = now;
            }
            (Some(to), None) if now.duration_since(last_retry) >= RETRY => {
                send(to, &Msg::Hello { token: cfg.token, name: cfg.name.clone() });
                last_retry = now;
            }
            (Some(to), Some(id)) => {
                let jack = *shared.jack.lock().unwrap();
                if jack_sent != Some(jack) || now.duration_since(last_jack) >= Duration::from_secs(1) {
                    send(to, &Msg::Jack { session: id, headphones: jack.headphones, mic: jack.mic });
                    jack_sent = Some(jack);
                    last_jack = now;
                }
                if now.duration_since(last_send) >= KEEPALIVE {
                    seq = seq.wrapping_add(1);
                    send(to, &Msg::State { session: id, seq, pad });
                    sent = pad;
                    last_send = now;
                }
                if now.duration_since(last_ping) >= PING_EVERY {
                    ping_id = ping_id.wrapping_add(1);
                    send(to, &Msg::Ping { session: id, id: ping_id, micros: micros() });
                    last_ping = now;
                }
                if now.duration_since(last_pong) > LINK_LOST {
                    // host restarted or the network dropped: start over
                    session = None;
                    shared.update(|v| {
                        v.phase = Phase::Lost;
                        v.rtt_ms = None;
                    });
                }
            }
            _ => {}
        }

        let want = host.zip(session).map(|(to, _)| Want { host: to.ip(), jack: *shared.jack.lock().unwrap() });
        if want != routed {
            if want.is_none() {
                jack_sent = None;
            }
            routed = want;
            audio.set(want);
            let live = want.is_some_and(|w| w.jack.headphones || w.jack.mic);
            shared.update(|v| {
                v.jack = want.map_or(*shared.jack.lock().unwrap(), |w| w.jack);
                v.audio_live = live;
            });
        }
    }

    drop(audio);
    motor.play(&mut dev, 0, 0, 0);
    ended.to = host.zip(session);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: EventType, code: u16, v: i32) -> InputEvent {
        InputEvent::new(t.0, code, v)
    }

    #[test]
    fn maps_xbox_events() {
        let mut p = Pad::default();
        apply_event(&mut p, &ev(EventType::KEY, KeyCode::BTN_SOUTH.0, 1));
        apply_event(&mut p, &ev(EventType::ABSOLUTE, Abs::ABS_X.0, -32768));
        apply_event(&mut p, &ev(EventType::ABSOLUTE, Abs::ABS_RZ.0, 300));
        apply_event(&mut p, &ev(EventType::ABSOLUTE, Abs::ABS_HAT0Y.0, -1));
        assert_eq!(p.buttons, button::SOUTH | button::DPAD_UP);
        assert_eq!((p.lx, p.rt), (-32768, 255));
        apply_event(&mut p, &ev(EventType::ABSOLUTE, Abs::ABS_HAT0Y.0, 0));
        apply_event(&mut p, &ev(EventType::KEY, KeyCode::BTN_SOUTH.0, 0));
        apply_event(&mut p, &ev(EventType::KEY, KeyCode::BTN_TRIGGER_HAPPY2.0, 1));
        assert_eq!(p.buttons, button::DPAD_RIGHT);
    }
}
