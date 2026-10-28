//! Quits cleanly before the handheld suspends: holds a logind "delay" sleep
//! inhibitor and, on PrepareForSleep, asks the app to shut down. The inhibitor
//! is released once cleanup is done (or when the process exits).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedFd;

use crate::link::Shared;

pub struct SleepLock {
    _fd: Option<OwnedFd>,
}

/// Starts the watcher; keep the returned lock alive until cleanup finished.
pub fn watch(shared: Arc<Shared>) -> SleepLock {
    let Ok(conn) = Connection::system() else { return SleepLock { _fd: None } };
    let Ok(proxy) = Proxy::new(&conn, "org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager")
    else {
        return SleepLock { _fd: None };
    };
    let fd: Option<OwnedFd> = proxy
        .call("Inhibit", &("sleep", "flipad", "Hand the controls back before suspending", "delay"))
        .ok();
    let Ok(signals) = proxy.receive_signal("PrepareForSleep") else { return SleepLock { _fd: fd } };
    std::thread::spawn(move || {
        let _proxy = proxy;
        for msg in signals {
            if msg.body().deserialize::<bool>().unwrap_or(false) {
                eprintln!("suspending: shutting down");
                shared.quit.store(true, Ordering::Relaxed);
                (shared.notify)();
                // logind waits for the delay lock at most InhibitDelayMaxSec;
                // the main thread drops it as soon as cleanup is done.
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    });
    SleepLock { _fd: fd }
}
