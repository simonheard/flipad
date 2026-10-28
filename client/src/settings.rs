//! User settings and recorded macros, kept in ~/.config/flipad as plain text.

use std::path::PathBuf;
use std::time::Duration;

use flipad_proto::Pad;

pub const MACRO_SLOTS: usize = 4;
/// Turbo bits for the analog triggers, next to the button bits.
pub const TURBO_LT: u32 = 1 << 20;
pub const TURBO_RT: u32 = 1 << 21;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Wifi,
    Bluetooth,
}

/// Where the handheld plays sound coming from the host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Output {
    /// The handheld's own default (headphones when plugged in, else speaker).
    #[default]
    Follow,
    Headphones,
    Speaker,
}

/// Bluetooth address as text ("AA:BB:CC:DD:EE:FF"), fixed size so Settings stays Copy.
pub type Addr = [u8; 17];

pub fn addr_str(a: &Addr) -> &str {
    std::str::from_utf8(a).unwrap_or("")
}

pub fn parse_addr(s: &str) -> Option<Addr> {
    let b = s.trim().as_bytes();
    (b.len() == 17).then(|| b.try_into().unwrap())
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub mode: Mode,
    /// Bluetooth host to connect to; None = whichever paired host comes first.
    pub bt_target: Option<Addr>,
    /// Swap A/B and X/Y (buttons labelled like a Nintendo pad).
    pub nintendo: bool,
    pub rumble: bool,
    /// Route the PC's audio to plugged-in headphones.
    pub audio: bool,
    /// Send the headset microphone to the PC.
    pub mic: bool,
    pub audio_latency_ms: u32,
    /// Button bits (plus TURBO_LT / TURBO_RT) that auto-repeat while held.
    pub turbo: u32,
    pub turbo_hz: u32,
    /// Touchpad pointer speed in percent.
    pub pointer_speed: u32,
    pub output: Output,
}

impl Default for Settings {
    fn default() -> Self {
        Self { mode: Mode::Wifi, bt_target: None, nintendo: false, rumble: true, audio: true, mic: true, audio_latency_ms: 30, turbo: 0, turbo_hz: 15, pointer_speed: 160, output: Output::Follow }
    }
}

fn dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/flipad")
}

impl Settings {
    pub fn load() -> Self {
        let mut s = Self::default();
        let text = std::fs::read_to_string(dir().join("settings")).unwrap_or_default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            let b = v == "true";
            let n = v.parse::<u32>().ok();
            match k {
                "mode" => s.mode = if v == "bluetooth" { Mode::Bluetooth } else { Mode::Wifi },
                "bt_target" => s.bt_target = parse_addr(v),
                "nintendo" => s.nintendo = b,
                "rumble" => s.rumble = b,
                "audio" => s.audio = b,
                "mic" => s.mic = b,
                "audio_latency_ms" => s.audio_latency_ms = n.unwrap_or(30).clamp(10, 200),
                "turbo" => s.turbo = n.unwrap_or(0),
                "turbo_hz" => s.turbo_hz = n.unwrap_or(15).clamp(2, 30),
                "output" => {
                    s.output = match v {
                        "headphones" => Output::Headphones,
                        "speaker" => Output::Speaker,
                        _ => Output::Follow,
                    }
                }
                "pointer_speed" => s.pointer_speed = n.unwrap_or(160).clamp(20, 500),
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        let text = format!(
            "mode={}\nbt_target={}\nnintendo={}\nrumble={}\naudio={}\nmic={}\naudio_latency_ms={}\nturbo={}\nturbo_hz={}\npointer_speed={}\noutput={}\n",
            if self.mode == Mode::Bluetooth { "bluetooth" } else { "wifi" },
            self.bt_target.as_ref().map_or("", addr_str),
            self.nintendo,
            self.rumble,
            self.audio,
            self.mic,
            self.audio_latency_ms,
            self.turbo,
            self.turbo_hz,
            self.pointer_speed,
            match self.output {
                Output::Follow => "follow",
                Output::Headphones => "headphones",
                Output::Speaker => "speaker",
            }
        );
        let _ = std::fs::create_dir_all(dir());
        let _ = std::fs::write(dir().join("settings"), text);
    }
}

/// A recorded input sequence: (offset from start, pad state from then on).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Macro {
    pub steps: Vec<(Duration, Pad)>,
}

impl Macro {
    pub fn length(&self) -> Duration {
        self.steps.last().map_or(Duration::ZERO, |s| s.0)
    }
    /// The state at `t`, or None once the macro has finished.
    pub fn at(&self, t: Duration) -> Option<Pad> {
        if t > self.length() {
            return None;
        }
        self.steps.iter().take_while(|s| s.0 <= t).last().map(|s| s.1)
    }
    /// When the state next changes after `t`.
    pub fn next_change(&self, t: Duration) -> Option<Duration> {
        self.steps.iter().map(|s| s.0).find(|&o| o > t)
    }
}

pub fn load_macros() -> [Macro; MACRO_SLOTS] {
    let mut m: [Macro; MACRO_SLOTS] = Default::default();
    let text = std::fs::read_to_string(dir().join("macros")).unwrap_or_default();
    for line in text.lines() {
        let v: Vec<i64> = line.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        if v.len() != 9 || !(0..MACRO_SLOTS as i64).contains(&v[0]) {
            continue;
        }
        let pad = Pad {
            buttons: v[2] as u32,
            lx: v[3] as i16,
            ly: v[4] as i16,
            rx: v[5] as i16,
            ry: v[6] as i16,
            lt: v[7] as u8,
            rt: v[8] as u8,
        };
        m[v[0] as usize].steps.push((Duration::from_millis(v[1] as u64), pad));
    }
    m
}

pub fn save_macros(m: &[Macro; MACRO_SLOTS]) {
    let mut text = String::from("# slot ms buttons lx ly rx ry lt rt\n");
    for (slot, mac) in m.iter().enumerate() {
        for (t, p) in &mac.steps {
            text += &format!(
                "{slot} {} {} {} {} {} {} {} {}\n",
                t.as_millis(),
                p.buttons,
                p.lx,
                p.ly,
                p.rx,
                p.ry,
                p.lt,
                p.rt
            );
        }
    }
    let _ = std::fs::create_dir_all(dir());
    let _ = std::fs::write(dir().join("macros"), text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addr_roundtrip() {
        let a = parse_addr("12:34:56:78:9A:BC").unwrap();
        assert_eq!(addr_str(&a), "12:34:56:78:9A:BC");
        assert!(parse_addr("nope").is_none());
    }

    #[test]
    fn macro_lookup() {
        let a = Pad { buttons: 1, ..Pad::default() };
        let m = Macro { steps: vec![(Duration::ZERO, a), (Duration::from_millis(100), Pad::default())] };
        assert_eq!(m.at(Duration::from_millis(50)), Some(a));
        assert_eq!(m.at(Duration::from_millis(100)), Some(Pad::default()));
        assert_eq!(m.at(Duration::from_millis(101)), None);
        assert_eq!(m.next_change(Duration::from_millis(10)), Some(Duration::from_millis(100)));
    }
}
