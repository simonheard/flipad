//! Turns the physical controller state into what the host receives:
//! Home chords, Nintendo/Xbox face-button swap, turbo, and macros.
//!
//! Home is held back for a moment when pressed: tapped alone it reaches the
//! host as a short press, held alone it is passed through, and pressed
//! together with another button it becomes a shortcut that the host never
//! sees.

use std::time::{Duration, Instant};

use flipad_proto::{HostAction, Pad, button};

use crate::settings::{MACRO_SLOTS, Macro, Settings, TURBO_LT, TURBO_RT};

/// Holding Home alone this long passes it through as held.
const HOME_HOLD: Duration = Duration::from_millis(350);
/// A lone Home tap is sent as a press this long.
const HOME_TAP: Duration = Duration::from_millis(80);
/// Hold Home + Select this long to quit.
pub const EXIT_HOLD: Duration = Duration::from_millis(1500);
const MAX_RECORDING: Duration = Duration::from_secs(120);
/// Analog trigger depth that counts as "held" for turbo.
const TRIGGER_HELD: u8 = 30;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    Action(HostAction),
    PlayMacro(usize),
    ToggleScreen,
    Quit,
}

/// Home + this physical button → event.
fn chord(bit: u32) -> Option<Event> {
    Some(match bit {
        button::SOUTH => Event::PlayMacro(0),
        button::EAST => Event::PlayMacro(1),
        button::WEST => Event::PlayMacro(2),
        button::NORTH => Event::PlayMacro(3),
        button::TL => Event::Action(HostAction::VolumeDown),
        button::TR => Event::Action(HostAction::VolumeUp),
        button::START => Event::Action(HostAction::SteamScreenshot),
        button::DPAD_DOWN => Event::ToggleScreen,
        _ => return None,
    })
}

struct Home {
    since: Instant,
    chorded: bool,
    forwarded: bool,
}

struct Recording {
    slot: usize,
    start: Instant,
    steps: Vec<(Duration, Pad)>,
}

struct Playing {
    slot: usize,
    start: Instant,
}

pub struct Shaper {
    prev: Pad,
    home: Option<Home>,
    tap_until: Option<Instant>,
    /// Buttons used in a chord, hidden until physically released.
    swallowed: u32,
    exit_since: Option<Instant>,
    epoch: Instant,
    pub macros: [Macro; MACRO_SLOTS],
    recording: Option<Recording>,
    playing: Option<Playing>,
}

fn swap_bits(b: u32, x: u32, y: u32) -> u32 {
    let (hx, hy) = (b & x != 0, b & y != 0);
    let mut out = b & !(x | y);
    if hx {
        out |= y;
    }
    if hy {
        out |= x;
    }
    out
}

impl Shaper {
    pub fn new(macros: [Macro; MACRO_SLOTS], now: Instant) -> Self {
        Self {
            prev: Pad::default(),
            home: None,
            tap_until: None,
            swallowed: 0,
            exit_since: None,
            epoch: now,
            macros,
            recording: None,
            playing: None,
        }
    }

    pub fn recording(&self) -> Option<usize> {
        self.recording.as_ref().map(|r| r.slot)
    }

    pub fn playing(&self) -> Option<usize> {
        self.playing.as_ref().map(|p| p.slot)
    }

    pub fn exit_progress(&self, now: Instant) -> f32 {
        self.exit_since.map_or(0.0, |s| (now.duration_since(s).as_secs_f32() / EXIT_HOLD.as_secs_f32()).min(1.0))
    }

    pub fn start_recording(&mut self, slot: usize, now: Instant) {
        self.playing = None;
        self.recording = Some(Recording { slot, start: now, steps: Vec::new() });
    }

    /// Ends the recording; returns the slot whose macro changed.
    pub fn stop_recording(&mut self, now: Instant) -> Option<usize> {
        let mut r = self.recording.take()?;
        // always end released, so playback never leaves anything held
        r.steps.push((now.duration_since(r.start), Pad::default()));
        // drop leading idle time
        let first_input = r.steps.iter().find(|s| s.1 != Pad::default()).map(|s| s.0);
        let shift = first_input.unwrap_or(Duration::ZERO);
        let steps: Vec<_> = r.steps.into_iter().filter(|s| s.0 >= shift).map(|(t, p)| (t - shift, p)).collect();
        self.macros[r.slot] = Macro { steps: if first_input.is_some() { steps } else { Vec::new() } };
        Some(r.slot)
    }

    pub fn play(&mut self, slot: usize, now: Instant) {
        if self.recording.is_none() && !self.macros[slot].steps.is_empty() {
            self.playing = Some(Playing { slot, start: now });
        }
    }

    pub fn clear(&mut self, slot: usize) {
        if self.playing.as_ref().is_some_and(|p| p.slot == slot) {
            self.playing = None;
        }
        self.macros[slot] = Macro::default();
    }

    /// Computes the state to send. `events` receives shortcuts to run.
    pub fn update(&mut self, phys: Pad, s: &Settings, now: Instant, events: &mut Vec<Event>) -> Pad {
        let pressed = phys.buttons & !self.prev.buttons;
        self.swallowed &= phys.buttons;
        let home_held = phys.buttons & button::MODE != 0;

        if pressed & button::MODE != 0 {
            self.home = Some(Home { since: now, chorded: false, forwarded: false });
        }
        if let Some(h) = &mut self.home {
            if home_held {
                if !h.forwarded {
                    for bit in (0..32).map(|i| 1u32 << i).filter(|b| pressed & b != 0) {
                        if let Some(ev) = chord(bit) {
                            h.chorded = true;
                            self.swallowed |= bit;
                            events.push(ev);
                        }
                    }
                    if phys.buttons & button::SELECT != 0 {
                        h.chorded = true;
                        self.swallowed |= button::SELECT;
                        let since = *self.exit_since.get_or_insert(now);
                        if now.duration_since(since) >= EXIT_HOLD {
                            events.push(Event::Quit);
                        }
                    } else {
                        self.exit_since = None;
                    }
                }
                if !h.chorded && !h.forwarded && now.duration_since(h.since) >= HOME_HOLD {
                    h.forwarded = true;
                }
            } else {
                if !h.chorded && !h.forwarded {
                    self.tap_until = Some(now + HOME_TAP);
                }
                self.home = None;
                self.exit_since = None;
            }
        }
        if self.tap_until.is_some_and(|t| now >= t) {
            self.tap_until = None;
        }

        let mut out = phys;
        out.buttons &= !(self.swallowed | button::MODE);
        if self.home.as_ref().is_some_and(|h| h.forwarded) || self.tap_until.is_some() {
            out.buttons |= button::MODE;
        }
        if s.nintendo {
            out.buttons = swap_bits(out.buttons, button::SOUTH, button::EAST);
            out.buttons = swap_bits(out.buttons, button::WEST, button::NORTH);
        }

        // turbo: held buttons blink at turbo_hz
        if s.turbo != 0 {
            let half = 500 / s.turbo_hz.max(1) as u128;
            let on = (now.duration_since(self.epoch).as_millis() / half) % 2 == 0;
            if !on {
                out.buttons &= !(s.turbo & 0xFFFF);
                if s.turbo & TURBO_LT != 0 && out.lt >= TRIGGER_HELD {
                    out.lt = 0;
                }
                if s.turbo & TURBO_RT != 0 && out.rt >= TRIGGER_HELD {
                    out.rt = 0;
                }
            }
        }

        if let Some(r) = &mut self.recording {
            let t = now.duration_since(r.start);
            if r.steps.last().is_none_or(|l| l.1 != out) {
                r.steps.push((t, out));
            }
            if t >= MAX_RECORDING {
                self.stop_recording(now);
            }
        }

        if let Some(p) = &self.playing {
            match self.macros[p.slot].at(now.duration_since(p.start)) {
                // the macro drives everything except Home, which stays usable
                Some(m) => out = Pad { buttons: (m.buttons & !button::MODE) | (out.buttons & button::MODE), ..m },
                None => self.playing = None,
            }
        }

        self.prev = phys;
        out
    }

    /// The next moment `update` must run even without new input.
    pub fn deadline(&self, s: &Settings, now: Instant) -> Option<Instant> {
        let mut d: Vec<Instant> = Vec::new();
        if let Some(h) = &self.home
            && !h.chorded
            && !h.forwarded
        {
            d.push(h.since + HOME_HOLD);
        }
        if self.exit_since.is_some() {
            d.push(now + Duration::from_millis(30));
        }
        if let Some(t) = self.tap_until {
            d.push(t);
        }
        let turbo_held = s.turbo & 0xFFFF & self.prev.buttons != 0
            || (s.turbo & TURBO_LT != 0 && self.prev.lt >= TRIGGER_HELD)
            || (s.turbo & TURBO_RT != 0 && self.prev.rt >= TRIGGER_HELD);
        if turbo_held {
            let half = 500 / s.turbo_hz.max(1) as u64;
            let el = now.duration_since(self.epoch).as_millis() as u64;
            d.push(now + Duration::from_millis(half - el % half));
        }
        if let Some(p) = &self.playing {
            let t = now.duration_since(p.start);
            let next = self.macros[p.slot].next_change(t).unwrap_or(self.macros[p.slot].length() + Duration::from_millis(1));
            d.push(p.start + next);
        }
        d.into_iter().min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad(buttons: u32) -> Pad {
        Pad { buttons, ..Pad::default() }
    }
    fn ms(t: Instant, m: u64) -> Instant {
        t + Duration::from_millis(m)
    }

    #[test]
    fn home_tap_becomes_short_press() {
        let (t, s) = (Instant::now(), Settings::default());
        let mut sh = Shaper::new(Default::default(), t);
        let mut ev = vec![];
        assert_eq!(sh.update(pad(button::MODE), &s, t, &mut ev).buttons, 0, "held back at first");
        assert_eq!(sh.update(pad(0), &s, ms(t, 50), &mut ev).buttons, button::MODE, "tap goes out on release");
        assert_eq!(sh.update(pad(0), &s, ms(t, 200), &mut ev).buttons, 0);
        assert!(ev.is_empty());
    }

    #[test]
    fn home_held_passes_through() {
        let (t, s) = (Instant::now(), Settings::default());
        let mut sh = Shaper::new(Default::default(), t);
        let mut ev = vec![];
        sh.update(pad(button::MODE), &s, t, &mut ev);
        assert_eq!(sh.update(pad(button::MODE), &s, ms(t, 400), &mut ev).buttons, button::MODE);
        assert_eq!(sh.update(pad(0), &s, ms(t, 500), &mut ev).buttons, 0);
    }

    #[test]
    fn home_chord_runs_action_and_hides_both_buttons() {
        let (t, s) = (Instant::now(), Settings::default());
        let mut sh = Shaper::new(Default::default(), t);
        let mut ev = vec![];
        sh.update(pad(button::MODE), &s, t, &mut ev);
        let out = sh.update(pad(button::MODE | button::TR), &s, ms(t, 100), &mut ev);
        assert_eq!(out.buttons, 0);
        assert_eq!(ev, vec![Event::Action(HostAction::VolumeUp)]);
        // Home released first: RB stays hidden until it is released too
        assert_eq!(sh.update(pad(button::TR), &s, ms(t, 200), &mut ev).buttons, 0);
        assert_eq!(sh.update(pad(0), &s, ms(t, 300), &mut ev).buttons, 0, "no stray Home tap after a chord");
        assert_eq!(sh.update(pad(button::TR), &s, ms(t, 400), &mut ev).buttons, button::TR);
    }

    #[test]
    fn home_select_hold_quits() {
        let (t, s) = (Instant::now(), Settings::default());
        let mut sh = Shaper::new(Default::default(), t);
        let mut ev = vec![];
        sh.update(pad(button::MODE), &s, t, &mut ev);
        sh.update(pad(button::MODE | button::SELECT), &s, ms(t, 10), &mut ev);
        assert!(ev.is_empty());
        assert!(sh.exit_progress(ms(t, 760)) > 0.4);
        sh.update(pad(button::MODE | button::SELECT), &s, ms(t, 1600), &mut ev);
        assert_eq!(ev, vec![Event::Quit]);
    }

    #[test]
    fn nintendo_layout_swaps_face_buttons() {
        let t = Instant::now();
        let s = Settings { nintendo: true, ..Settings::default() };
        let mut sh = Shaper::new(Default::default(), t);
        let out = sh.update(pad(button::SOUTH | button::NORTH | button::TL), &s, t, &mut vec![]);
        assert_eq!(out.buttons, button::EAST | button::WEST | button::TL);
    }

    #[test]
    fn turbo_blinks_held_button() {
        let t = Instant::now();
        let s = Settings { turbo: button::SOUTH, turbo_hz: 10, ..Settings::default() };
        let mut sh = Shaper::new(Default::default(), t);
        let mut ev = vec![];
        let a = pad(button::SOUTH | button::EAST);
        assert_eq!(sh.update(a, &s, t, &mut ev).buttons, button::SOUTH | button::EAST);
        assert_eq!(sh.update(a, &s, ms(t, 60), &mut ev).buttons, button::EAST, "off half: only A blinks");
        assert_eq!(sh.update(a, &s, ms(t, 110), &mut ev).buttons, button::SOUTH | button::EAST);
        assert_eq!(sh.deadline(&s, ms(t, 110)), Some(ms(t, 150)));
    }

    #[test]
    fn record_then_play() {
        let (t, s) = (Instant::now(), Settings::default());
        let mut sh = Shaper::new(Default::default(), t);
        let mut ev = vec![];
        sh.start_recording(2, t);
        sh.update(pad(0), &s, ms(t, 500), &mut ev); // idle before input is trimmed
        sh.update(pad(button::SOUTH), &s, ms(t, 1000), &mut ev);
        sh.update(pad(0), &s, ms(t, 1100), &mut ev);
        sh.update(pad(button::EAST), &s, ms(t, 1300), &mut ev);
        assert_eq!(sh.stop_recording(ms(t, 1400)), Some(2));
        let m = &sh.macros[2];
        assert_eq!(m.steps.first().unwrap(), &(Duration::ZERO, pad(button::SOUTH)));
        assert_eq!(m.length(), Duration::from_millis(400));

        let p0 = ms(t, 5000);
        sh.play(2, p0);
        assert_eq!(sh.update(pad(0), &s, p0, &mut ev).buttons, button::SOUTH);
        assert_eq!(sh.update(pad(0), &s, ms(p0, 150), &mut ev).buttons, 0);
        assert_eq!(sh.update(pad(0), &s, ms(p0, 350), &mut ev).buttons, button::EAST);
        assert_eq!(sh.update(pad(0), &s, ms(p0, 400), &mut ev).buttons, 0);
        assert_eq!(sh.playing(), Some(2));
        sh.update(pad(button::WEST), &s, ms(p0, 450), &mut ev);
        assert_eq!(sh.playing(), None, "finished");
        assert_eq!(sh.update(pad(button::WEST), &s, ms(p0, 460), &mut ev).buttons, button::WEST);
    }

    #[test]
    fn chord_plays_macro() {
        let (t, s) = (Instant::now(), Settings::default());
        let mut sh = Shaper::new(Default::default(), t);
        let mut ev = vec![];
        sh.update(pad(button::MODE), &s, t, &mut ev);
        sh.update(pad(button::MODE | button::EAST), &s, ms(t, 50), &mut ev);
        assert_eq!(ev, vec![Event::PlayMacro(1)]);
    }
}
