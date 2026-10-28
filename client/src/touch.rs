//! Reads the touchscreen directly for the touchpad tab.
//!
//! gamescope turns touches into an emulated mouse for the app (one pointer,
//! jumping to wherever a finger lands), which cannot express two fingers or
//! relative motion. The panel's evdev node is readable by the user and not
//! grabbed, so we read multi-touch contacts here. Their orientation relative
//! to the screen is learned from the emulated presses gamescope also sends.

use std::sync::{Arc, Mutex};

use evdev::raw_stream::RawDevice;
use evdev::{AbsoluteAxisCode as Abs, EventType, PropType};

/// One finger, in raw panel units normalised to 0..1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Contact {
    pub id: i32,
    pub u: f32,
    pub v: f32,
}

/// Contacts after each sync, oldest first; drained by the UI.
pub type Frames = Arc<Mutex<Vec<Vec<Contact>>>>;

/// Ways the panel can be mounted relative to the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    Normal,
    Left,
    UpsideDown,
    Right,
}

impl Orientation {
    pub const ALL: [Orientation; 4] = [Orientation::Normal, Orientation::Left, Orientation::UpsideDown, Orientation::Right];

    /// Raw 0..1 → screen 0..1.
    pub fn map(self, u: f32, v: f32) -> (f32, f32) {
        match self {
            Orientation::Normal => (u, v),
            Orientation::Left => (v, 1.0 - u),
            Orientation::UpsideDown => (1.0 - u, 1.0 - v),
            Orientation::Right => (1.0 - v, u),
        }
    }

    /// The orientation that puts raw point (u, v) closest to screen point (x, y).
    pub fn best(u: f32, v: f32, x: f32, y: f32) -> (Orientation, f32) {
        Self::ALL
            .into_iter()
            .map(|o| {
                let (mx, my) = o.map(u, v);
                (o, (mx - x).hypot(my - y))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap()
    }
}

fn find_touchscreen() -> Option<RawDevice> {
    for e in std::fs::read_dir("/dev/input").ok()?.flatten() {
        let path = e.path();
        if !path.file_name()?.to_string_lossy().starts_with("event") {
            continue;
        }
        let Ok(dev) = RawDevice::open(&path) else { continue };
        let direct = dev.properties().contains(PropType::DIRECT);
        let mt = dev.supported_absolute_axes().is_some_and(|a| a.contains(Abs::ABS_MT_POSITION_X));
        // testing aid: pick a specific (e.g. virtual) touchscreen by name
        let wanted = std::env::var("FLIPAD_TOUCH_NAME").ok();
        let name_ok = wanted.as_deref().is_none_or(|w| dev.name().is_some_and(|n| n.contains(w)));
        if direct && mt && name_ok {
            eprintln!("touch: reading {} ({:?})", path.display(), dev.name().unwrap_or(""));
            return Some(dev);
        }
    }
    None
}

fn ranges(dev: &RawDevice) -> Option<((f32, f32), (f32, f32))> {
    let abs: Vec<_> = dev.get_absinfo().ok()?.collect();
    let range = |axis: Abs| {
        abs.iter().find(|(a, _)| *a == axis).map(|(_, i)| (i.minimum() as f32, i.maximum() as f32)).unwrap_or((0.0, 1.0))
    };
    Some((range(Abs::ABS_MT_POSITION_X), range(Abs::ABS_MT_POSITION_Y)))
}

/// Starts the reader; returns None if there is no readable touchscreen.
/// If the device goes away (e.g. across suspend) it is looked up again.
pub fn start(notify: impl Fn() + Send + 'static) -> Option<Frames> {
    let first = find_touchscreen()?;
    let frames: Frames = Arc::default();
    let out = frames.clone();
    std::thread::Builder::new()
        .name("touch".into())
        .spawn(move || {
            let mut next = Some(first);
            loop {
                let Some(mut dev) = next.take().or_else(find_touchscreen) else {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                };
                let Some(((x0, x1), (y0, y1))) = ranges(&dev) else { continue };
                // multi-touch protocol B: per-slot tracking id and position
                let mut slots = vec![(-1i32, 0f32, 0f32); 10];
                let mut slot = 0usize;
                while let Ok(events) = dev.fetch_events() {
                    for ev in events {
                        match ev.event_type() {
                            EventType::ABSOLUTE => {
                                let v = ev.value();
                                match Abs(ev.code()) {
                                    Abs::ABS_MT_SLOT => slot = (v.max(0) as usize).min(slots.len() - 1),
                                    Abs::ABS_MT_TRACKING_ID => slots[slot].0 = v,
                                    Abs::ABS_MT_POSITION_X => slots[slot].1 = (v as f32 - x0) / (x1 - x0).max(1.0),
                                    Abs::ABS_MT_POSITION_Y => slots[slot].2 = (v as f32 - y0) / (y1 - y0).max(1.0),
                                    _ => {}
                                }
                            }
                            EventType::SYNCHRONIZATION => {
                                let contacts: Vec<Contact> =
                                    slots.iter().filter(|s| s.0 >= 0).map(|&(id, u, v)| Contact { id, u, v }).collect();
                                let mut f = out.lock().unwrap();
                                f.push(contacts);
                                if f.len() > 256 {
                                    f.drain(..128); // nobody is draining
                                }
                                drop(f);
                                notify();
                            }
                            _ => {}
                        }
                    }
                }
                eprintln!("touch: device went away, looking for it again");
                // all fingers are gone as far as anyone can tell
                out.lock().unwrap().push(Vec::new());
                notify();
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        })
        .ok()?;
    Some(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_orientation_from_one_sample() {
        // portrait panel mounted to the left: raw (0.1, 0.9) shows up at screen (0.9, 0.9)
        let (o, d) = Orientation::best(0.1, 0.9, 0.9, 0.9);
        assert_eq!(o, Orientation::Left);
        assert!(d < 1e-6);
        assert_eq!(Orientation::best(0.3, 0.2, 0.3, 0.2).0, Orientation::Normal);
    }
}
