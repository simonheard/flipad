//! Catch-all plugin: whatever Steam game is running gets its name, play time
//! and a few in-game shortcuts.

use std::time::Instant;

use evdev::KeyCode;
use flipad_proto::{HostAction, Widget};

use super::{Ctx, GameInfo, GamePlugin};

#[derive(Default)]
pub struct CurrentGame {
    last_minutes: Option<u64>,
}

const SCREENSHOT: u16 = 1;
const OVERLAY: u16 = 2;
const ESCAPE: u16 = 3;
const ALT_TAB: u16 = 4;

impl CurrentGame {
    fn panel(&mut self, ctx: &mut Ctx) {
        let minutes = ctx.game.started.map_or(0, |s| Instant::now().duration_since(s).as_secs() / 60);
        self.last_minutes = Some(minutes);
        let played = if minutes < 1 { "刚刚开始".to_string() } else { format!("已玩 {minutes} 分钟") };
        let mut widgets = vec![
            Widget::Text { text: ctx.game.name.clone(), big: true },
            Widget::Text { text: played, big: false },
        ];
        if let Some(id) = ctx.game.app_id {
            widgets.push(Widget::Text { text: format!("Steam App {id} · {}", ctx.game.process), big: false });
        }
        widgets.extend([
            Widget::Button { id: SCREENSHOT, label: "Steam 截图".into() },
            Widget::Button { id: OVERLAY, label: "Steam 叠加层".into() },
            Widget::Button { id: ESCAPE, label: "Esc / 暂停".into() },
            Widget::Button { id: ALT_TAB, label: "Alt + Tab".into() },
        ]);
        ctx.set_panel("当前游戏", widgets);
    }
}

impl GamePlugin for CurrentGame {
    fn id(&self) -> &'static str {
        "current_game"
    }

    fn matches(&self, game: Option<&GameInfo>) -> bool {
        game.is_some()
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        self.panel(ctx);
    }

    fn tick(&mut self, ctx: &mut Ctx) {
        let minutes = ctx.game.started.map_or(0, |s| s.elapsed().as_secs() / 60);
        if self.last_minutes != Some(minutes) {
            self.panel(ctx);
        }
    }

    fn button(&mut self, id: u16, ctx: &mut Ctx) {
        match id {
            SCREENSHOT => ctx.action(HostAction::SteamScreenshot),
            OVERLAY => ctx.keys(&[KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_TAB]),
            ESCAPE => ctx.action(HostAction::Escape),
            ALT_TAB => ctx.action(HostAction::AltTab),
            _ => {}
        }
    }
}
