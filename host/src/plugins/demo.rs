//! Demo plugin (enable with FLIPAD_DEMO_PLUGIN=1): an animated radar that
//! exercises the image path the way a minimap plugin would — draw or capture
//! a picture, then `ctx.send_rgba` it a few times a second.

use std::f32::consts::TAU;
use std::time::Instant;

use flipad_proto::Widget;
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};

use super::{Ctx, GameInfo, GamePlugin};

const W: u16 = 400;
const H: u16 = 300;
const RADAR: u16 = 1;
const PING: u16 = 10;

pub struct Radar {
    start: Instant,
    last_frame: Option<Instant>,
    pings: u32,
}

impl Default for Radar {
    fn default() -> Self {
        Self { start: Instant::now(), last_frame: None, pings: 0 }
    }
}

impl Radar {
    fn panel(&self, ctx: &mut Ctx) {
        ctx.set_panel(
            "插件演示",
            vec![
                Widget::Image { id: RADAR, aspect: (W as u32 * 1000 / H as u32) as u16 },
                Widget::Text { text: "雷达".into(), big: true },
                Widget::Text { text: format!("点了 {} 次", self.pings), big: false },
                Widget::Button { id: PING, label: "Ping".into() },
            ],
        );
    }

    fn draw(&self) -> Pixmap {
        let mut pm = Pixmap::new(W as u32, H as u32).unwrap();
        pm.fill(Color::from_rgba8(10, 20, 16, 255));
        let (cx, cy, r) = (W as f32 / 2.0, H as f32 / 2.0, H as f32 * 0.45);
        let mut paint = Paint { anti_alias: true, ..Paint::default() };
        paint.set_color_rgba8(60, 220, 120, 120);
        let st = Stroke { width: 2.0, ..Stroke::default() };
        for k in 1..=3 {
            let c = PathBuilder::from_circle(cx, cy, r * k as f32 / 3.0).unwrap();
            pm.stroke_path(&c, &paint, &st, Transform::identity(), None);
        }
        let a = self.start.elapsed().as_secs_f32() * TAU / 3.0;
        let mut pb = PathBuilder::new();
        pb.move_to(cx, cy);
        pb.line_to(cx + r * a.cos(), cy + r * a.sin());
        paint.set_color_rgba8(120, 255, 160, 255);
        pm.stroke_path(&pb.finish().unwrap(), &paint, &Stroke { width: 4.0, ..Stroke::default() }, Transform::identity(), None);
        paint.set_color_rgba8(255, 90, 90, 255);
        let blip = PathBuilder::from_circle(cx + r * 0.5, cy - r * 0.3, 8.0).unwrap();
        pm.fill_path(&blip, &paint, FillRule::Winding, Transform::identity(), None);
        pm
    }
}

impl GamePlugin for Radar {
    fn id(&self) -> &'static str {
        "demo_radar"
    }

    fn matches(&self, _game: Option<&GameInfo>) -> bool {
        true
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        self.panel(ctx);
    }

    fn tick(&mut self, ctx: &mut Ctx) {
        // ~5 fps is plenty for a map and keeps Wi-Fi traffic small
        if self.last_frame.is_none_or(|t| t.elapsed().as_millis() >= 200) {
            self.last_frame = Some(Instant::now());
            let pm = self.draw();
            ctx.send_rgba(RADAR, W, H, pm.data());
        }
    }

    fn button(&mut self, id: u16, ctx: &mut Ctx) {
        if id == PING {
            self.pings += 1;
            self.panel(ctx);
        }
    }
}
