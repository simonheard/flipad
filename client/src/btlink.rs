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
use crate::link::{Cmd, Motor, Phase, Shared, View, apply_event};
use crate::settings::{load_macros, save_macros};
use crate::shaper::{Event, Shaper};

fn battery() -> u8 {
    std::fs::read_to_string("/sys/class/power_supply/battery/capacity")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(100)
}

pub fn run(mut dev: RawDevice, wake: OwnedFd, shared: Arc<Shared>) -> std::io::Result<()> {
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
        let wait = shaper
            .deadline(&settings, now)
            .map_or(250, |d| d.saturating_duration_since(now).as_millis().clamp(1, 250) as i32);
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
        // the touchpad has no host to talk to here
        shared.pointer.lock().unwrap().clear();

        let mut macros_changed = false;
        for c in std::mem::take(&mut *shared.cmds.lock().unwrap()) {
            match c {
                Cmd::Action(_) => {}
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
                Event::Action(_) => {}
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
                motor.play(&mut dev, r.strong, r.weak, r.ms);
                shared.update(|v| v.rumble = r.strong > 0 || r.weak > 0);
            }
        }

        if now.duration_since(last_poll) >= Duration::from_secs(1) {
            last_poll = now;
            let host = ble.connected_host();
            let phase = match (&host, subscribed) {
                (Some(_), true) => Phase::Connected,
                (Some(_), false) => Phase::Connecting,
                _ => Phase::Searching,
            };
            shared.update(|v| {
                v.phase = phase;
                v.host = host.unwrap_or_default();
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
    }
    motor.play(&mut dev, 0, 0, 0);
    Ok(())
}
