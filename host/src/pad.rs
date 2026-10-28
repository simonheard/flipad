//! Virtual Xbox 360 controller on the host, fed from the handheld's state.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, RawFd};

use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode as Abs, AttributeSet, BusType, EventSummary, EventType, FFEffectCode, FFEffectKind,
    InputEvent, InputId, KeyCode, UInputCode, UinputAbsSetup,
};
use flipad_proto::{Pad, button};

/// What the host should tell the handheld's motors to do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rumble {
    pub strong: u16,
    pub weak: u16,
    pub ms: u16,
}

const BUTTONS: [(u32, KeyCode); 11] = [
    (button::SOUTH, KeyCode::BTN_SOUTH),
    (button::EAST, KeyCode::BTN_EAST),
    (button::NORTH, KeyCode::BTN_NORTH),
    (button::WEST, KeyCode::BTN_WEST),
    (button::TL, KeyCode::BTN_TL),
    (button::TR, KeyCode::BTN_TR),
    (button::SELECT, KeyCode::BTN_SELECT),
    (button::START, KeyCode::BTN_START),
    (button::MODE, KeyCode::BTN_MODE),
    (button::THUMBL, KeyCode::BTN_THUMBL),
    (button::THUMBR, KeyCode::BTN_THUMBR),
];

pub struct VirtualPad {
    dev: VirtualDevice,
    state: Pad,
    effects: HashMap<i16, Rumble>,
}

fn hat(buttons: u32, neg: u32, pos: u32) -> i32 {
    (buttons & pos != 0) as i32 - (buttons & neg != 0) as i32
}

impl VirtualPad {
    pub fn new() -> io::Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for (_, k) in BUTTONS {
            keys.insert(k);
        }
        let stick = AbsInfo::new(0, -32768, 32767, 16, 128, 0);
        let trigger = AbsInfo::new(0, 0, 255, 0, 0, 0);
        let hat = AbsInfo::new(0, -1, 1, 0, 0, 0);
        let mut ff = AttributeSet::<FFEffectCode>::new();
        ff.insert(FFEffectCode::FF_RUMBLE);
        let mut b = VirtualDevice::builder()?
            .name("Microsoft X-Box 360 pad")
            .input_id(InputId::new(BusType::BUS_USB, 0x045e, 0x028e, 0x0110))
            .with_keys(&keys)?
            .with_ff(&ff)?
            .with_ff_effects_max(16);
        for (axis, info) in [
            (Abs::ABS_X, stick),
            (Abs::ABS_Y, stick),
            (Abs::ABS_RX, stick),
            (Abs::ABS_RY, stick),
            (Abs::ABS_Z, trigger),
            (Abs::ABS_RZ, trigger),
            (Abs::ABS_HAT0X, hat),
            (Abs::ABS_HAT0Y, hat),
        ] {
            b = b.with_absolute_axis(&UinputAbsSetup::new(axis, info))?;
        }
        let dev = b.build()?;
        set_nonblocking(dev.as_raw_fd())?;
        Ok(Self { dev, state: Pad::default(), effects: HashMap::new() })
    }

    pub fn fd(&self) -> RawFd {
        self.dev.as_raw_fd()
    }

    pub fn state(&self) -> Pad {
        self.state
    }

    /// Emits only what changed since the last state, in one report.
    pub fn apply(&mut self, new: Pad) -> io::Result<()> {
        let old = self.state;
        let mut ev = Vec::with_capacity(8);
        let changed = old.buttons ^ new.buttons;
        for (bit, key) in BUTTONS {
            if changed & bit != 0 {
                ev.push(InputEvent::new(EventType::KEY.0, key.0, (new.buttons & bit != 0) as i32));
            }
        }
        let axes = [
            (Abs::ABS_X, old.lx as i32, new.lx as i32),
            (Abs::ABS_Y, old.ly as i32, new.ly as i32),
            (Abs::ABS_RX, old.rx as i32, new.rx as i32),
            (Abs::ABS_RY, old.ry as i32, new.ry as i32),
            (Abs::ABS_Z, old.lt as i32, new.lt as i32),
            (Abs::ABS_RZ, old.rt as i32, new.rt as i32),
            (
                Abs::ABS_HAT0X,
                hat(old.buttons, button::DPAD_LEFT, button::DPAD_RIGHT),
                hat(new.buttons, button::DPAD_LEFT, button::DPAD_RIGHT),
            ),
            (
                Abs::ABS_HAT0Y,
                hat(old.buttons, button::DPAD_UP, button::DPAD_DOWN),
                hat(new.buttons, button::DPAD_UP, button::DPAD_DOWN),
            ),
        ];
        for (axis, o, n) in axes {
            if o != n {
                ev.push(InputEvent::new(EventType::ABSOLUTE.0, axis.0, n));
            }
        }
        self.state = new;
        if ev.is_empty() { Ok(()) } else { self.dev.emit(&ev) }
    }

    /// Services force-feedback requests from games; returns rumble commands.
    pub fn poll_ff(&mut self) -> Vec<Rumble> {
        let mut out = Vec::new();
        let events: Vec<InputEvent> = match self.dev.fetch_events() {
            Ok(it) => it.collect(),
            Err(_) => return out,
        };
        for ev in events {
            match ev.destructure() {
                EventSummary::UInput(ev, UInputCode::UI_FF_UPLOAD, _) => {
                    if let Ok(mut up) = self.dev.process_ff_upload(ev) {
                        let data = up.effect();
                        if let FFEffectKind::Rumble { strong_magnitude, weak_magnitude } = data.kind {
                            let r = Rumble { strong: strong_magnitude, weak: weak_magnitude, ms: data.replay.length };
                            self.effects.insert(up.effect_id(), r);
                        }
                        up.set_retval(0);
                    }
                }
                EventSummary::UInput(ev, UInputCode::UI_FF_ERASE, _) => {
                    if let Ok(mut er) = self.dev.process_ff_erase(ev) {
                        self.effects.remove(&(er.effect_id() as i16));
                        er.set_retval(0);
                    }
                }
                EventSummary::ForceFeedback(_, code, value) if code != FFEffectCode::FF_GAIN => {
                    let stop = Rumble { strong: 0, weak: 0, ms: 0 };
                    let r = if value > 0 { self.effects.get(&(code.0 as i16)).copied().unwrap_or(stop) } else { stop };
                    out.push(r);
                }
                _ => {}
            }
        }
        out
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::hat;
    use flipad_proto::button;

    #[test]
    fn dpad_to_hat() {
        assert_eq!(hat(button::DPAD_LEFT, button::DPAD_LEFT, button::DPAD_RIGHT), -1);
        assert_eq!(hat(button::DPAD_RIGHT, button::DPAD_LEFT, button::DPAD_RIGHT), 1);
        assert_eq!(hat(button::DPAD_LEFT | button::DPAD_RIGHT, button::DPAD_LEFT, button::DPAD_RIGHT), 0);
    }
}
