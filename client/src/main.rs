//! flipad client: runs on the Retroid Flip 2 (launched from its Steam) and
//! turns it into a wireless controller for the PC, with a status screen.
//!
//! While running, the handheld's own controls are borrowed from InputPlumber
//! and grabbed, so the local Steam sees nothing. Quitting the app (exit combo,
//! on-screen button, Steam stopping it, suspend, or a crash via the guard
//! process) hands them back.

mod audio;
mod ble;
mod btlink;
mod link;
mod paint;
mod panel;
mod plumber;
mod settings;
mod shaper;
mod touch;
mod sleep;
mod ui;

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use evdev::raw_stream::RawDevice;
use flipad_proto::{PORT, parse_token};
use link::{Phase, Shared, View};
use settings::Mode;
use winit::event_loop::EventLoop;

fn config_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/flipad")
}

fn read_host(arg: Option<String>) -> Option<SocketAddr> {
    let s = arg.or_else(|| std::fs::read_to_string(config_dir().join("host")).ok())?;
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let with_port = if s.contains(':') { s.to_string() } else { format!("{s}:{PORT}") };
    with_port.to_socket_addrs().ok()?.next()
}

/// Wi-Fi power saving adds tens of milliseconds of jitter; turn it off while
/// running. Needs a sudoers rule for exactly these commands, else it is skipped.
fn wifi_power_save(on: bool) {
    let _ = std::process::Command::new("sudo")
        .args(["-n", "iw", "dev", "wlan0", "set", "power_save", if on { "on" } else { "off" }])
        .stderr(std::process::Stdio::null())
        .status();
}

/// False once the borrowed controller node has gone away.
fn controller_alive(dev: &RawDevice) -> bool {
    use std::os::fd::AsRawFd;
    let mut pfd = libc::pollfd { fd: dev.as_raw_fd(), events: 0, revents: 0 };
    unsafe { libc::poll(&mut pfd, 1, 0) };
    pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) == 0
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut host_arg = None;
    let mut bluetooth = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--guard" => {
                let pid = args.next().and_then(|p| p.parse().ok()).unwrap_or(0);
                plumber::guard(pid);
                return;
            }
            "--restore" => {
                match plumber::restore() {
                    Ok(true) => println!("restored"),
                    Ok(false) => println!("nothing to restore"),
                    Err(e) => eprintln!("{e}"),
                }
                return;
            }
            "--host" => host_arg = args.next(),
            "--bluetooth" => bluetooth = true,
            "--preview" => {
                let out = args.next().unwrap_or_else(|| "flipad-preview.png".into());
                if let Err(e) = ui::preview(&out) {
                    eprintln!("{e}");
                }
                return;
            }
            _ => {
                eprintln!("usage: flipad-client [--host ADDR[:PORT] | --bluetooth] | --restore | --guard PID");
                std::process::exit(2);
            }
        }
    }

    // before any threads exist (the fallback guard forks)
    plumber::spawn_guard();

    let event_loop = EventLoop::<()>::with_user_event().build().expect("event loop");
    let proxy = event_loop.create_proxy();
    let quit = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT, signal_hook::consts::SIGHUP] {
        let _ = signal_hook::flag::register(sig, quit.clone());
    }
    let (wake_r, wake_w) = {
        let mut fds = [0; 2];
        unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
        use std::os::fd::FromRawFd;
        unsafe { (std::os::fd::OwnedFd::from_raw_fd(fds[0]), std::os::fd::OwnedFd::from_raw_fd(fds[1])) }
    };
    let shared = Arc::new(Shared {
        view: Mutex::new(View::default()),
        quit,
        jack: Arc::default(),
        settings: Mutex::new({
            let mut s = settings::Settings::load();
            if bluetooth {
                s.mode = Mode::Bluetooth;
            }
            s
        }),
        cmds: Mutex::default(),
        pointer: Mutex::default(),
        frames: Mutex::default(),
        wake: wake_w,
        notify: Box::new(move || {
            let _ = proxy.send_event(());
        }),
    });
    let sleep_lock = sleep::watch(shared.clone());
    // debugging aid: SIGUSR2 runs the touchpad and shortcut paths without touch
    {
        let probe = Arc::new(AtomicBool::new(false));
        let _ = signal_hook::flag::register(signal_hook::consts::SIGUSR2, probe.clone());
        let s = shared.clone();
        std::thread::spawn(move || {
            while !s.quit.load(Ordering::Relaxed) {
                if probe.swap(false, Ordering::Relaxed) {
                    eprintln!("self-test: pointer +120 then back, volume up then down");
                    s.pointer(link::PointerOp::Move(120.0, 0.0));
                    std::thread::sleep(std::time::Duration::from_millis(600));
                    s.pointer(link::PointerOp::Move(-120.0, 0.0));
                    s.command(link::Cmd::Action(flipad_proto::HostAction::VolumeUp));
                    std::thread::sleep(std::time::Duration::from_millis(600));
                    s.command(link::Cmd::Action(flipad_proto::HostAction::VolumeDown));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });
    }
    // debugging aid: SIGUSR1 flips between Wi-Fi and Bluetooth like the panel does
    {
        let flip = Arc::new(AtomicBool::new(false));
        let _ = signal_hook::flag::register(signal_hook::consts::SIGUSR1, flip.clone());
        let s = shared.clone();
        std::thread::spawn(move || {
            while !s.quit.load(Ordering::Relaxed) {
                if flip.swap(false, Ordering::Relaxed) {
                    let mut st = s.settings.lock().unwrap();
                    st.mode = if st.mode == Mode::Wifi { Mode::Bluetooth } else { Mode::Wifi };
                    eprintln!("mode -> {:?}", st.mode);
                    drop(st);
                    s.poke_pub();
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });
    }
    {
        let s = shared.clone();
        audio::watch_jack(shared.jack.clone(), move || {
            let j = *s.jack.lock().unwrap();
            s.view.lock().unwrap().jack = j;
            (s.notify)();
        });
    }

    let token = std::fs::read_to_string(config_dir().join("token")).ok().and_then(|s| parse_token(&s));
    let host = read_host(host_arg);
    let link_shared = shared.clone();
    let link_thread = std::thread::spawn(move || {
        let fail = |msg: String| {
            eprintln!("{msg}");
            link_shared.view.lock().unwrap().phase = Phase::Rejected(msg);
            (link_shared.notify)();
        };
        let path = match plumber::borrow() {
            Ok(p) => p,
            Err(e) => return fail(format!("接管按键失败：{e}")),
        };
        // udev fixes up the node's group a moment after it appears
        let open = || {
            let mut tries = 0;
            loop {
                match RawDevice::open(&path) {
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && tries < 30 => {
                        tries += 1;
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    r => return r,
                }
            }
        };
        let mut dev = match open() {
            Ok(d) => d,
            Err(e) => {
                let _ = plumber::restore();
                return fail(format!("{}: {e}", path.display()));
            }
        };
        if let Err(e) = dev.grab() {
            let _ = plumber::restore();
            return fail(format!("grab: {e}"));
        }

        // one borrowed controller, two ways to reach a host; the panel switches
        let name = std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "flip2".into());
        let sh = &link_shared;
        let mode_now = || sh.settings.lock().unwrap().mode;
        let wait_for_switch = |from: Mode| {
            while !sh.quit.load(Ordering::Relaxed) && mode_now() == from {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        };
        while !sh.quit.load(Ordering::Relaxed) {
            let mode = mode_now();
            sh.update(|v| {
                v.bluetooth = mode == Mode::Bluetooth;
                v.phase = Phase::Searching;
                v.host.clear();
                v.rtt_ms = None;
            });
            let result = match mode {
                Mode::Wifi => {
                    let Some(token) = token else {
                        fail(format!("缺少 {}", config_dir().join("token").display()));
                        wait_for_switch(mode);
                        continue;
                    };
                    let cfg = link::Config { token, host, name: name.clone() };
                    wifi_power_save(false);
                    let r = link::run(&mut dev, &wake_r, &cfg, sh.clone());
                    wifi_power_save(true);
                    r
                }
                Mode::Bluetooth => btlink::run(&mut dev, &wake_r, sh.clone()),
            };
            if let Err(e) = result {
                eprintln!("{mode:?}: {e}");
                if !controller_alive(&dev) {
                    break;
                }
                // e.g. Bluetooth failed to start: keep the error up until switched
                wait_for_switch(mode);
            }
        }
        drop(dev);
        if let Err(e) = plumber::restore() {
            eprintln!("restore: {e}");
        }
        sh.quit.store(true, Ordering::Relaxed);
        (sh.notify)();
    });

    let mut app = ui::App::new(shared.clone());
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("ui: {e}");
    }
    shared.quit.store(true, Ordering::Relaxed);
    let _ = link_thread.join();
    drop(sleep_lock);
}
