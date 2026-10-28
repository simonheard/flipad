//! Full-screen status page shown on the handheld while it acts as a controller.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use flipad_proto::{Pad, button};
use tiny_skia::{Pixmap, Transform};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, TouchPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::window::{Fullscreen, Window, WindowId};

use crate::link::{Phase, Shared, View};
use crate::paint::{Align, Canvas, Text, circle, rgba, rrect};

const W: f32 = 1920.0;
const H: f32 = 1080.0;
const FRAME: Duration = Duration::from_millis(33);

const BG: u32 = 0x0d0f13;
const CARD: u32 = 0x161920;
const TEXT: u32 = 0xe9ebf1;
const DIM: u32 = 0x8a90a0;
const FAINT: u32 = 0x3a3f4b;
const ACCENT: u32 = 0x6ea8fe;
const GOOD: u32 = 0x4ade80;
const WARN: u32 = 0xf5b041;
const BAD: u32 = 0xf87171;

/// The exit button, in design units.
const EXIT_BTN: (f32, f32, f32, f32) = (1572.0, 932.0, 300.0, 96.0);

pub struct App {
    shared: Arc<Shared>,
    text: Option<Text>,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    canvas: Option<Canvas>,
    cursor: (f64, f64),
    dirty: bool,
    last_draw: Instant,
    battery: (Option<u8>, bool),
    battery_read: Instant,
}

impl App {
    pub fn new(shared: Arc<Shared>) -> Self {
        let text = Text::load().map_err(|e| eprintln!("fonts: {e}")).ok();
        Self {
            shared,
            text,
            window: None,
            surface: None,
            canvas: None,
            cursor: (0.0, 0.0),
            dirty: true,
            last_draw: Instant::now() - FRAME,
            battery: (None, false),
            battery_read: Instant::now() - Duration::from_secs(60),
        }
    }

    fn quit(&self, el: &ActiveEventLoop) {
        self.shared.quit.store(true, Ordering::Relaxed);
        el.exit();
    }

    fn hit_exit(&self, x: f64, y: f64) -> bool {
        let Some(cv) = &self.canvas else { return false };
        let (dx, dy) = ((x as f32 - cv.ox) / cv.scale, (y as f32 - cv.oy) / cv.scale);
        let (bx, by, bw, bh) = EXIT_BTN;
        dx >= bx && dx <= bx + bw && dy >= by && dy <= by + bh
    }

    fn draw(&mut self) {
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else { return };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        if self.canvas.as_ref().is_none_or(|c| c.pm.width() != size.width || c.pm.height() != size.height) {
            let scale = (size.width as f32 / W).min(size.height as f32 / H);
            let (ox, oy) = ((size.width as f32 - W * scale) / 2.0, (size.height as f32 - H * scale) / 2.0);
            self.canvas = Some(Canvas {
                pm: Pixmap::new(size.width, size.height).unwrap(),
                ts: Transform::from_row(scale, 0.0, 0.0, scale, ox, oy),
                scale,
                ox,
                oy,
            });
        }
        if self.battery_read.elapsed() > Duration::from_secs(20) {
            self.battery = read_battery();
            self.battery_read = Instant::now();
        }
        let view = self.shared.view.lock().unwrap().clone();
        let cv = self.canvas.as_mut().unwrap();
        cv.pm.fill(rgba(BG, 1.0));
        if let Some(text) = &mut self.text {
            render(cv, text, &view, self.battery);
        }
        if surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buf) = surface.buffer_mut() else { return };
        for (dst, px) in buf.iter_mut().zip(cv.pm.pixels()) {
            *dst = (px.red() as u32) << 16 | (px.green() as u32) << 8 | px.blue() as u32;
        }
        let _ = buf.present();
        self.last_draw = Instant::now();
        self.dirty = false;
    }
}

impl ApplicationHandler<()> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Flip Pad")
            .with_fullscreen(Some(Fullscreen::Borderless(None)));
        let window = match el.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                eprintln!("window: {e}");
                return self.quit(el);
            }
        };
        window.set_cursor_visible(false);
        let surface = softbuffer::Context::new(window.clone()).and_then(|ctx| softbuffer::Surface::new(&ctx, window.clone()));
        match surface {
            Ok(s) => self.surface = Some(s),
            Err(e) => {
                eprintln!("surface: {e}");
                return self.quit(el);
            }
        }
        self.window = Some(window);
        self.dirty = true;
    }

    fn user_event(&mut self, el: &ActiveEventLoop, _: ()) {
        if self.shared.quit.load(Ordering::Relaxed) {
            el.exit();
        }
        self.dirty = true;
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => self.quit(el),
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::Resized(_) => self.dirty = true,
            WindowEvent::CursorMoved { position, .. } => self.cursor = (position.x, position.y),
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                if self.hit_exit(self.cursor.0, self.cursor.1) {
                    self.quit(el);
                }
            }
            WindowEvent::Touch(t) if t.phase == TouchPhase::Started => {
                if self.hit_exit(t.location.x, t.location.y) {
                    self.quit(el);
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.shared.quit.load(Ordering::Relaxed) {
            return el.exit();
        }
        let now = Instant::now();
        // repaint at least once a second (battery), at most ~30 times a second
        if now.duration_since(self.last_draw) >= Duration::from_secs(1) {
            self.dirty = true;
        }
        let next_frame = self.last_draw + FRAME;
        if !self.dirty {
            el.set_control_flow(ControlFlow::WaitUntil(self.last_draw + Duration::from_secs(1)));
        } else if now >= next_frame {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            el.set_control_flow(ControlFlow::WaitUntil(now + FRAME));
        } else {
            el.set_control_flow(ControlFlow::WaitUntil(next_frame));
        }
    }
}

/// Renders a sample screen to a PNG (for checking the design off-device).
pub fn preview(path: &str) -> Result<(), String> {
    use flipad_proto::Status;
    let mut text = Text::load()?;
    let scale = 1.0;
    let mut cv = Canvas { pm: Pixmap::new(1920, 1080).unwrap(), ts: Transform::identity(), scale, ox: 0.0, oy: 0.0 };
    cv.pm.fill(rgba(BG, 1.0));
    let view = View {
        phase: Phase::Connected,
        host: "my-desktop".into(),
        rtt_ms: Some(2.4),
        status: Status { cpu_pct: Some(23), cpu_temp: Some(54.0), gpu_pct: Some(78), gpu_temp: Some(71.0), mem_pct: Some(41) },
        pad: Pad { buttons: button::SOUTH | button::TR | button::DPAD_LEFT, lx: -14000, ly: 22000, rx: 9000, ry: -4000, lt: 200, rt: 40 },
        exit_hold: 0.4,
        rumble: true,
    };
    render(&mut cv, &mut text, &view, (Some(76), false));
    cv.pm.save_png(path).map_err(|e| e.to_string())
}

fn read_battery() -> (Option<u8>, bool) {
    let base = "/sys/class/power_supply/battery";
    let cap = std::fs::read_to_string(format!("{base}/capacity")).ok().and_then(|s| s.trim().parse().ok());
    let charging = std::fs::read_to_string(format!("{base}/status")).is_ok_and(|s| s.trim() == "Charging");
    (cap, charging)
}

fn card(cv: &mut Canvas, x: f32, y: f32, w: f32, h: f32) {
    cv.fill(&rrect(x, y, w, h, 28.0), rgba(CARD, 1.0));
    cv.stroke(&rrect(x + 0.5, y + 0.5, w - 1.0, h - 1.0, 28.0), rgba(0xffffff, 0.06), 1.0);
}

fn bar(cv: &mut Canvas, x: f32, y: f32, w: f32, h: f32, frac: f32, color: u32) {
    cv.fill(&rrect(x, y, w, h, h / 2.0), rgba(FAINT, 0.6));
    let f = frac.clamp(0.0, 1.0);
    if f > 0.0 {
        cv.fill(&rrect(x, y, (w * f).max(h), h, h / 2.0), rgba(color, 1.0));
    }
}

fn level_color(pct: f32) -> u32 {
    if pct >= 0.9 { BAD } else if pct >= 0.7 { WARN } else { ACCENT }
}

fn temp_color(t: f32) -> u32 {
    if t >= 85.0 { BAD } else if t >= 70.0 { WARN } else { DIM }
}

fn render(cv: &mut Canvas, t: &mut Text, v: &View, battery: (Option<u8>, bool)) {
    // header
    t.draw(cv, "Flip Pad", 64.0, 112.0, 56.0, rgba(TEXT, 1.0), true, Align::Left);
    t.draw(cv, "无线手柄模式", 64.0, 158.0, 28.0, rgba(DIM, 1.0), false, Align::Left);

    let (dot, label) = match &v.phase {
        Phase::Searching => (WARN, "正在寻找主机…".to_string()),
        Phase::Connecting => (WARN, format!("正在连接 {}", v.host)),
        Phase::Connected => (GOOD, format!("已连接 {}", v.host)),
        Phase::Lost => (WARN, "连接中断，正在重连…".to_string()),
        Phase::Rejected(r) => (BAD, format!("无法连接：{r}")),
    };
    let lw = t.measure(&label, 30.0, false);
    let latency = v.rtt_ms.map(|r| format!("{r:.1} ms"));
    let latw = latency.as_ref().map_or(0.0, |s| t.measure(s, 30.0, true) + 40.0);
    let pill_w = lw + latw + 96.0;
    let px = W - 64.0 - pill_w;
    cv.fill(&rrect(px, 72.0, pill_w, 72.0, 36.0), rgba(CARD, 1.0));
    cv.fill(&circle(px + 40.0, 108.0, 9.0), rgba(dot, 1.0));
    t.draw(cv, &label, px + 66.0, 119.0, 30.0, rgba(TEXT, 1.0), false, Align::Left);
    if let Some(l) = &latency {
        t.draw(cv, l, W - 64.0 - 36.0, 119.0, 30.0, rgba(dot, 1.0), true, Align::Right);
    }

    controller(cv, t, &v.pad, v.rumble);
    host_stats(cv, t, v);

    // footer
    let (bx, by, bw, bh) = EXIT_BTN;
    if let (Some(b), charging) = battery {
        let s = format!("掌机电量 {b}%{}", if charging { " · 充电中" } else { "" });
        let c = if b <= 15 { BAD } else { DIM };
        t.draw(cv, &s, 64.0, 992.0, 30.0, rgba(c, 1.0), false, Align::Left);
    }
    let hint = "长按 Home + Select 退出，或点右边的按钮";
    t.draw(cv, hint, 64.0, 1036.0, 26.0, rgba(DIM, 0.8), false, Align::Left);
    if v.exit_hold > 0.0 {
        bar(cv, 64.0, 1052.0, 560.0, 8.0, v.exit_hold, BAD);
    }
    cv.fill(&rrect(bx, by, bw, bh, bh / 2.0), rgba(BAD, 0.14));
    cv.stroke(&rrect(bx, by, bw, bh, bh / 2.0), rgba(BAD, 0.6), 2.0);
    t.draw(cv, "退出", bx + bw / 2.0, by + 62.0, 36.0, rgba(BAD, 1.0), true, Align::Center);
}

fn controller(cv: &mut Canvas, t: &mut Text, p: &Pad, rumble: bool) {
    let (x, y, w, h) = (64.0, 200.0, 880.0, 700.0);
    card(cv, x, y, w, h);
    t.draw(cv, "手柄", x + 40.0, y + 64.0, 34.0, rgba(TEXT, 1.0), true, Align::Left);
    if rumble {
        t.draw(cv, "震动中", x + w - 40.0, y + 64.0, 28.0, rgba(ACCENT, 1.0), true, Align::Right);
    }
    let on = |b: u32| p.buttons & b != 0;

    // triggers and bumpers
    for (i, (name, val, bump, bname)) in
        [("LT", p.lt, button::TL, "LB"), ("RT", p.rt, button::TR, "RB")].into_iter().enumerate()
    {
        let bx = x + 40.0 + i as f32 * 420.0;
        t.draw(cv, name, bx, y + 132.0, 26.0, rgba(DIM, 1.0), true, Align::Left);
        bar(cv, bx + 56.0, y + 114.0, 300.0, 22.0, val as f32 / 255.0, ACCENT);
        let pressed = on(bump);
        cv.fill(&rrect(bx, y + 160.0, 356.0, 52.0, 26.0), rgba(if pressed { ACCENT } else { FAINT }, if pressed { 0.9 } else { 0.45 }));
        t.draw(cv, bname, bx + 178.0, y + 196.0, 26.0, rgba(TEXT, 1.0), true, Align::Center);
    }

    let stick = |cv: &mut Canvas, cx: f32, cy: f32, sx: i16, sy: i16, pressed: bool| {
        cv.fill(&circle(cx, cy, 88.0), rgba(FAINT, if pressed { 0.9 } else { 0.35 }));
        cv.stroke(&circle(cx, cy, 88.0), rgba(0xffffff, 0.08), 2.0);
        let (dx, dy) = (sx as f32 / 32768.0 * 58.0, sy as f32 / 32768.0 * 58.0);
        cv.fill(&circle(cx + dx, cy + dy, 30.0), rgba(if pressed { ACCENT } else { TEXT }, 0.95));
    };
    // Xbox layout: left stick and face buttons on top, d-pad and right stick below
    stick(cv, x + 190.0, y + 390.0, p.lx, p.ly, on(button::THUMBL));
    stick(cv, x + 590.0, y + 590.0, p.rx, p.ry, on(button::THUMBR));

    let (dx, dy) = (x + 330.0, y + 590.0);
    for (bit, ox, oy) in
        [(button::DPAD_UP, 0.0, -56.0), (button::DPAD_DOWN, 0.0, 56.0), (button::DPAD_LEFT, -56.0, 0.0), (button::DPAD_RIGHT, 56.0, 0.0)]
    {
        let pressed = on(bit);
        cv.fill(&rrect(dx + ox - 27.0, dy + oy - 27.0, 54.0, 54.0, 12.0), rgba(if pressed { ACCENT } else { FAINT }, if pressed { 0.95 } else { 0.5 }));
    }

    let (fx, fy) = (x + 700.0, y + 380.0);
    for (bit, label, ox, oy) in [
        (button::NORTH, "Y", 0.0, -62.0),
        (button::SOUTH, "A", 0.0, 62.0),
        (button::WEST, "X", -62.0, 0.0),
        (button::EAST, "B", 62.0, 0.0),
    ] {
        let pressed = on(bit);
        cv.fill(&circle(fx + ox, fy + oy, 31.0), rgba(if pressed { ACCENT } else { FAINT }, if pressed { 0.95 } else { 0.5 }));
        t.draw(cv, label, fx + ox, fy + oy + 10.0, 28.0, rgba(TEXT, 1.0), true, Align::Center);
    }

    for (i, (bit, label)) in [(button::SELECT, "Select"), (button::MODE, "Home"), (button::START, "Start")].into_iter().enumerate() {
        let cx = x + 340.0 + i as f32 * 104.0;
        let pressed = on(bit);
        cv.fill(&rrect(cx - 46.0, y + 360.0, 92.0, 40.0, 20.0), rgba(if pressed { ACCENT } else { FAINT }, if pressed { 0.95 } else { 0.5 }));
        t.draw(cv, label, cx, y + 387.0, 19.0, rgba(TEXT, 0.9), false, Align::Center);
    }
}

fn host_stats(cv: &mut Canvas, t: &mut Text, v: &View) {
    let (x, y, w, h) = (976.0, 200.0, 880.0, 700.0);
    card(cv, x, y, w, h);
    t.draw(cv, "主机", x + 40.0, y + 64.0, 34.0, rgba(TEXT, 1.0), true, Align::Left);
    if !v.host.is_empty() {
        t.draw(cv, &v.host, x + w - 40.0, y + 64.0, 28.0, rgba(DIM, 1.0), false, Align::Right);
    }
    let s = &v.status;
    let rows: [(&str, Option<u8>, Option<f32>); 3] =
        [("CPU", s.cpu_pct, s.cpu_temp), ("GPU", s.gpu_pct, s.gpu_temp), ("内存", s.mem_pct, None)];
    for (i, (name, pct, temp)) in rows.into_iter().enumerate() {
        let ry = y + 120.0 + i as f32 * 185.0;
        t.draw(cv, name, x + 40.0, ry + 40.0, 32.0, rgba(DIM, 1.0), true, Align::Left);
        let value = pct.map_or("—".to_string(), |p| format!("{p}%"));
        t.draw(cv, &value, x + 40.0, ry + 112.0, 64.0, rgba(TEXT, 1.0), true, Align::Left);
        if let Some(tc) = temp {
            t.draw(cv, &format!("{tc:.0}°C"), x + w - 40.0, ry + 112.0, 44.0, rgba(temp_color(tc), 1.0), true, Align::Right);
        }
        let f = pct.map_or(0.0, |p| p as f32 / 100.0);
        bar(cv, x + 40.0, ry + 136.0, w - 80.0, 14.0, f, level_color(f));
    }
}
