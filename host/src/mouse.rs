//! Virtual mouse driven by the handheld's touchpad tab.

use std::io;

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode, RelativeAxisCode as Rel};

const BUTTONS: [(u8, KeyCode); 3] = [(1, KeyCode::BTN_LEFT), (2, KeyCode::BTN_RIGHT), (4, KeyCode::BTN_MIDDLE)];

pub struct Mouse {
    dev: VirtualDevice,
    buttons: u8,
    /// Hi-res wheel units not yet sent as whole legacy notches.
    notch: (i32, i32),
}

impl Mouse {
    /// Created at startup so the desktop has picked it up before first use.
    pub fn new() -> io::Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for (_, k) in BUTTONS {
            keys.insert(k);
        }
        let mut rel = AttributeSet::<Rel>::new();
        for a in [Rel::REL_X, Rel::REL_Y, Rel::REL_WHEEL, Rel::REL_HWHEEL, Rel::REL_WHEEL_HI_RES, Rel::REL_HWHEEL_HI_RES] {
            rel.insert(a);
        }
        let dev = VirtualDevice::builder()?
            .name("flipad touchpad mouse")
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x1209, 0xf1a1, 1))
            .with_keys(&keys)?
            .with_relative_axes(&rel)?
            .build()?;
        Ok(Self { dev, buttons: 0, notch: (0, 0) })
    }

    pub fn apply(&mut self, dx: i16, dy: i16, wheel: i16, hwheel: i16, buttons: u8) {
        let rel = |a: Rel, v: i32| InputEvent::new(EventType::RELATIVE.0, a.0, v);
        let mut ev = Vec::with_capacity(8);
        if dx != 0 {
            ev.push(rel(Rel::REL_X, dx as i32));
        }
        if dy != 0 {
            ev.push(rel(Rel::REL_Y, dy as i32));
        }
        for (v, hi, lo, acc) in [
            (wheel, Rel::REL_WHEEL_HI_RES, Rel::REL_WHEEL, &mut self.notch.0),
            (hwheel, Rel::REL_HWHEEL_HI_RES, Rel::REL_HWHEEL, &mut self.notch.1),
        ] {
            if v != 0 {
                ev.push(rel(hi, v as i32));
                *acc += v as i32;
                let n = *acc / 120;
                if n != 0 {
                    *acc -= n * 120;
                    ev.push(rel(lo, n));
                }
            }
        }
        let changed = self.buttons ^ buttons;
        for (bit, key) in BUTTONS {
            if changed & bit != 0 {
                ev.push(InputEvent::new(EventType::KEY.0, key.0, (buttons & bit != 0) as i32));
            }
        }
        self.buttons = buttons;
        if !ev.is_empty() {
            let _ = self.dev.emit(&ev);
        }
    }

    /// Lets go of every button (the handheld disconnected).
    pub fn release(&mut self) {
        self.apply(0, 0, 0, 0, 0);
    }
}
