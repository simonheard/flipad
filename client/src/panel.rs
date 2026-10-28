//! The touch panel: draws each tab and records where its buttons are.
//!
//! Layout (design units, 1920×1080): a status bar with the exit button on top,
//! the page in the middle, and the tab bar along the bottom edge, nearest the
//! thumbs.

use std::time::{Duration, Instant};

use flipad_proto::{HostAction, Pad, Widget, button};

use std::collections::HashMap;
use std::sync::Arc;

use tiny_skia::{FilterQuality, Pixmap, PixmapPaint};

use crate::link::{Phase, View};
use crate::paint::{Align, Canvas, Text, circle, rgba, rrect};
use crate::settings::{Addr, MACRO_SLOTS, Mode, Output, Settings, TURBO_LT, TURBO_RT};

pub const W: f32 = 1920.0;
const M: f32 = 32.0;
/// Page area between the status bar and the tab bar.
const TOP: f32 = 140.0;
const BOTTOM: f32 = 900.0;

const BG: u32 = 0x0d0f13;
const CARD: u32 = 0x161920;
const RAISED: u32 = 0x232835;
const TEXT: u32 = 0xe9ebf1;
const DIM: u32 = 0x9aa0af;
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
    Game,
    Touchpad,
    Quick,
    Macros,
    Settings,
}

/// Tabs for the current state: Bluetooth hides what needs the host service,
/// and the game tab appears while a game plugin is active.
pub fn tabs(v: &View) -> Vec<(Tab, &'static str)> {
    let mut t = vec![(Tab::Overview, "概览")];
    if !v.bluetooth && v.plugin.is_some() {
        t.push((Tab::Game, "游戏"));
    }
    t.push((Tab::Touchpad, "触控板"));
    t.push((Tab::Quick, "快捷"));
    t.push((Tab::Macros, "宏 / 连发"));
    t.push((Tab::Settings, "设置"));
    t
}

/// Where the controller sends its input.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target {
    Wifi,
    BtAuto,
    Bt(Addr),
}

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
    PointerSpeed(u32),
    Output(Output),
    OpenSwitcher,
    CloseSwitcher,
    Connect(Target),
    PluginButton(u16),
    PairNew,
    StopPairing,
    Forget(Addr),
    /// The touchpad surface; handled as a gesture, not a tap.
    Pad,
    /// Vertical strip that scrolls; also a gesture.
    ScrollStrip,
    MouseLeft,
    MouseRight,
    DragLock,
}

pub struct Ui<'a> {
    pub cv: &'a mut Canvas,
    pub text: &'a mut Text,
    pub hits: &'a mut Vec<(Rect, Hit)>,
    /// The last tapped button and when, for feedback.
    pub flash: Option<(Hit, Instant)>,
    pub now: Instant,
    /// Touchpad state for drawing: drag lock on, fingers down.
    pub drag_lock: bool,
    pub fingers: usize,
    pub switcher: bool,
    pub frames: &'a HashMap<u16, Arc<Pixmap>>,
    /// Host whose "forget" needs a confirming second tap.
    pub forget_armed: Option<Addr>,
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

    #[allow(clippy::too_many_arguments)]
    fn text(&mut self, s: &str, x: f32, y: f32, size: f32, color: u32, bold: bool, align: Align) {
        self.text.draw(self.cv, s, x, y, size, rgba(color, 1.0), bold, align);
    }

    fn card(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.cv.fill(&rrect(x, y, w, h, 28.0), rgba(CARD, 1.0));
        self.cv.stroke(&rrect(x + 0.5, y + 0.5, w - 1.0, h - 1.0, 28.0), rgba(0xffffff, 0.06), 1.0);
    }

    /// A tappable rounded button with an optional second line. `on` = selected look.
    #[allow(clippy::too_many_arguments)]
    fn button2(&mut self, hit: Hit, x: f32, y: f32, w: f32, h: f32, label: &str, hint: &str, size: f32, on: bool, color: u32) {
        self.hits.push((Rect { x, y, w, h }, hit));
        let flash = self.flashing(hit);
        let (bg, a) = if flash || on { (color, if flash { 1.0 } else { 0.9 }) } else { (RAISED, 1.0) };
        self.cv.fill(&rrect(x, y, w, h, (h / 2.0).min(30.0)), rgba(bg, a));
        let fg = if on || flash { BG } else { TEXT };
        if hint.is_empty() {
            self.text(label, x + w / 2.0, y + h / 2.0 + size * 0.36, size, fg, true, Align::Center);
        } else {
            self.text(label, x + w / 2.0, y + h / 2.0 + size * 0.1, size, fg, true, Align::Center);
            let hc = if on || flash { BG } else { DIM };
            self.text(hint, x + w / 2.0, y + h / 2.0 + size * 0.1 + 40.0, 26.0, hc, false, Align::Center);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn button(&mut self, hit: Hit, x: f32, y: f32, w: f32, h: f32, label: &str, size: f32, on: bool, color: u32) {
        self.button2(hit, x, y, w, h, label, "", size, on, color);
    }

    /// An on/off row: label (and hint) on the left, a switch on the right.
    #[allow(clippy::too_many_arguments)]
    fn switch(&mut self, hit: Hit, x: f32, y: f32, w: f32, label: &str, hint: &str, on: bool) {
        self.hits.push((Rect { x, y, w, h: 120.0 }, hit));
        self.text(label, x, y + 48.0, 42.0, TEXT, true, Align::Left);
        if !hint.is_empty() {
            self.text(hint, x, y + 96.0, 28.0, DIM, false, Align::Left);
        }
        let (sw, sh) = (150.0, 80.0);
        let sx = x + w - sw;
        self.cv.fill(&rrect(sx, y + 10.0, sw, sh, sh / 2.0), rgba(if on { ACCENT } else { FAINT }, 1.0));
        let kx = if on { sx + sw - sh / 2.0 } else { sx + sh / 2.0 };
        self.cv.fill(&circle(kx, y + 10.0 + sh / 2.0, sh / 2.0 - 8.0), rgba(0xffffff, 1.0));
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
        HostAction::SteamScreenshot => "Home + Start",
        HostAction::Screenshot => "Print Screen",
        HostAction::ShowDesktop => "Meta + D",
        HostAction::BigPicture => "Steam 大屏幕",
        HostAction::AltTab => "上一个窗口",
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
        ui.cv.fill(&circle(W / 2.0 - 84.0, 1026.0, 7.0), rgba(dot, 1.0));
        ui.text("触摸唤醒", W / 2.0 - 62.0, 1038.0, 30.0, 0x2a2d33, false, Align::Left);
        return;
    }
    ui.cv.pm.fill(rgba(BG, 1.0));
    let tabs = tabs(v);
    let tab = if tabs.iter().any(|t| t.0 == tab) { tab } else { Tab::Overview };
    status_bar(ui, v, battery);
    match tab {
        Tab::Overview => overview(ui, v),
        Tab::Game => game(ui, v),
        Tab::Touchpad => touchpad(ui),
        Tab::Quick => quick(ui, v.bluetooth),
        Tab::Macros => macros(ui, v, s),
        Tab::Settings => settings(ui, v, s),
    }
    tab_bar(ui, &tabs, tab);
    if ui.switcher {
        switcher(ui, v, s);
    }
}

fn target_label(v: &View, t: Target) -> String {
    match t {
        Target::Wifi => format!("Wi-Fi · {}", if v.wifi_host.is_empty() { "电脑" } else { &v.wifi_host }),
        Target::BtAuto => "蓝牙 · 自动".into(),
        Target::Bt(a) => format!(
            "蓝牙 · {}",
            v.bt_hosts.iter().find(|h| h.addr == a).map_or(crate::settings::addr_str(&a).to_string(), |h| h.name.clone())
        ),
    }
}

pub fn current_target(s: &Settings) -> Target {
    match (s.mode, s.bt_target) {
        (Mode::Wifi, _) => Target::Wifi,
        (Mode::Bluetooth, None) => Target::BtAuto,
        (Mode::Bluetooth, Some(a)) => Target::Bt(a),
    }
}

/// Modal list of places to connect to.
fn switcher(ui: &mut Ui, v: &View, s: &Settings) {
    ui.hits.clear();
    ui.hits.push((Rect { x: 0.0, y: 0.0, w: W, h: 1080.0 }, Hit::CloseSwitcher));
    ui.cv.fill(&rrect(0.0, 0.0, W, 1080.0, 0.0), rgba(0x000000, 0.6));
    let mut options = vec![Target::Wifi, Target::BtAuto];
    options.extend(v.bt_hosts.iter().map(|h| Target::Bt(h.addr)));
    let (w, row) = (1100.0, 104.0);
    // one more row for "pair a new device"
    let h = 150.0 + (options.len() + 1) as f32 * (row + 16.0) + 70.0;
    let (x, y) = ((W - w) / 2.0, ((1080.0 - h) / 2.0).max(20.0));
    ui.hits.push((Rect { x, y, w, h }, Hit::OpenSwitcher)); // taps on the card keep it open
    ui.card(x, y, w, h);
    ui.text("连接到", x + 40.0, y + 72.0, 48.0, TEXT, true, Align::Left);
    ui.button(Hit::CloseSwitcher, x + w - 40.0 - 180.0, y + 24.0, 180.0, 80.0, "关闭", 34.0, false, ACCENT);
    let cur = current_target(s);
    for (i, t) in options.into_iter().enumerate() {
        let ry = y + 130.0 + i as f32 * (row + 16.0);
        let mut label = target_label(v, t);
        if let Target::Bt(a) = t
            && v.bt_hosts.iter().any(|h| h.addr == a && h.connected)
        {
            label += "（已连接）";
        }
        if t == Target::BtAuto {
            label += "（任一已配对设备）";
        }
        if let Target::Bt(a) = t {
            // a host row also gets a small "forget" button
            ui.button(Hit::Connect(t), x + 40.0, ry, w - 80.0 - 200.0, row, &label, 36.0, t == cur, ACCENT);
            let armed = ui.forget_armed == Some(a);
            ui.button(Hit::Forget(a), x + w - 40.0 - 184.0, ry, 184.0, row, if armed { "确认忘记？" } else { "忘记" }, 30.0, armed, BAD);
        } else {
            ui.button(Hit::Connect(t), x + 40.0, ry, w - 80.0, row, &label, 38.0, t == cur, ACCENT);
        }
    }
    let ry = y + 130.0 + (v.bt_hosts.len() + 2) as f32 * (row + 16.0);
    let pairing = v.bluetooth && v.pairing_left.is_some();
    let label = if pairing { format!("配对中…（剩 {} 秒）点此取消", v.pairing_left.unwrap_or(0)) } else { "＋ 配对新设备".into() };
    ui.button(if pairing { Hit::StopPairing } else { Hit::PairNew }, x + 40.0, ry, w - 80.0, row, &label, 36.0, pairing, GOOD);
    let hint = "配对时，在电脑或手机的蓝牙设置里选择「Flip Pad」";
    ui.text(hint, x + 40.0, y + h - 30.0, 26.0, DIM, false, Align::Left);
}

fn game(ui: &mut Ui, v: &View) {
    let Some(p) = &v.plugin else { return };
    let (x, y) = (M, TOP + 8.0);
    let (w, h) = (W - 2.0 * M, BOTTOM - 8.0 - y);
    ui.card(x, y, w, h);
    let image = p.widgets.iter().find_map(|wd| if let Widget::Image { id, aspect } = wd { Some((*id, *aspect)) } else { None });
    // an image (minimap…) takes the left side at its own aspect ratio
    let mut cx = x + 40.0;
    if let Some((id, aspect)) = image {
        let ih = h - 80.0;
        let iw = (ih * aspect.max(1) as f32 / 1000.0).min(w * 0.62);
        let ih = iw * 1000.0 / aspect.max(1) as f32;
        let (ix, iy) = (x + 40.0, y + (h - ih) / 2.0);
        ui.cv.fill(&rrect(ix, iy, iw, ih, 16.0), rgba(0x000000, 1.0));
        if let Some(pm) = ui.frames.get(&id).cloned() {
            let sx = iw / pm.width() as f32;
            let sy = ih / pm.height() as f32;
            let ts = ui.cv.ts.pre_translate(ix, iy).pre_scale(sx, sy);
            let paint = PixmapPaint { quality: FilterQuality::Bilinear, ..PixmapPaint::default() };
            ui.cv.pm.draw_pixmap(0, 0, pm.as_ref().as_ref(), &paint, ts, None);
        } else {
            ui.text("等待画面…", ix + iw / 2.0, iy + ih / 2.0, 32.0, DIM, false, Align::Center);
        }
        cx = ix + iw + 40.0;
    }
    let cw = x + w - 40.0 - cx;
    let mut ty = y + 76.0;
    ui.text(&p.title, cx, ty, 30.0, DIM, true, Align::Left);
    ty += 30.0;
    for wd in &p.widgets {
        if let Widget::Text { text, big } = wd {
            let size = if *big { 52.0 } else { 32.0 };
            ty += size + 18.0;
            ui.text(text, cx, ty, size, if *big { TEXT } else { DIM }, *big, Align::Left);
        }
    }
    let buttons: Vec<(u16, &str)> =
        p.widgets.iter().filter_map(|wd| if let Widget::Button { id, label } = wd { Some((*id, label.as_str())) } else { None }).collect();
    if !buttons.is_empty() {
        let cols = if cw > 900.0 { 3 } else { 2 };
        let rows = buttons.len().div_ceil(cols);
        let gap = 20.0;
        let top = ty + 40.0;
        let bh = ((y + h - 40.0 - top - (rows as f32 - 1.0) * gap) / rows as f32).clamp(90.0, 170.0);
        let bw = (cw - (cols as f32 - 1.0) * gap) / cols as f32;
        for (i, (id, label)) in buttons.into_iter().enumerate() {
            let bx = cx + (i % cols) as f32 * (bw + gap);
            let by = top + (i / cols) as f32 * (bh + gap);
            ui.button(Hit::PluginButton(id), bx, by, bw, bh, label, 40.0, false, ACCENT);
        }
    }
}

fn status_bar(ui: &mut Ui, v: &View, battery: (Option<u8>, bool)) {
    let (dot, label) = match &v.phase {
        _ if v.bluetooth && v.pairing_left.is_some() => {
            (ACCENT, format!("蓝牙配对中（剩 {} 秒）", v.pairing_left.unwrap_or(0)))
        }
        Phase::Searching if v.bluetooth => (WARN, "蓝牙：等待主机连接".to_string()),
        Phase::Connecting if v.bluetooth => (WARN, format!("蓝牙：{} 正在启用手柄", v.host)),
        Phase::Connected if v.bluetooth => (GOOD, format!("蓝牙：{}", v.host)),
        Phase::Searching => (WARN, "正在寻找主机…".to_string()),
        Phase::Connecting => (WARN, "正在连接…".to_string()),
        Phase::Connected => (GOOD, v.host.clone()),
        Phase::Lost => (WARN, "重连中…".to_string()),
        Phase::Rejected(r) => (BAD, r.clone()),
    };
    ui.cv.fill(&circle(M + 22.0, 64.0, 12.0), rgba(dot, 1.0));
    let lw = ui.text.measure(&label, 36.0, true).min(640.0);
    ui.text(&label, M + 52.0, 78.0, 36.0, TEXT, true, Align::Left);
    let mut x = M + 52.0 + lw + 28.0;
    if let Some(r) = v.rtt_ms.filter(|_| v.phase == Phase::Connected) {
        let l = format!("{r:.1} ms");
        ui.text(&l, x, 78.0, 34.0, dot, true, Align::Left);
        x += ui.text.measure(&l, 34.0, true) + 40.0;
    }

    // second line: battery and headset, or a toast after a shortcut
    if let Some((a, t)) = v.last_action
        && ui.now.duration_since(t) < Duration::from_millis(1500)
    {
        ui.text(&format!("已发送：{}", action_label(a)), M, 124.0, 30.0, ACCENT, true, Align::Left);
    } else {
        let mut parts = Vec::new();
        if let (Some(b), charging) = battery {
            parts.push(format!("电量 {b}%{}", if charging { "（充电中）" } else { "" }));
        }
        parts.push(match (v.jack.headphones, v.jack.mic, v.audio_live) {
            (false, _, _) => "未插耳机".to_string(),
            (true, mic, true) => format!("耳机已接通{}", if mic { " · 麦克风" } else { "" }),
            (true, _, false) => "耳机已插入".to_string(),
        });
        if v.rumble {
            parts.push("震动中".into());
        }
        ui.text(&parts.join("　·　"), M, 124.0, 28.0, DIM, false, Align::Left);
    }
    // small: switch between Wi-Fi and Bluetooth hosts
    ui.button(Hit::OpenSwitcher, x, 40.0, 150.0, 56.0, "切换", 28.0, false, ACCENT);

    if v.exit_hold > 0.0 {
        ui.bar(M, 132.0, 700.0, 8.0, v.exit_hold, BAD);
    }

    ui.button(Hit::ScreenOff, W - M - 300.0 - 24.0 - 230.0, 24.0, 230.0, 96.0, "熄屏", 36.0, false, ACCENT);
    let r = Rect { x: W - M - 300.0, y: 24.0, w: 300.0, h: 96.0 };
    ui.hits.push((r, Hit::Exit));
    ui.cv.fill(&rrect(r.x, r.y, r.w, r.h, 48.0), rgba(BAD, if ui.flashing(Hit::Exit) { 0.5 } else { 0.14 }));
    ui.cv.stroke(&rrect(r.x, r.y, r.w, r.h, 48.0), rgba(BAD, 0.6), 2.0);
    ui.text("退出", r.x + r.w / 2.0, r.y + 62.0, 40.0, BAD, true, Align::Center);
}

fn tab_bar(ui: &mut Ui, tabs: &[(Tab, &str)], tab: Tab) {
    let (y, h) = (BOTTOM + 16.0, 1080.0 - BOTTOM - 16.0 - 16.0);
    let tw = (W - 2.0 * M) / tabs.len() as f32;
    ui.cv.fill(&rrect(M, y, W - 2.0 * M, h, 36.0), rgba(CARD, 1.0));
    for (i, &(t, label)) in tabs.iter().enumerate() {
        let x = M + i as f32 * tw;
        ui.hits.push((Rect { x, y, w: tw, h }, Hit::Tab(t)));
        let selected = t == tab;
        if selected {
            ui.cv.fill(&rrect(x + 8.0, y + 8.0, tw - 16.0, h - 16.0, 30.0), rgba(RAISED, 1.0));
            ui.cv.fill(&rrect(x + tw / 2.0 - 36.0, y + h - 22.0, 72.0, 6.0, 3.0), rgba(ACCENT, 1.0));
        }
        let c = if selected { TEXT } else { DIM };
        ui.text(label, x + tw / 2.0, y + h / 2.0 + 15.0, 42.0, c, selected, Align::Center);
    }
}

fn bluetooth_info(ui: &mut Ui, v: &View, x: f32, w: f32) {
    let (y, h) = (TOP + 8.0, BOTTOM - 8.0 - TOP - 8.0);
    ui.card(x, y, w, h);
    let (ix, mut ty) = (x + 40.0, y + 76.0);
    ui.text("蓝牙模式", ix, ty, 48.0, TEXT, true, Align::Left);
    ty += 70.0;
    let state = match v.phase {
        Phase::Connected => format!("已连接到 {}", v.host),
        Phase::Connecting => format!("{} 已连接，等待它启用手柄…", v.host),
        _ => "等待连接".to_string(),
    };
    let c = if v.phase == Phase::Connected { GOOD } else { WARN };
    ui.text(&state, ix, ty, 34.0, c, true, Align::Left);
    ty += 90.0;
    if let Some(left) = v.pairing_left {
        // pairing window open: say what to do, offer to stop
        ui.cv.fill(&rrect(ix - 16.0, ty - 50.0, w - 48.0, 230.0, 24.0), rgba(ACCENT, 0.12));
        ui.text(&format!("配对模式，还剩 {left} 秒"), ix, ty, 38.0, ACCENT, true, Align::Left);
        ui.text("在电脑或手机的蓝牙设置里选择「Flip Pad」", ix, ty + 52.0, 30.0, TEXT, false, Align::Left);
        ui.button(Hit::StopPairing, ix, ty + 84.0, 220.0, 80.0, "取消", 32.0, false, ACCENT);
        return;
    }
    let lines = [
        "第一次使用：在电脑或手机的蓝牙设置里",
        "搜索并配对「Flip Pad」",
        "",
        "配对过的设备打开蓝牙后会自动重连",
        "电脑上不用装任何东西，按 Xbox 手柄识别",
        "",
        "多台设备：点左上角「切换」选择连哪一台",
    ];
    for l in lines {
        if !l.is_empty() {
            ui.text(l, ix, ty, 30.0, if l.starts_with('搜') { TEXT } else { DIM }, l.starts_with('搜'), Align::Left);
        }
        ty += 52.0;
    }
}

fn overview(ui: &mut Ui, v: &View) {
    controller(ui, &v.pad, 1040.0);
    if v.bluetooth {
        bluetooth_info(ui, v, M + 1040.0 + 24.0, W - 2.0 * M - 1040.0 - 24.0);
        return;
    }

    // compact host readings
    let (x, w) = (M + 1040.0 + 24.0, W - 2.0 * M - 1040.0 - 24.0);
    let (y, h) = (TOP + 8.0, 236.0);
    ui.card(x, y, w, h);
    let s = &v.status;
    let cols: [(&str, Option<u8>, Option<f32>); 3] =
        [("CPU", s.cpu_pct, s.cpu_temp), ("GPU", s.gpu_pct, s.gpu_temp), ("内存", s.mem_pct, None)];
    let cw = (w - 48.0) / 3.0;
    for (i, (name, pct, temp)) in cols.into_iter().enumerate() {
        let cx = x + 24.0 + i as f32 * cw + 16.0;
        ui.text(name, cx, y + 56.0, 30.0, DIM, true, Align::Left);
        ui.text(&pct.map_or("—".into(), |p| format!("{p}%")), cx, y + 132.0, 56.0, TEXT, true, Align::Left);
        if let Some(t) = temp {
            let c = if t >= 85.0 { BAD } else if t >= 70.0 { WARN } else { DIM };
            ui.text(&format!("{t:.0}°C"), cx, y + 180.0, 30.0, c, true, Align::Left);
        }
        let f = pct.map_or(0.0, |p| p as f32 / 100.0);
        let c = if f >= 0.9 { BAD } else if f >= 0.7 { WARN } else { ACCENT };
        ui.bar(cx, y + 200.0, cw - 48.0, 10.0, f, c);
    }

    // shortcuts that matter mid-game
    let picks = [
        HostAction::VolumeDown,
        HostAction::VolumeUp,
        HostAction::PlayPause,
        HostAction::Mute,
        HostAction::SteamScreenshot,
        HostAction::AltTab,
    ];
    let (gy, gap) = (y + h + 24.0, 20.0);
    let bw = (w - gap) / 2.0;
    let bh = (BOTTOM - 8.0 - gy - 2.0 * gap) / 3.0;
    for (i, a) in picks.into_iter().enumerate() {
        let bx = x + (i % 2) as f32 * (bw + gap);
        let by = gy + (i / 2) as f32 * (bh + gap);
        ui.button2(Hit::Action(a), bx, by, bw, bh, action_label(a), action_hint(a), 42.0, false, ACCENT);
    }
}

fn touchpad(ui: &mut Ui) {
    let (x, y) = (M, TOP + 8.0);
    let (w, h) = (1400.0, BOTTOM - 8.0 - y);
    ui.hits.push((Rect { x, y, w, h }, Hit::Pad));
    let active = ui.fingers > 0;
    ui.cv.fill(&rrect(x, y, w, h, 32.0), rgba(if active { 0x1b2130 } else { CARD }, 1.0));
    ui.cv.stroke(&rrect(x + 1.0, y + 1.0, w - 2.0, h - 2.0, 32.0), rgba(if active { ACCENT } else { 0xffffff }, if active { 0.5 } else { 0.08 }), 2.0);
    // dot grid
    let mut gy = y + 60.0;
    while gy < y + h - 30.0 {
        let mut gx = x + 60.0;
        while gx < x + w - 30.0 {
            ui.cv.fill(&circle(gx, gy, 2.5), rgba(0xffffff, 0.07));
            gx += 80.0;
        }
        gy += 80.0;
    }
    if !active {
        ui.text("触控板", x + w / 2.0, y + h / 2.0 - 30.0, 56.0, FAINT, true, Align::Center);
        let hint = "单指移动 · 轻点左键 · 双指轻点右键 · 双指滑动滚动";
        ui.text(hint, x + w / 2.0, y + h / 2.0 + 40.0, 32.0, DIM, false, Align::Center);
    }

    let (cx, cw) = (x + w + 24.0, W - M - (x + w + 24.0));
    let lock = ui.drag_lock;
    ui.button(Hit::MouseLeft, cx, y, cw, 180.0, "左键", 44.0, false, ACCENT);
    ui.button(Hit::MouseRight, cx, y + 200.0, cw, 140.0, "右键", 44.0, false, ACCENT);
    ui.button2(Hit::DragLock, cx, y + 360.0, cw, 140.0, "拖拽锁定", if lock { "左键按住中" } else { "按住左键拖动" }, 36.0, lock, WARN);
    // scroll strip
    let (sy, sh) = (y + 520.0, h - 520.0);
    ui.hits.push((Rect { x: cx, y: sy, w: cw, h: sh }, Hit::ScrollStrip));
    ui.cv.fill(&rrect(cx, sy, cw, sh, 30.0), rgba(RAISED, 1.0));
    ui.text("滚动", cx + cw / 2.0, sy + sh / 2.0 + 14.0, 38.0, DIM, true, Align::Center);
    ui.text("▲", cx + cw / 2.0, sy + 54.0, 30.0, FAINT, false, Align::Center);
    ui.text("▼", cx + cw / 2.0, sy + sh - 24.0, 30.0, FAINT, false, Align::Center);
}

fn quick(ui: &mut Ui, bluetooth: bool) {
    let (y0, gap) = (TOP + 8.0, 20.0);
    let tw = (W - 2.0 * M - 3.0 * gap) / 4.0;
    let th = (BOTTOM - 8.0 - y0 - 2.0 * gap) / 3.0;
    // over Bluetooth only what a keyboard or media keys can do
    let actions = HostAction::ALL.into_iter().filter(|a| !bluetooth || crate::ble::shortcut(*a).is_some());
    for (i, a) in actions.enumerate() {
        let (col, row) = ((i % 4) as f32, (i / 4) as f32);
        let (x, y) = (M + col * (tw + gap), y0 + row * (th + gap));
        ui.button2(Hit::Action(a), x, y, tw, th, action_label(a), action_hint(a), 52.0, false, ACCENT);
    }
}

fn macros(ui: &mut Ui, v: &View, s: &Settings) {
    let (y, h) = (TOP + 8.0, BOTTOM - 8.0 - TOP - 8.0);
    // turbo
    let (x, w) = (M, 700.0);
    ui.card(x, y, w, h);
    ui.text("连发", x + 36.0, y + 70.0, 46.0, TEXT, true, Align::Left);
    ui.text("开启后按住即自动连按", x + 36.0, y + 118.0, 28.0, DIM, false, Align::Left);
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
    let cw = (w - 72.0 - 3.0 * 16.0) / 4.0;
    for (i, (label, bit)) in chips.into_iter().enumerate() {
        let cx = x + 36.0 + (i % 4) as f32 * (cw + 16.0);
        let cy = y + 150.0 + (i / 4) as f32 * 140.0;
        ui.button(Hit::Turbo(bit), cx, cy, cw, 124.0, label, 44.0, s.turbo & bit != 0, ACCENT);
    }
    ui.text("频率（次/秒）", x + 36.0, y + 478.0, 32.0, DIM, true, Align::Left);
    for (i, hz) in [10, 15, 20, 25].into_iter().enumerate() {
        let bx = x + 36.0 + i as f32 * (cw + 16.0);
        ui.button(Hit::TurboHz(hz), bx, y + 506.0, cw, 110.0, &hz.to_string(), 40.0, s.turbo_hz == hz, ACCENT);
    }

    // macros
    let (x, w) = (M + 700.0 + 24.0, W - 2.0 * M - 700.0 - 24.0);
    ui.card(x, y, w, h);
    ui.text("宏", x + 36.0, y + 70.0, 46.0, TEXT, true, Align::Left);
    ui.text("录制一段操作，点“播放”或用 Home 组合键回放", x + 36.0, y + 118.0, 28.0, DIM, false, Align::Left);
    let chords = ["Home + A", "Home + B", "Home + X", "Home + Y"];
    let rh = (h - 150.0 - 16.0) / MACRO_SLOTS as f32;
    for (i, chord) in chords.into_iter().enumerate().take(MACRO_SLOTS) {
        let ry = y + 150.0 + i as f32 * rh;
        let recording = v.recording == Some(i);
        let playing = v.playing == Some(i);
        if recording {
            ui.cv.fill(&rrect(x + 16.0, ry, w - 32.0, rh - 10.0, 22.0), rgba(BAD, 0.12));
        }
        ui.text(&format!("宏 {}", i + 1), x + 36.0, ry + 56.0, 42.0, TEXT, true, Align::Left);
        ui.text(chord, x + 36.0, ry + 100.0, 26.0, DIM, false, Align::Left);
        let (state, c) = if recording {
            ("录制中…".to_string(), BAD)
        } else if playing {
            ("播放中".to_string(), GOOD)
        } else if let Some(len) = v.macros[i] {
            (format!("{:.1} 秒", len.as_secs_f32()), TEXT)
        } else {
            ("空".to_string(), DIM)
        };
        ui.text(&state, x + 250.0, ry + 74.0, 36.0, c, false, Align::Left);
        let (bw, bh, gap) = (170.0, rh - 30.0, 16.0);
        let bx = x + w - 36.0 - 3.0 * bw - 2.0 * gap;
        let by = ry + 6.0;
        if recording {
            ui.button(Hit::StopRecording, bx, by, bw, bh, "停止", 38.0, true, BAD);
        } else {
            ui.button(Hit::Record(i), bx, by, bw, bh, "录制", 38.0, false, BAD);
        }
        if v.macros[i].is_some() && !recording {
            ui.button(Hit::Play(i), bx + bw + gap, by, bw, bh, "播放", 38.0, playing, GOOD);
            ui.button(Hit::Clear(i), bx + 2.0 * (bw + gap), by, bw, bh, "清除", 38.0, false, ACCENT);
        }
    }
}

fn settings(ui: &mut Ui, v: &View, s: &Settings) {
    let (y, h) = (TOP + 8.0, BOTTOM - 8.0 - TOP - 8.0);
    let w = (W - 2.0 * M - 24.0) / 2.0;

    let x = M;
    ui.card(x, y, w, h);
    let (ix, iw) = (x + 36.0, w - 72.0);
    ui.text("连接", ix, y + 66.0, 42.0, TEXT, true, Align::Left);
    ui.text(&target_label(v, current_target(s)), ix, y + 116.0, 32.0, ACCENT, true, Align::Left);
    ui.button(Hit::OpenSwitcher, ix + iw - 200.0, y + 34.0, 200.0, 96.0, "切换", 38.0, false, ACCENT);
    ui.text("按键布局", ix, y + 230.0, 42.0, TEXT, true, Align::Left);
    ui.text("任天堂：对调 A/B 和 X/Y", ix, y + 276.0, 28.0, DIM, false, Align::Left);
    let bw = (iw - 16.0) / 2.0;
    ui.button(Hit::Nintendo(false), ix, y + 300.0, bw, 100.0, "Xbox", 40.0, !s.nintendo, ACCENT);
    ui.button(Hit::Nintendo(true), ix + bw + 16.0, y + 300.0, bw, 100.0, "任天堂", 40.0, s.nintendo, ACCENT);
    ui.switch(Hit::Rumble, ix, y + 420.0, iw, "震动", "游戏的震动传到掌机", s.rumble);
    ui.text("掌机出声", ix, y + 600.0, 40.0, TEXT, true, Align::Left);
    let ow = (iw - 200.0 - 32.0) / 3.0;
    for (i, (label, o)) in [("跟随", Output::Follow), ("耳机", Output::Headphones), ("扬声器", Output::Speaker)].into_iter().enumerate() {
        let bx = ix + 200.0 + i as f32 * (ow + 16.0);
        ui.button(Hit::Output(o), bx, y + 556.0, ow, 92.0, label, 36.0, s.output == o, ACCENT);
    }
    ui.text("跟随：由掌机决定，插耳机走耳机，否则外放", ix, y + h - 30.0, 26.0, DIM, false, Align::Left);

    let x = M + w + 24.0;
    ui.card(x, y, w, h);
    let (ix, iw) = (x + 36.0, w - 72.0);
    let speed_row = |ui: &mut Ui, ty: f32| {
        ui.text("触控板速度", ix, ty, 40.0, TEXT, true, Align::Left);
        let sw = (iw - 32.0) / 3.0;
        for (i, (label, pct)) in [("慢", 100), ("中", 160), ("快", 240)].into_iter().enumerate() {
            let bx = ix + i as f32 * (sw + 16.0);
            ui.button(Hit::PointerSpeed(pct), bx, ty + 24.0, sw, 92.0, label, 38.0, s.pointer_speed == pct, ACCENT);
        }
    };
    if v.bluetooth {
        ui.text("蓝牙音频", ix, y + 66.0, 42.0, TEXT, true, Align::Left);
        let lines = [
            "在电脑的声音设置里选「Flip Pad」作为输出",
            "声音从左边「掌机出声」选的位置出来",
            "蓝牙音频延迟较高，玩游戏建议用 Wi-Fi 模式",
            "蓝牙模式没有麦克风，需要麦克风请用 Wi-Fi 模式",
        ];
        for (i, l) in lines.into_iter().enumerate() {
            ui.text(l, ix, y + 130.0 + i as f32 * 52.0, 30.0, DIM, false, Align::Left);
        }
        speed_row(ui, y + 400.0);
        return;
    }
    ui.switch(Hit::Audio, ix, y + 20.0, iw, "主机声音", "插上耳机时传来电脑的声音", s.audio);
    ui.switch(Hit::Mic, ix, y + 160.0, iw, "耳机麦克风", "当作电脑的麦克风", s.mic);
    ui.text("音频缓冲", ix, y + 352.0, 40.0, TEXT, true, Align::Left);
    let lw = (iw - 48.0) / 4.0;
    for (i, ms) in [20, 30, 45, 60].into_iter().enumerate() {
        let bx = ix + i as f32 * (lw + 16.0);
        ui.button(Hit::Latency(ms), bx, y + 376.0, lw, 92.0, &format!("{ms}ms"), 36.0, s.audio_latency_ms == ms, ACCENT);
    }
    speed_row(ui, y + 540.0);
}

fn controller(ui: &mut Ui, p: &Pad, w: f32) {
    let (x, y) = (M, TOP + 8.0);
    let h = BOTTOM - 8.0 - y;
    ui.card(x, y, w, h);
    let on = |b: u32| p.buttons & b != 0;
    let lit = |b: bool| if b { (ACCENT, 0.95) } else { (FAINT, 0.5) };
    let cx = x + w / 2.0;

    for (i, (name, val, bump, bname)) in
        [("LT", p.lt, button::TL, "LB"), ("RT", p.rt, button::TR, "RB")].into_iter().enumerate()
    {
        let bx = if i == 0 { x + 40.0 } else { x + w - 40.0 - 420.0 };
        ui.text(name, bx, y + 66.0, 32.0, DIM, true, Align::Left);
        ui.bar(bx + 70.0, y + 44.0, 350.0, 26.0, val as f32 / 255.0, ACCENT);
        let (c, a) = lit(on(bump));
        ui.cv.fill(&rrect(bx, y + 96.0, 420.0, 64.0, 32.0), rgba(c, a));
        ui.text(bname, bx + 210.0, y + 140.0, 32.0, TEXT, true, Align::Center);
    }

    let stick = |ui: &mut Ui, sx: f32, sy: f32, ax: i16, ay: i16, pressed: bool| {
        ui.cv.fill(&circle(sx, sy, 110.0), rgba(FAINT, if pressed { 0.9 } else { 0.35 }));
        ui.cv.stroke(&circle(sx, sy, 110.0), rgba(0xffffff, 0.08), 2.0);
        let (dx, dy) = (ax as f32 / 32768.0 * 72.0, ay as f32 / 32768.0 * 72.0);
        ui.cv.fill(&circle(sx + dx, sy + dy, 38.0), rgba(if pressed { ACCENT } else { TEXT }, 0.95));
    };
    stick(ui, x + 200.0, y + 330.0, p.lx, p.ly, on(button::THUMBL));
    stick(ui, cx + 200.0, y + 590.0, p.rx, p.ry, on(button::THUMBR));

    let (dx, dy) = (cx - 200.0, y + 590.0);
    for (bit, ox, oy) in
        [(button::DPAD_UP, 0.0, -70.0), (button::DPAD_DOWN, 0.0, 70.0), (button::DPAD_LEFT, -70.0, 0.0), (button::DPAD_RIGHT, 70.0, 0.0)]
    {
        let (c, a) = lit(on(bit));
        ui.cv.fill(&rrect(dx + ox - 34.0, dy + oy - 34.0, 68.0, 68.0, 14.0), rgba(c, a));
    }

    let (fx, fy) = (x + w - 200.0, y + 330.0);
    for (bit, label, ox, oy) in [
        (button::NORTH, "Y", 0.0, -78.0),
        (button::SOUTH, "A", 0.0, 78.0),
        (button::WEST, "X", -78.0, 0.0),
        (button::EAST, "B", 78.0, 0.0),
    ] {
        let (c, a) = lit(on(bit));
        ui.cv.fill(&circle(fx + ox, fy + oy, 40.0), rgba(c, a));
        ui.text(label, fx + ox, fy + oy + 13.0, 36.0, TEXT, true, Align::Center);
    }

    for (i, (bit, label)) in [(button::SELECT, "Select"), (button::MODE, "Home"), (button::START, "Start")].into_iter().enumerate() {
        let bx = cx + (i as f32 - 1.0) * 140.0;
        let (c, a) = lit(on(bit));
        ui.cv.fill(&rrect(bx - 62.0, y + 186.0, 124.0, 56.0, 28.0), rgba(c, a));
        ui.text(label, bx, y + 224.0, 26.0, TEXT, true, Align::Center);
    }
}
