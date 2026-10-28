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
use crate::panel::{self, FLASH, Hit, Rect, Tab, Target, Ui};
use crate::settings::{Mode, Settings};

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
    source: Source,
    /// Finger id → last position (design units); None until its first move,
    /// because an emulated pointer may report the press before it warps.
    fingers: Vec<(u64, Option<(f32, f32)>)>,
    max_fingers: usize,
    start: Instant,
    last_event: Instant,
    travel: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    /// gamescope's emulated pointer (one "finger").
    Mouse,
    /// Touch events from the window system.
    Touch,
    /// The touchscreen read directly (see touch.rs): real multi-touch.
    Raw,
}

/// Mouse events stand in for touch when the compositor only sends a pointer.
const MOUSE_FINGER: u64 = u64::MAX;
/// A single step longer than this (design px) is a pointer warp, not motion.
const JUMP: f32 = 160.0;

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
    /// Direct touchscreen frames, the panel's learned orientation, and the
    /// last frame seen (for learning it and for diffing contacts).
    touch: Option<crate::touch::Frames>,
    orientation: Option<crate::touch::Orientation>,
    raw_prev: Vec<crate::touch::Contact>,
    raw_prev_at: Instant,
    switcher: bool,
    /// A host whose "forget" button was tapped once (tap again to confirm).
    forget_armed: Option<crate::settings::Addr>,
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
            touch: None,
            // learned afresh each run from the first touch, never trusted from disk
            orientation: None,
            raw_prev: Vec::new(),
            raw_prev_at: Instant::now(),
            switcher: false,
            forget_armed: None,
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

    fn hit_at(&self, x: f32, y: f32) -> Option<Hit> {
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|h| h.1)
    }

    /// Direct touchscreen reading is set up and knows which way is up. Then
    /// every touch is handled from it, and gamescope's emulated pointer (which
    /// can get stuck "pressed" after two-finger touches) is only used to check
    /// the orientation.
    fn raw_ready(&self) -> bool {
        self.touch.is_some() && self.orientation.is_some()
    }

    /// A finger (or the emulated mouse) went down.
    fn touch_down(&mut self, el: &ActiveEventLoop, id: u64, x: f64, y: f64) {
        let (dx, dy) = self.design(x, y);
        let source = if id == MOUSE_FINGER { Source::Mouse } else { Source::Touch };
        let hit = self.hit_at(dx, dy);
        let on_pad = matches!(hit, Some(Hit::Pad) | Some(Hit::ScrollStrip));
        // the touchpad itself is read from the panel directly when possible
        if on_pad && self.raw_ready() && source == Source::Mouse {
            return;
        }
        if let Some(g) = &mut self.gesture {
            let stale = g.last_event.elapsed() > Duration::from_secs(2);
            if g.source == source && !stale && source == Source::Touch {
                if !g.strip && !g.fingers.iter().any(|f| f.0 == id) {
                    g.fingers.push((id, Some((dx, dy))));
                    g.max_fingers = g.max_fingers.max(g.fingers.len());
                    self.dirty = true;
                }
                return;
            }
            if g.source != Source::Raw || stale {
                // a press without the previous release: drop the old gesture
                self.gesture = None;
            } else if !on_pad {
                return;
            }
        }
        match hit {
            Some(Hit::Pad) | Some(Hit::ScrollStrip) => {
                let pos = if source == Source::Mouse { None } else { Some((dx, dy)) };
                self.begin_gesture(hit == Some(Hit::ScrollStrip), source, id, pos);
            }
            Some(h) => self.tap(el, h),
            None => {}
        }
    }

    fn begin_gesture(&mut self, strip: bool, source: Source, id: u64, pos: Option<(f32, f32)>) {
        let now = Instant::now();
        self.gesture =
            Some(Gesture { strip, source, fingers: vec![(id, pos)], max_fingers: 1, start: now, last_event: now, travel: 0.0 });
        self.dirty = true;
    }

    fn touch_move(&mut self, id: u64, x: f64, y: f64) {
        let p = self.design(x, y);
        self.gesture_move(id, p);
    }

    fn gesture_move(&mut self, id: u64, (nx, ny): (f32, f32)) {
        let Some(g) = &mut self.gesture else { return };
        let n = g.fingers.len() as f32;
        let Some(f) = g.fingers.iter_mut().find(|f| f.0 == id) else { return };
        g.last_event = Instant::now();
        let Some((ox, oy)) = f.1.replace((nx, ny)) else { return };
        let (ddx, ddy) = (nx - ox, ny - oy);
        if ddx.hypot(ddy) > JUMP {
            return; // a warp, not a slide
        }
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

    /// Learns (or corrects) the panel's orientation from an emulated press
    /// that happened while exactly one finger was on the panel.
    fn calibrate(&mut self, x: f64, y: f64) {
        let Some(cv) = &self.canvas else { return };
        if self.raw_prev.len() != 1 || self.raw_prev_at.elapsed() > Duration::from_millis(300) {
            return;
        }
        let c = self.raw_prev[0];
        let (sx, sy) = (x as f32 / cv.pm.width() as f32, y as f32 / cv.pm.height() as f32);
        let (o, dist) = crate::touch::Orientation::best(c.u, c.v, sx, sy);
        if dist < 0.06 && self.orientation != Some(o) {
            eprintln!("touch: panel orientation {o:?}");
            self.orientation = Some(o);
        }
    }

    /// Handles directly-read touchscreen frames: taps on buttons and
    /// touchpad gestures.
    fn process_raw(&mut self, el: &ActiveEventLoop) {
        let Some(frames) = &self.touch else { return };
        let frames: Vec<Vec<crate::touch::Contact>> = std::mem::take(&mut *frames.lock().unwrap());
        for frame in frames {
            if self.raw_ready()
                && let (Some(o), Some(cv)) = (self.orientation, &self.canvas)
            {
                let (w, h) = (cv.pm.width() as f64, cv.pm.height() as f64);
                let to_design = |c: &crate::touch::Contact| {
                    let (sx, sy) = o.map(c.u, c.v);
                    (sx as f64 * w, sy as f64 * h)
                };
                let prev: Vec<i32> = self.raw_prev.iter().map(|c| c.id).collect();
                for c in &frame {
                    let (x, y) = to_design(c);
                    let pos = self.design(x, y);
                    let id = c.id as u64;
                    if !prev.contains(&c.id) {
                        match &mut self.gesture {
                            Some(g) if g.source == Source::Raw && !g.strip => {
                                g.fingers.push((id, Some(pos)));
                                g.max_fingers = g.max_fingers.max(g.fingers.len());
                            }
                            Some(_) => {}
                            None => match self.hit_at(pos.0, pos.1) {
                                Some(hit @ (Hit::Pad | Hit::ScrollStrip)) => {
                                    self.begin_gesture(hit == Hit::ScrollStrip, Source::Raw, id, Some(pos));
                                }
                                Some(hit) if frame.len() == 1 => self.tap(el, hit),
                                _ => {}
                            },
                        }
                    } else {
                        self.gesture_move(id, pos);
                    }
                }
                for id in prev.iter().filter(|id| !frame.iter().any(|c| c.id == **id)) {
                    if self.gesture.as_ref().is_some_and(|g| g.source == Source::Raw) {
                        self.touch_up(*id as u64);
                    }
                }
            }
            self.raw_prev = frame;
            self.raw_prev_at = Instant::now();
        }
    }

    fn tap(&mut self, el: &ActiveEventLoop, hit: Hit) {
        if debug_input() {
            eprintln!("input: tap {hit:?}");
        }
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
                if t != Tab::Touchpad {
                    self.gesture = None;
                }
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
            Hit::Output(o) => set(&|s| s.output = o),
            Hit::OpenSwitcher => self.switcher = true,
            Hit::CloseSwitcher => {
                self.switcher = false;
                self.forget_armed = None;
            }
            Hit::Connect(t) => {
                set(&|s| match t {
                    Target::Wifi => s.mode = Mode::Wifi,
                    Target::BtAuto => {
                        s.mode = Mode::Bluetooth;
                        s.bt_target = None;
                    }
                    Target::Bt(a) => {
                        s.mode = Mode::Bluetooth;
                        s.bt_target = Some(a);
                    }
                });
                self.switcher = false;
                sh.poke_pub();
            }
            Hit::PluginButton(id) => sh.command(Cmd::PluginButton(id)),
            Hit::PairNew => {
                // pairing happens in Bluetooth mode; switch there if needed
                let bt = sh.settings.lock().unwrap().mode == Mode::Bluetooth;
                if bt {
                    sh.command(Cmd::Pairing(true));
                } else {
                    sh.pair_request.store(true, std::sync::atomic::Ordering::Relaxed);
                    set(&|s| s.mode = Mode::Bluetooth);
                    sh.poke_pub();
                }
                self.switcher = false;
            }
            Hit::StopPairing => sh.command(Cmd::Pairing(false)),
            Hit::Forget(a) => {
                if self.forget_armed == Some(a) {
                    sh.command(Cmd::Forget(a));
                    self.forget_armed = None;
                } else {
                    // ask for a second tap before removing a pairing
                    self.forget_armed = Some(a);
                }
            }
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
            let frames = self.shared.frames.lock().unwrap().clone();
            let mut ui = Ui {
                cv,
                text,
                hits: &mut self.hits,
                flash: self.flash,
                now: Instant::now(),
                drag_lock: self.drag_lock,
                fingers,
                switcher: self.switcher,
                frames: &frames,
                forget_armed: self.forget_armed,
            };
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
        if self.touch.is_none() {
            let s = self.shared.clone();
            self.touch = crate::touch::start(move || (s.notify)());
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, _: ()) {
        if self.shared.quit.load(Ordering::Relaxed) {
            el.exit();
        }
        self.process_raw(el);
        self.dirty = true;
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if debug_input() {
            match &event {
                WindowEvent::Touch(t) => eprintln!("input: touch {:?} id={} at {:.0},{:.0}", t.phase, t.id, t.location.x, t.location.y),
                WindowEvent::CursorMoved { position, .. } => eprintln!("input: cursor {:.0},{:.0}", position.x, position.y),
                WindowEvent::MouseInput { state, button, .. } => eprintln!("input: mouse {button:?} {state:?}"),
                _ => {}
            }
        }
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
                if self.mouse_ok() && !self.raw_ready() {
                    self.touch_move(MOUSE_FINGER, position.x, position.y);
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } if self.mouse_ok() => {
                let (x, y) = self.cursor;
                match state {
                    ElementState::Pressed => {
                        let was_ready = self.raw_ready();
                        self.process_raw(el);
                        self.calibrate(x, y);
                        // once calibrated, the raw reader handles touches itself
                        if !was_ready {
                            self.touch_down(el, MOUSE_FINGER, x, y)
                        }
                    }
                    ElementState::Released if !self.raw_ready() => self.touch_up(MOUSE_FINGER),
                    ElementState::Released => {}
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

fn debug_input() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("FLIPAD_DEBUG_INPUT").is_some())
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
        bt_hosts: vec![],
        wifi_host: "my-desktop".into(),
        plugin: None,
        pairing_left: None,
    };
    let settings = Settings { turbo: button::SOUTH | crate::settings::TURBO_RT, ..Settings::default() };
    let bt_settings = Settings { mode: Mode::Bluetooth, ..settings };
    let host = |addr: &str, name: &str, connected| crate::link::BtHost {
        addr: crate::settings::parse_addr(addr).unwrap(),
        name: name.into(),
        connected,
    };
    let bt_view = View {
        bluetooth: true,
        host: "MY-DESKTOP".into(),
        rtt_ms: None,
        bt_hosts: vec![host("11:22:33:44:55:66", "my-desktop", true), host("AA:BB:CC:DD:EE:01", "iPad", false)],
        ..view.clone()
    };
    let game_view = View {
        plugin: Some(crate::link::PluginPanel {
            plugin: "demo".into(),
            title: "插件演示".into(),
            widgets: vec![
                flipad_proto::Widget::Image { id: 1, aspect: 1333 },
                flipad_proto::Widget::Text { text: "Elden Ring".into(), big: true },
                flipad_proto::Widget::Text { text: "已玩 42 分钟".into(), big: false },
                flipad_proto::Widget::Button { id: 1, label: "Steam 截图".into() },
                flipad_proto::Widget::Button { id: 2, label: "Steam 叠加层".into() },
                flipad_proto::Widget::Button { id: 3, label: "Esc / 暂停".into() },
                flipad_proto::Widget::Button { id: 4, label: "Alt + Tab".into() },
            ],
        }),
        ..view.clone()
    };
    // a stand-in "minimap" frame
    let mut img = Pixmap::new(400, 300).unwrap();
    for (i, px) in img.pixels_mut().iter_mut().enumerate() {
        let (x, y) = ((i % 400) as u8, (i / 400) as u8);
        *px = tiny_skia::ColorU8::from_rgba(x / 3, 60 + y / 3, 90, 255).premultiply();
    }
    let frames: std::collections::HashMap<u16, std::sync::Arc<Pixmap>> = [(1u16, std::sync::Arc::new(img))].into();

    let pages: Vec<(Tab, View, Settings, bool)> = vec![
        (Tab::Overview, view.clone(), settings, false),
        (Tab::Game, game_view, settings, false),
        (Tab::Touchpad, view.clone(), settings, false),
        (Tab::Quick, view.clone(), settings, false),
        (Tab::Macros, view.clone(), settings, false),
        (Tab::Settings, view.clone(), settings, false),
        (Tab::Overview, View { screen_off: true, ..view.clone() }, settings, false),
        (Tab::Overview, bt_view.clone(), bt_settings, false),
        (Tab::Settings, bt_view.clone(), bt_settings, false),
        (Tab::Overview, bt_view.clone(), bt_settings, true),
        (Tab::Overview, View { pairing_left: Some(42), ..bt_view }, bt_settings, false),
    ];
    let mut sheet = Pixmap::new(1920, 1080 * pages.len() as u32).unwrap();
    for (i, (tab, v, st, switcher)) in pages.into_iter().enumerate() {
        let mut cv = Canvas { pm: Pixmap::new(1920, 1080).unwrap(), ts: Transform::identity(), scale: 1.0, ox: 0.0, oy: 0.0 };
        let mut hits = Vec::new();
        let flash = (tab == Tab::Quick).then(|| (Hit::Action(HostAction::PlayPause), Instant::now()));
        let mut ui = Ui {
            cv: &mut cv,
            text: &mut text,
            hits: &mut hits,
            flash,
            now: Instant::now(),
            drag_lock: false,
            fingers: 0,
            switcher,
            frames: &frames,
            forget_armed: None,
        };
        panel::render(&mut ui, &v, &st, tab, (Some(76), false));
        sheet.draw_pixmap(0, (i * 1080) as i32, cv.pm.as_ref(), &Default::default(), Default::default(), None);
    }
    sheet.save_png(path).map_err(|e| e.to_string())
}
