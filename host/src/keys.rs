//! Runs the handheld's shortcuts on the host, mostly as key presses from a
//! virtual keyboard so the desktop handles them natively (volume OSD, media
//! players, Steam's F12 screenshot, KDE's Alt+Tab and Meta+D).

use std::io;
use std::thread::sleep;
use std::time::Duration;

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode};
use flipad_proto::HostAction;

/// A key press request: a standard shortcut, or any combination (plugins).
pub enum KeyJob {
    Action(HostAction),
    Combo(Vec<KeyCode>),
}

pub struct Keys {
    dev: Option<VirtualDevice>,
}

fn combo(action: HostAction) -> &'static [KeyCode] {
    match action {
        HostAction::VolumeDown => &[KeyCode::KEY_VOLUMEDOWN],
        HostAction::VolumeUp => &[KeyCode::KEY_VOLUMEUP],
        HostAction::Mute => &[KeyCode::KEY_MUTE],
        HostAction::PlayPause => &[KeyCode::KEY_PLAYPAUSE],
        HostAction::Previous => &[KeyCode::KEY_PREVIOUSSONG],
        HostAction::Next => &[KeyCode::KEY_NEXTSONG],
        HostAction::SteamScreenshot => &[KeyCode::KEY_F12],
        HostAction::Screenshot => &[KeyCode::KEY_SYSRQ],
        HostAction::AltTab => &[KeyCode::KEY_LEFTALT, KeyCode::KEY_TAB],
        HostAction::Escape => &[KeyCode::KEY_ESC],
        HostAction::ShowDesktop => &[KeyCode::KEY_LEFTMETA, KeyCode::KEY_D],
        HostAction::BigPicture => &[],
    }
}

impl Keys {
    /// Creates the keyboard up front: the desktop needs a moment to pick up a
    /// new input device, and a key sent before that is lost.
    pub fn new() -> Self {
        let mut k = Self { dev: None };
        if let Err(e) = k.device() {
            eprintln!("shortcut keyboard: {e}");
        }
        k
    }

    fn device(&mut self) -> io::Result<&mut VirtualDevice> {
        if self.dev.is_none() {
            // every ordinary key, so plugins can press anything
            let mut keys = AttributeSet::<KeyCode>::new();
            for code in 1..=248u16 {
                keys.insert(KeyCode(code));
            }
            for a in HostAction::ALL {
                for k in combo(a) {
                    keys.insert(*k);
                }
            }
            let dev = VirtualDevice::builder()?
                .name("flipad shortcuts")
                .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x1209, 0xf1a0, 1))
                .with_keys(&keys)?
                .build()?;
            self.dev = Some(dev);
            // give the compositor a moment to pick up the new keyboard
            sleep(Duration::from_millis(300));
        }
        Ok(self.dev.as_mut().unwrap())
    }

    pub fn run(&mut self, job: KeyJob) {
        match job {
            KeyJob::Action(HostAction::BigPicture) => {
                let _ = std::process::Command::new("xdg-open").arg("steam://open/bigpicture").spawn();
            }
            KeyJob::Action(a) => self.press(combo(a)),
            KeyJob::Combo(keys) => self.press(&keys),
        }
    }

    fn press(&mut self, keys: &[KeyCode]) {
        let Ok(dev) = self.device() else { return };
        let ev = |k: &KeyCode, v| InputEvent::new(EventType::KEY.0, k.0, v);
        // press in order, release in reverse, as a person would
        for k in keys {
            let _ = dev.emit(&[ev(k, 1)]);
            sleep(Duration::from_millis(15));
        }
        sleep(Duration::from_millis(30));
        for k in keys.iter().rev() {
            let _ = dev.emit(&[ev(k, 0)]);
            sleep(Duration::from_millis(15));
        }
    }
}
