//! Borrowing the handheld's controls from InputPlumber.
//!
//! InputPlumber owns the physical gamepad and feeds the local Steam through
//! its "target" devices. While flipad runs we switch the target to a plain
//! Xbox 360 evdev device and grab it, so nothing local sees input. The
//! original targets are written to a restore file first; `restore` puts them
//! back and is also run by the guard process if the client dies.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use zbus::blocking::Connection;

const BUS: &str = "org.shadowblip.InputPlumber";
const COMPOSITE: &str = "/org/shadowblip/InputPlumber/CompositeDevice0";
const COMPOSITE_IF: &str = "org.shadowblip.Input.CompositeDevice";
const TARGET_IF: &str = "org.shadowblip.Input.Target";
/// InputPlumber's evdev Xbox 360 target, which we grab while running.
const BORROW_TARGET: &str = "gamepad";
const BORROWED_NAME: &str = "Microsoft X-Box 360 pad";

pub fn restore_file() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    Path::new(&dir).join("flipad-restore")
}

fn get<T: TryFrom<zbus::zvariant::OwnedValue>>(conn: &Connection, path: &str, iface: &str, prop: &str) -> zbus::Result<T>
where
    T::Error: Into<zbus::Error>,
{
    let p = zbus::blocking::fdo::PropertiesProxy::builder(conn).destination(BUS)?.path(path)?.build()?;
    let v = p.get(iface.try_into()?, prop)?;
    T::try_from(v).map_err(Into::into)
}

fn set_targets(conn: &Connection, types: &[String]) -> zbus::Result<()> {
    conn.call_method(Some(BUS), COMPOSITE, Some(COMPOSITE_IF), "SetTargetDevices", &(types,))?;
    Ok(())
}

fn current_target_types(conn: &Connection) -> zbus::Result<Vec<String>> {
    let paths: Vec<String> = get(conn, COMPOSITE, COMPOSITE_IF, "TargetDevices")?;
    let mut types = Vec::new();
    for p in paths {
        let t: String = get(conn, &p, TARGET_IF, "DeviceType")?;
        if t != "dbus" {
            types.push(t);
        }
    }
    Ok(types)
}

/// Puts back the targets saved in the restore file, if any.
pub fn restore() -> Result<bool, String> {
    let path = restore_file();
    let Ok(saved) = std::fs::read_to_string(&path) else { return Ok(false) };
    let types: Vec<String> = saved.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
    let conn = Connection::system().map_err(|e| e.to_string())?;
    set_targets(&conn, &types).map_err(|e| format!("SetTargetDevices: {e}"))?;
    let _ = std::fs::remove_file(&path);
    Ok(true)
}

/// (node, device name) pairs: node numbers get reused when InputPlumber
/// swaps targets, so the name is part of the identity.
fn event_nodes() -> HashSet<(String, String)> {
    std::fs::read_dir("/sys/class/input")
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("event"))
                .map(|n| {
                    let name = node_name(&n);
                    (n, name)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn node_name(node: &str) -> String {
    std::fs::read_to_string(format!("/sys/class/input/{node}/device/name")).unwrap_or_default().trim().to_string()
}

/// Switches InputPlumber to the Xbox 360 target and returns its evdev path.
pub fn borrow() -> Result<PathBuf, String> {
    // a previous run died without cleaning up and its guard did not either
    restore()?;
    let conn = Connection::system().map_err(|e| e.to_string())?;
    let original = current_target_types(&conn).map_err(|e| format!("read targets: {e}"))?;
    if original.iter().any(|t| t == BORROW_TARGET) {
        return Err(format!("InputPlumber already uses a '{BORROW_TARGET}' target"));
    }
    std::fs::write(restore_file(), original.join("\n")).map_err(|e| format!("write restore file: {e}"))?;

    let before = event_nodes();
    if let Err(e) = set_targets(&conn, &[BORROW_TARGET.to_string()]) {
        let _ = restore();
        return Err(format!("SetTargetDevices: {e}"));
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some((node, _)) = event_nodes().difference(&before).find(|(_, name)| name == BORROWED_NAME) {
            return Ok(Path::new("/dev/input").join(node));
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    let _ = restore();
    Err("InputPlumber did not create the Xbox 360 target".into())
}

/// Runs in a separate process: waits for `pid` to exit, then restores.
pub fn guard(pid: i32) {
    while Path::new(&format!("/proc/{pid}")).exists() {
        std::thread::sleep(Duration::from_millis(250));
    }
    crate::audio::restore_quantum();
    crate::ble::restore_alias();
    match restore() {
        Ok(true) => eprintln!("guard: restored InputPlumber targets after pid {pid} exited"),
        Ok(false) => {}
        Err(e) => eprintln!("guard: restore failed: {e}"),
    }
}

/// Starts the guard outside Steam's process tree, so it outlives a kill of
/// the game's whole process group.
pub fn spawn_guard() {
    let me = std::process::id();
    let Ok(exe) = std::env::current_exe() else { return };
    let ok = std::process::Command::new("systemd-run")
        .args(["--user", "--quiet", "--collect", &format!("--unit=flipad-guard-{me}")])
        .arg(&exe)
        .args(["--guard", &me.to_string()])
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("systemd-run failed; falling back to a detached guard");
        unsafe {
            if libc::fork() == 0 {
                libc::setsid();
                guard(me as i32);
                libc::_exit(0);
            }
        }
    }
}
