//! Host readings shown on the handheld: CPU load and temperature, memory, GPU.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use flipad_proto::Status;

pub struct Stats {
    prev_cpu: Option<(u64, u64)>,
    cpu_temp_path: Option<String>,
    gpu: Arc<Mutex<(Option<u8>, Option<f32>)>>,
}

impl Stats {
    pub fn new() -> Self {
        let gpu = Arc::new(Mutex::new((None, None)));
        spawn_nvidia_smi(gpu.clone());
        Self { prev_cpu: None, cpu_temp_path: find_cpu_temp(), gpu }
    }

    /// Call about once a second; CPU load is measured between calls.
    pub fn sample(&mut self) -> Status {
        let (gpu_pct, gpu_temp) = *self.gpu.lock().unwrap();
        Status {
            cpu_pct: self.cpu_pct(),
            cpu_temp: self.cpu_temp_path.as_deref().and_then(read_millideg),
            gpu_pct,
            gpu_temp,
            mem_pct: mem_pct(),
        }
    }

    fn cpu_pct(&mut self) -> Option<u8> {
        let text = std::fs::read_to_string("/proc/stat").ok()?;
        let line = text.lines().next()?;
        let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
        let idle = v.get(3)? + v.get(4).unwrap_or(&0);
        let total: u64 = v.iter().sum();
        let prev = self.prev_cpu.replace((idle, total))?;
        let dt = total.saturating_sub(prev.1);
        (dt > 0).then(|| (100 * (dt - idle.saturating_sub(prev.0).min(dt)) / dt) as u8)
    }
}

fn read_millideg(path: &str) -> Option<f32> {
    std::fs::read_to_string(path).ok()?.trim().parse::<f32>().ok().map(|m| m / 1000.0)
}

/// Prefers the CPU package sensor (coretemp "Package id 0", k10temp "Tctl").
fn find_cpu_temp() -> Option<String> {
    let mut fallback = None;
    for hw in std::fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
        let dir = hw.path();
        let name = std::fs::read_to_string(dir.join("name")).unwrap_or_default();
        if !matches!(name.trim(), "coretemp" | "k10temp" | "zenpower") {
            continue;
        }
        for i in 1..=64 {
            let input = dir.join(format!("temp{i}_input"));
            if !input.exists() {
                continue;
            }
            let label = std::fs::read_to_string(dir.join(format!("temp{i}_label"))).unwrap_or_default();
            let path = input.to_string_lossy().into_owned();
            if label.starts_with("Package") || label.starts_with("Tctl") {
                return Some(path);
            }
            fallback.get_or_insert(path);
        }
    }
    fallback
}

fn mem_pct() -> Option<u8> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let field = |k: &str| -> Option<u64> {
        text.lines().find(|l| l.starts_with(k))?.split_whitespace().nth(1)?.parse().ok()
    };
    let (total, avail) = (field("MemTotal:")?, field("MemAvailable:")?);
    (total > 0).then(|| (100 * (total - avail.min(total)) / total) as u8)
}

/// Keeps one `nvidia-smi` running in loop mode instead of spawning it every second.
fn spawn_nvidia_smi(out: Arc<Mutex<(Option<u8>, Option<f32>)>>) {
    std::thread::spawn(move || {
        let child = Command::new("nvidia-smi")
            .args(["--query-gpu=utilization.gpu,temperature.gpu", "--format=csv,noheader,nounits", "-lms", "1000"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else { return };
        let Some(stdout) = child.stdout.take() else { return };
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let mut it = line.split(',').map(|s| s.trim());
            let util = it.next().and_then(|s| s.parse::<u8>().ok());
            let temp = it.next().and_then(|s| s.parse::<f32>().ok());
            *out.lock().unwrap() = (util, temp);
        }
    });
}
