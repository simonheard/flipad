//! Window and touch handling for the handheld's panel (see `panel.rs` for
//! what is drawn).

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tiny_skia::{Pixmap, Transform};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, TouchPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::window::{Fullscreen, Window, WindowId};

use crate::link::{Cmd, PointerOp, Shared, View};
use crate::paint::{Canvas, Text};
use crate::panel::{self, FLASH, Hit, Rect, Tab, Ui};
use crate::settings::Settings;

const H: f32 = 1080.0;
const FRAME: Duration = Duration::from_millis(33);
/// A touch shorter and smaller than this is a tap (a click).
const TAP_TIME: Duration = Duration::from_millis(250);
const TAP_MOVE: f32 = 24.0;
/// Two-finger / strip scrolling: 1/120 notches per design pixel.
const SCROLL_PER_PX: f32 = 3.0;

/// A touch sequence on the touchpad or the scroll strip.
struct Gesture {
    strip: bool,
    /// Finger id → last position (design units).
    fingers: Vec<(u64, f32, f32)>,
    max_fingers: usize,
    start: Instant,
    travel: f32,
}

/// Mouse events stand in for touch when the compositor only sends a pointer.
const MOUSE_FINGER: u64 = u64::MAX;

pub struct App {
    shared: Arc<Shared>,
    text: Option<Text>,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    canvas: Option<Canvas>,
    hits: Vec<(Rect, Hit)>,
    tab: Tab,
    flash: Option<(Hit, Instant)>,
    cursor: (f64, f64),
    gesture: Option<Gesture>,
    drag_lock: bool,
    last_touch: Option<Instant>,
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
            hits: Vec::new(),
            tab: Tab::default(),
            flash: None,
            cursor: (0.0, 0.0),
            gesture: None,
            drag_lock: false,
            last_touch: None,
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

    fn design(&self, x: f64, y: f64) -> (f32, f32) {
        match &self.canvas {
            Some(cv) => ((x as f32 - cv.ox) / cv.scale, (y as f32 - cv.oy) / cv.scale),
            None => (x as f32, y as f32),
        }
    }

    fn base_buttons(&self) -> u8 {
        self.drag_lock as u8
    }

    fn click(&self, bit: u8) {
        let base = self.base_buttons();
        self.shared.pointer(PointerOp::Buttons(base | bit));
        self.shared.pointer(PointerOp::Buttons(base));
    }

    /// A finger (or the mouse) went down.
    fn touch_down(&mut self, el: &ActiveEventLoop, id: u64, x: f64, y: f64) {
        let (dx, dy) = self.design(x, y);
        if let Some(g) = &mut self.gesture {
            if !g.strip && !g.fingers.iter().any(|f| f.0 == id) {
                g.fingers.push((id, dx, dy));
                g.max_fingers = g.max_fingers.max(g.fingers.len());
                self.dirty = true;
            }
            return;
        }
        let hit = self.hits.iter().rev().find(|(r, _)| r.contains(dx, dy)).map(|h| h.1);
        match hit {
            Some(Hit::Pad) | Some(Hit::ScrollStrip) => {
                self.gesture = Some(Gesture {
                    strip: hit == Some(Hit::ScrollStrip),
                    fingers: vec![(id, dx, dy)],
                    max_fingers: 1,
                    start: Instant::now(),
                    travel: 0.0,
                });
                self.dirty = true;
            }
            Some(h) => self.tap(el, h),
            None => {}
        }
    }

    fn touch_move(&mut self, id: u64, x: f64, y: f64) {
        let (nx, ny) = self.design(x, y);
        let Some(g) = &mut self.gesture else { return };
        let n = g.fingers.len() as f32;
        let Some(f) = g.fingers.iter_mut().find(|f| f.0 == id) else { return };
        let (ddx, ddy) = (nx - f.1, ny - f.2);
        f.1 = nx;
        f.2 = ny;
        g.travel += ddx.hypot(ddy);
        if g.strip {
            self.shared.pointer(PointerOp::Scroll(-ddy * SCROLL_PER_PX * 1.5, 0.0));
        } else if n >= 2.0 {
            // natural scrolling: content follows the fingers
            self.shared.pointer(PointerOp::Scroll(ddy * SCROLL_PER_PX / n, -ddx * SCROLL_PER_PX / n));
        } else {
            self.shared.pointer(PointerOp::Move(ddx, ddy));
        }
    }

    fn touch_up(&mut self, id: u64) {
        let Some(g) = &mut self.gesture else { return };
        g.fingers.retain(|f| f.0 != id);
        if !g.fingers.is_empty() {
            return;
        }
        let g = self.gesture.take().unwrap();
        self.dirty = true;
        if !g.strip && g.start.elapsed() < TAP_TIME && g.travel < TAP_MOVE {
            self.click(if g.max_fingers >= 2 { 2 } else { 1 });
        }
    }

    fn tap(&mut self, el: &ActiveEventLoop, hit: Hit) {
        self.flash = Some((hit, Instant::now()));
        self.dirty = true;
        let sh = &self.shared;
        let set = |f: &dyn Fn(&mut Settings)| {
            let mut s = sh.settings.lock().unwrap();
            f(&mut s);
            s.save();
        };
        match hit {
            Hit::Tab(t) => {
                if t != Tab::Touchpad && self.drag_lock {
                    self.drag_lock = false;
                    sh.pointer(PointerOp::Buttons(0));
                }
                self.tab = t;
            }
            Hit::MouseLeft => self.click(1),
            Hit::MouseRight => self.click(2),
            Hit::DragLock => {
                self.drag_lock = !self.drag_lock;
                sh.pointer(PointerOp::Buttons(self.base_buttons()));
            }
            Hit::PointerSpeed(p) => set(&|s| s.pointer_speed = p),
            Hit::Pad | Hit::ScrollStrip => {}
            Hit::Exit => self.quit(el),
            Hit::ScreenOff => sh.update(|v| v.screen_off = true),
            Hit::Wake => sh.update(|v| v.screen_off = false),
            Hit::Action(a) => sh.command(Cmd::Action(a)),
            Hit::Record(i) => sh.command(Cmd::Record(i)),
            Hit::StopRecording => sh.command(Cmd::StopRecording),
            Hit::Play(i) => sh.command(Cmd::Play(i)),
            Hit::Clear(i) => sh.command(Cmd::Clear(i)),
            Hit::Turbo(bit) => set(&|s| s.turbo ^= bit),
            Hit::TurboHz(hz) => set(&|s| s.turbo_hz = hz),
            Hit::Nintendo(on) => set(&|s| s.nintendo = on),
            Hit::Rumble => set(&|s| s.rumble = !s.rumble),
            Hit::Audio => set(&|s| s.audio = !s.audio),
            Hit::Mic => set(&|s| s.mic = !s.mic),
            Hit::Latency(ms) => set(&|s| s.audio_latency_ms = ms),
        }
    }

    fn mouse_ok(&self) -> bool {
        self.last_touch.is_none_or(|t| t.elapsed() > Duration::from_secs(2))
    }

    fn draw(&mut self) {
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else { return };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        if self.canvas.as_ref().is_none_or(|c| c.pm.width() != size.width || c.pm.height() != size.height) {
            let scale = (size.width as f32 / panel::W).min(size.height as f32 / H);
            let (ox, oy) = ((size.width as f32 - panel::W * scale) / 2.0, (size.height as f32 - H * scale) / 2.0);
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
        let settings = *self.shared.settings.lock().unwrap();
        let cv = self.canvas.as_mut().unwrap();
        if let Some(text) = &mut self.text {
            let fingers = self.gesture.as_ref().map_or(0, |g| g.fingers.len());
            let mut ui =
                Ui { cv, text, hits: &mut self.hits, flash: self.flash, now: Instant::now(), drag_lock: self.drag_lock, fingers };
            panel::render(&mut ui, &view, &settings, self.tab, self.battery);
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
        let attrs = Window::default_attributes().with_title("Flip Pad").with_fullscreen(Some(Fullscreen::Borderless(None)));
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
            // real touch events win; the pointer is only a fallback
            WindowEvent::Touch(t) => {
                self.last_touch = Some(Instant::now());
                let (x, y) = (t.location.x, t.location.y);
                match t.phase {
                    TouchPhase::Started => self.touch_down(el, t.id, x, y),
                    TouchPhase::Moved => self.touch_move(t.id, x, y),
                    TouchPhase::Ended | TouchPhase::Cancelled => self.touch_up(t.id),
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x, position.y);
                if self.mouse_ok() {
                    self.touch_move(MOUSE_FINGER, position.x, position.y);
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } if self.mouse_ok() => {
                let (x, y) = self.cursor;
                match state {
                    ElementState::Pressed => self.touch_down(el, MOUSE_FINGER, x, y),
                    ElementState::Released => self.touch_up(MOUSE_FINGER),
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
        // keep animating while a tap flash or a status toast fades
        let flashing = self.flash.is_some_and(|(_, t)| now.duration_since(t) < FLASH + FRAME);
        let toast = self.shared.view.lock().unwrap().last_action.is_some_and(|(_, t)| now.duration_since(t) < Duration::from_millis(1600));
        let idle_redraw = if toast { Duration::from_millis(250) } else { Duration::from_secs(1) };
        if flashing || now.duration_since(self.last_draw) >= idle_redraw {
            self.dirty = true;
        }
        let next_frame = self.last_draw + FRAME;
        if !self.dirty {
            el.set_control_flow(ControlFlow::WaitUntil(self.last_draw + idle_redraw));
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

fn read_battery() -> (Option<u8>, bool) {
    let base = "/sys/class/power_supply/battery";
    let cap = std::fs::read_to_string(format!("{base}/capacity")).ok().and_then(|s| s.trim().parse().ok());
    let charging = std::fs::read_to_string(format!("{base}/status")).is_ok_and(|s| s.trim() == "Charging");
    (cap, charging)
}

/// Renders every tab with sample data into one tall PNG (for checking the
/// design off-device).
pub fn preview(path: &str) -> Result<(), String> {
    use crate::audio::Jack;
    use flipad_proto::{HostAction, Pad, Status, button};

    let mut text = Text::load()?;
    let view = View {
        phase: crate::link::Phase::Connected,
        host: "my-desktop".into(),
        rtt_ms: Some(2.4),
        status: Status { cpu_pct: Some(23), cpu_temp: Some(54.0), gpu_pct: Some(78), gpu_temp: Some(71.0), mem_pct: Some(41) },
        pad: Pad { buttons: button::SOUTH | button::TR | button::DPAD_LEFT, lx: -14000, ly: 22000, rx: 9000, ry: -4000, lt: 200, rt: 40 },
        exit_hold: 0.0,
        rumble: true,
        jack: Jack { headphones: true, mic: true },
        audio_live: true,
        macros: [Some(Duration::from_millis(3200)), None, Some(Duration::from_millis(800)), None],
        recording: Some(1),
        playing: Some(2),
        screen_off: false,
        last_action: Some((HostAction::VolumeUp, Instant::now())),
        bluetooth: false,
    };
    let settings = Settings { turbo: button::SOUTH | crate::settings::TURBO_RT, ..Settings::default() };
    let tabs = [Tab::Overview, Tab::Touchpad, Tab::Quick, Tab::Macros, Tab::Settings];
    let mut sheet = Pixmap::new(1920, 1080 * (tabs.len() as u32 + 2)).unwrap();
    for (i, tab) in tabs.iter().chain([Tab::Overview, Tab::Overview].iter()).enumerate() {
        let mut cv = Canvas { pm: Pixmap::new(1920, 1080).unwrap(), ts: Transform::identity(), scale: 1.0, ox: 0.0, oy: 0.0 };
        let mut hits = Vec::new();
        let v = View {
            screen_off: i == tabs.len(),
            bluetooth: i == tabs.len() + 1,
            host: if i == tabs.len() + 1 { "MY-DESKTOP".into() } else { view.host.clone() },
            ..view.clone()
        };
        let flash = (*tab == Tab::Quick).then(|| (Hit::Action(HostAction::PlayPause), Instant::now()));
        let mut ui = Ui { cv: &mut cv, text: &mut text, hits: &mut hits, flash, now: Instant::now(), drag_lock: false, fingers: 0 };
        panel::render(&mut ui, &v, &settings, *tab, (Some(76), false));
        sheet.draw_pixmap(0, (i * 1080) as i32, cv.pm.as_ref(), &Default::default(), Default::default(), None);
    }
    sheet.save_png(path).map_err(|e| e.to_string())
}
