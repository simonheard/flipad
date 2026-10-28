//! Renders the intro video's scenes from the real panel code (hidden
//! `--make-video DIR`). Each scene is drawn frame by frame at 1920×1080 and
//! piped to ffmpeg as one clip; photos come from `DIR/assets/photos`.

use std::collections::HashMap;
use std::f32::consts::TAU;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use flipad_proto::{HostAction, Pad, Status, Widget, button};
use tiny_skia::{
    Color, FillRule, FilterQuality, Mask, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform,
};

use crate::audio::Jack;
use crate::link::{BtHost, Phase, PluginPanel, View};
use crate::paint::{Align, Canvas, Text, circle, rgba, rrect};
use crate::panel::{self, Hit, Tab, Ui};
use crate::settings::{Mode, Settings, parse_addr};

const W: u32 = 1920;
const H: u32 = 1080;
const FPS: f32 = 30.0;
const BG_TOP: u32 = 0x10131a;
const BG_BOTTOM: u32 = 0x07080b;
const TEXT: u32 = 0xe9ebf1;
const DIM: u32 = 0x9aa0af;
const ACCENT: u32 = 0x6ea8fe;
const GOOD: u32 = 0x4ade80;

fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Fade in over [a, a+d].
fn fade(t: f32, a: f32, d: f32) -> f32 {
    ease((t - a) / d)
}

struct Studio {
    text: Text,
    photos: HashMap<&'static str, Pixmap>,
    /// Rendered panel screens, keyed by a state string, reused across frames.
    ui_cache: HashMap<String, Arc<Pixmap>>,
}

fn base_view() -> View {
    View {
        phase: Phase::Connected,
        host: "我的电脑".into(),
        wifi_host: "我的电脑".into(),
        rtt_ms: Some(2.1),
        status: Status { cpu_pct: Some(34), cpu_temp: Some(58.0), gpu_pct: Some(72), gpu_temp: Some(66.0), mem_pct: Some(46) },
        jack: Jack { headphones: true, mic: true },
        audio_live: true,
        macros: [Some(Duration::from_millis(2400)), Some(Duration::from_millis(900)), None, None],
        ..View::default()
    }
}

impl Studio {
    /// Renders the panel for a state; `key` identifies it for caching.
    fn screen(&mut self, key: &str, tab: Tab, v: &View, s: &Settings, extra: impl FnOnce(&mut UiExtra)) -> Arc<Pixmap> {
        if let Some(p) = self.ui_cache.get(key) {
            return p.clone();
        }
        let mut ex = UiExtra::default();
        extra(&mut ex);
        let mut cv = Canvas { pm: Pixmap::new(W, H).unwrap(), ts: Transform::identity(), scale: 1.0, ox: 0.0, oy: 0.0 };
        let mut hits = Vec::new();
        let mut ui = Ui {
            cv: &mut cv,
            text: &mut self.text,
            hits: &mut hits,
            flash: ex.flash.map(|h| (h, Instant::now())),
            now: Instant::now(),
            drag_lock: false,
            fingers: ex.fingers,
            switcher: ex.switcher,
            frames: &ex.frames,
            forget_armed: None,
        };
        panel::render(&mut ui, v, s, tab, (Some(82), false));
        let p = Arc::new(cv.pm);
        // only cache static states; animated ones pass a unique key anyway
        if self.ui_cache.len() < 64 {
            self.ui_cache.insert(key.to_string(), p.clone());
        }
        p
    }
}

#[derive(Default)]
struct UiExtra {
    flash: Option<Hit>,
    fingers: usize,
    switcher: bool,
    frames: HashMap<u16, Arc<Pixmap>>,
}

/// Background gradient.
fn background(pm: &mut Pixmap) {
    let top = rgba(BG_TOP, 1.0);
    let bottom = rgba(BG_BOTTOM, 1.0);
    if let Some(shader) = tiny_skia::LinearGradient::new(
        tiny_skia::Point::from_xy(0.0, 0.0),
        tiny_skia::Point::from_xy(0.0, H as f32),
        vec![tiny_skia::GradientStop::new(0.0, top), tiny_skia::GradientStop::new(1.0, bottom)],
        tiny_skia::SpreadMode::Pad,
        Transform::identity(),
    ) {
        let paint = Paint { shader, ..Paint::default() };
        pm.fill_rect(tiny_skia::Rect::from_xywh(0.0, 0.0, W as f32, H as f32).unwrap(), &paint, Transform::identity(), None);
    }
}

/// Draws `src` into the rect (x, y, w, h), cropped to fill it, with rounded
/// corners and opacity.
fn image(pm: &mut Pixmap, src: &Pixmap, x: f32, y: f32, w: f32, h: f32, radius: f32, alpha: f32) {
    if alpha <= 0.0 {
        return;
    }
    let s = (w / src.width() as f32).max(h / src.height() as f32);
    let (dw, dh) = (src.width() as f32 * s, src.height() as f32 * s);
    let ts = Transform::from_translate(x + (w - dw) / 2.0, y + (h - dh) / 2.0).pre_scale(s, s);
    let mut mask = Mask::new(W, H).unwrap();
    mask.fill_path(&rrect(x, y, w, h, radius), FillRule::Winding, true, Transform::identity());
    let paint = PixmapPaint { opacity: alpha, quality: FilterQuality::Bicubic, ..PixmapPaint::default() };
    pm.draw_pixmap(0, 0, src.as_ref(), &paint, ts, Some(&mask));
}

/// A stylised handheld showing `screen` (a 1920×1080 panel render).
/// Returns the screen rect for overlays.
fn device(pm: &mut Pixmap, screen: &Pixmap, cx: f32, cy: f32, sw: f32, alpha: f32) -> (f32, f32, f32, f32) {
    let sh = sw * 9.0 / 16.0;
    let (bx, by, bw, bh) = (cx - sw / 2.0 - 26.0, cy - sh / 2.0 - 26.0, sw + 52.0, sh + 52.0);
    let mut paint = Paint { anti_alias: true, ..Paint::default() };
    // soft shadow
    for i in 0..6 {
        let g = 10.0 + i as f32 * 8.0;
        paint.set_color(Color::from_rgba(0.0, 0.0, 0.0, 0.06 * alpha).unwrap());
        pm.fill_path(&rrect(bx - g / 2.0, by - g / 2.0 + 16.0, bw + g, bh + g, 46.0 + g / 2.0), &paint, FillRule::Winding, Transform::identity(), None);
    }
    paint.set_color(Color::from_rgba8(30, 33, 40, (alpha * 255.0) as u8));
    pm.fill_path(&rrect(bx, by, bw, bh, 44.0), &paint, FillRule::Winding, Transform::identity(), None);
    paint.set_color(Color::from_rgba8(255, 255, 255, (alpha * 22.0) as u8));
    pm.stroke_path(&rrect(bx + 1.0, by + 1.0, bw - 2.0, bh - 2.0, 43.0), &paint, &Stroke { width: 2.0, ..Stroke::default() }, Transform::identity(), None);
    let (sx, sy) = (cx - sw / 2.0, cy - sh / 2.0);
    image(pm, screen, sx, sy, sw, sh, 14.0, alpha);
    (sx, sy, sw, sh)
}

fn text(st: &mut Studio, pm: &mut Pixmap, s: &str, x: f32, y: f32, size: f32, color: u32, alpha: f32, bold: bool, align: Align) {
    if alpha <= 0.01 {
        return;
    }
    let mut cv = Canvas { pm: std::mem::replace(pm, Pixmap::new(1, 1).unwrap()), ts: Transform::identity(), scale: 1.0, ox: 0.0, oy: 0.0 };
    st.text.draw(&mut cv, s, x, y, size, rgba(color, alpha), bold, align);
    *pm = cv.pm;
}

/// Scene heading: a small kicker and a large title, top left.
fn heading(st: &mut Studio, pm: &mut Pixmap, t: f32, kicker: &str, title: &str) {
    let a = fade(t, 0.15, 0.5);
    let dy = (1.0 - a) * 20.0;
    text(st, pm, kicker, 110.0, 150.0 + dy, 34.0, ACCENT, a, true, Align::Left);
    text(st, pm, title, 110.0, 230.0 + dy, 68.0, TEXT, a, true, Align::Left);
}

/// Bullet points appearing one after another on the right.
fn points(st: &mut Studio, pm: &mut Pixmap, t: f32, start: f32, x: f32, y: f32, items: &[&str]) {
    for (i, s) in items.iter().enumerate() {
        let a = fade(t, start + i as f32 * 0.6, 0.45);
        let dx = (1.0 - a) * 24.0;
        let yy = y + i as f32 * 92.0;
        let mut paint = Paint { anti_alias: true, ..Paint::default() };
        paint.set_color(rgba(ACCENT, a));
        pm.fill_path(&circle(x + dx + 10.0, yy - 14.0, 8.0), &paint, FillRule::Winding, Transform::identity(), None);
        text(st, pm, s, x + 40.0 + dx, yy, 44.0, TEXT, a, false, Align::Left);
    }
}

fn finger(pm: &mut Pixmap, x: f32, y: f32, alpha: f32) {
    let mut paint = Paint { anti_alias: true, ..Paint::default() };
    paint.set_color(Color::from_rgba(1.0, 1.0, 1.0, 0.28 * alpha).unwrap());
    pm.fill_path(&circle(x, y, 26.0), &paint, FillRule::Winding, Transform::identity(), None);
    paint.set_color(Color::from_rgba(1.0, 1.0, 1.0, 0.85 * alpha).unwrap());
    pm.stroke_path(&circle(x, y, 26.0), &paint, &Stroke { width: 3.0, ..Stroke::default() }, Transform::identity(), None);
}

fn cursor(pm: &mut Pixmap, x: f32, y: f32) {
    let mut pb = PathBuilder::new();
    pb.move_to(x, y);
    pb.line_to(x, y + 44.0);
    pb.line_to(x + 11.0, y + 34.0);
    pb.line_to(x + 19.0, y + 52.0);
    pb.line_to(x + 27.0, y + 48.0);
    pb.line_to(x + 19.0, y + 31.0);
    pb.line_to(x + 33.0, y + 31.0);
    pb.close();
    let path = pb.finish().unwrap();
    let mut paint = Paint { anti_alias: true, ..Paint::default() };
    paint.set_color_rgba8(255, 255, 255, 255);
    pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    paint.set_color_rgba8(0, 0, 0, 255);
    pm.stroke_path(&path, &paint, &Stroke { width: 2.5, ..Stroke::default() }, Transform::identity(), None);
}

fn label_chip(st: &mut Studio, pm: &mut Pixmap, s: &str, x: f32, y: f32, alpha: f32) {
    let w = st.text.measure(s, 30.0, true) + 48.0;
    let mut paint = Paint { anti_alias: true, ..Paint::default() };
    paint.set_color(Color::from_rgba8(10, 12, 16, (alpha * 220.0) as u8));
    pm.fill_path(&rrect(x, y, w, 60.0, 30.0), &paint, FillRule::Winding, Transform::identity(), None);
    text(st, pm, s, x + 24.0, y + 41.0, 30.0, TEXT, alpha, true, Align::Left);
}

fn radar(t: f32) -> Pixmap {
    let (w, h) = (400u32, 300u32);
    let mut pm = Pixmap::new(w, h).unwrap();
    pm.fill(Color::from_rgba8(10, 20, 16, 255));
    let (cx, cy, r) = (w as f32 / 2.0, h as f32 / 2.0, h as f32 * 0.45);
    let mut paint = Paint { anti_alias: true, ..Paint::default() };
    paint.set_color_rgba8(60, 220, 120, 120);
    for k in 1..=3 {
        pm.stroke_path(&circle(cx, cy, r * k as f32 / 3.0), &paint, &Stroke { width: 2.0, ..Stroke::default() }, Transform::identity(), None);
    }
    let a = t * TAU / 2.5;
    let mut pb = PathBuilder::new();
    pb.move_to(cx, cy);
    pb.line_to(cx + r * a.cos(), cy + r * a.sin());
    paint.set_color_rgba8(120, 255, 160, 255);
    pm.stroke_path(&pb.finish().unwrap(), &paint, &Stroke { width: 4.0, ..Stroke::default() }, Transform::identity(), None);
    paint.set_color_rgba8(255, 90, 90, 255);
    pm.fill_path(&circle(cx + r * 0.5, cy - r * 0.3, 8.0), &paint, FillRule::Winding, Transform::identity(), None);
    paint.set_color_rgba8(250, 210, 80, 255);
    pm.fill_path(&circle(cx - r * 0.35, cy + r * 0.45, 6.0), &paint, FillRule::Winding, Transform::identity(), None);
    pm
}

type SceneFn = fn(&mut Studio, &mut Pixmap, f32);

fn scene_intro(st: &mut Studio, pm: &mut Pixmap, t: f32) {
    background(pm);
    let v = base_view();
    let scr = st.screen("intro", Tab::Overview, &v, &Settings::default(), |_| {});
    let a = fade(t, 0.0, 0.8);
    let w = 980.0 + 120.0 * ease(t / 6.0);
    device(pm, &scr, W as f32 / 2.0, 470.0, w, a);
    let b = fade(t, 1.2, 0.8);
    text(st, pm, "你的掌机，也可以是电脑的手柄。", W as f32 / 2.0, 960.0, 64.0, TEXT, b, true, Align::Center);
}

fn scene_placeholder(st: &mut Studio, pm: &mut Pixmap, _t: f32) {
    pm.fill(rgba(0x000000, 1.0));
    text(st, pm, "P2 · 真机镜头", W as f32 / 2.0, 500.0, 80.0, TEXT, 1.0, true, Align::Center);
    text(st, pm, "在 Kdenlive 里把这一段换成你拍的视频（约 8 秒）", W as f32 / 2.0, 590.0, 40.0, DIM, 1.0, false, Align::Center);
}

fn scene_controller(st: &mut Studio, pm: &mut Pixmap, t: f32) {
    background(pm);
    heading(st, pm, t, "WI-FI · 约 2 毫秒", "无线 Xbox 手柄");
    let ang = t * 2.2;
    let mut b = 0;
    if (t * 2.0) as i32 % 3 == 0 {
        b |= button::SOUTH;
    }
    if (t * 2.0) as i32 % 5 == 2 {
        b |= button::EAST;
    }
    if (t * 1.3) as i32 % 2 == 1 {
        b |= button::TR;
    }
    let pad = Pad {
        buttons: b,
        lx: (ang.cos() * 26000.0) as i16,
        ly: (ang.sin() * 26000.0) as i16,
        rx: ((t * 1.3).sin() * 18000.0) as i16,
        ry: 0,
        lt: 0,
        rt: (((t * 1.5).sin() * 0.5 + 0.5) * 255.0) as u8,
    };
    let v = View { pad, rumble: (t % 3.0) > 2.2, ..base_view() };
    let key = format!("ctrl{}", (t * FPS) as i32);
    let scr = st.screen(&key, Tab::Overview, &v, &Settings::default(), |_| {});
    device(pm, &scr, 640.0, 650.0, 960.0, fade(t, 0.0, 0.6));
    let pc = st.photos.get("pc").cloned();
    if let Some(p) = &pc {
        image(pm, p, 1240.0, 330.0, 580.0, 330.0, 24.0, fade(t, 0.8, 0.6));
        label_chip(st, pm, "游戏电脑上即插即用", 1262.0, 590.0, fade(t, 1.0, 0.6));
    }
    points(st, pm, t, 1.6, 1240.0, 760.0, &["震动回传到掌机", "Xbox / 任天堂布局一键切换"]);
}

fn scene_touchpad(st: &mut Studio, pm: &mut Pixmap, t: f32) {
    background(pm);
    heading(st, pm, t, "触控板", "屏幕就是触控板");
    let two = t > 5.0;
    let v = base_view();
    let key = format!("tp{}", two as u8);
    let scr = st.screen(&key, Tab::Touchpad, &v, &Settings::default(), |ex| ex.fingers = if two { 2 } else { 1 });
    let (sx, sy, sw, sh) = device(pm, &scr, 640.0, 650.0, 960.0, fade(t, 0.0, 0.6));
    // finger path over the pad area (design 32..1432 x 148..892)
    let k = sw / W as f32;
    let u = ((t * 0.9).sin() * 0.5 + 0.5) * 900.0 + 250.0;
    let w = ((t * 1.3).cos() * 0.5 + 0.5) * 420.0 + 300.0;
    let (fx, fy) = (sx + u * k, sy + w * k);
    let fa = fade(t, 0.6, 0.4);
    finger(pm, fx, fy, fa);
    if two {
        finger(pm, fx + 70.0, fy, fa);
    }
    if let Some(p) = st.photos.get("sofa").cloned() {
        image(pm, &p, 1240.0, 330.0, 580.0, 330.0, 24.0, fade(t, 0.8, 0.6));
        label_chip(st, pm, "不用离开沙发", 1262.0, 590.0, fade(t, 1.0, 0.6));
        // the PC's pointer follows the finger
        if !two {
            cursor(pm, 1300.0 + (u - 250.0) / 900.0 * 440.0, 360.0 + (w - 300.0) / 420.0 * 200.0);
        }
    }
    let _ = sh;
    points(st, pm, t, 1.6, 1240.0, 760.0, &["单指移动 · 轻点点击", "双指滚动 · 双指轻点右键"]);
}

fn scene_shortcuts(st: &mut Studio, pm: &mut Pixmap, t: f32) {
    background(pm);
    heading(st, pm, t, "快捷操作 · 宏 · 连发", "一键搞定");
    let v = base_view();
    let (key, tab, flash) = if t < 5.0 {
        let order = [HostAction::VolumeUp, HostAction::SteamScreenshot, HostAction::AltTab, HostAction::PlayPause];
        let i = ((t - 1.0).max(0.0) / 1.0) as usize % order.len();
        let on = (t - 1.0).fract() < 0.35 && t > 1.0;
        (format!("quick{i}{}", on as u8), Tab::Quick, on.then_some(Hit::Action(order[i])))
    } else {
        ("macros".into(), Tab::Macros, None)
    };
    let s = Settings { turbo: button::SOUTH | crate::settings::TURBO_RT, ..Settings::default() };
    let v = View { playing: (tab == Tab::Macros).then_some(0), ..v };
    let scr = st.screen(&key, tab, &v, &s, |ex| ex.flash = flash);
    device(pm, &scr, 640.0, 650.0, 960.0, fade(t, 0.0, 0.6));
    points(st, pm, t, 1.0, 1240.0, 420.0, &["音量、媒体、截图", "Alt+Tab、Esc、显示桌面", "Home 组合键随手触发", "录宏回放 · 按键连发"]);
}

fn scene_audio(st: &mut Studio, pm: &mut Pixmap, t: f32) {
    background(pm);
    heading(st, pm, t, "3.5mm 耳机", "就像插在手柄上");
    if let Some(p) = st.photos.get("headphones").cloned() {
        image(pm, &p, 110.0, 330.0, 760.0, 520.0, 28.0, fade(t, 0.3, 0.6));
    }
    let s = Settings::default();
    let scr = st.screen("audio", Tab::Settings, &base_view(), &s, |_| {});
    device(pm, &scr, 1410.0, 600.0, 760.0, fade(t, 0.6, 0.6));
    // sound flowing from the PC label to the handheld
    text(st, pm, "电脑声音", 970.0, 520.0, 40.0, DIM, fade(t, 1.2, 0.5), true, Align::Center);
    let mut paint = Paint { anti_alias: true, ..Paint::default() };
    for i in 0..8 {
        let p = ((t * 0.6 + i as f32 / 8.0) % 1.0) * fade(t, 1.2, 0.5);
        paint.set_color(rgba(ACCENT, 0.9 * (1.0 - (p - 0.5).abs() * 2.0).max(0.0)));
        pm.fill_path(&circle(900.0 + p * 140.0, 600.0 + (p * TAU).sin() * 30.0, 9.0), &paint, FillRule::Winding, Transform::identity(), None);
    }
    points(st, pm, t, 2.0, 110.0, 960.0, &["耳机麦克风也能当电脑的麦克风"]);
}

fn scene_plugins(st: &mut Studio, pm: &mut Pixmap, t: f32) {
    background(pm);
    heading(st, pm, t, "游戏插件", "小地图放到掌机上");
    let v = View {
        plugin: Some(PluginPanel {
            plugin: "demo".into(),
            title: "插件演示".into(),
            widgets: vec![
                Widget::Image { id: 1, aspect: 1333 },
                Widget::Text { text: "当前游戏".into(), big: true },
                Widget::Text { text: "已玩 42 分钟".into(), big: false },
                Widget::Button { id: 1, label: "Steam 截图".into() },
                Widget::Button { id: 2, label: "叠加层".into() },
                Widget::Button { id: 3, label: "Esc / 暂停".into() },
                Widget::Button { id: 4, label: "Alt + Tab".into() },
            ],
        }),
        ..base_view()
    };
    let frame = Arc::new(radar(t));
    let key = format!("game{}", (t * FPS) as i32);
    let scr = st.screen(&key, Tab::Game, &v, &Settings::default(), |ex| {
        ex.frames.insert(1, frame);
    });
    device(pm, &scr, 640.0, 650.0, 960.0, fade(t, 0.0, 0.6));
    points(st, pm, t, 1.0, 1240.0, 460.0, &["识别正在玩的游戏", "推送图像、文字和按钮", "插件接口开放，自己写"]);
}

fn scene_bluetooth(st: &mut Studio, pm: &mut Pixmap, t: f32) {
    background(pm);
    heading(st, pm, t, "蓝牙模式", "电脑和手机免安装");
    let s = Settings { mode: Mode::Bluetooth, ..Settings::default() };
    let host = |a: &str, n: &str, c| BtHost { addr: parse_addr(a).unwrap(), name: n.into(), connected: c };
    let v = View {
        bluetooth: true,
        host: "我的电脑".into(),
        rtt_ms: None,
        bt_hosts: vec![host("11:22:33:44:55:66", "我的电脑", true), host("11:22:33:44:55:77", "我的手机", false)],
        pairing_left: (t > 3.0).then_some((60.0 - (t - 3.0)) as u32),
        ..base_view()
    };
    let switcher = t <= 3.0;
    let key = if switcher { "bt_switch".to_string() } else { format!("bt_pair{}", (60.0 - (t - 3.0)) as u32) };
    let scr = st.screen(&key, Tab::Overview, &v, &s, |ex| ex.switcher = switcher);
    device(pm, &scr, 640.0, 650.0, 960.0, fade(t, 0.0, 0.6));
    if let Some(p) = st.photos.get("phone").cloned() {
        image(pm, &p, 1240.0, 330.0, 580.0, 330.0, 24.0, fade(t, 0.8, 0.6));
        label_chip(st, pm, "蓝牙设置 → Flip Pad", 1262.0, 590.0, fade(t, 1.0, 0.6));
    }
    points(st, pm, t, 1.4, 1240.0, 760.0, &["按 Xbox 手柄识别", "多台设备随时切换"]);
}

fn scene_end(st: &mut Studio, pm: &mut Pixmap, t: f32) {
    background(pm);
    let a = fade(t, 0.2, 0.8);
    text(st, pm, "Flip Pad", W as f32 / 2.0, 470.0, 150.0, TEXT, a, true, Align::Center);
    text(st, pm, "开源 · Rust · MIT", W as f32 / 2.0, 570.0, 44.0, ACCENT, fade(t, 0.7, 0.6), true, Align::Center);
    text(st, pm, "github.com/simonheard/flipad", W as f32 / 2.0, 680.0, 52.0, TEXT, fade(t, 1.2, 0.6), false, Align::Center);
    let credit = "图片：blakespot、blacklerphotos、tachyondecay、pestoverde（CC BY 2.0，Flickr）";
    text(st, pm, credit, W as f32 / 2.0, 1030.0, 24.0, DIM, fade(t, 1.6, 0.6), false, Align::Center);
    let _ = GOOD;
}

const SCENES: [(&str, f32, SceneFn); 9] = [
    ("01_intro", 6.0, scene_intro),
    ("02_real_footage_placeholder", 8.0, scene_placeholder),
    ("03_controller", 10.0, scene_controller),
    ("04_touchpad", 9.0, scene_touchpad),
    ("05_shortcuts", 10.0, scene_shortcuts),
    ("06_headset", 8.0, scene_audio),
    ("07_plugins", 7.0, scene_plugins),
    ("08_bluetooth", 6.0, scene_bluetooth),
    ("09_end", 6.0, scene_end),
];

fn load_photo(dir: &Path, name: &str) -> Option<Pixmap> {
    Pixmap::load_png(dir.join(format!("{name}.png"))).ok()
}

/// Renders every scene to `DIR/assets/clips/NN_name.mp4`.
pub fn make(dir: &str, only: Option<&str>) -> Result<(), String> {
    let dir = Path::new(dir);
    let photos_dir = dir.join("assets/photos");
    let clips = dir.join("assets/clips");
    std::fs::create_dir_all(&clips).map_err(|e| e.to_string())?;
    let mut photos = HashMap::new();
    for name in ["pc", "headphones", "sofa", "phone"] {
        if let Some(p) = load_photo(&photos_dir, name) {
            photos.insert(name, p);
        }
    }
    let mut st = Studio { text: Text::load()?, photos, ui_cache: HashMap::new() };
    for (name, secs, f) in SCENES {
        if only.is_some_and(|o| !name.contains(o)) {
            continue;
        }
        let out = clips.join(format!("{name}.mp4"));
        eprintln!("rendering {name} ({secs}s)");
        let mut ff = Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgba", "-s", "1920x1080", "-r", "30", "-i", "-"])
            .args(["-c:v", "libx264", "-preset", "medium", "-crf", "18", "-pix_fmt", "yuv420p"])
            .arg(&out)
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| format!("ffmpeg: {e}"))?;
        let mut stdin = ff.stdin.take().unwrap();
        let frames = (secs * FPS) as u32;
        for i in 0..frames {
            let t = i as f32 / FPS;
            let mut pm = Pixmap::new(W, H).unwrap();
            f(&mut st, &mut pm, t);
            // fade to black at the cut
            let edge = (t / 0.25).min((secs - t) / 0.25).min(1.0);
            if edge < 1.0 {
                for px in pm.data_mut().chunks_exact_mut(4) {
                    for c in &mut px[..3] {
                        *c = (*c as f32 * edge) as u8;
                    }
                }
            }
            stdin.write_all(pm.data()).map_err(|e| e.to_string())?;
        }
        drop(stdin);
        ff.wait().map_err(|e| e.to_string())?;
        st.ui_cache.retain(|k, _| !k.starts_with("ctrl") && !k.starts_with("game") && !k.starts_with("bt_pair"));
    }
    Ok(())
}
