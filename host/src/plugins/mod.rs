//! Game plugins: per-game features on the handheld's "游戏" tab.
//!
//! Every second the host looks at what is running; the first plugin whose
//! `matches` accepts it becomes active and owns the tab. A plugin can show
//! text and buttons, stream images (a minimap, a map region, a scoreboard…)
//! and press keys on the host.
//!
//! To add one: write a type implementing [`GamePlugin`] in this directory and
//! list it in [`registry`]. Order matters: specific plugins go before the
//! catch-all `current_game`.

mod current_game;
mod demo;

use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flipad_proto::{FRAME_CHUNK, HostAction, Msg, Widget};

use crate::keys::KeyJob;

/// What is running on the host.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GameInfo {
    /// Steam app id, for games started by Steam.
    pub app_id: Option<u32>,
    /// Name from the Steam manifest, or the process name.
    pub name: String,
    /// Process name (`/proc/<pid>/comm`) of the game.
    pub process: String,
    pub pid: u32,
    pub started: Option<Instant>,
}

/// What a plugin may do while active.
pub struct Ctx<'a> {
    pub game: &'a GameInfo,
    panel: &'a mut (String, Vec<Widget>),
    keys: &'a Sender<KeyJob>,
    frames: &'a mut Vec<(u16, Vec<u8>)>,
}

impl Ctx<'_> {
    /// Replaces the panel shown on the handheld.
    pub fn set_panel(&mut self, title: &str, widgets: Vec<Widget>) {
        *self.panel = (title.to_string(), widgets);
    }

    /// Runs one of the standard shortcuts.
    pub fn action(&self, a: HostAction) {
        let _ = self.keys.send(KeyJob::Action(a));
    }

    /// Presses a key combination (pressed in order, released in reverse).
    pub fn keys(&self, combo: &[evdev::KeyCode]) {
        let _ = self.keys.send(KeyJob::Combo(combo.to_vec()));
    }

    /// Sends a JPEG for an `Image` widget.
    #[allow(dead_code)] // for plugins that already have JPEG data
    pub fn send_jpeg(&mut self, image: u16, jpeg: Vec<u8>) {
        self.frames.push((image, jpeg));
    }

    /// Encodes RGBA pixels as JPEG and sends them for an `Image` widget.
    pub fn send_rgba(&mut self, image: u16, width: u16, height: u16, rgba: &[u8]) {
        let mut out = Vec::new();
        let enc = jpeg_encoder::Encoder::new(&mut out, 72);
        if enc.encode(rgba, width, height, jpeg_encoder::ColorType::Rgba).is_ok() {
            self.frames.push((image, out));
        }
    }
}

pub trait GamePlugin: Send {
    /// Stable id, sent with button taps.
    fn id(&self) -> &'static str;
    /// Whether this plugin wants to be active for `game` (None = no game).
    fn matches(&self, game: Option<&GameInfo>) -> bool;
    fn activate(&mut self, _ctx: &mut Ctx) {}
    fn deactivate(&mut self) {}
    /// Called about ten times a second while active.
    fn tick(&mut self, _ctx: &mut Ctx) {}
    fn button(&mut self, _id: u16, _ctx: &mut Ctx) {}
}

/// All plugins, most specific first.
fn registry() -> Vec<Box<dyn GamePlugin>> {
    let mut v: Vec<Box<dyn GamePlugin>> = Vec::new();
    if std::env::var_os("FLIPAD_DEMO_PLUGIN").is_some() {
        v.push(Box::new(demo::Radar::default()));
    }
    // game-specific plugins go here, before the catch-all
    v.push(Box::new(current_game::CurrentGame::default()));
    v
}

/// Finds the running Steam game, if any.
fn detect(prev: Option<&GameInfo>) -> Option<GameInfo> {
    let me = unsafe { libc::getuid() };
    let mut found: Option<(u32, u32, String)> = None;
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
        let path = entry.path();
        if std::fs::metadata(&path).map(|m| std::os::unix::fs::MetadataExt::uid(&m)).ok() != Some(me) {
            continue;
        }
        let Ok(env) = std::fs::read(path.join("environ")) else { continue };
        let app = env
            .split(|&b| b == 0)
            .find_map(|kv| kv.strip_prefix(b"SteamAppId=").map(|v| String::from_utf8_lossy(v).parse::<u32>().unwrap_or(0)))
            .filter(|&a| a != 0);
        let Some(app) = app else { continue };
        let comm = std::fs::read_to_string(path.join("comm")).unwrap_or_default().trim().to_string();
        // skip Steam's own helpers that inherit the variable
        if matches!(comm.as_str(), "reaper" | "steam" | "pressure-vessel" | "pv-bwrap" | "srt-bwrap" | "steam-launch-wr") {
            continue;
        }
        // the newest process is usually the game itself rather than a launcher
        if found.as_ref().is_none_or(|f| pid > f.1) {
            found = Some((app, pid, comm));
        }
    }
    let (app, pid, process) = found?;
    if let Some(p) = prev
        && p.app_id == Some(app)
    {
        return Some(GameInfo { pid, process, ..p.clone() });
    }
    let name = steam_name(app).unwrap_or_else(|| process.clone());
    Some(GameInfo { app_id: Some(app), name, process, pid, started: Some(Instant::now()) })
}

fn steam_roots() -> Vec<std::path::PathBuf> {
    let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let mut roots = vec![home.join(".local/share/Steam"), home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam")];
    if let Ok(text) = std::fs::read_to_string(roots[0].join("steamapps/libraryfolders.vdf")) {
        for line in text.lines() {
            let parts: Vec<&str> = line.split('"').collect();
            if parts.len() >= 4 && parts[1] == "path" {
                roots.push(parts[3].into());
            }
        }
    }
    roots
}

/// Game name from `appmanifest_<id>.acf`.
pub fn steam_name(app: u32) -> Option<String> {
    for root in steam_roots() {
        let Ok(text) = std::fs::read_to_string(root.join(format!("steamapps/appmanifest_{app}.acf"))) else { continue };
        if let Some(name) = parse_acf_name(&text) {
            return Some(name);
        }
    }
    None
}

fn parse_acf_name(text: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let parts: Vec<&str> = l.split('"').collect();
        (parts.len() >= 4 && parts[1] == "name").then(|| parts[3].to_string())
    })
}

/// Runs plugins and talks to the handheld. Button taps arrive on `buttons`.
pub fn run(sock: UdpSocket, peer: Arc<Mutex<Option<SocketAddr>>>, keys: Sender<KeyJob>, buttons: Receiver<(String, u16)>) {
    let mut plugins = registry();
    let mut active: Option<usize> = None;
    let mut game: Option<GameInfo> = None;
    let mut last_detect = Instant::now() - Duration::from_secs(10);
    let mut panel: (String, Vec<Widget>) = Default::default();
    let mut sent_panel: Option<(Option<usize>, (String, Vec<Widget>))> = None;
    let mut last_panel_send = Instant::now();
    let mut frame_no: u32 = 0;
    let mut frames: Vec<(u16, Vec<u8>)> = Vec::new();
    let mut had_peer = false;

    loop {
        std::thread::sleep(Duration::from_millis(100));
        let now = Instant::now();
        let to = *peer.lock().unwrap();
        if to.is_some() && !had_peer {
            sent_panel = None; // a handheld (re)connected: send everything again
        }
        had_peer = to.is_some();

        if now.duration_since(last_detect) >= Duration::from_secs(1) {
            last_detect = now;
            game = detect(game.as_ref());
            let want = plugins.iter().position(|p| p.matches(game.as_ref()));
            if want != active {
                if let Some(i) = active {
                    plugins[i].deactivate();
                }
                panel = Default::default();
                active = want;
                if let Some(i) = active {
                    eprintln!("plugin: {} active for {:?}", plugins[i].id(), game.as_ref().map(|g| &g.name));
                    let g = game.clone().unwrap_or_default();
                    let mut ctx = Ctx { game: &g, panel: &mut panel, keys: &keys, frames: &mut frames };
                    plugins[i].activate(&mut ctx);
                }
            }
        }

        if let Some(i) = active {
            let g = game.clone().unwrap_or_default();
            let mut ctx = Ctx { game: &g, panel: &mut panel, keys: &keys, frames: &mut frames };
            while let Ok((plugin, id)) = buttons.try_recv() {
                if plugin == plugins[i].id() {
                    plugins[i].button(id, &mut ctx);
                }
            }
            plugins[i].tick(&mut ctx);
        } else {
            while buttons.try_recv().is_ok() {}
        }

        let Some(to) = to else {
            frames.clear();
            continue;
        };
        let current = (active, panel.clone());
        if sent_panel.as_ref() != Some(&current) || now.duration_since(last_panel_send) >= Duration::from_secs(2) {
            let plugin = active.map(|i| plugins[i].id().to_string()).unwrap_or_default();
            let msg = Msg::Panel { plugin, title: panel.0.clone(), widgets: panel.1.clone() };
            let _ = sock.send_to(&msg.encode(), to);
            sent_panel = Some(current);
            last_panel_send = now;
        }
        for (image, jpeg) in frames.drain(..) {
            frame_no = frame_no.wrapping_add(1);
            let parts = jpeg.len().div_ceil(FRAME_CHUNK).max(1);
            for (part, chunk) in jpeg.chunks(FRAME_CHUNK).enumerate() {
                let msg = Msg::Frame { image, frame: frame_no, part: part as u16, parts: parts as u16, data: chunk.to_vec() };
                let _ = sock.send_to(&msg.encode(), to);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acf_name() {
        let acf = "\"AppState\"\n{\n\t\"appid\"\t\t\"1070560\"\n\t\"name\"\t\t\"Steam Linux Runtime 1.0\"\n}";
        assert_eq!(parse_acf_name(acf).as_deref(), Some("Steam Linux Runtime 1.0"));
    }

    #[test]
    fn catch_all_is_last() {
        let r = registry();
        assert_eq!(r.last().unwrap().id(), "current_game");
        assert!(!r.last().unwrap().matches(None), "no panel without a game");
    }
}
