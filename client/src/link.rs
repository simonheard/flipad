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
    KeyCode,
};
use flipad_proto::{HostAction, Msg, PORT, Pad, Status, TOKEN_LEN, Widget, button};
use tiny_skia::Pixmap;

use crate::audio::{Audio, Jack, Want};
use crate::settings::{Addr, MACRO_SLOTS, Mode, Settings, load_macros, save_macros};
use crate::shaper::{Event, Shaper};

/// Re-send the current state this often even when nothing changes, so a lost
/// packet is repaired quickly and the host knows we are alive.
const KEEPALIVE: Duration = Duration::from_millis(20);
const PING_EVERY: Duration = Duration::from_millis(250);
const LINK_LOST: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_millis(700);

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
    /// Per macro slot: length, or None when empty.
    pub macros: [Option<Duration>; MACRO_SLOTS],
    pub recording: Option<usize>,
    pub playing: Option<usize>,
    /// Black screen to save power (AMOLED); any touch wakes it.
    pub screen_off: bool,
    /// Last shortcut sent, for a brief confirmation on screen.
    pub last_action: Option<(HostAction, Instant)>,
    /// Running as a Bluetooth controller (no host service: no shortcuts,
    /// touchpad, audio or host readings).
    pub bluetooth: bool,
    /// Bluetooth hosts that have used this controller, and which is connected.
    pub bt_hosts: Vec<BtHost>,
    /// Name of the Wi-Fi host last connected to.
    pub wifi_host: String,
    /// The active game plugin's panel on the host, if any.
    pub plugin: Option<PluginPanel>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BtHost {
    pub addr: Addr,
    pub name: String,
    pub connected: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PluginPanel {
    pub plugin: String,
    pub title: String,
    pub widgets: Vec<Widget>,
}

/// Touchpad input queued by the panel, in design pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerOp {
    Move(f32, f32),
    /// Scroll in 1/120 notches (vertical, horizontal); positive = up / right.
    Scroll(f32, f32),
    /// New button state: bit 0 left, 1 right, 2 middle.
    Buttons(u8),
}

/// Requests from the touch panel to the forwarding loop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cmd {
    Action(HostAction),
    Record(usize),
    StopRecording,
    Play(usize),
    Clear(usize),
    PluginButton(u16),
}

pub struct Shared {
    pub view: Mutex<View>,
    pub quit: Arc<AtomicBool>,
    pub jack: Arc<Mutex<Jack>>,
    pub settings: Mutex<Settings>,
    pub cmds: Mutex<Vec<Cmd>>,
    pub pointer: Mutex<Vec<PointerOp>>,
    /// Latest decoded image per plugin image widget.
    pub frames: Mutex<std::collections::HashMap<u16, Arc<Pixmap>>>,
    /// Write end of a pipe the forwarding loop polls, to act on panel input
    /// immediately instead of at its next keepalive.
    pub wake: std::os::fd::OwnedFd,
    /// Called whenever `view` changed.
    pub notify: Box<dyn Fn() + Send + Sync>,
}

impl Shared {
    pub fn command(&self, c: Cmd) {
        self.cmds.lock().unwrap().push(c);
        self.poke();
    }

    pub fn pointer(&self, op: PointerOp) {
        self.pointer.lock().unwrap().push(op);
        self.poke();
    }

    /// Wakes the forwarding loop (e.g. after a settings change).
    pub fn poke_pub(&self) {
        self.poke();
    }

    fn poke(&self) {
        use std::os::fd::AsRawFd;
        unsafe { libc::write(self.wake.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
    }

    pub fn update(&self, f: impl FnOnce(&mut View)) {
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

pub struct Motor {
    pub effect: Option<FFEffect>,
}

impl Motor {
    pub fn play(&mut self, dev: &mut RawDevice, strong: u16, weak: u16, ms: u16) {
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
/// Runs until quit or until the panel switches to Bluetooth.
pub fn run(dev: &mut RawDevice, wake: &std::os::fd::OwnedFd, cfg: &Config, shared: Arc<Shared>) -> std::io::Result<()> {
    let sock = open_socket()?;
    let mut ended = Ended { sock: &sock, to: None };
    serve(dev, wake, cfg, shared, &sock, &mut ended)
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

fn serve(
    dev: &mut RawDevice,
    wake: &std::os::fd::OwnedFd,
    cfg: &Config,
    shared: Arc<Shared>,
    sock: &UdpSocket,
    ended: &mut Ended,
) -> std::io::Result<()> {
    let broadcast: SocketAddr = ([255, 255, 255, 255], PORT).into();
    let mut host = cfg.host;
    let mut session: Option<u32> = None;
    let mut phys = Pad::default();
    let mut sent = Pad::default();
    let mut seq: u32 = 0;
    let mut last_send = Instant::now() - KEEPALIVE;
    let mut last_ping = Instant::now() - PING_EVERY;
    let mut last_pong = Instant::now();
    let mut last_retry = Instant::now() - RETRY;
    let mut ping_id: u32 = 0;
    let mut action_id: u32 = 0;
    let mut pointer_seq: u32 = 0;
    let mut pointer_buttons: u8 = 0;
    // sub-unit leftovers so slow finger motion is not rounded away
    let mut pointer_rest = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    let mut motor = Motor { effect: None };
    let mut buf = [0u8; 2048];
    let mut shaper = Shaper::new(load_macros(), Instant::now());
    let mut events: Vec<Event> = Vec::new();
    let mut settings = *shared.settings.lock().unwrap();
    let mut audio = Audio::start(settings.audio_latency_ms);
    let mut routed: Option<Want> = None;
    let mut jack_sent: Option<Jack> = None;
    let mut last_jack = Instant::now();
    let publish_macros = |shaper: &Shaper, v: &mut View| {
        v.macros = std::array::from_fn(|i| (!shaper.macros[i].steps.is_empty()).then(|| shaper.macros[i].length()));
        v.recording = shaper.recording();
        v.playing = shaper.playing();
    };
    shared.update(|v| {
        v.phase = if host.is_some() { Phase::Connecting } else { Phase::Searching };
        publish_macros(&shaper, v);
    });

    let send = |to: SocketAddr, m: &Msg| {
        let _ = sock.send_to(&m.encode(), to);
    };

    let mut frames: std::collections::HashMap<u16, (u32, Vec<Option<Vec<u8>>>)> = Default::default();
    while !shared.quit.load(Ordering::Relaxed) && shared.settings.lock().unwrap().mode == Mode::Wifi {
        ended.to = host.zip(session);
        let now = Instant::now();
        let mut wake_at = last_send + KEEPALIVE;
        if let Some(d) = shaper.deadline(&settings, now) {
            wake_at = wake_at.min(d);
        }
        let wait = wake_at.saturating_duration_since(now).as_millis().clamp(1, 20) as i32;
        let mut fds = [
            libc::pollfd { fd: dev.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: sock.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        unsafe { libc::poll(fds.as_mut_ptr(), 3, wait) };
        if fds[2].revents & libc::POLLIN != 0 {
            let mut drain = [0u8; 64];
            unsafe { libc::read(wake.as_raw_fd(), drain.as_mut_ptr().cast(), drain.len()) };
        }
        let now = Instant::now();

        // settings may have been changed on the panel
        let new_settings = *shared.settings.lock().unwrap();
        if new_settings != settings {
            if new_settings.audio_latency_ms != settings.audio_latency_ms {
                drop(audio);
                audio = Audio::start(new_settings.audio_latency_ms);
                routed = None;
            }
            if !new_settings.rumble {
                motor.play(dev, 0, 0, 0);
            }
            settings = new_settings;
        }

        if fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(std::io::Error::other("controller went away"));
        }
        if fds[0].revents & libc::POLLIN != 0 {
            for ev in dev.fetch_events()?.collect::<Vec<InputEvent>>() {
                if ev.event_type() != EventType::SYNCHRONIZATION {
                    apply_event(&mut phys, &ev);
                }
            }
        }

        // panel commands
        let cmds: Vec<Cmd> = std::mem::take(&mut *shared.cmds.lock().unwrap());
        let mut macros_changed = false;
        for c in cmds {
            match c {
                Cmd::Action(a) => events.push(Event::Action(a)),
                Cmd::Record(i) => shaper.start_recording(i, now),
                Cmd::StopRecording => macros_changed |= shaper.stop_recording(now).is_some(),
                Cmd::Play(i) => shaper.play(i, now),
                Cmd::Clear(i) => {
                    shaper.clear(i);
                    macros_changed = true;
                }
                Cmd::PluginButton(id) => {
                    let plugin = shared.view.lock().unwrap().plugin.as_ref().map(|p| p.plugin.clone());
                    if let (Some(to), Some(sid), Some(plugin)) = (host, session, plugin) {
                        for _ in 0..2 {
                            send(to, &Msg::PluginButton { session: sid, plugin: plugin.clone(), id });
                        }
                    }
                }
            }
        }

        let was = (shaper.recording(), shaper.playing());
        let out = shaper.update(phys, &settings, now, &mut events);
        if shaper.recording().is_none() && was.0.is_some() {
            macros_changed = true; // hit the length limit
        }
        for e in events.drain(..) {
            match e {
                Event::Action(a) => {
                    if let (Some(to), Some(id)) = (host, session) {
                        action_id = action_id.wrapping_add(1);
                        for _ in 0..3 {
                            send(to, &Msg::Action { session: id, id: action_id, action: a });
                        }
                    }
                    shared.update(|v| v.last_action = Some((a, now)));
                }
                Event::PlayMacro(i) => shaper.play(i, now),
                Event::ToggleScreen => shared.update(|v| v.screen_off = !v.screen_off),
                Event::Quit => shared.quit.store(true, Ordering::Relaxed),
            }
        }
        if macros_changed {
            save_macros(&shaper.macros);
        }

        // touchpad: merge motion, and send every button change on its own
        let ops: Vec<PointerOp> = std::mem::take(&mut *shared.pointer.lock().unwrap());
        if !ops.is_empty()
            && let (Some(to), Some(id)) = (host, session)
        {
            let speed = settings.pointer_speed as f32 / 100.0;
            let mut acc = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            let mut flush = |acc: &mut (f32, f32, f32, f32), buttons: u8, twice: bool| {
                let r = &mut pointer_rest;
                let take = |v: f32, rest: &mut f32| {
                    let t = v + *rest;
                    let whole = t.trunc().clamp(i16::MIN as f32, i16::MAX as f32);
                    *rest = t - whole;
                    whole as i16
                };
                let dx = take(acc.0, &mut r.0);
                let dy = take(acc.1, &mut r.1);
                let wheel = take(acc.2, &mut r.2);
                let hwheel = take(acc.3, &mut r.3);
                *acc = (0.0, 0.0, 0.0, 0.0);
                if dx == 0 && dy == 0 && wheel == 0 && hwheel == 0 && !twice {
                    return;
                }
                pointer_seq = pointer_seq.wrapping_add(1);
                let m = Msg::Pointer { session: id, seq: pointer_seq, dx, dy, wheel, hwheel, buttons };
                send(to, &m);
                if twice {
                    send(to, &m); // button changes must not get lost
                }
            };
            for op in ops {
                match op {
                    PointerOp::Move(x, y) => {
                        acc.0 += x * speed;
                        acc.1 += y * speed;
                    }
                    PointerOp::Scroll(v, h) => {
                        acc.2 += v;
                        acc.3 += h;
                    }
                    PointerOp::Buttons(b) => {
                        flush(&mut acc, pointer_buttons, false);
                        pointer_buttons = b;
                        flush(&mut acc, b, true);
                    }
                }
            }
            flush(&mut acc, pointer_buttons, false);
        }
        let progress = shaper.exit_progress(now);
        let status = (shaper.recording(), shaper.playing());
        {
            let mut v = shared.view.lock().unwrap();
            let changed = v.pad != phys || v.exit_hold != progress || status != was || macros_changed;
            v.pad = phys;
            v.exit_hold = progress;
            if status != (v.recording, v.playing) || macros_changed {
                publish_macros(&shaper, &mut v);
            }
            drop(v);
            if changed {
                (shared.notify)();
            }
        }

        if out != sent
            && let (Some(to), Some(id)) = (host, session)
        {
            seq = seq.wrapping_add(1);
            send(to, &Msg::State { session: id, seq, pad: out });
            sent = out;
            last_send = now;
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
                        sent = Pad { buttons: !out.buttons, ..out }; // force a full send
                        shared.update(|v| {
                            v.wifi_host = name.clone();
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
                        if settings.rumble {
                            motor.play(dev, strong, weak, ms);
                            shared.update(|v| v.rumble = strong > 0 || weak > 0);
                        }
                    }
                    Some(Msg::Status(s)) if Some(from) == host => shared.update(|v| v.status = s),
                    Some(Msg::Panel { plugin, title, widgets }) if Some(from) == host => {
                        let p = (!plugin.is_empty()).then_some(PluginPanel { plugin, title, widgets });
                        if shared.view.lock().unwrap().plugin != p {
                            eprintln!("plugin panel: {:?}", p.as_ref().map(|p| (&p.plugin, &p.title, p.widgets.len())));
                            if p.is_none() {
                                shared.frames.lock().unwrap().clear();
                            }
                            shared.update(|v| v.plugin = p);
                        }
                    }
                    Some(Msg::Frame { image, frame, part, parts, data }) if Some(from) == host => {
                        if let Some(jpeg) = assemble(&mut frames, image, frame, part, parts, data)
                            && let Some(pm) = decode_jpeg(&jpeg)
                        {
                            if !shared.frames.lock().unwrap().contains_key(&image) {
                                eprintln!("plugin image {image}: first frame {}x{}", pm.width(), pm.height());
                            }
                            shared.frames.lock().unwrap().insert(image, Arc::new(pm));
                            (shared.notify)();
                        }
                    }
                    _ => {}
                }
            }
        }

        // headset routing follows the jack, filtered by the panel switches
        let jack = {
            let j = *shared.jack.lock().unwrap();
            // "speaker" plays the host even without headphones plugged in
            let play = j.headphones || settings.output == crate::settings::Output::Speaker;
            Jack { headphones: play && settings.audio, mic: j.mic && settings.mic }
        };
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
                if jack_sent != Some(jack) || now.duration_since(last_jack) >= Duration::from_secs(1) {
                    send(to, &Msg::Jack { session: id, headphones: jack.headphones, mic: jack.mic });
                    jack_sent = Some(jack);
                    last_jack = now;
                }
                if now.duration_since(last_send) >= KEEPALIVE {
                    seq = seq.wrapping_add(1);
                    send(to, &Msg::State { session: id, seq, pad: out });
                    sent = out;
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

        let want = host.zip(session).map(|(to, _)| Want { host: to.ip(), jack, output: settings.output });
        if want != routed {
            if want.is_none() {
                jack_sent = None;
            }
            routed = want;
            audio.set(want);
            let live = want.is_some_and(|w| w.jack.headphones || w.jack.mic);
            shared.update(|v| v.audio_live = live);
        }
    }

    if shaper.stop_recording(Instant::now()).is_some() {
        save_macros(&shaper.macros);
    }
    drop(audio);
    motor.play(dev, 0, 0, 0);
    ended.to = host.zip(session);
    shared.frames.lock().unwrap().clear();
    shared.update(|v| v.plugin = None);
    Ok(())
}

/// Collects frame chunks; returns the whole JPEG once every part is in.
/// An older frame still in flight is dropped when a newer one starts.
fn assemble(
    pending: &mut std::collections::HashMap<u16, (u32, Vec<Option<Vec<u8>>>)>,
    image: u16,
    frame: u32,
    part: u16,
    parts: u16,
    data: Vec<u8>,
) -> Option<Vec<u8>> {
    if parts == 0 || part >= parts || parts > 512 {
        return None;
    }
    let slot = pending.entry(image).or_insert_with(|| (frame, vec![None; parts as usize]));
    if slot.0 != frame {
        if !flipad_proto::seq_newer(frame, slot.0) {
            return None;
        }
        *slot = (frame, vec![None; parts as usize]);
    }
    slot.1[part as usize] = Some(data);
    if slot.1.iter().all(Option::is_some) {
        let jpeg = slot.1.iter_mut().flat_map(|c| c.take().unwrap()).collect();
        pending.remove(&image);
        return Some(jpeg);
    }
    None
}

fn decode_jpeg(jpeg: &[u8]) -> Option<Pixmap> {
    let opts = zune_jpeg::zune_core::options::DecoderOptions::default()
        .jpeg_set_out_colorspace(zune_jpeg::zune_core::colorspace::ColorSpace::RGBA);
    let mut dec = zune_jpeg::JpegDecoder::new_with_options(zune_jpeg::zune_core::bytestream::ZCursor::new(jpeg), opts);
    let rgba = dec.decode().ok()?;
    let info = dec.info()?;
    let (w, h) = (info.width, info.height);
    Pixmap::from_vec(rgba, tiny_skia::IntSize::from_wh(w as u32, h as u32)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: EventType, code: u16, v: i32) -> InputEvent {
        InputEvent::new(t.0, code, v)
    }

    #[test]
    fn frames_reassemble_and_drop_stale() {
        let mut p = Default::default();
        assert_eq!(assemble(&mut p, 1, 5, 1, 2, vec![3, 4]), None);
        assert_eq!(assemble(&mut p, 1, 5, 0, 2, vec![1, 2]), Some(vec![1, 2, 3, 4]));
        assert_eq!(assemble(&mut p, 1, 7, 0, 2, vec![9]), None);
        assert_eq!(assemble(&mut p, 1, 6, 1, 2, vec![8]), None, "older frame ignored");
        assert_eq!(assemble(&mut p, 1, 7, 1, 2, vec![10]), Some(vec![9, 10]));
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
