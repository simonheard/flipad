//! The touch panel: draws each tab and records where its buttons are.

use std::time::{Duration, Instant};

use flipad_proto::{HostAction, Pad, button};

use crate::link::{Phase, View};
use crate::paint::{Align, Canvas, Text, circle, rgba, rrect};
use crate::settings::{MACRO_SLOTS, Settings, TURBO_LT, TURBO_RT};

pub const W: f32 = 1920.0;

const BG: u32 = 0x0d0f13;
const CARD: u32 = 0x161920;
const RAISED: u32 = 0x222733;
const TEXT: u32 = 0xe9ebf1;
const DIM: u32 = 0x8a90a0;
const FAINT: u32 = 0x3a3f4b;
const ACCENT: u32 = 0x6ea8fe;
const GOOD: u32 = 0x4ade80;
const WARN: u32 = 0xf5b041;
const BAD: u32 = 0xf87171;

/// How long a tapped button stays lit.
pub const FLASH: Duration = Duration::from_millis(180);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Overview,
    Quick,
    Macros,
    Settings,
}

const TABS: [(Tab, &str); 4] = [(Tab::Overview, "概览"), (Tab::Quick, "快捷"), (Tab::Macros, "宏 / 连发"), (Tab::Settings, "设置")];

/// Something on screen that reacts to a touch.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    Tab(Tab),
    Exit,
    ScreenOff,
    Wake,
    Action(HostAction),
    Turbo(u32),
    TurboHz(u32),
    Record(usize),
    StopRecording,
    Play(usize),
    Clear(usize),
    Nintendo(bool),
    Rumble,
    Audio,
    Mic,
    Latency(u32),
}

pub struct Ui<'a> {
    pub cv: &'a mut Canvas,
    pub text: &'a mut Text,
    pub hits: &'a mut Vec<(Rect, Hit)>,
    /// The last tapped button and when, for feedback.
    pub flash: Option<(Hit, Instant)>,
    pub now: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }
}

impl Ui<'_> {
    fn flashing(&self, hit: Hit) -> bool {
        self.flash.is_some_and(|(h, t)| h == hit && self.now.duration_since(t) < FLASH)
    }

    fn text(&mut self, s: &str, x: f32, y: f32, size: f32, color: u32, bold: bool, align: Align) {
        self.text.draw(self.cv, s, x, y, size, rgba(color, 1.0), bold, align);
    }

    fn card(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.cv.fill(&rrect(x, y, w, h, 28.0), rgba(CARD, 1.0));
        self.cv.stroke(&rrect(x + 0.5, y + 0.5, w - 1.0, h - 1.0, 28.0), rgba(0xffffff, 0.06), 1.0);
    }

    /// A tappable rounded button. `on` = selected/enabled look.
    #[allow(clippy::too_many_arguments)]
    fn button(&mut self, hit: Hit, x: f32, y: f32, w: f32, h: f32, label: &str, size: f32, on: bool, color: u32) {
        self.hits.push((Rect { x, y, w, h }, hit));
        let flash = self.flashing(hit);
        let (bg, a) = if flash {
            (color, 1.0)
        } else if on {
            (color, 0.9)
        } else {
            (RAISED, 1.0)
        };
        self.cv.fill(&rrect(x, y, w, h, h.min(36.0)), rgba(bg, a));
        let fg = if on || flash { 0x0d0f13 } else { TEXT };
        self.text(label, x + w / 2.0, y + h / 2.0 + size * 0.36, size, fg, true, Align::Center);
    }

    /// An on/off switch with a label to its left.
    fn switch(&mut self, hit: Hit, x: f32, y: f32, label: &str, hint: &str, on: bool) {
        self.text(label, x, y + 40.0, 34.0, TEXT, true, Align::Left);
        if !hint.is_empty() {
            self.text(hint, x, y + 82.0, 24.0, DIM, false, Align::Left);
        }
        let (sx, sw, sh) = (x + 640.0, 120.0, 64.0);
        self.hits.push((Rect { x, y, w: sw + 640.0, h: 100.0 }, hit));
        self.cv.fill(&rrect(sx, y + 8.0, sw, sh, sh / 2.0), rgba(if on { ACCENT } else { FAINT }, 1.0));
        let kx = if on { sx + sw - sh / 2.0 } else { sx + sh / 2.0 };
        self.cv.fill(&circle(kx, y + 8.0 + sh / 2.0, sh / 2.0 - 7.0), rgba(0xffffff, 1.0));
    }

    fn bar(&mut self, x: f32, y: f32, w: f32, h: f32, frac: f32, color: u32) {
        self.cv.fill(&rrect(x, y, w, h, h / 2.0), rgba(FAINT, 0.6));
        let f = frac.clamp(0.0, 1.0);
        if f > 0.0 {
            self.cv.fill(&rrect(x, y, (w * f).max(h), h, h / 2.0), rgba(color, 1.0));
        }
    }
}

pub fn action_label(a: HostAction) -> &'static str {
    match a {
        HostAction::VolumeDown => "音量 −",
        HostAction::VolumeUp => "音量 ＋",
        HostAction::Mute => "静音",
        HostAction::PlayPause => "播放 / 暂停",
        HostAction::Previous => "上一首",
        HostAction::Next => "下一首",
        HostAction::SteamScreenshot => "Steam 截图",
        HostAction::Screenshot => "系统截图",
        HostAction::AltTab => "Alt + Tab",
        HostAction::Escape => "Esc",
        HostAction::ShowDesktop => "显示桌面",
        HostAction::BigPicture => "大屏幕模式",
    }
}

fn action_hint(a: HostAction) -> &'static str {
    match a {
        HostAction::VolumeDown => "Home + LB",
        HostAction::VolumeUp => "Home + RB",
        HostAction::SteamScreenshot => "Home + Start · 游戏内 F12",
        HostAction::Screenshot => "Print Screen",
        HostAction::ShowDesktop => "Meta + D",
        HostAction::BigPicture => "打开 Steam 大屏幕",
        HostAction::AltTab => "切换到上一个窗口",
        _ => "",
    }
}

pub fn render(ui: &mut Ui, v: &View, s: &Settings, tab: Tab, battery: (Option<u8>, bool)) {
    ui.hits.clear();
    if v.screen_off {
        // AMOLED: black pixels are off. Touch anywhere to wake.
        ui.cv.pm.fill(rgba(0x000000, 1.0));
        ui.hits.push((Rect { x: 0.0, y: 0.0, w: W, h: 1080.0 }, Hit::Wake));
        let dot = if v.phase == Phase::Connected { 0x1d4a2e } else { 0x4a3a1d };
        ui.cv.fill(&circle(W / 2.0 - 70.0, 1032.0, 6.0), rgba(dot, 1.0));
        ui.text("触摸唤醒", W / 2.0 - 52.0, 1041.0, 24.0, 0x2a2d33, false, Align::Left);
        return;
    }
    ui.cv.pm.fill(rgba(BG, 1.0));
    header(ui, v, tab);
    match tab {
        Tab::Overview => {
            controller(ui, &v.pad, v.rumble);
            host_stats(ui, v);
        }
        Tab::Quick => quick(ui),
        Tab::Macros => macros(ui, v, s),
        Tab::Settings => settings(ui, s),
    }
    footer(ui, v, battery);
}

fn header(ui: &mut Ui, v: &View, tab: Tab) {
    ui.text("Flip Pad", 64.0, 112.0, 52.0, TEXT, true, Align::Left);

    // tabs
    let (tx, tw, th) = (360.0, 200.0, 76.0);
    ui.cv.fill(&rrect(tx - 8.0, 52.0, tw * 4.0 + 16.0, th + 16.0, 46.0), rgba(CARD, 1.0));
    for (i, (t, label)) in TABS.into_iter().enumerate() {
        let x = tx + i as f32 * tw;
        let hit = Hit::Tab(t);
        ui.hits.push((Rect { x, y: 60.0, w: tw, h: th }, hit));
        if t == tab {
            ui.cv.fill(&rrect(x, 60.0, tw, th, 38.0), rgba(RAISED, 1.0));
        }
        let c = if t == tab { TEXT } else { DIM };
        ui.text(label, x + tw / 2.0, 110.0, 30.0, c, t == tab, Align::Center);
    }

    let (dot, label) = match &v.phase {
        Phase::Searching => (WARN, "正在寻找主机…".to_string()),
        Phase::Connecting => (WARN, "正在连接…".to_string()),
        Phase::Connected => (GOOD, v.host.clone()),
        Phase::Lost => (WARN, "重连中…".to_string()),
        Phase::Rejected(r) => (BAD, r.clone()),
    };
    let latency = v.rtt_ms.filter(|_| v.phase == Phase::Connected).map(|r| format!("{r:.1} ms"));
    let lw = ui.text.measure(&label, 28.0, false).min(560.0);
    let latw = latency.as_ref().map_or(0.0, |l| ui.text.measure(l, 28.0, true) + 32.0);
    let pw = lw + latw + 84.0;
    let px = W - 64.0 - pw;
    ui.cv.fill(&rrect(px, 52.0, pw, 92.0, 46.0), rgba(CARD, 1.0));
    ui.cv.fill(&circle(px + 38.0, 98.0, 9.0), rgba(dot, 1.0));
    ui.text(&label, px + 62.0, 108.0, 28.0, TEXT, false, Align::Left);
    if let Some(l) = latency {
        ui.text(&l, W - 64.0 - 30.0, 108.0, 28.0, dot, true, Align::Right);
    }
}

fn footer(ui: &mut Ui, v: &View, battery: (Option<u8>, bool)) {
    let mut fx = 64.0;
    if let (Some(b), charging) = battery {
        let s = format!("掌机电量 {b}%{}", if charging { " · 充电中" } else { "" });
        ui.text(&s, fx, 992.0, 30.0, if b <= 15 { BAD } else { DIM }, false, Align::Left);
        fx += ui.text.measure(&s, 30.0, false) + 48.0;
    }
    let (audio, color) = match (v.jack.headphones, v.jack.mic, v.audio_live) {
        (false, _, _) => ("未插耳机".to_string(), DIM),
        (true, mic, true) => (format!("耳机已接通主机声音{}", if mic { " · 麦克风" } else { "" }), GOOD),
        (true, _, false) => ("耳机已插入".to_string(), WARN),
    };
    ui.cv.fill(&circle(fx + 8.0, 981.0, 7.0), rgba(color, 1.0));
    ui.text(&audio, fx + 28.0, 992.0, 30.0, if color == DIM { DIM } else { TEXT }, false, Align::Left);

    if let Some((a, t)) = v.last_action
        && ui.now.duration_since(t) < Duration::from_millis(1500)
    {
        ui.text(&format!("已发送：{}", action_label(a)), 64.0, 1040.0, 26.0, ACCENT, true, Align::Left);
    } else {
        let hint = "Home + LB/RB 音量 · Home + Start 截图 · Home + ↓ 熄屏 · 长按 Home + Select 退出";
        ui.text(hint, 64.0, 1040.0, 24.0, DIM, false, Align::Left);
    }
    if v.exit_hold > 0.0 {
        ui.bar(64.0, 1056.0, 560.0, 8.0, v.exit_hold, BAD);
    }

    ui.button(Hit::ScreenOff, 1300.0, 940.0, 240.0, 92.0, "熄屏", 32.0, false, ACCENT);
    let r = Rect { x: 1572.0, y: 940.0, w: 284.0, h: 92.0 };
    ui.hits.push((r, Hit::Exit));
    ui.cv.fill(&rrect(r.x, r.y, r.w, r.h, 46.0), rgba(BAD, if ui.flashing(Hit::Exit) { 0.5 } else { 0.14 }));
    ui.cv.stroke(&rrect(r.x, r.y, r.w, r.h, 46.0), rgba(BAD, 0.6), 2.0);
    ui.text("退出", r.x + r.w / 2.0, r.y + 58.0, 34.0, BAD, true, Align::Center);
}

fn quick(ui: &mut Ui) {
    let (x0, y0, gap) = (64.0, 176.0, 24.0);
    let tw = (W - 128.0 - 3.0 * gap) / 4.0;
    let th = 230.0;
    for (i, a) in HostAction::ALL.into_iter().enumerate() {
        let (col, row) = ((i % 4) as f32, (i / 4) as f32);
        let (x, y) = (x0 + col * (tw + gap), y0 + row * (th + gap));
        let hit = Hit::Action(a);
        ui.hits.push((Rect { x, y, w: tw, h: th }, hit));
        let lit = ui.flashing(hit);
        ui.cv.fill(&rrect(x, y, tw, th, 28.0), rgba(if lit { ACCENT } else { CARD }, 1.0));
        ui.cv.stroke(&rrect(x + 0.5, y + 0.5, tw - 1.0, th - 1.0, 28.0), rgba(0xffffff, 0.06), 1.0);
        let fg = if lit { 0x0d0f13 } else { TEXT };
        ui.text(action_label(a), x + tw / 2.0, y + th / 2.0 + 10.0, 44.0, fg, true, Align::Center);
        let hint = action_hint(a);
        if !hint.is_empty() {
            ui.text(hint, x + tw / 2.0, y + th - 34.0, 22.0, if lit { 0x0d0f13 } else { DIM }, false, Align::Center);
        }
    }
}

fn macros(ui: &mut Ui, v: &View, s: &Settings) {
    // turbo
    let (x, y, w, h) = (64.0, 176.0, 700.0, 724.0);
    ui.card(x, y, w, h);
    ui.text("连发", x + 40.0, y + 66.0, 36.0, TEXT, true, Align::Left);
    ui.text("开启后，按住即自动连按", x + 40.0, y + 110.0, 24.0, DIM, false, Align::Left);
    let chips = [
        ("A", button::SOUTH),
        ("B", button::EAST),
        ("X", button::WEST),
        ("Y", button::NORTH),
        ("LB", button::TL),
        ("RB", button::TR),
        ("LT", TURBO_LT),
        ("RT", TURBO_RT),
    ];
    for (i, (label, bit)) in chips.into_iter().enumerate() {
        let cx = x + 40.0 + (i % 4) as f32 * 160.0;
        let cy = y + 150.0 + (i / 4) as f32 * 124.0;
        ui.button(Hit::Turbo(bit), cx, cy, 140.0, 104.0, label, 36.0, s.turbo & bit != 0, ACCENT);
    }
    ui.text("频率（次/秒）", x + 40.0, y + 466.0, 28.0, DIM, true, Align::Left);
    for (i, hz) in [10, 15, 20, 25].into_iter().enumerate() {
        let bx = x + 40.0 + i as f32 * 160.0;
        ui.button(Hit::TurboHz(hz), bx, y + 494.0, 140.0, 84.0, &hz.to_string(), 32.0, s.turbo_hz == hz, ACCENT);
    }
    ui.text("连发按键以 Xbox 布局命名", x + 40.0, y + 660.0, 22.0, DIM, false, Align::Left);

    // macros
    let (x, w) = (796.0, W - 64.0 - 796.0);
    ui.card(x, y, w, h);
    ui.text("宏", x + 40.0, y + 66.0, 36.0, TEXT, true, Align::Left);
    ui.text("录制一段操作，再点“播放”或用 Home 组合键回放", x + 40.0, y + 110.0, 24.0, DIM, false, Align::Left);
    let chords = ["Home + A", "Home + B", "Home + X", "Home + Y"];
    for (i, chord) in chords.into_iter().enumerate().take(MACRO_SLOTS) {
        let ry = y + 146.0 + i as f32 * 140.0;
        let recording = v.recording == Some(i);
        let playing = v.playing == Some(i);
        if recording {
            ui.cv.fill(&rrect(x + 24.0, ry, w - 48.0, 124.0, 22.0), rgba(BAD, 0.12));
        }
        ui.text(&format!("宏 {}", i + 1), x + 40.0, ry + 56.0, 34.0, TEXT, true, Align::Left);
        ui.text(chord, x + 40.0, ry + 98.0, 22.0, DIM, false, Align::Left);
        let (state, c) = if recording {
            ("录制中…".to_string(), BAD)
        } else if playing {
            ("播放中".to_string(), GOOD)
        } else if let Some(len) = v.macros[i] {
            (format!("{:.1} 秒", len.as_secs_f32()), TEXT)
        } else {
            ("空".to_string(), DIM)
        };
        ui.text(&state, x + 230.0, ry + 74.0, 30.0, c, false, Align::Left);
        let bx = x + w - 40.0 - 3.0 * 170.0 + 20.0;
        if recording {
            ui.button(Hit::StopRecording, bx, ry + 22.0, 150.0, 80.0, "停止", 30.0, true, BAD);
        } else {
            ui.button(Hit::Record(i), bx, ry + 22.0, 150.0, 80.0, "录制", 30.0, false, BAD);
        }
        if v.macros[i].is_some() && !recording {
            ui.button(Hit::Play(i), bx + 170.0, ry + 22.0, 150.0, 80.0, "播放", 30.0, playing, GOOD);
            ui.button(Hit::Clear(i), bx + 340.0, ry + 22.0, 150.0, 80.0, "清除", 30.0, false, ACCENT);
        }
    }
}

fn settings(ui: &mut Ui, s: &Settings) {
    let (y, h) = (176.0, 724.0);
    let (x, w) = (64.0, 880.0);
    ui.card(x, y, w, h);
    ui.text("手柄", x + 40.0, y + 66.0, 36.0, TEXT, true, Align::Left);
    ui.text("按键布局", x + 40.0, y + 150.0, 34.0, TEXT, true, Align::Left);
    ui.text("任天堂布局会对调 A/B 和 X/Y", x + 40.0, y + 192.0, 24.0, DIM, false, Align::Left);
    ui.button(Hit::Nintendo(false), x + 460.0, y + 112.0, 170.0, 80.0, "Xbox", 30.0, !s.nintendo, ACCENT);
    ui.button(Hit::Nintendo(true), x + 646.0, y + 112.0, 194.0, 80.0, "任天堂", 30.0, s.nintendo, ACCENT);
    ui.switch(Hit::Rumble, x + 40.0, y + 236.0, "震动", "游戏的震动传到掌机", s.rumble);

    ui.text("Home 组合键", x + 40.0, y + 420.0, 30.0, DIM, true, Align::Left);
    let lines = [
        "Home + A / B / X / Y　回放宏 1–4",
        "Home + LB / RB　主机音量",
        "Home + Start　Steam 截图",
        "Home + ↓　熄屏 / 唤醒",
        "长按 Home + Select　退出",
    ];
    for (i, l) in lines.into_iter().enumerate() {
        ui.text(l, x + 40.0, y + 474.0 + i as f32 * 48.0, 26.0, TEXT, false, Align::Left);
    }

    let (x, w) = (976.0, 880.0);
    ui.card(x, y, w, h);
    ui.text("耳机", x + 40.0, y + 66.0, 36.0, TEXT, true, Align::Left);
    ui.switch(Hit::Audio, x + 40.0, y + 104.0, "主机声音", "插上耳机时把电脑的声音传过来", s.audio);
    ui.switch(Hit::Mic, x + 40.0, y + 236.0, "耳机麦克风", "把耳机麦克风作为电脑的麦克风", s.mic);
    ui.text("音频缓冲", x + 40.0, y + 420.0, 34.0, TEXT, true, Align::Left);
    ui.text("越小延迟越低，有杂音就调大", x + 40.0, y + 462.0, 24.0, DIM, false, Align::Left);
    for (i, ms) in [20, 30, 45, 60].into_iter().enumerate() {
        let bx = x + 40.0 + i as f32 * 196.0;
        ui.button(Hit::Latency(ms), bx, y + 496.0, 176.0, 84.0, &format!("{ms} ms"), 30.0, s.audio_latency_ms == ms, ACCENT);
    }
}

fn controller(ui: &mut Ui, p: &Pad, rumble: bool) {
    let (x, y, w, h) = (64.0, 176.0, 880.0, 724.0);
    ui.card(x, y, w, h);
    ui.text("手柄", x + 40.0, y + 64.0, 34.0, TEXT, true, Align::Left);
    if rumble {
        ui.text("震动中", x + w - 40.0, y + 64.0, 28.0, ACCENT, true, Align::Right);
    }
    let on = |b: u32| p.buttons & b != 0;
    let lit = |b: bool| if b { (ACCENT, 0.95) } else { (FAINT, 0.5) };

    for (i, (name, val, bump, bname)) in
        [("LT", p.lt, button::TL, "LB"), ("RT", p.rt, button::TR, "RB")].into_iter().enumerate()
    {
        let bx = x + 40.0 + i as f32 * 420.0;
        ui.text(name, bx, y + 132.0, 26.0, DIM, true, Align::Left);
        ui.bar(bx + 56.0, y + 114.0, 300.0, 22.0, val as f32 / 255.0, ACCENT);
        let (c, a) = lit(on(bump));
        ui.cv.fill(&rrect(bx, y + 160.0, 356.0, 52.0, 26.0), rgba(c, a));
        ui.text(bname, bx + 178.0, y + 196.0, 26.0, TEXT, true, Align::Center);
    }

    let stick = |ui: &mut Ui, cx: f32, cy: f32, sx: i16, sy: i16, pressed: bool| {
        ui.cv.fill(&circle(cx, cy, 88.0), rgba(FAINT, if pressed { 0.9 } else { 0.35 }));
        ui.cv.stroke(&circle(cx, cy, 88.0), rgba(0xffffff, 0.08), 2.0);
        let (dx, dy) = (sx as f32 / 32768.0 * 58.0, sy as f32 / 32768.0 * 58.0);
        ui.cv.fill(&circle(cx + dx, cy + dy, 30.0), rgba(if pressed { ACCENT } else { TEXT }, 0.95));
    };
    stick(ui, x + 190.0, y + 400.0, p.lx, p.ly, on(button::THUMBL));
    stick(ui, x + 590.0, y + 600.0, p.rx, p.ry, on(button::THUMBR));

    let (dx, dy) = (x + 330.0, y + 600.0);
    for (bit, ox, oy) in
        [(button::DPAD_UP, 0.0, -56.0), (button::DPAD_DOWN, 0.0, 56.0), (button::DPAD_LEFT, -56.0, 0.0), (button::DPAD_RIGHT, 56.0, 0.0)]
    {
        let (c, a) = lit(on(bit));
        ui.cv.fill(&rrect(dx + ox - 27.0, dy + oy - 27.0, 54.0, 54.0, 12.0), rgba(c, a));
    }

    let (fx, fy) = (x + 700.0, y + 390.0);
    for (bit, label, ox, oy) in [
        (button::NORTH, "Y", 0.0, -62.0),
        (button::SOUTH, "A", 0.0, 62.0),
        (button::WEST, "X", -62.0, 0.0),
        (button::EAST, "B", 62.0, 0.0),
    ] {
        let (c, a) = lit(on(bit));
        ui.cv.fill(&circle(fx + ox, fy + oy, 31.0), rgba(c, a));
        ui.text(label, fx + ox, fy + oy + 10.0, 28.0, TEXT, true, Align::Center);
    }

    for (i, (bit, label)) in [(button::SELECT, "Select"), (button::MODE, "Home"), (button::START, "Start")].into_iter().enumerate() {
        let cx = x + 340.0 + i as f32 * 104.0;
        let (c, a) = lit(on(bit));
        ui.cv.fill(&rrect(cx - 46.0, y + 370.0, 92.0, 40.0, 20.0), rgba(c, a));
        ui.text(label, cx, y + 397.0, 19.0, TEXT, false, Align::Center);
    }
}

fn host_stats(ui: &mut Ui, v: &View) {
    let (x, y, w, h) = (976.0, 176.0, 880.0, 724.0);
    ui.card(x, y, w, h);
    ui.text("主机", x + 40.0, y + 64.0, 34.0, TEXT, true, Align::Left);
    if !v.host.is_empty() {
        ui.text(&v.host, x + w - 40.0, y + 64.0, 28.0, DIM, false, Align::Right);
    }
    let s = &v.status;
    let rows: [(&str, Option<u8>, Option<f32>); 3] =
        [("CPU", s.cpu_pct, s.cpu_temp), ("GPU", s.gpu_pct, s.gpu_temp), ("内存", s.mem_pct, None)];
    for (i, (name, pct, temp)) in rows.into_iter().enumerate() {
        let ry = y + 120.0 + i as f32 * 190.0;
        ui.text(name, x + 40.0, ry + 40.0, 32.0, DIM, true, Align::Left);
        let value = pct.map_or("—".to_string(), |p| format!("{p}%"));
        ui.text(&value, x + 40.0, ry + 112.0, 64.0, TEXT, true, Align::Left);
        if let Some(tc) = temp {
            let c = if tc >= 85.0 { BAD } else if tc >= 70.0 { WARN } else { DIM };
            ui.text(&format!("{tc:.0}°C"), x + w - 40.0, ry + 112.0, 44.0, c, true, Align::Right);
        }
        let f = pct.map_or(0.0, |p| p as f32 / 100.0);
        let c = if f >= 0.9 { BAD } else if f >= 0.7 { WARN } else { ACCENT };
        ui.bar(x + 40.0, ry + 136.0, w - 80.0, 14.0, f, c);
    }
}
