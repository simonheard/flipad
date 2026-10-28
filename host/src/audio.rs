//! Host side of the headset: while headphones are plugged into the handheld,
//! a "Flip 2 耳机" sink streams to it and becomes the default output; while a
//! headset mic is plugged in, a "Flip 2 麦克风" source becomes the default
//! input. Unplugging or disconnecting puts the previous defaults back.
//!
//! Runs on its own thread because starting PipeWire nodes takes a moment and
//! must not stall controller input.

use std::net::IpAddr;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use flipad_proto::audio::{Channels, PwProcess, node_names, pactl, receiver_conf, sender_conf};
use flipad_proto::{AUDIO_PORT, MIC_PORT};

const SINK: &str = "flipad_headphones";
const SOURCE: &str = "flipad_mic";
/// Jitter buffer for the microphone stream.
const MIC_LATENCY_MS: u32 = 30;

/// What the handheld currently wants; `None` = no handheld connected.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Want {
    pub ip: IpAddr,
    pub headphones: bool,
    pub mic: bool,
}

pub struct Audio {
    tx: Sender<Option<Want>>,
    thread: Option<JoinHandle<()>>,
}

impl Audio {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new().name("audio".into()).spawn(move || run(rx)).ok();
        Self { tx, thread }
    }

    pub fn set(&self, want: Option<Want>) {
        let _ = self.tx.send(want);
    }

    /// Tears everything down and restores the defaults before returning.
    pub fn shutdown(mut self) {
        let _ = self.tx.send(None);
        drop(std::mem::replace(&mut self.tx, mpsc::channel().0));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// One routed direction: the PipeWire process plus the default it replaced.
struct Route {
    proc: PwProcess,
    ip: Option<IpAddr>,
    previous: Option<String>,
    kind: &'static str,
    node: &'static str,
}

impl Route {
    fn start(kind: &'static str, node: &'static str, ip: Option<IpAddr>, conf: String) -> Option<Self> {
        let proc = match PwProcess::spawn(node, &conf) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("audio: cannot start {node}: {e}");
                return None;
            }
        };
        let list = if kind == "sink" { "sinks" } else { "sources" };
        let deadline = Instant::now() + Duration::from_secs(3);
        while !node_names(list).iter().any(|n| n == node) {
            if Instant::now() > deadline {
                eprintln!("audio: {node} did not appear");
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let previous = pactl(&[&format!("get-default-{kind}")]).filter(|p| p != node);
        pactl(&[&format!("set-default-{kind}"), node]);
        eprintln!("audio: {node} is now the default {kind} (was {previous:?})");
        Some(Self { proc, ip, previous, kind, node })
    }
}

impl Drop for Route {
    fn drop(&mut self) {
        // only undo our own change; the user may have picked something else since
        let current = pactl(&[&format!("get-default-{}", self.kind)]);
        if current.as_deref() == Some(self.node)
            && let Some(prev) = &self.previous
        {
            pactl(&[&format!("set-default-{}", self.kind), prev]);
        }
        eprintln!("audio: {} stopped", self.node);
    }
}

fn run(rx: Receiver<Option<Want>>) {
    let mut want: Option<Want> = None;
    let mut out: Option<Route> = None;
    let mut mic: Option<Route> = None;
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(w) => want = w,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => want = None,
        }
        let disconnected = matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected));

        // headphones: host → handheld
        let out_ip = want.filter(|w| w.headphones).map(|w| w.ip);
        if out.as_mut().is_some_and(|r| r.ip != out_ip || !r.proc.alive()) {
            out = None;
        }
        if out.is_none()
            && let Some(ip) = out_ip
        {
            let props = [("node.name", SINK), ("node.description", "Flip 2 耳机"), ("media.class", "Audio/Sink")];
            out = Route::start("sink", SINK, Some(ip), sender_conf(ip, AUDIO_PORT, Channels::Stereo, &props));
        }

        // microphone: handheld → host
        let mic_on = want.is_some_and(|w| w.mic);
        if mic.as_mut().is_some_and(|r| !mic_on || !r.proc.alive()) {
            mic = None;
        }
        if mic.is_none() && mic_on {
            let props = [("node.name", SOURCE), ("node.description", "Flip 2 麦克风"), ("media.class", "Audio/Source")];
            mic = Route::start("source", SOURCE, None, receiver_conf(MIC_PORT, MIC_LATENCY_MS, Channels::Mono, &props));
        }

        if disconnected && want.is_none() {
            return; // routes drop here, restoring defaults
        }
    }
}
