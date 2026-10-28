//! Bluetooth mode, host side: when the handheld connects as a Bluetooth
//! controller, also bring up its audio (A2DP) and make it the default output;
//! put the previous default back when it goes away.
//!
//! Needed because a Linux host keeps the LE link for the controller and does
//! not open the classic audio link by itself, and many desktops are not
//! connectable, so the handheld cannot open it either.

use std::process::{Command, Stdio};
use std::time::Duration;

use flipad_proto::audio::{node_names, pactl};

/// How the handheld identifies itself (BlueZ DeviceID on the handheld).
const MODALIAS: &str = "usb:v045Ep0B13";
const A2DP_SINK: &str = "0000110b-0000-1000-8000-00805f9b34fb";

fn run(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Connected Bluetooth devices that are a Flip Pad.
fn connected_pads() -> Vec<String> {
    run("bluetoothctl", &["devices", "Connected"])
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1).map(String::from))
        .filter(|addr| run("bluetoothctl", &["info", addr]).contains(MODALIAS))
        .collect()
}

fn previous_file() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::Path::new(&home).join(".config/flipad/previous-default-sink")
}

pub fn run_forever() {
    let mut asked: Vec<String> = Vec::new();
    let mut switched: Option<String> = None;
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let pads = connected_pads();
        asked.retain(|a| pads.contains(a));
        let sinks = node_names("sinks");
        for addr in &pads {
            let tag = addr.replace(':', "_");
            let sink = sinks.iter().find(|s| s.starts_with("bluez_output.") && s.contains(&tag)).cloned();
            match sink {
                None if !asked.contains(addr) => {
                    asked.push(addr.clone());
                    let path = format!("/org/bluez/hci0/dev_{tag}");
                    let ok = Command::new("busctl")
                        .args(["call", "--timeout=15", "org.bluez", &path, "org.bluez.Device1", "ConnectProfile", "s", A2DP_SINK])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .is_ok_and(|s| s.success());
                    eprintln!("bt audio: connect {addr}: {}", if ok { "ok" } else { "failed" });
                }
                Some(sink) if switched.as_deref() != Some(&sink) => {
                    if let Some(prev) = pactl(&["get-default-sink"]).filter(|p| !p.starts_with("bluez_output.")) {
                        let _ = std::fs::write(previous_file(), &prev);
                    }
                    pactl(&["set-default-sink", &sink]);
                    eprintln!("bt audio: {sink} is now the default sink");
                    switched = Some(sink);
                }
                _ => {}
            }
        }
        if let Some(s) = &switched
            && !sinks.contains(s)
        {
            if let Ok(prev) = std::fs::read_to_string(previous_file()) {
                pactl(&["set-default-sink", prev.trim()]);
            }
            eprintln!("bt audio: {s} went away, default restored");
            switched = None;
        }
    }
}
