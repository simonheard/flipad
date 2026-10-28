//! Handheld side of the headset: watches the 3.5mm jack, plays the host's
//! audio into the headphones and sends the headset microphone to the host.

use std::net::IpAddr;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use evdev::SwitchCode;
use evdev::raw_stream::RawDevice;
use flipad_proto::audio::{Channels, PwProcess, node_names, receiver_conf, sender_conf};
use flipad_proto::{AUDIO_PORT, MIC_PORT};

const JACK_NAME: &str = "RetroidPocket Headset Jack";
/// Graph cycle while streaming. Pocknix pins 1024 frames (~21 ms), which makes
/// the headphone device alone slower than the jitter buffer; 512 measured
/// clean on the SM8250 while 256 caused capture xruns.
const STREAM_QUANTUM: u32 = 512;

fn quantum_file() -> std::path::PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    std::path::Path::new(&dir).join("flipad-quantum")
}

fn force_quantum(q: u32) {
    let _ = std::process::Command::new("pw-metadata")
        .args(["-n", "settings", "0", "clock.force-quantum", &q.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn current_quantum() -> u32 {
    let out = std::process::Command::new("pw-metadata").args(["-n", "settings", "0", "clock.force-quantum"]).output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    // "update: id:0 key:'clock.force-quantum' value:'1024' type:''"
    text.split("value:'").nth(1).and_then(|v| v.split('\'').next()).and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// Lowers the graph quantum, remembering the old value for `restore_quantum`.
fn lower_quantum() {
    let q: u32 = std::env::var("FLIPAD_QUANTUM").ok().and_then(|v| v.parse().ok()).unwrap_or(STREAM_QUANTUM);
    if q == 0 {
        return;
    }
    if !quantum_file().exists() {
        let _ = std::fs::write(quantum_file(), current_quantum().to_string());
    }
    force_quantum(q);
}

/// Puts back the quantum saved by `lower_quantum`, if any. Also run by the guard.
pub fn restore_quantum() {
    if let Ok(prev) = std::fs::read_to_string(quantum_file()) {
        force_quantum(prev.trim().parse().unwrap_or(0));
        let _ = std::fs::remove_file(quantum_file());
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Jack {
    pub headphones: bool,
    pub mic: bool,
}

/// Keeps `state` in sync with the jack. `FLIPAD_FORCE_JACK=hp,mic` fakes it
/// for testing without a headset.
pub fn watch_jack(state: Arc<Mutex<Jack>>, notify: impl Fn() + Send + 'static) {
    if let Ok(forced) = std::env::var("FLIPAD_FORCE_JACK") {
        *state.lock().unwrap() = Jack { headphones: forced.contains("hp"), mic: forced.contains("mic") };
        return;
    }
    std::thread::spawn(move || {
        let Some(dev) = open_jack() else {
            eprintln!("jack: '{JACK_NAME}' not found; headset audio disabled");
            return;
        };
        loop {
            // the switch state can be read even while InputPlumber grabs the device
            if let Ok(sw) = dev.get_switch_state() {
                let now = Jack {
                    headphones: sw.contains(SwitchCode::SW_HEADPHONE_INSERT),
                    mic: sw.contains(SwitchCode::SW_MICROPHONE_INSERT),
                };
                let changed = std::mem::replace(&mut *state.lock().unwrap(), now) != now;
                if changed {
                    notify();
                }
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    });
}

fn open_jack() -> Option<RawDevice> {
    for e in std::fs::read_dir("/sys/class/input").ok()?.flatten() {
        let node = e.file_name().to_string_lossy().into_owned();
        if !node.starts_with("event") {
            continue;
        }
        let name = std::fs::read_to_string(e.path().join("device/name")).unwrap_or_default();
        if name.trim() == JACK_NAME {
            return RawDevice::open(format!("/dev/input/{node}")).ok();
        }
    }
    None
}

/// Desired routing; `None` = not connected to a host.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Want {
    pub host: IpAddr,
    pub jack: Jack,
}

pub struct Audio {
    tx: Option<Sender<Option<Want>>>,
    thread: Option<JoinHandle<()>>,
}

impl Audio {
    pub fn start(latency_ms: u32) -> Self {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new().name("audio".into()).spawn(move || run(rx, latency_ms)).ok();
        Self { tx: Some(tx), thread }
    }

    pub fn set(&self, want: Option<Want>) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(want);
        }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// First node of `kind` whose name contains `hint`.
fn find_node(kind: &str, hint: &str) -> Option<String> {
    node_names(kind).into_iter().find(|n| n.contains(hint) && !n.ends_with(".monitor"))
}

fn run(rx: Receiver<Option<Want>>, latency_ms: u32) {
    let mut want: Option<Want> = None;
    let mut playing: Option<(IpAddr, PwProcess)> = None;
    let mut sending: Option<(IpAddr, PwProcess)> = None;
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(w) => want = w,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                drop(playing.take());
                restore_quantum();
                return;
            }
        }

        let hp = want.filter(|w| w.jack.headphones).map(|w| w.host);
        if playing.as_mut().is_some_and(|(ip, p)| Some(*ip) != hp || !p.alive()) {
            playing = None;
            restore_quantum();
            eprintln!("audio: headphones stream stopped");
        }
        if playing.is_none()
            && let Some(ip) = hp
        {
            let target = find_node("sinks", "Headphones").unwrap_or_default();
            let props = [
                ("node.name", "flipad_from_host"),
                ("node.description", "Flip Pad 主机音频"),
                ("media.class", "Stream/Output/Audio"),
                ("target.object", target.as_str()),
            ];
            lower_quantum();
            match PwProcess::spawn("flipad-headphones", &receiver_conf(AUDIO_PORT, latency_ms, Channels::Stereo, &props)) {
                Ok(p) => {
                    eprintln!("audio: playing host audio into {target:?}");
                    playing = Some((ip, p));
                }
                Err(e) => eprintln!("audio: {e}"),
            }
        }

        let mic = want.filter(|w| w.jack.mic).map(|w| w.host);
        if sending.as_mut().is_some_and(|(ip, p)| Some(*ip) != mic || !p.alive()) {
            sending = None;
            eprintln!("audio: microphone stream stopped");
        }
        if sending.is_none()
            && let Some(ip) = mic
        {
            let source = find_node("sources", "Headset").unwrap_or_default();
            let props = [
                ("node.name", "flipad_mic_out"),
                ("node.description", "Flip Pad 麦克风"),
                // capture from the headset mic instead of acting as a sink
                ("media.class", "Stream/Input/Audio"),
                ("target.object", source.as_str()),
            ];
            match PwProcess::spawn("flipad-mic", &sender_conf(ip, MIC_PORT, Channels::Mono, &props)) {
                Ok(p) => {
                    eprintln!("audio: sending {source:?} to the host");
                    sending = Some((ip, p));
                }
                Err(e) => eprintln!("audio: {e}"),
            }
        }
    }
}
