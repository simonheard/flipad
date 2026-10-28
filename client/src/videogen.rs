//! Renders the intro video's scenes from the real panel code (hidden
//! `--make-video DIR`). Each scene is drawn frame by frame at 1920×1080 and
//! piped to ffmpeg as one clip. The device is the official Flip 2 product
//! shot (`DIR/assets/photos/flip2-front.png`) with the panel drawn on its
//! screen; a virtual camera pushes in and out and tilts it in perspective.

use std::collections::HashMap;
use std::f32::consts::TAU;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use flipad_proto::{HostAction, Pad, Status, Widget, button};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};

use crate::audio::Jack;
use crate::link::{BtHost, Phase, PluginPanel, View};
use crate::paint::{Align, Canvas, Text, circle, rgba, rrect};
use crate::panel::{self, Hit, Rect, Tab, Ui};
use crate::settings::{Mode, Output, Settings, parse_addr};

const W: u32 = 1920;
const H: u32 = 1080;
const FPS: f32 = 30.0;
const BG_TOP: u32 = 0x10131a;
const BG_BOTTOM: u32 = 0x07080b;
const TEXT: u32 = 0xe9ebf1;
const ACCENT: u32 = 0x6ea8fe;
const DIM: u32 = 0x9aa0af;
/// Left edge of the text column on the right.
const COL: f32 = 1300.0;

/// Where the screen sits in `flip2-front.png` (official product shot,
/// cropped): the panel is tilted back, so it is wider than 16:9.
const SCREEN: (f32, f32, f32, f32) = (406.0, 200.0, 965.0, 415.0);
/// The camera aims at the middle of the screen.
const ANCHOR: (f32, f32) = (SCREEN.0 + SCREEN.2 / 2.0, SCREEN.1 + SCREEN.3 / 2.0);
/// The 3.5 mm jack on the bottom edge, in the same image.
const JACK: (f32, f32) = (990.0, 1179.0);

fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Fade in over [a, a+d].
fn fade(t: f32, a: f32, d: f32) -> f32 {
    ease((t - a) / d)
}

/// Camera framing: the screen's middle lands at (x, y), photo scaled by k.
#[derive(Clone, Copy, Debug)]
struct Pose {
    k: f32,
    x: f32,
    y: f32,
}

/// Device on the left, text column on the right.
const HOME: Pose = Pose { k: 0.95, x: 650.0, y: 340.0 };

/// Pushed in on a point of the panel (design units), still clear of the text.
fn close(fx: f32, fy: f32) -> Pose {
    Pose { k: 1.25, x: 625.0 + (0.5 - fx / W as f32) * 90.0, y: 470.0 + (0.5 - fy / H as f32) * 260.0 }
}

/// Camera path through keyframes, eased between each pair.
fn cam(t: f32, keys: &[(f32, Pose)]) -> Pose {
    let mut p = keys[0].1;
    for w in keys.windows(2) {
        let ((t0, a), (t1, b)) = (w[0], w[1]);
        if t >= t0 {
            let e = ease((t - t0) / (t1 - t0).max(1e-3));
            p = Pose { k: a.k + (b.k - a.k) * e, x: a.x + (b.x - a.x) * e, y: a.y + (b.y - a.y) * e };
        }
    }
    p
}

type Mat = [f32; 9];

fn mul(a: &Mat, b: &Mat) -> Mat {
    let mut r = [0.0; 9];
    for i in 0..3 {
        for j in 0..3 {
            r[i * 3 + j] = (0..3).map(|k| a[i * 3 + k] * b[k * 3 + j]).sum();
        }
    }
    r
}

#[allow(non_snake_case)]
fn inv(m: &Mat) -> Mat {
    let [a, b, c, d, e, f, g, h, i] = *m;
    let (A, B, C) = (e * i - f * h, -(d * i - f * g), d * h - e * g);
    let s = 1.0 / (a * A + b * B + c * C);
    [
        A * s, -(b * i - c * h) * s, (b * f - c * e) * s,
        B * s, (a * i - c * g) * s, -(a * f - c * d) * s,
        C * s, -(a * h - b * g) * s, (a * e - b * d) * s,
    ]
}

fn apply(m: &Mat, x: f32, y: f32) -> (f32, f32) {
    let w = m[6] * x + m[7] * y + m[8];
    ((m[0] * x + m[1] * y + m[2]) / w, (m[3] * x + m[4] * y + m[5]) / w)
}

/// Photo coordinates → frame: the device plane, turned by (ax, ay) radians
/// about the anchor, seen through a lens, then framed by the pose.
fn homography(p: Pose, ax: f32, ay: f32) -> Mat {
    let f = 2600.0;
    let (sx, cx) = ax.sin_cos();
    let (sy, cy) = ay.sin_cos();
    // R = Ry · Rx; only the columns for the plane's x and y axes matter
    let (r11, r12) = (cy, sy * sx);
    let (r21, r22) = (0.0, cx);
    let (r31, r32) = (-sy, cy * sx);
    let to_frame = [
        p.k * f * r11 + p.x * r31, p.k * f * r12 + p.x * r32, p.x * f,
        p.k * f * r21 + p.y * r31, p.k * f * r22 + p.y * r32, p.y * f,
        r31, r32, f,
    ];
    let centre = [1.0, 0.0, -ANCHOR.0, 0.0, 1.0, -ANCHOR.1, 0.0, 0.0, 1.0];
    mul(&to_frame, &centre)
}

/// Draws `src` through homography `m` (source pixels → frame), bilinear,
/// premultiplied source-over.
fn warp(dst: &mut Pixmap, src: &Pixmap, m: &Mat, alpha: f32) {
    if alpha <= 0.0 {
        return;
    }
    let (sw, sh) = (src.width() as f32, src.height() as f32);
    let corners = [(0.0, 0.0), (sw, 0.0), (0.0, sh), (sw, sh)].map(|(x, y)| apply(m, x, y));
    let x0 = corners.iter().map(|c| c.0).fold(f32::MAX, f32::min).floor().max(0.0) as usize;
    let x1 = corners.iter().map(|c| c.0).fold(f32::MIN, f32::max).ceil().min(W as f32) as usize;
    let y0 = corners.iter().map(|c| c.1).fold(f32::MAX, f32::min).floor().max(0.0) as usize;
    let y1 = corners.iter().map(|c| c.1).fold(f32::MIN, f32::max).ceil().min(H as f32) as usize;
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let hi = inv(m);
    let stride = src.width() as usize * 4;
    let s = src.data();
    let d = dst.data_mut();
    for y in y0..y1 {
        let fy = y as f32 + 0.5;
        for x in x0..x1 {
            let fx = x as f32 + 0.5;
            let w = hi[6] * fx + hi[7] * fy + hi[8];
            let u = (hi[0] * fx + hi[1] * fy + hi[2]) / w - 0.5;
            let v = (hi[3] * fx + hi[4] * fy + hi[5]) / w - 0.5;
            if u < 0.0 || v < 0.0 || u >= sw - 1.0 || v >= sh - 1.0 {
                continue;
            }
            let (iu, iv) = (u as usize, v as usize);
            let (du, dv) = (u - iu as f32, v - iv as f32);
            let i00 = iv * stride + iu * 4;
            let (i01, i10) = (i00 + 4, i00 + stride);
            let i11 = i10 + 4;
            let mut px = [0f32; 4];
            for c in 0..4 {
                let top = s[i00 + c] as f32 * (1.0 - du) + s[i01 + c] as f32 * du;
                let bot = s[i10 + c] as f32 * (1.0 - du) + s[i11 + c] as f32 * du;
                px[c] = (top * (1.0 - dv) + bot * dv) * alpha;
            }
            if px[3] < 0.5 {
                continue;
            }
            let o = (y * W as usize + x) * 4;
            let keep = 1.0 - px[3] / 255.0;
            for c in 0..4 {
                d[o + c] = (px[c] + d[o + c] as f32 * keep).min(255.0) as u8;
            }
        }
    }
}

/// Half-size copy (2×2 box filter), so zoomed-out screens don't shimmer.
fn half(src: &Pixmap) -> Pixmap {
    let (w, h) = (src.width() / 2, src.height() / 2);
    let mut out = Pixmap::new(w, h).unwrap();
    let s = src.data();
    let stride = src.width() as usize * 4;
    let d = out.data_mut();
    for y in 0..h as usize {
        for x in 0..w as usize {
            let i = y * 2 * stride + x * 8;
            for c in 0..4 {
                let sum = s[i + c] as u32 + s[i + 4 + c] as u32 + s[i + stride + c] as u32 + s[i + stride + 4 + c] as u32;
                d[(y * w as usize + x) * 4 + c] = (sum / 4) as u8;
            }
        }
    }
    out
}

/// Where things land on the frame for the current camera.
#[derive(Clone, Copy)]
struct Placed {
    m: Mat,
}

impl Placed {
    /// A point of the photo.
    fn at(&self, x: f32, y: f32) -> (f32, f32) {
        apply(&self.m, x, y)
    }
    /// A point on the panel, in design units (1920×1080).
    fn screen(&self, (x, y): (f32, f32)) -> (f32, f32) {
        let (sx, sy, sw, sh) = SCREEN;
        self.at(sx + x / W as f32 * sw, sy + y / H as f32 * sh)
    }
}

/// Fades the photo's edges out so it sits on the dark background.
fn feather(pm: &mut Pixmap) {
    let (w, h) = (pm.width() as usize, pm.height() as usize);
    let ramp = |d: f32, m: f32| ease(d / m);
    for (i, px) in pm.pixels_mut().iter_mut().enumerate() {
        let (x, y) = ((i % w) as f32 / w as f32, (i / w) as f32 / h as f32);
        let a = ramp(x, 0.14) * ramp(1.0 - x, 0.14) * ramp(y, 0.16) * ramp(1.0 - y, 0.07);
        let c = px.demultiply();
        *px = tiny_skia::ColorU8::from_rgba(c.red(), c.green(), c.blue(), (c.alpha() as f32 * a) as u8).premultiply();
    }
}

type Hits = Arc<Vec<(Rect, Hit)>>;

struct Studio {
    text: Text,
    photo: Pixmap,
    /// Rendered panel screens and their touch targets, keyed by state.
    ui_cache: HashMap<String, (Arc<Pixmap>, Hits)>,
    /// The last screen of the previous scene, cross-faded at the cut.
    prev: Option<Arc<Pixmap>>,
    last: Option<Arc<Pixmap>>,
    /// Current scene length, for fading the text out at its end.
    dur: f32,
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

#[derive(Default)]
struct UiExtra {
    flash: Option<Hit>,
    fingers: usize,
    switcher: bool,
    frames: HashMap<u16, Arc<Pixmap>>,
}

impl Studio {
    /// Renders the panel for a state; `key` identifies it for caching.
    fn screen(&mut self, key: &str, tab: Tab, v: &View, s: &Settings, extra: impl FnOnce(&mut UiExtra)) -> (Arc<Pixmap>, Hits) {
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
        let out = (Arc::new(cv.pm), Arc::new(hits));
        if self.ui_cache.len() < 64 {
            self.ui_cache.insert(key.to_string(), out.clone());
        }
        out
    }
}

/// Middle of the first touch target matching `want`, in design units.
fn hit_at(hits: &Hits, want: Hit) -> (f32, f32) {
    hits.iter()
        .find(|(_, h)| *h == want)
        .map(|(r, _)| (r.x + r.w / 2.0, r.y + r.h / 2.0))
        .unwrap_or((960.0, 540.0))
}

fn quad(m: &Mat, x: f32, y: f32, w: f32, h: f32) -> tiny_skia::Path {
    let mut pb = PathBuilder::new();
    let p = [(x, y), (x + w, y), (x + w, y + h), (x, y + h)].map(|(a, b)| apply(m, a, b));
    pb.move_to(p[0].0, p[0].1);
    for q in &p[1..] {
        pb.line_to(q.0, q.1);
    }
    pb.close();
    pb.finish().unwrap()
}

/// Screen state for one frame of the device.
struct Shot<'a> {
    screen: &'a Pixmap,
    pose: Pose,
    /// Tilt in radians: back/forward, left/right.
    tilt: (f32, f32),
    alpha: f32,
    /// Display brightness 0..1 (0 = off), plus a white flash when it wakes.
    power: f32,
    flash: f32,
    /// Cross-fade from the previous scene's screen, 0..1.
    mix: f32,
}

/// The Flip 2 photo with the panel on its display, through the camera.
/// While fading, it is drawn whole on a layer first so the photo's own
/// screen never shows through.
fn device(st: &mut Studio, pm: &mut Pixmap, shot: Shot) -> Placed {
    if shot.alpha < 1.0 {
        if shot.alpha <= 0.0 {
            return Placed { m: homography(shot.pose, shot.tilt.0, shot.tilt.1) };
        }
        let mut layer = Pixmap::new(W, H).unwrap();
        let placed = device(st, &mut layer, Shot { alpha: 1.0, ..shot });
        let paint = tiny_skia::PixmapPaint { opacity: shot.alpha, ..Default::default() };
        pm.draw_pixmap(0, 0, layer.as_ref(), &paint, Transform::identity(), None);
        return placed;
    }
    let m = homography(shot.pose, shot.tilt.0, shot.tilt.1);
    warp(pm, &st.photo, &m, shot.alpha);
    let (sx, sy, sw, sh) = SCREEN;
    let mut paint = Paint { anti_alias: true, ..Paint::default() };
    paint.set_color(Color::from_rgba(0.0, 0.0, 0.0, shot.alpha).unwrap());
    pm.fill_path(&quad(&m, sx, sy, sw, sh), &paint, FillRule::Winding, Transform::identity(), None);
    // panel pixels → photo → frame; the half-size copy when shrinking a lot
    let small = shot.pose.k * sw / (W as f32) < 0.7;
    let draw = |pm: &mut Pixmap, src: &Pixmap, a: f32| {
        let scaled;
        let (src, f) = if small {
            scaled = half(src);
            (&scaled, 2.0)
        } else {
            (src, 1.0)
        };
        let to_photo = [sw / W as f32 * f, 0.0, sx, 0.0, sh / H as f32 * f, sy, 0.0, 0.0, 1.0];
        warp(pm, src, &mul(&m, &to_photo), a);
    };
    let on = shot.alpha * shot.power;
    if shot.mix < 1.0 {
        if let Some(prev) = &st.prev {
            draw(pm, prev, on);
        }
    }
    draw(pm, shot.screen, on * shot.mix);
    if shot.flash > 0.0 {
        paint.set_color(Color::from_rgba(1.0, 1.0, 1.0, (shot.flash * shot.alpha).min(1.0)).unwrap());
        pm.fill_path(&quad(&m, sx, sy, sw, sh), &paint, FillRule::Winding, Transform::identity(), None);
    }
    // a soft glass sheen that slides as the device turns
    let shift = shot.tilt.1 * 3.0;
    let (g0, g1) = (apply(&m, sx, sy), apply(&m, sx + sw, sy + sh));
    if let Some(shader) = tiny_skia::LinearGradient::new(
        tiny_skia::Point::from_xy(g0.0, g0.1),
        tiny_skia::Point::from_xy(g1.0, g1.1),
        vec![
            tiny_skia::GradientStop::new((0.25 + shift).clamp(0.0, 1.0), Color::from_rgba(1.0, 1.0, 1.0, 0.0).unwrap()),
            tiny_skia::GradientStop::new((0.4 + shift).clamp(0.0, 1.0), Color::from_rgba(1.0, 1.0, 1.0, 0.07 * shot.alpha).unwrap()),
            tiny_skia::GradientStop::new((0.55 + shift).clamp(0.0, 1.0), Color::from_rgba(1.0, 1.0, 1.0, 0.0).unwrap()),
        ],
        tiny_skia::SpreadMode::Pad,
        Transform::identity(),
    ) {
        let p = Paint { shader, anti_alias: true, ..Paint::default() };
        pm.fill_path(&quad(&m, sx, sy, sw, sh), &p, FillRule::Winding, Transform::identity(), None);
    }
    st.last = Some(Arc::new(shot.screen.clone()));
    Placed { m }
}

/// Idle sway plus a lean into camera moves.
fn sway(t: f32, keys: &[(f32, Pose)]) -> (Pose, (f32, f32)) {
    let p = cam(t, keys);
    let q = cam(t + 1.0 / FPS, keys);
    let (vx, vk) = ((q.x - p.x) * FPS, (q.k - p.k) * FPS);
    let ax = 0.035 * (t * 0.7).sin() + vk * 0.12;
    let ay = 0.05 * (t * 0.5 + 1.0).sin() - vx * 0.00035;
    let p = Pose { x: p.x + (t * 0.8).sin() * 5.0, y: p.y + (t * 0.6).cos() * 4.0, ..p };
    (p, (ax, ay))
}

/// One scene's device frame with the usual cross-fade at its start.
fn shoot(st: &mut Studio, pm: &mut Pixmap, t: f32, screen: &Pixmap, keys: &[(f32, Pose)], shake: f32) -> Placed {
    let (mut pose, tilt) = sway(t, keys);
    pose.x += (t * 95.0).sin() * shake;
    pose.y += (t * 77.0).cos() * shake * 0.6;
    device(st, pm, Shot { screen, pose, tilt, alpha: 1.0, power: 1.0, flash: 0.0, mix: fade(t, 0.0, 0.4) })
}

/// Background gradient with a slow glow behind the device.
fn background(pm: &mut Pixmap, t: f32) {
    if let Some(shader) = tiny_skia::LinearGradient::new(
        tiny_skia::Point::from_xy(0.0, 0.0),
        tiny_skia::Point::from_xy(0.0, H as f32),
        vec![tiny_skia::GradientStop::new(0.0, rgba(BG_TOP, 1.0)), tiny_skia::GradientStop::new(1.0, rgba(BG_BOTTOM, 1.0))],
        tiny_skia::SpreadMode::Pad,
        Transform::identity(),
    ) {
        let paint = Paint { shader, ..Paint::default() };
        pm.fill_rect(tiny_skia::Rect::from_xywh(0.0, 0.0, W as f32, H as f32).unwrap(), &paint, Transform::identity(), None);
    }
    let (gx, gy) = (700.0 + (t * 0.3).sin() * 120.0, 520.0 + (t * 0.23).cos() * 60.0);
    if let Some(shader) = tiny_skia::RadialGradient::new(
        tiny_skia::Point::from_xy(gx, gy),
        tiny_skia::Point::from_xy(gx, gy),
        900.0,
        vec![
            tiny_skia::GradientStop::new(0.0, Color::from_rgba8(110, 70, 220, 46)),
            tiny_skia::GradientStop::new(1.0, Color::from_rgba8(110, 70, 220, 0)),
        ],
        tiny_skia::SpreadMode::Pad,
        Transform::identity(),
    ) {
        let paint = Paint { shader, ..Paint::default() };
        pm.fill_rect(tiny_skia::Rect::from_xywh(0.0, 0.0, W as f32, H as f32).unwrap(), &paint, Transform::identity(), None);
    }
}

#[allow(clippy::too_many_arguments)]
fn text(st: &mut Studio, pm: &mut Pixmap, s: &str, x: f32, y: f32, size: f32, color: u32, alpha: f32, bold: bool, align: Align) {
    if alpha <= 0.01 {
        return;
    }
    let mut cv = Canvas { pm: std::mem::replace(pm, Pixmap::new(1, 1).unwrap()), ts: Transform::identity(), scale: 1.0, ox: 0.0, oy: 0.0 };
    st.text.draw(&mut cv, s, x, y, size, rgba(color, alpha), bold, align);
    *pm = cv.pm;
}

/// 1 while the scene's text should show, easing out at the end.
fn outro(st: &Studio, t: f32) -> f32 {
    1.0 - fade(t, st.dur - 0.5, 0.45)
}

/// Darkens the right side so the text column stays readable over the device.
fn scrim(pm: &mut Pixmap) {
    if let Some(shader) = tiny_skia::LinearGradient::new(
        tiny_skia::Point::from_xy(COL - 160.0, 0.0),
        tiny_skia::Point::from_xy(COL - 20.0, 0.0),
        vec![
            tiny_skia::GradientStop::new(0.0, Color::from_rgba8(9, 10, 14, 0)),
            tiny_skia::GradientStop::new(1.0, Color::from_rgba8(9, 10, 14, 235)),
        ],
        tiny_skia::SpreadMode::Pad,
        Transform::identity(),
    ) {
        let paint = Paint { shader, ..Paint::default() };
        let x = COL - 160.0;
        pm.fill_rect(tiny_skia::Rect::from_xywh(x, 0.0, W as f32 - x, H as f32).unwrap(), &paint, Transform::identity(), None);
    }
}

/// Scene heading in the text column: kicker, then title sliding up.
fn heading(st: &mut Studio, pm: &mut Pixmap, t: f32, kicker: &str, title: &str) {
    scrim(pm);
    let out = outro(st, t);
    let a = fade(t, 0.2, 0.45) * out;
    let b = fade(t, 0.35, 0.5) * out;
    let mut paint = Paint { anti_alias: true, ..Paint::default() };
    paint.set_color(rgba(ACCENT, a));
    let bar = 60.0 * fade(t, 0.2, 0.5);
    pm.fill_path(&rrect(COL, 268.0, bar.max(6.0), 6.0, 3.0), &paint, FillRule::Winding, Transform::identity(), None);
    text(st, pm, kicker, COL, 330.0, 32.0, ACCENT, a, true, Align::Left);
    text(st, pm, title, COL, 415.0 + (1.0 - b) * 40.0, 64.0, TEXT, b, true, Align::Left);
}

/// Bullet points that appear at `times`; the newest one is highlighted.
fn points(st: &mut Studio, pm: &mut Pixmap, t: f32, times: &[f32], items: &[&str]) {
    let out = outro(st, t);
    let current = times.iter().rposition(|&s| t >= s);
    for (i, s) in items.iter().enumerate() {
        let a = fade(t, times[i], 0.4) * out;
        let now = current == Some(i);
        let lit = if now { 1.0 } else { 0.5 };
        let dx = (1.0 - a) * 30.0;
        let yy = 540.0 + i as f32 * 80.0;
        let mut paint = Paint { anti_alias: true, ..Paint::default() };
        paint.set_color(rgba(ACCENT, a * if now { 1.0 } else { 0.7 }));
        pm.fill_path(&circle(COL + dx + 10.0, yy - 14.0, if now { 9.0 } else { 6.0 }), &paint, FillRule::Winding, Transform::identity(), None);
        text(st, pm, s, COL + 40.0 + dx, yy, 38.0, TEXT, a * lit, now, Align::Left);
    }
}

fn finger(pm: &mut Pixmap, (x, y): (f32, f32), r: f32, alpha: f32) {
    if alpha <= 0.0 {
        return;
    }
    let mut paint = Paint { anti_alias: true, ..Paint::default() };
    paint.set_color(Color::from_rgba(1.0, 1.0, 1.0, 0.3 * alpha).unwrap());
    pm.fill_path(&circle(x, y, r), &paint, FillRule::Winding, Transform::identity(), None);
    paint.set_color(Color::from_rgba(1.0, 1.0, 1.0, 0.9 * alpha).unwrap());
    pm.stroke_path(&circle(x, y, r), &paint, &Stroke { width: 3.0, ..Stroke::default() }, Transform::identity(), None);
}

fn ripple(pm: &mut Pixmap, (x, y): (f32, f32), phase: f32) {
    if !(0.0..1.0).contains(&phase) {
        return;
    }
    let mut paint = Paint { anti_alias: true, ..Paint::default() };
    paint.set_color(Color::from_rgba(1.0, 1.0, 1.0, 0.8 * (1.0 - phase)).unwrap());
    pm.stroke_path(&circle(x, y, 24.0 + phase * 60.0), &paint, &Stroke { width: 4.0, ..Stroke::default() }, Transform::identity(), None);
}

/// A finger that comes in, taps `at` at time t0 and leaves.
fn tap(pm: &mut Pixmap, at: (f32, f32), t: f32, t0: f32) {
    let d = t - t0;
    if !(-0.4..0.7).contains(&d) {
        return;
    }
    let come = ease((d + 0.4) / 0.3);
    let go = 1.0 - fade(d, 0.3, 0.35);
    let press = if (0.0..0.15).contains(&d) { 0.78 } else { 1.0 };
    let off = 1.0 - come;
    finger(pm, (at.0 + off * 50.0, at.1 + off * 70.0), 28.0 * press, come * go);
    ripple(pm, at, d / 0.5);
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
    pm.fill_path(&circle(cx + r * 0.5 + (t * 0.7).sin() * 10.0, cy - r * 0.3, 8.0), &paint, FillRule::Winding, Transform::identity(), None);
    paint.set_color_rgba8(250, 210, 80, 255);
    pm.fill_path(&circle(cx - r * 0.35, cy + r * 0.45, 6.0), &paint, FillRule::Winding, Transform::identity(), None);
    pm
}

type SceneFn = fn(&mut Studio, &mut Pixmap, f32);

fn scene_intro(st: &mut Studio, pm: &mut Pixmap, t: f32) {
    background(pm, t);
    let (scr, _) = st.screen("intro", Tab::Overview, &base_view(), &Settings::default(), |_| {});
    let keys = [
        (0.0, Pose { k: 0.62, x: 960.0, y: 760.0 }),
        (1.6, Pose { k: 0.8, x: 960.0, y: 470.0 }),
        (4.6, Pose { k: 0.86, x: 960.0, y: 455.0 }),
        (6.0, HOME),
    ];
    let (pose, (ax, ay)) = sway(t, &keys);
    // rises tilted back, settles upright, then the screen wakes
    let ax = ax + 0.45 * (1.0 - ease(t / 1.6));
    let flash = if t > 1.5 { 0.6 * (1.0 - fade(t, 1.55, 0.4)) } else { 0.0 };
    device(st, pm, Shot { screen: &scr, pose, tilt: (ax, ay), alpha: fade(t, 0.0, 0.7), power: fade(t, 1.5, 0.15), flash, mix: 1.0 });
    let a = fade(t, 2.0, 0.6) * (1.0 - fade(t, 4.8, 0.5));
    text(st, pm, "你的掌机，也可以是电脑的手柄。", W as f32 / 2.0, 140.0 - (1.0 - fade(t, 2.0, 0.6)) * 30.0, 60.0, TEXT, a, true, Align::Center);
    let b = fade(t, 2.6, 0.6) * (1.0 - fade(t, 4.8, 0.5));
    text(st, pm, "Retroid Pocket Flip 2 · 已刷 Linux（Pocknix），不是原厂安卓", W as f32 / 2.0, 205.0, 34.0, ACCENT, b, true, Align::Center);
}

/// Live controller on the overview: sticks circling, buttons, rumble shakes.
fn controller(st: &mut Studio, pm: &mut Pixmap, t: f32, keys: &[(f32, Pose)], rumble_from: f32) {
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
    let rumble = t > rumble_from && (t - rumble_from) % 2.0 < 0.5;
    let v = View { pad, rumble, ..base_view() };
    let key = format!("ctrl{}", (t * FPS) as i32);
    let (scr, _) = st.screen(&key, Tab::Overview, &v, &Settings::default(), |_| {});
    shoot(st, pm, t, &scr, keys, if rumble { 4.0 } else { 0.0 });
}

/// Why this exists: moving house, the PS5 is packed, 影之刃零 is out.
fn scene_story(st: &mut Studio, pm: &mut Pixmap, t: f32) {
    background(pm, t);
    let keys = [(0.0, HOME), (3.0, HOME), (4.0, close(560.0, 560.0)), (9.0, close(560.0, 560.0)), (10.4, HOME), (12.0, HOME)];
    controller(st, pm, t, &keys, 99.0);
    heading(st, pm, t, "起因", "影之刃零，憋不住了");
    points(st, pm, t, &[0.4, 2.0, 6.8], &["搬家，PS5 和手柄全打包了", "影之刃零马上解禁", "手边只有一台 Flip 2"]);
}

/// The handheld runs Linux, not the stock Android: said loud and clear.
fn scene_linux(st: &mut Studio, pm: &mut Pixmap, t: f32) {
    background(pm, t);
    let (scr, _) = st.screen("intro", Tab::Overview, &base_view(), &Settings::default(), |_| {});
    let hero = Pose { k: 0.8, x: 960.0, y: 520.0 };
    let keys = [(0.0, HOME), (1.2, hero), (6.6, Pose { k: 0.84, ..hero }), (8.0, HOME)];
    shoot(st, pm, t, &scr, &keys, 0.0);
    let out = outro(st, t);
    let a = fade(t, 1.4, 0.5) * out;
    text(st, pm, "这台掌机刷的是 Linux", W as f32 / 2.0, 150.0 - (1.0 - a) * 30.0, 72.0, TEXT, a, true, Align::Center);
    let b = fade(t, 3.4, 0.5) * out;
    text(st, pm, "Pocknix（Arch Linux ARM）· 不是原厂安卓，安卓版没做", W as f32 / 2.0, 225.0, 36.0, ACCENT, b, true, Align::Center);
}

/// Xbox controller to the PC; one tap swaps to the Nintendo layout.
fn scene_layout(st: &mut Studio, pm: &mut Pixmap, t: f32) {
    background(pm, t);
    let s = Settings { nintendo: t > 4.7, ..Settings::default() };
    let (scr, hits) = st.screen(&format!("layout{}", t > 4.7), Tab::Settings, &base_view(), &s, |_| {});
    let at = hit_at(&hits, Hit::Nintendo(true));
    let near = close(at.0, at.1);
    let keys = [(0.0, HOME), (1.2, near), (5.4, near), (6.0, HOME)];
    let p = shoot(st, pm, t, &scr, &keys, 0.0);
    tap(pm, p.screen(at), t, 4.6);
    heading(st, pm, t, "WI-FI · 约 2 毫秒", "无线 Xbox 手柄");
    points(st, pm, t, &[1.4, 4.4], &["电脑识别为 Xbox 手柄", "一键换成 Switch 键位"]);
}

fn scene_touchpad(st: &mut Studio, pm: &mut Pixmap, t: f32) {
    background(pm, t);
    let fingers = if (4.6..6.5).contains(&t) { 2 } else if (1.2..4.2).contains(&t) { 1 } else { 0 };
    let (scr, _) = st.screen(&format!("tp{fingers}"), Tab::Touchpad, &base_view(), &Settings::default(), |ex| ex.fingers = fingers);
    let keys = [(0.0, HOME), (1.0, close(730.0, 520.0)), (6.6, close(730.0, 520.0)), (7.6, HOME), (8.0, HOME)];
    let p = shoot(st, pm, t, &scr, &keys, 0.0);
    // one finger wanders and taps; then two fingers scroll and tap
    if fingers == 1 {
        let s = t - 1.2;
        let at = p.screen((300.0 + s * 260.0 + (s * 2.0).sin() * 60.0, 520.0 + (s * 1.7).sin() * 220.0));
        let a = fade(t, 1.2, 0.25) * (1.0 - fade(t, 3.9, 0.3));
        finger(pm, at, if (2.9..3.05).contains(&t) { 22.0 } else { 28.0 }, a);
        ripple(pm, at, (t - 2.9) / 0.5);
    } else if fingers == 2 {
        let s = t - 4.6;
        let w = 700.0 - (s * 2.2).sin().abs() * 360.0;
        let a = fade(t, 4.6, 0.25) * (1.0 - fade(t, 6.2, 0.3));
        let r = if (5.9..6.05).contains(&t) { 22.0 } else { 28.0 };
        for u in [620.0, 800.0] {
            let at = p.screen((u, w));
            finger(pm, at, r, a);
            ripple(pm, at, (t - 5.9) / 0.5);
        }
    }
    heading(st, pm, t, "触控板", "屏幕就是触控板");
    points(st, pm, t, &[0.6, 4.6, 5.9], &["单指移动，轻点点击", "双指滚动", "双指轻点 = 右键"]);
}

fn scene_shortcuts(st: &mut Studio, pm: &mut Pixmap, t: f32) {
    background(pm, t);
    let taps = [(1.6, HostAction::VolumeUp), (2.4, HostAction::SteamScreenshot), (3.2, HostAction::AltTab), (4.0, HostAction::PlayPause)];
    let s0 = Settings { turbo: button::SOUTH | crate::settings::TURBO_RT, ..Settings::default() };
    let (quick, quick_hits) = st.screen("quick", Tab::Quick, &base_view(), &s0, |_| {});
    let (_, macro_hits) = st.screen("macros_hits", Tab::Macros, &base_view(), &s0, |_| {});
    let flashing = taps.iter().find(|(t0, _)| (*t0..*t0 + 0.35).contains(&t)).map(|(_, a)| *a);
    let scr = if t < 5.0 {
        match flashing {
            Some(a) => st.screen(&format!("quick{}", a as u8), Tab::Quick, &base_view(), &s0, |ex| ex.flash = Some(Hit::Action(a))).0,
            None => quick,
        }
    } else {
        let v = View { playing: (t > 6.7).then_some(0), ..base_view() };
        let s1 = Settings { turbo: s0.turbo | if t > 8.1 { button::NORTH } else { 0 }, ..s0 };
        st.screen(&format!("macros{}{}", t > 6.7, t > 8.1), Tab::Macros, &v, &s1, |_| {}).0
    };
    let keys = [
        (0.0, HOME),
        (1.0, close(960.0, 520.0)),
        (4.6, close(960.0, 520.0)),
        (5.6, close(1150.0, 560.0)),
        (9.0, close(1150.0, 560.0)),
        (9.8, HOME),
        (10.0, HOME),
    ];
    let p = shoot(st, pm, t, &scr, &keys, 0.0);
    for (t0, a) in taps {
        tap(pm, p.screen(hit_at(&quick_hits, Hit::Action(a))), t, t0);
    }
    tap(pm, p.screen(hit_at(&quick_hits, Hit::Tab(Tab::Macros))), t, 4.9);
    tap(pm, p.screen(hit_at(&macro_hits, Hit::Play(0))), t, 6.6);
    tap(pm, p.screen(hit_at(&macro_hits, Hit::Turbo(button::NORTH))), t, 8.0);
    heading(st, pm, t, "快捷操作 · 宏 · 连发", "一键搞定");
    points(st, pm, t, &[0.6, 1.8, 3.0, 6.0], &["音量、媒体、截图", "Alt+Tab、Esc、显示桌面", "Home 组合键直接触发", "录宏回放，按键连发"]);
}

fn scene_audio(st: &mut Studio, pm: &mut Pixmap, t: f32) {
    background(pm, t);
    let s = Settings { output: if t > 5.7 { Output::Speaker } else { Output::Follow }, ..Settings::default() };
    let (scr, hits) = st.screen(&format!("audio{}", t > 5.7), Tab::Settings, &base_view(), &s, |_| {});
    let out_btn = hit_at(&hits, Hit::Output(Output::Speaker));
    let wide = Pose { k: 0.78, x: 650.0, y: 330.0 };
    let near = close(out_btn.0, out_btn.1);
    let keys = [(0.0, HOME), (1.2, wide), (4.0, wide), (4.9, near), (7.0, near), (8.0, HOME)];
    let p = shoot(st, pm, t, &scr, &keys, 0.0);
    // pulse rings around the jack
    let (jx, jy) = p.at(JACK.0, JACK.1);
    let on = fade(t, 1.3, 0.4) * (1.0 - fade(t, 4.0, 0.4));
    let mut paint = Paint { anti_alias: true, ..Paint::default() };
    for i in 0..3 {
        let ph = (t * 0.8 + i as f32 / 3.0) % 1.0;
        paint.set_color(rgba(ACCENT, on * (1.0 - ph) * 0.9));
        pm.stroke_path(&circle(jx, jy, 10.0 + ph * 60.0), &paint, &Stroke { width: 4.0, ..Stroke::default() }, Transform::identity(), None);
    }
    paint.set_color(rgba(ACCENT, on));
    pm.fill_path(&circle(jx, jy, 7.0), &paint, FillRule::Winding, Transform::identity(), None);
    label_chip(st, pm, "耳机插这里", jx + 70.0, jy - 50.0, on);
    tap(pm, p.screen(out_btn), t, 5.6);
    heading(st, pm, t, "3.5mm 耳机", "就像插在手柄上");
    points(st, pm, t, &[1.4, 2.6, 5.2], &["电脑声音传到掌机耳机", "耳机麦克风传回电脑", "也能选掌机外放"]);
}

fn scene_plugins(st: &mut Studio, pm: &mut Pixmap, t: f32) {
    background(pm, t);
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
    let (over, over_hits) = st.screen("plug_over", Tab::Overview, &v, &Settings::default(), |_| {});
    let frame = Arc::new(radar(t));
    let flash = (5.0..5.35).contains(&t).then_some(Hit::PluginButton(1));
    let (scr, hits) = if t < 1.0 {
        (over, over_hits.clone())
    } else {
        st.screen(&format!("game{}", (t * FPS) as i32), Tab::Game, &v, &Settings::default(), |ex| {
            ex.frames.insert(1, frame);
            ex.flash = flash;
        })
    };
    let keys = [(0.0, HOME), (1.2, close(500.0, 480.0)), (6.6, close(500.0, 480.0)), (7.6, HOME), (8.0, HOME)];
    let p = shoot(st, pm, t, &scr, &keys, 0.0);
    tap(pm, p.screen(hit_at(&over_hits, Hit::Tab(Tab::Game))), t, 0.9);
    tap(pm, p.screen(hit_at(&hits, Hit::PluginButton(1))), t, 5.0);
    heading(st, pm, t, "游戏插件", "小地图放到掌机上");
    points(st, pm, t, &[1.0, 2.6, 4.4], &["识别正在玩的游戏", "推送图像、文字和按钮", "插件接口开放"]);
}

fn scene_bluetooth(st: &mut Studio, pm: &mut Pixmap, t: f32) {
    background(pm, t);
    let s = Settings { mode: Mode::Bluetooth, ..Settings::default() };
    let host = |a: &str, n: &str, c| BtHost { addr: parse_addr(a).unwrap(), name: n.into(), connected: c };
    let v = View {
        bluetooth: true,
        host: "我的电脑".into(),
        rtt_ms: None,
        bt_hosts: vec![host("11:22:33:44:55:66", "我的电脑", true), host("11:22:33:44:55:77", "我的手机", false)],
        ..base_view()
    };
    let (plain, base_hits) = st.screen("bt", Tab::Overview, &v, &s, |_| {});
    let (switcher, sw_hits) = st.screen("bt_switch", Tab::Overview, &v, &s, |ex| ex.switcher = true);
    let scr = if t < 1.1 {
        plain
    } else if t < 2.7 {
        switcher
    } else {
        let left = (60.0 - (t - 2.7)) as u32;
        let v = View { pairing_left: Some(left), ..v.clone() };
        st.screen(&format!("bt_pair{left}"), Tab::Overview, &v, &s, |_| {}).0
    };
    let keys = [(0.0, HOME), (0.8, close(800.0, 420.0)), (5.2, close(800.0, 420.0)), (6.0, HOME)];
    let p = shoot(st, pm, t, &scr, &keys, 0.0);
    tap(pm, p.screen(hit_at(&base_hits, Hit::OpenSwitcher)), t, 1.0);
    tap(pm, p.screen(hit_at(&sw_hits, Hit::PairNew)), t, 2.6);
    heading(st, pm, t, "蓝牙模式", "电脑手机免安装");
    points(st, pm, t, &[0.5, 2.0, 3.4], &["蓝牙设置里选 Flip Pad", "多台设备随时切换", "按 Xbox 手柄识别"]);
}

fn scene_end(st: &mut Studio, pm: &mut Pixmap, t: f32) {
    background(pm, t);
    if let Some(prev) = st.prev.clone() {
        // the device backs away and fades as the title comes in
        let keys = [(0.0, HOME), (1.4, Pose { k: 0.5, x: 960.0, y: 260.0 })];
        let (pose, (ax, ay)) = sway(t, &keys);
        let ax = ax + 0.5 * ease(t / 1.4);
        device(st, pm, Shot { screen: &prev, pose, tilt: (ax, ay), alpha: 1.0 - fade(t, 0.3, 1.0), power: 1.0, flash: 0.0, mix: 1.0 });
    }
    let a = fade(t, 0.8, 0.7);
    text(st, pm, "Flip Pad", W as f32 / 2.0, 500.0 + (1.0 - a) * 40.0, 150.0, TEXT, a, true, Align::Center);
    text(st, pm, "开源 · Rust · MIT · 不是教程，纯分享", W as f32 / 2.0, 600.0, 44.0, ACCENT, fade(t, 1.3, 0.6), true, Align::Center);
    text(st, pm, "github.com/simonheard/flipad", W as f32 / 2.0, 710.0, 52.0, TEXT, fade(t, 1.8, 0.6), false, Align::Center);
    text(st, pm, "有需求欢迎提 PR", W as f32 / 2.0, 790.0, 36.0, DIM, fade(t, 2.0, 0.6), false, Align::Center);
    text(st, pm, "产品图片来自 Retroid 官网", W as f32 / 2.0, 1030.0, 24.0, DIM, fade(t, 2.2, 0.6), false, Align::Center);
}

/// (name, length on screen, length the choreography was written for, scene).
/// Lengths are whole bars at 120 BPM so cuts land on the music; the scene's
/// clock runs at base/length so a scene can be stretched to fit the voice.
const SCENES: [(&str, f32, f32, SceneFn); 10] = [
    ("01_intro", 8.0, 6.0, scene_intro),
    ("02_story", 12.0, 12.0, scene_story),
    ("03_linux", 8.0, 8.0, scene_linux),
    ("04_layout", 6.0, 6.0, scene_layout),
    ("05_touchpad", 6.0, 8.0, scene_touchpad),
    ("06_shortcuts", 10.0, 10.0, scene_shortcuts),
    ("07_headset", 8.0, 8.0, scene_audio),
    ("08_plugins", 8.0, 8.0, scene_plugins),
    ("09_bluetooth", 8.0, 6.0, scene_bluetooth),
    ("10_end", 6.0, 6.0, scene_end),
];

/// Renders every scene to `DIR/assets/clips/NN_name.mp4`. With `only`, just
/// the scenes whose name contains it (their cuts then fade from nothing).
pub fn make(dir: &str, only: Option<&str>) -> Result<(), String> {
    let dir = Path::new(dir);
    let clips = dir.join("assets/clips");
    std::fs::create_dir_all(&clips).map_err(|e| e.to_string())?;
    let photo_path = dir.join("assets/photos/flip2-front.png");
    let mut photo = Pixmap::load_png(&photo_path).map_err(|e| format!("{}: {e}", photo_path.display()))?;
    feather(&mut photo);
    let mut st = Studio { text: Text::load()?, photo, ui_cache: HashMap::new(), prev: None, last: None, dur: 0.0 };
    for (n, (name, secs, base, f)) in SCENES.into_iter().enumerate() {
        if only.is_some_and(|o| !name.contains(o)) {
            continue;
        }
        st.dur = base;
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
            f(&mut st, &mut pm, t * base / secs);
            // fade from/to black only at the very start and end
            let edge = if n == 0 {
                (t / 0.4).min(1.0)
            } else if n == SCENES.len() - 1 {
                ((secs - t) / 0.6).min(1.0)
            } else {
                1.0
            };
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
        st.prev = st.last.take();
        st.ui_cache.retain(|k, _| !k.starts_with("ctrl") && !k.starts_with("game") && !k.starts_with("bt_pair"));
    }
    Ok(())
}
