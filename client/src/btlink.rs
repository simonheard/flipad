//! Bluetooth mode: the same controls, layout switch, turbo and macros as the
//! Wi-Fi mode, sent to the host as an Xbox Wireless Controller over BLE.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use evdev::raw_stream::RawDevice;
use evdev::{EventType, InputEvent};
use flipad_proto::Pad;

use crate::ble::Peripheral;
use crate::ble::shortcut;
use crate::link::{BtHost, Cmd, Motor, Phase, PointerOp, Shared, View, apply_event};
use crate::settings::{Addr, Mode, addr_str, load_macros, parse_addr, save_macros};
use crate::shaper::{Event, Shaper};

fn battery() -> u8 {
    std::fs::read_to_string("/sys/class/power_supply/battery/capacity")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(100)
}

fn hosts_file() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/flipad/bt-hosts")
}

/// Hosts that have used this controller over Bluetooth: "ADDR<TAB>name" lines.
pub fn load_hosts() -> Vec<(Addr, String)> {
    std::fs::read_to_string(hosts_file())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (a, n) = l.split_once('\t').unwrap_or((l, ""));
            Some((parse_addr(a)?, n.to_string()))
        })
        .collect()
}

fn save_hosts(h: &[(Addr, String)]) {
    let text: String = h.iter().map(|(a, n)| format!("{}\t{n}\n", addr_str(a))).collect();
    let _ = std::fs::write(hosts_file(), text);
}

/// Runs until quit or until the panel switches to Wi-Fi.
pub fn run(dev: &mut RawDevice, wake: &OwnedFd, shared: Arc<Shared>) -> std::io::Result<()> {
    let ble = match Peripheral::start(battery()) {
        Ok(p) => p,
        Err(e) => {
            shared.update(|v| v.phase = Phase::Rejected(format!("蓝牙启动失败：{e}")));
            return Err(std::io::Error::other(e.to_string()));
        }
    };
    let mut phys = Pad::default();
    let mut sent: Option<Pad> = None;
    let mut motor = Motor { effect: None };
    let mut shaper = Shaper::new(load_macros(), Instant::now());
    let mut events: Vec<Event> = Vec::new();
    let mut last_poll = Instant::now() - Duration::from_secs(10);
    let mut last_battery = Instant::now();
    let mut was_subscribed = false;
    // touchpad: current buttons and sub-unit leftovers (x, y, wheel, pan)
    let mut mouse_buttons: u8 = 0;
    let mut rest = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    // shortcut keys go up again shortly after going down
    let mut release_at: Option<Instant> = None;
    let mut router = crate::audio::BtRouter::default();
    let mut audio_asked: Option<crate::settings::Addr> = None;
    let mut hosts = load_hosts();
    let mut target = shared.settings.lock().unwrap().bt_target;
    ble.apply_target(target.as_ref(), &hosts.iter().map(|h| h.0).collect::<Vec<_>>());
    let publish = |shaper: &Shaper, v: &mut View| {
        v.macros = std::array::from_fn(|i| (!shaper.macros[i].steps.is_empty()).then(|| shaper.macros[i].length()));
        v.recording = shaper.recording();
        v.playing = shaper.playing();
    };
    shared.update(|v| {
        v.phase = Phase::Searching;
        v.host.clear();
        publish(&shaper, v);
    });

    while !shared.quit.load(Ordering::Relaxed) {
        let now = Instant::now();
        let settings = *shared.settings.lock().unwrap();
        if settings.mode != Mode::Bluetooth {
            break;
        }
        if settings.bt_target != target {
            target = settings.bt_target;
            ble.apply_target(target.as_ref(), &hosts.iter().map(|h| h.0).collect::<Vec<_>>());
            last_poll = now - Duration::from_secs(10);
        }
        let deadline = [shaper.deadline(&settings, now), release_at].into_iter().flatten().min();
        let wait = deadline.map_or(250, |d| d.saturating_duration_since(now).as_millis().clamp(1, 250) as i32);
        let mut fds = [
            libc::pollfd { fd: dev.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        unsafe { libc::poll(fds.as_mut_ptr(), 2, wait) };
        let now = Instant::now();
        if fds[1].revents & libc::POLLIN != 0 {
            let mut drain = [0u8; 64];
            unsafe { libc::read(wake.as_raw_fd(), drain.as_mut_ptr().cast(), drain.len()) };
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
        // touchpad → Bluetooth mouse
        let ops: Vec<PointerOp> = std::mem::take(&mut *shared.pointer.lock().unwrap());
        if !ops.is_empty() {
            let speed = settings.pointer_speed as f32 / 100.0;
            let mut acc = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            let mut flush = |acc: &mut (f32, f32, f32, f32), buttons: u8, force: bool| {
                let take = |v: f32, rest: &mut f32, scale: f32, max: f32| {
                    let t = v / scale + *rest;
                    let whole = t.trunc().clamp(-max, max);
                    *rest = t - whole;
                    whole
                };
                let dx = take(acc.0 * speed, &mut rest.0, 1.0, 32767.0) as i16;
                let dy = take(acc.1 * speed, &mut rest.1, 1.0, 32767.0) as i16;
                // wheel arrives in 1/120 notches; the report carries whole notches
                let wheel = take(acc.2, &mut rest.2, 120.0, 127.0) as i8;
                let pan = take(acc.3, &mut rest.3, 120.0, 127.0) as i8;
                *acc = (0.0, 0.0, 0.0, 0.0);
                if force || dx != 0 || dy != 0 || wheel != 0 || pan != 0 {
                    ble.send_mouse(buttons, dx, dy, wheel, pan);
                }
            };
            for op in ops {
                match op {
                    PointerOp::Move(x, y) => {
                        acc.0 += x;
                        acc.1 += y;
                    }
                    PointerOp::Scroll(v, h) => {
                        acc.2 += v;
                        acc.3 += h;
                    }
                    PointerOp::Buttons(b) => {
                        flush(&mut acc, mouse_buttons, false);
                        mouse_buttons = b;
                        flush(&mut acc, b, true);
                    }
                }
            }
            flush(&mut acc, mouse_buttons, false);
        }
        if release_at.is_some_and(|t| now >= t) {
            release_at = None;
            ble.send_media(0);
            ble.send_keys(0, &[]);
        }
        let mut run_shortcut = |a: flipad_proto::HostAction| {
            if let Some((media, mods, keys)) = shortcut(a) {
                if media != 0 {
                    ble.send_media(media);
                }
                if mods != 0 || !keys.is_empty() {
                    ble.send_keys(mods, keys);
                }
                release_at = Some(now + Duration::from_millis(40));
                shared.update(|v| v.last_action = Some((a, now)));
            }
        };

        let mut macros_changed = false;
        for c in std::mem::take(&mut *shared.cmds.lock().unwrap()) {
            match c {
                Cmd::Action(a) => run_shortcut(a),
                Cmd::PluginButton(_) => {}
                Cmd::Record(i) => shaper.start_recording(i, now),
                Cmd::StopRecording => macros_changed |= shaper.stop_recording(now).is_some(),
                Cmd::Play(i) => shaper.play(i, now),
                Cmd::Clear(i) => {
                    shaper.clear(i);
                    macros_changed = true;
                }
            }
        }
        let was = (shaper.recording(), shaper.playing());
        let out = shaper.update(phys, &settings, now, &mut events);
        if was.0.is_some() && shaper.recording().is_none() {
            macros_changed = true;
        }
        for e in events.drain(..) {
            match e {
                Event::Action(a) => run_shortcut(a),
                Event::PlayMacro(i) => shaper.play(i, now),
                Event::ToggleScreen => shared.update(|v| v.screen_off = !v.screen_off),
                Event::Quit => shared.quit.store(true, Ordering::Relaxed),
            }
        }
        if macros_changed {
            save_macros(&shaper.macros);
        }

        // a host just subscribed: send the current state right away
        let subscribed = ble.subscribed();
        if subscribed && !was_subscribed {
            sent = None;
        }
        was_subscribed = subscribed;
        if sent != Some(out) && subscribed {
            ble.send(&out);
            sent = Some(out);
        }

        while let Ok(r) = ble.rumble.try_recv() {
            if settings.rumble {
                motor.play(dev, r.strong, r.weak, r.ms);
                shared.update(|v| v.rumble = r.strong > 0 || r.weak > 0);
            }
        }

        if now.duration_since(last_poll) >= Duration::from_secs(1) {
            last_poll = now;
            // remember every host that uses us, for the switcher
            if let Some((addr, name)) = ble.peer() {
                match hosts.iter_mut().find(|h| h.0 == addr) {
                    Some(h) if h.1 != name => {
                        h.1 = name;
                        save_hosts(&hosts);
                    }
                    Some(_) => {}
                    None => {
                        hosts.push((addr, name));
                        save_hosts(&hosts);
                    }
                }
            }
            let list: Vec<BtHost> =
                hosts.iter().map(|(a, n)| BtHost { addr: *a, name: n.clone(), connected: ble.is_connected(a) }).collect();
            let host = list.iter().find(|h| h.connected).map(|h| h.name.clone());
            // sound arrives over A2DP (the host opens it); send it to the
            // chosen output, or turn it away when host sound is off
            let connected = list.iter().find(|h| h.connected).map(|h| h.addr);
            if connected.is_none() || settings.audio {
                audio_asked = None;
            } else if audio_asked != connected {
                audio_asked = connected;
                ble.disconnect_audio(connected.unwrap());
            }
            router.update(settings.output);
            let phase = match (&host, subscribed) {
                (Some(_), true) => Phase::Connected,
                (Some(_), false) => Phase::Connecting,
                _ => Phase::Searching,
            };
            shared.update(|v| {
                v.phase = phase;
                v.host = host.unwrap_or_default();
                v.bt_hosts = list;
            });
        }
        if now.duration_since(last_battery) >= Duration::from_secs(30) {
            last_battery = now;
            ble.set_battery(battery());
        }

        let progress = shaper.exit_progress(now);
        let status = (shaper.recording(), shaper.playing());
        let mut v = shared.view.lock().unwrap();
        let changed = v.pad != phys || v.exit_hold != progress || status != was || macros_changed;
        v.pad = phys;
        v.exit_hold = progress;
        if status != (v.recording, v.playing) || macros_changed {
            publish(&shaper, &mut v);
        }
        drop(v);
        if changed {
            (shared.notify)();
        }
    }

    if shaper.stop_recording(Instant::now()).is_some() {
        save_macros(&shaper.macros);
    }
    // release everything before going away so nothing stays held on the host
    if ble.subscribed() {
        ble.send(&Pad::default());
        ble.send_mouse(0, 0, 0, 0, 0);
        ble.send_media(0);
        ble.send_keys(0, &[]);
    }
    motor.play(dev, 0, 0, 0);
    Ok(())
}
