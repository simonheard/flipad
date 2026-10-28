//! Bluetooth LE HID (HOGP) peripheral that reports as an Xbox Wireless
//! Controller (045e:0b13, firmware 5.x report layout), so Windows, Linux,
//! macOS and Steam drive it with their own Xbox support — nothing to install
//! on the host. The PnP ID comes from BlueZ's `DeviceID` in main.conf.
//!
//! Exposed over BlueZ's D-Bus GATT API: HID service (report map, input report
//! 1, rumble output report 3), battery service, an advertisement and a
//! "just works" pairing agent.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use flipad_proto::{Pad, button};

use crate::settings::{Addr, addr_str, parse_addr};
use zbus::blocking::{Connection, Proxy, connection};
use zbus::interface;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

/// What hosts list us as. Their Xbox support keys off the PnP ID, not the name.
pub const NAME: &str = "Flip Pad";
const APP: &str = "/flipad";
const INPUT: &str = "/flipad/hid/input";
const MOUSE: &str = "/flipad/hid2/mouse";
const MEDIA: &str = "/flipad/hid2/media";
const KEYS: &str = "/flipad/hid2/keys";
const BATTERY_LEVEL: &str = "/flipad/battery/level";
const ADV: &str = "/flipad_adv";
const AGENT: &str = "/flipad_agent";

/// HID report descriptor, modelled on the Xbox Wireless Controller over BLE.
#[rustfmt::skip]
pub const REPORT_MAP: &[u8] = &[
    0x05, 0x01, 0x09, 0x05, 0xA1, 0x01,             // Generic Desktop / Game Pad / Application
    0x85, 0x01,                                     //   Report ID 1
    0x09, 0x01, 0xA1, 0x00,                         //   Pointer, Physical
    0x09, 0x30, 0x09, 0x31,                         //     X, Y
    0x15, 0x00, 0x27, 0xFF, 0xFF, 0x00, 0x00,       //     0..65535
    0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0xC0,       //     16 bits x2, Input
    0x09, 0x01, 0xA1, 0x00,                         //   Pointer, Physical
    0x09, 0x32, 0x09, 0x35,                         //     Z, Rz
    0x15, 0x00, 0x27, 0xFF, 0xFF, 0x00, 0x00,
    0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0xC0,
    0x05, 0x02, 0x09, 0xC5,                         //   Simulation / Brake (LT)
    0x15, 0x00, 0x26, 0xFF, 0x03,                   //     0..1023
    0x75, 0x0A, 0x95, 0x01, 0x81, 0x02,
    0x25, 0x00, 0x75, 0x06, 0x95, 0x01, 0x81, 0x03, //     pad to 16 bits
    0x09, 0xC4,                                     //   Accelerator (RT)
    0x15, 0x00, 0x26, 0xFF, 0x03,
    0x75, 0x0A, 0x95, 0x01, 0x81, 0x02,
    0x25, 0x00, 0x75, 0x06, 0x95, 0x01, 0x81, 0x03,
    0x05, 0x01, 0x09, 0x39,                         //   Hat switch
    0x15, 0x01, 0x25, 0x08, 0x35, 0x00, 0x46, 0x3B, 0x01, 0x66, 0x14, 0x00,
    0x75, 0x04, 0x95, 0x01, 0x81, 0x42,             //     4 bits, null state
    0x75, 0x04, 0x95, 0x01, 0x15, 0x00, 0x25, 0x00, 0x35, 0x00, 0x45, 0x00, 0x65, 0x00,
    0x81, 0x03,                                     //     pad
    0x05, 0x09, 0x19, 0x01, 0x29, 0x0F,             //   Buttons 1..15
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0F, 0x81, 0x02,
    0x25, 0x00, 0x75, 0x01, 0x95, 0x01, 0x81, 0x03, //     pad
    0x05, 0x0C, 0x0A, 0xB2, 0x00,                   //   Consumer / Record (Share)
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02,
    0x25, 0x00, 0x75, 0x07, 0x95, 0x01, 0x81, 0x03,
    0x05, 0x0F, 0x09, 0x21, 0x85, 0x03,             //   PID / Set Effect Report, Report ID 3
    0xA1, 0x02,                                     //   Logical
    0x09, 0x97, 0x15, 0x00, 0x25, 0x01, 0x75, 0x04, 0x95, 0x01, 0x91, 0x02, // enable actuators
    0x15, 0x00, 0x25, 0x00, 0x75, 0x04, 0x95, 0x01, 0x91, 0x03,             // pad
    0x09, 0x70, 0x15, 0x00, 0x25, 0x64, 0x75, 0x08, 0x95, 0x04, 0x91, 0x02, // magnitude x4
    0x09, 0x50, 0x09, 0xA7, 0x09, 0x7C, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x03, 0x91, 0x02, // duration, delay, loops
    0xC0,
    0xC0,
];

/// Second HID service: mouse (report 2), media keys (4) and keyboard (5),
/// for the touchpad and shortcuts. Kept apart from the gamepad's service so
/// that one stays byte-for-byte what an Xbox controller reports.
#[rustfmt::skip]
pub const EXTRAS_MAP: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x02, // Mouse, report 2
    0x09, 0x01, 0xA1, 0x00,
    0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x03, 0x81, 0x02,
    0x75, 0x05, 0x95, 0x01, 0x81, 0x03,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x16, 0x01, 0x80, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x02, 0x81, 0x06,
    0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06,             // wheel
    0x05, 0x0C, 0x0A, 0x38, 0x02, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, // AC Pan
    0xC0, 0xC0,
    0x05, 0x0C, 0x09, 0x01, 0xA1, 0x01, 0x85, 0x04, // Consumer, report 4
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x06,
    0x09, 0xE9, 0x09, 0xEA, 0x09, 0xE2, 0x09, 0xCD, 0x09, 0xB5, 0x09, 0xB6, 0x81, 0x02,
    0x75, 0x02, 0x95, 0x01, 0x81, 0x03,
    0xC0,
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x85, 0x05, // Keyboard, report 5
    0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02,
    0x75, 0x08, 0x95, 0x01, 0x81, 0x03,
    0x05, 0x07, 0x19, 0x00, 0x29, 0xFF, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x06, 0x81, 0x00,
    0xC0,
];

/// Media keys in the consumer report's bit order.
pub mod media {
    pub const VOLUME_UP: u8 = 1 << 0;
    pub const VOLUME_DOWN: u8 = 1 << 1;
    pub const MUTE: u8 = 1 << 2;
    pub const PLAY_PAUSE: u8 = 1 << 3;
    pub const NEXT: u8 = 1 << 4;
    pub const PREVIOUS: u8 = 1 << 5;
}

/// What a shortcut becomes over Bluetooth: media bits, or keyboard
/// modifiers + key usages. None when it needs the host service.
pub fn shortcut(a: flipad_proto::HostAction) -> Option<(u8, u8, &'static [u8])> {
    use flipad_proto::HostAction as A;
    const ALT: u8 = 0x04;
    const GUI: u8 = 0x08;
    Some(match a {
        A::VolumeUp => (media::VOLUME_UP, 0, &[]),
        A::VolumeDown => (media::VOLUME_DOWN, 0, &[]),
        A::Mute => (media::MUTE, 0, &[]),
        A::PlayPause => (media::PLAY_PAUSE, 0, &[]),
        A::Next => (media::NEXT, 0, &[]),
        A::Previous => (media::PREVIOUS, 0, &[]),
        A::SteamScreenshot => (0, 0, &[0x45]),   // F12
        A::Screenshot => (0, 0, &[0x46]),        // Print Screen
        A::AltTab => (0, ALT, &[0x2B]),          // Tab
        A::Escape => (0, 0, &[0x29]),
        A::ShowDesktop => (0, GUI, &[0x07]),     // D
        A::BigPicture => return None,
    })
}

/// Xbox BLE button bit for each of our button bits.
const BUTTONS: [(u32, u16); 11] = [
    (button::SOUTH, 0),
    (button::EAST, 1),
    (button::WEST, 3),
    (button::NORTH, 4),
    (button::TL, 6),
    (button::TR, 7),
    (button::SELECT, 10),
    (button::START, 11),
    (button::MODE, 12),
    (button::THUMBL, 13),
    (button::THUMBR, 14),
];

/// Input report 1 (16 bytes, without the report id).
pub fn report(p: &Pad) -> [u8; 16] {
    let axis = |v: i16| (v as i32 + 32768) as u16;
    let trig = |v: u8| (v as u32 * 1023 / 255) as u16;
    let up = p.buttons & button::DPAD_UP != 0;
    let down = p.buttons & button::DPAD_DOWN != 0;
    let left = p.buttons & button::DPAD_LEFT != 0;
    let right = p.buttons & button::DPAD_RIGHT != 0;
    let hat: u8 = match (up && !down, down && !up, left && !right, right && !left) {
        (true, _, false, false) => 1,
        (true, _, false, true) => 2,
        (false, false, false, true) => 3,
        (_, true, false, true) => 4,
        (_, true, false, false) => 5,
        (_, true, true, false) => 6,
        (false, false, true, false) => 7,
        (true, _, true, false) => 8,
        _ => 0,
    };
    let mut bits: u16 = 0;
    for (ours, theirs) in BUTTONS {
        if p.buttons & ours != 0 {
            bits |= 1 << theirs;
        }
    }
    let mut r = [0u8; 16];
    for (i, v) in [axis(p.lx), axis(p.ly), axis(p.rx), axis(p.ry), trig(p.lt), trig(p.rt)].into_iter().enumerate() {
        r[i * 2..i * 2 + 2].copy_from_slice(&v.to_le_bytes());
    }
    r[12] = hat;
    r[13..15].copy_from_slice(&bits.to_le_bytes());
    r
}

/// A rumble request from the host (output report 3).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rumble {
    pub strong: u16,
    pub weak: u16,
    pub ms: u16,
}

fn parse_rumble(v: &[u8]) -> Option<Rumble> {
    // some hosts include the report id
    let v = if v.len() == 9 && v[0] == 3 { &v[1..] } else { v };
    if v.len() < 8 {
        return None;
    }
    let enable = v[0];
    let mag = |i: usize, bit: u8| if enable & bit != 0 { v[1 + i].min(100) as u16 * 655 } else { 0 };
    // magnitudes: left trigger, right trigger, strong (left), weak (right)
    let strong = mag(2, 0x02);
    let weak = mag(3, 0x01);
    let loops = v[7] as u16 + 1;
    let ms = (v[5] as u16 * 10).saturating_mul(loops);
    Some(Rumble { strong, weak, ms })
}

// ---- GATT objects ----

struct Service {
    uuid: &'static str,
}

#[interface(name = "org.bluez.GattService1")]
impl Service {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        self.uuid.into()
    }
    #[zbus(property)]
    fn primary(&self) -> bool {
        true
    }
}

/// A characteristic with a fixed or updatable value.
struct Chr {
    uuid: &'static str,
    service: &'static str,
    flags: &'static [&'static str],
    value: Arc<Mutex<Vec<u8>>>,
    notifying: Option<Arc<AtomicBool>>,
    on_write: Option<mpsc::Sender<Vec<u8>>>,
    /// The host that last read or wrote, so it can be disconnected on exit.
    peer: Arc<Mutex<Option<OwnedObjectPath>>>,
}

fn remember_peer(peer: &Mutex<Option<OwnedObjectPath>>, options: &HashMap<String, OwnedValue>) {
    if let Some(d) = options.get("device").and_then(|v| OwnedObjectPath::try_from(v.try_clone().ok()?).ok()) {
        *peer.lock().unwrap() = Some(d);
    }
}

#[interface(name = "org.bluez.GattCharacteristic1")]
impl Chr {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        self.uuid.into()
    }
    #[zbus(property)]
    fn service(&self) -> OwnedObjectPath {
        ObjectPath::try_from(self.service).unwrap().into()
    }
    #[zbus(property)]
    fn flags(&self) -> Vec<String> {
        self.flags.iter().map(|s| s.to_string()).collect()
    }
    #[zbus(property)]
    fn value(&self) -> Vec<u8> {
        self.value.lock().unwrap().clone()
    }
    fn read_value(&self, options: HashMap<String, OwnedValue>) -> Vec<u8> {
        remember_peer(&self.peer, &options);
        let v = self.value.lock().unwrap().clone();
        let offset = options.get("offset").and_then(|o| u16::try_from(o).ok()).unwrap_or(0) as usize;
        v.get(offset..).map(<[u8]>::to_vec).unwrap_or_default()
    }
    fn write_value(&self, value: Vec<u8>, options: HashMap<String, OwnedValue>) {
        remember_peer(&self.peer, &options);
        if let Some(tx) = &self.on_write {
            let _ = tx.send(value);
        }
    }
    fn start_notify(&self) {
        eprintln!("ble: host subscribed to {}", self.uuid);
        if let Some(n) = &self.notifying {
            n.store(true, Ordering::Relaxed);
        }
    }
    fn stop_notify(&self) {
        eprintln!("ble: host unsubscribed from {}", self.uuid);
        if let Some(n) = &self.notifying {
            n.store(false, Ordering::Relaxed);
        }
    }
}

struct Desc {
    uuid: &'static str,
    chr: &'static str,
    value: Vec<u8>,
}

#[interface(name = "org.bluez.GattDescriptor1")]
impl Desc {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        self.uuid.into()
    }
    #[zbus(property)]
    fn characteristic(&self) -> OwnedObjectPath {
        ObjectPath::try_from(self.chr).unwrap().into()
    }
    #[zbus(property)]
    fn flags(&self) -> Vec<String> {
        vec!["read".into()]
    }
    fn read_value(&self, _options: HashMap<String, OwnedValue>) -> Vec<u8> {
        self.value.clone()
    }
}

struct Advertisement;

#[interface(name = "org.bluez.LEAdvertisement1")]
impl Advertisement {
    #[zbus(property, name = "Type")]
    fn kind(&self) -> String {
        "peripheral".into()
    }
    #[zbus(property, name = "ServiceUUIDs")]
    fn service_uuids(&self) -> Vec<String> {
        vec!["1812".into()]
    }
    #[zbus(property)]
    fn appearance(&self) -> u16 {
        0x03C4 // HID gamepad
    }
    #[zbus(property)]
    fn local_name(&self) -> String {
        NAME.into()
    }
    #[zbus(property)]
    fn discoverable(&self) -> bool {
        true
    }
    fn release(&self) {}
}

/// Accepts every pairing: there is no keyboard or code display to confirm with.
struct Agent;

#[interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {}
    fn request_authorization(&self, _device: OwnedObjectPath) {}
    fn authorize_service(&self, _device: OwnedObjectPath, _uuid: String) {}
    fn request_confirmation(&self, _device: OwnedObjectPath, _passkey: u32) {}
    fn request_pin_code(&self, _device: OwnedObjectPath) -> String {
        "0000".into()
    }
    fn request_passkey(&self, _device: OwnedObjectPath) -> u32 {
        0
    }
    fn display_passkey(&self, _device: OwnedObjectPath, _passkey: u32, _entered: u16) {}
    fn display_pin_code(&self, _device: OwnedObjectPath, _code: String) {}
    fn cancel(&self) {}
}

/// The running peripheral. Dropping it unregisters everything and restores
/// the adapter name.
pub struct Peripheral {
    conn: Connection,
    input_notifying: Arc<AtomicBool>,
    input_value: Arc<Mutex<Vec<u8>>>,
    battery: Arc<Mutex<Vec<u8>>>,
    pub rumble: mpsc::Receiver<Rumble>,
    peer: Arc<Mutex<Option<OwnedObjectPath>>>,
    /// Hosts whose Blocked flag we manage, unblocked again on exit.
    blocked: Mutex<Vec<Addr>>,
    /// Mouse, media and keyboard input reports: (path, value, subscribed).
    extras: Vec<(&'static str, Arc<Mutex<Vec<u8>>>, Arc<AtomicBool>)>,
    /// Adapter's classic-BT connectability before we opened it for audio.
    was_connectable: Option<bool>,
}

fn adapter(conn: &Connection, iface: &'static str) -> zbus::Result<Proxy<'static>> {
    Proxy::new(conn, "org.bluez", "/org/bluez/hci0", iface)
}

impl Peripheral {
    pub fn start(battery_pct: u8) -> zbus::Result<Self> {
        let conn = connection::Builder::system()?.build()?;
        let os = conn.object_server();
        os.at(APP, zbus::fdo::ObjectManager)?;

        let input_notifying = Arc::new(AtomicBool::new(false));
        let input_value = Arc::new(Mutex::new(report(&Pad::default()).to_vec()));
        let battery = Arc::new(Mutex::new(vec![battery_pct]));
        let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
        let peer: Arc<Mutex<Option<OwnedObjectPath>>> = Arc::default();
        let fixed = |v: &[u8]| Arc::new(Mutex::new(v.to_vec()));

        os.at("/flipad/hid", Service { uuid: "1812" })?;
        os.at(
            "/flipad/hid/info",
            Chr {
                uuid: "2a4a",
                service: "/flipad/hid",
                flags: &["read"],
                value: fixed(&[0x11, 0x01, 0x00, 0x02]),
                notifying: None,
                on_write: None,
                peer: peer.clone(),
            },
        )?;
        os.at(
            "/flipad/hid/map",
            Chr {
                uuid: "2a4b",
                service: "/flipad/hid",
                flags: &["read", "encrypt-read"],
                value: fixed(REPORT_MAP),
                notifying: None,
                on_write: None,
                peer: peer.clone(),
            },
        )?;
        os.at(
            "/flipad/hid/control",
            Chr {
                uuid: "2a4c",
                service: "/flipad/hid",
                flags: &["write-without-response"],
                value: fixed(&[0]),
                notifying: None,
                on_write: None,
                peer: peer.clone(),
            },
        )?;
        os.at(
            INPUT,
            Chr {
                uuid: "2a4d",
                service: "/flipad/hid",
                flags: &["read", "notify", "encrypt-read"],
                value: input_value.clone(),
                notifying: Some(input_notifying.clone()),
                on_write: None,
                peer: peer.clone(),
            },
        )?;
        os.at("/flipad/hid/input/ref", Desc { uuid: "2908", chr: INPUT, value: vec![1, 1] })?;
        os.at(
            "/flipad/hid/output",
            Chr {
                uuid: "2a4d",
                service: "/flipad/hid",
                flags: &["read", "write", "write-without-response", "encrypt-read", "encrypt-write"],
                value: fixed(&[0; 8]),
                notifying: None,
                on_write: Some(out_tx),
                peer: peer.clone(),
            },
        )?;
        os.at("/flipad/hid/output/ref", Desc { uuid: "2908", chr: "/flipad/hid/output", value: vec![3, 2] })?;
        let extras: [(&str, u8, usize); 3] = [(MOUSE, 2, 7), (MEDIA, 4, 1), (KEYS, 5, 8)];
        os.at("/flipad/hid2", Service { uuid: "1812" })?;
        os.at(
            "/flipad/hid2/info",
            Chr {
                uuid: "2a4a",
                service: "/flipad/hid2",
                flags: &["read"],
                value: fixed(&[0x11, 0x01, 0x00, 0x02]),
                notifying: None,
                on_write: None,
                peer: peer.clone(),
            },
        )?;
        os.at(
            "/flipad/hid2/map",
            Chr {
                uuid: "2a4b",
                service: "/flipad/hid2",
                flags: &["read", "encrypt-read"],
                value: fixed(EXTRAS_MAP),
                notifying: None,
                on_write: None,
                peer: peer.clone(),
            },
        )?;
        os.at(
            "/flipad/hid2/control",
            Chr {
                uuid: "2a4c",
                service: "/flipad/hid2",
                flags: &["write-without-response"],
                value: fixed(&[0]),
                notifying: None,
                on_write: None,
                peer: peer.clone(),
            },
        )?;
        let mut extra_values = Vec::new();
        for (path, id, len) in extras {
            let value = Arc::new(Mutex::new(vec![0u8; len]));
            let notifying = Arc::new(AtomicBool::new(false));
            os.at(
                path,
                Chr {
                    uuid: "2a4d",
                    service: "/flipad/hid2",
                    flags: &["read", "notify", "encrypt-read"],
                    value: value.clone(),
                    notifying: Some(notifying.clone()),
                    on_write: None,
                    peer: peer.clone(),
                },
            )?;
            let refpath: &'static str = Box::leak(format!("{path}/ref").into_boxed_str());
            os.at(refpath, Desc { uuid: "2908", chr: path, value: vec![id, 1] })?;
            extra_values.push((path, value, notifying));
        }
        os.at("/flipad/battery", Service { uuid: "180f" })?;
        os.at(
            BATTERY_LEVEL,
            Chr {
                uuid: "2a19",
                service: "/flipad/battery",
                flags: &["read", "notify"],
                value: battery.clone(),
                notifying: Some(Arc::new(AtomicBool::new(false))),
                on_write: None,
                peer: peer.clone(),
            },
        )?;
        os.at(ADV, Advertisement)?;
        os.at(AGENT, Agent)?;

        // the GAP name hosts show after connecting
        let props = zbus::blocking::fdo::PropertiesProxy::builder(&conn).destination("org.bluez")?.path("/org/bluez/hci0")?.build()?;
        let old_alias: Option<String> = std::fs::read_to_string(alias_file())
            .ok()
            .map(|s| s.trim().to_string())
            .or_else(|| props.get("org.bluez.Adapter1".try_into().ok()?, "Alias").ok().and_then(|v| String::try_from(v).ok()))
            .filter(|a| a != NAME);
        if let Some(a) = &old_alias {
            // the guard restores it from here if we get killed
            let _ = std::fs::write(alias_file(), a);
        }
        let _ = props.set("org.bluez.Adapter1".try_into()?, "Alias", Value::from(NAME));
        let _ = props.set("org.bluez.Adapter1".try_into()?, "Powered", Value::from(true));
        let _ = props.set("org.bluez.Adapter1".try_into()?, "Pairable", Value::from(true));
        // accept classic connections too, so the host can stream audio (A2DP)
        let was_connectable: Option<bool> =
            props.get("org.bluez.Adapter1".try_into()?, "Connectable").ok().and_then(|v| bool::try_from(v).ok());
        let _ = props.set("org.bluez.Adapter1".try_into()?, "Connectable", Value::from(true));

        let agents = Proxy::new(&conn, "org.bluez", "/org/bluez", "org.bluez.AgentManager1")?;
        let agent_path = ObjectPath::try_from(AGENT)?;
        if let Err(e) = agents.call_method("RegisterAgent", &(&agent_path, "NoInputNoOutput")) {
            eprintln!("ble: agent: {e}");
        }
        let _ = agents.call_method("RequestDefaultAgent", &(&agent_path,));

        let opts: HashMap<&str, Value> = HashMap::new();
        adapter(&conn, "org.bluez.GattManager1")?.call_method("RegisterApplication", &(ObjectPath::try_from(APP)?, &opts))?;
        adapter(&conn, "org.bluez.LEAdvertisingManager1")?.call_method("RegisterAdvertisement", &(ObjectPath::try_from(ADV)?, &opts))?;

        drop(os);
        let (rumble_tx, rumble_rx) = mpsc::channel();
        std::thread::spawn(move || {
            for v in out_rx {
                if let Some(r) = parse_rumble(&v) {
                    let _ = rumble_tx.send(r);
                }
            }
        });

        Ok(Self { conn, input_notifying, input_value, battery, rumble: rumble_rx, peer, blocked: Mutex::default(), extras: extra_values, was_connectable })
    }

    /// True once a host has subscribed to the input report.
    pub fn subscribed(&self) -> bool {
        self.input_notifying.load(Ordering::Relaxed)
    }

    /// Address and name of the host that last used the controller.
    pub fn peer(&self) -> Option<(Addr, String)> {
        let dev = self.peer.lock().unwrap().clone()?;
        let props = zbus::blocking::fdo::PropertiesProxy::builder(&self.conn).destination("org.bluez").ok()?.path(dev).ok()?.build().ok()?;
        let iface = "org.bluez.Device1".try_into().ok()?;
        let addr: String = props.get(iface, "Address").ok().and_then(|v| String::try_from(v).ok())?;
        let name = props.get("org.bluez.Device1".try_into().ok()?, "Alias").ok().and_then(|v| String::try_from(v).ok());
        Some((parse_addr(&addr)?, name.unwrap_or_else(|| addr.clone())))
    }

    fn device(&self, addr: &Addr) -> Option<zbus::blocking::fdo::PropertiesProxy<'static>> {
        let path = format!("/org/bluez/hci0/dev_{}", addr_str(addr).replace(':', "_"));
        zbus::blocking::fdo::PropertiesProxy::builder(&self.conn).destination("org.bluez").ok()?.path(path).ok()?.build().ok()
    }

    /// Drops the host's audio stream to us (A2DP) when sound is switched off.
    pub fn disconnect_audio(&self, addr: Addr) {
        let conn = self.conn.clone();
        std::thread::spawn(move || {
            let path = format!("/org/bluez/hci0/dev_{}", addr_str(&addr).replace(':', "_"));
            if let Ok(p) = Proxy::new(&conn, "org.bluez", path, "org.bluez.Device1") {
                let _ = p.call_method("DisconnectProfile", &("0000110a-0000-1000-8000-00805f9b34fb",));
            }
        });
    }

    /// Whether a known host is connected right now.
    pub fn is_connected(&self, addr: &Addr) -> bool {
        self.device(addr)
            .and_then(|p| p.get("org.bluez.Device1".try_into().ok()?, "Connected").ok())
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false)
    }

    /// Lets only `target` connect (None = any). Others are blocked, which
    /// keeps their pairing but rejects their connections, and dropped if
    /// connected. Only hosts in `known` are touched, never other devices.
    pub fn apply_target(&self, target: Option<&Addr>, known: &[Addr]) {
        for a in known {
            let block = target.is_some_and(|t| t != a);
            let Some(p) = self.device(a) else { continue };
            let Ok(iface) = "org.bluez.Device1".try_into() else { continue };
            let _ = p.set(iface, "Blocked", Value::from(block));
        }
        self.blocked.lock().unwrap().clone_from(&known.to_vec());
    }

    fn notify(&self, path: &str, value: &Arc<Mutex<Vec<u8>>>, data: &[u8]) {
        *value.lock().unwrap() = data.to_vec();
        if let Ok(iface) = self.conn.object_server().interface::<_, Chr>(path) {
            let _ = zbus::block_on(iface.get().value_changed(iface.signal_emitter()));
        }
    }

    pub fn send(&self, pad: &Pad) {
        if self.subscribed() {
            self.notify(INPUT, &self.input_value, &report(pad));
        }
    }

    fn send_extra(&self, path: &str, data: &[u8]) {
        if let Some((_, value, subscribed)) = self.extras.iter().find(|e| e.0 == path)
            && subscribed.load(Ordering::Relaxed)
        {
            self.notify(path, value, data);
        }
    }

    /// Relative mouse report: buttons (bit 0 left, 1 right, 2 middle), motion, wheel notches.
    pub fn send_mouse(&self, buttons: u8, dx: i16, dy: i16, wheel: i8, pan: i8) {
        let mut r = [0u8; 7];
        r[0] = buttons & 7;
        r[1..3].copy_from_slice(&dx.to_le_bytes());
        r[3..5].copy_from_slice(&dy.to_le_bytes());
        r[5] = wheel as u8;
        r[6] = pan as u8;
        self.send_extra(MOUSE, &r);
    }

    pub fn send_media(&self, bits: u8) {
        self.send_extra(MEDIA, &[bits]);
    }

    pub fn send_keys(&self, modifiers: u8, keys: &[u8]) {
        let mut r = [0u8; 8];
        r[0] = modifiers;
        for (i, k) in keys.iter().take(6).enumerate() {
            r[2 + i] = *k;
        }
        self.send_extra(KEYS, &r);
    }

    pub fn set_battery(&self, pct: u8) {
        if self.battery.lock().unwrap().first() != Some(&pct) {
            self.notify(BATTERY_LEVEL, &self.battery, &[pct]);
        }
    }
}

fn alias_file() -> std::path::PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    std::path::Path::new(&dir).join("flipad-bt-alias")
}

/// Puts the adapter's own name back (also run by the guard after a crash).
pub fn restore_alias() {
    let Ok(old) = std::fs::read_to_string(alias_file()) else { return };
    if let Ok(conn) = Connection::system()
        && let Ok(b) = zbus::blocking::fdo::PropertiesProxy::builder(&conn).destination("org.bluez")
        && let Ok(b) = b.path("/org/bluez/hci0")
        && let Ok(props) = b.build()
        && let Ok(name) = "org.bluez.Adapter1".try_into()
    {
        let _ = props.set(name, "Alias", Value::from(old.trim()));
    }
    let _ = std::fs::remove_file(alias_file());
}

impl Drop for Peripheral {
    fn drop(&mut self) {
        // cheapest and most visible first, in case we are killed midway
        restore_alias();
        let known = std::mem::take(&mut *self.blocked.lock().unwrap());
        self.apply_target(None, &known);
        if let Some(c) = self.was_connectable
            && let Ok(b) = zbus::blocking::fdo::PropertiesProxy::builder(&self.conn).destination("org.bluez")
            && let Ok(b) = b.path("/org/bluez/hci0")
            && let Ok(props) = b.build()
            && let Ok(name) = "org.bluez.Adapter1".try_into()
        {
            let _ = props.set(name, "Connectable", Value::from(c));
        }
        // tell the host right away instead of letting the link time out
        if let Some(dev) = self.peer.lock().unwrap().take()
            && let Ok(p) = Proxy::new(&self.conn, "org.bluez", dev, "org.bluez.Device1")
        {
            let _ = p.call_method("Disconnect", &());
        }
        if let Ok(p) = adapter(&self.conn, "org.bluez.LEAdvertisingManager1") {
            let _ = p.call_method("UnregisterAdvertisement", &(ObjectPath::try_from(ADV).unwrap(),));
        }
        if let Ok(p) = adapter(&self.conn, "org.bluez.GattManager1") {
            let _ = p.call_method("UnregisterApplication", &(ObjectPath::try_from(APP).unwrap(),));
        }
        if let Ok(p) = Proxy::new(&self.conn, "org.bluez", "/org/bluez", "org.bluez.AgentManager1") {
            let _ = p.call_method("UnregisterAgent", &(ObjectPath::try_from(AGENT).unwrap(),));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_layout() {
        let p = Pad {
            buttons: button::SOUTH | button::NORTH | button::MODE | button::DPAD_UP | button::DPAD_RIGHT,
            lx: -32768,
            ly: 0,
            rx: 32767,
            ry: 0,
            lt: 255,
            rt: 0,
        };
        let r = report(&p);
        assert_eq!(u16::from_le_bytes([r[0], r[1]]), 0);
        assert_eq!(u16::from_le_bytes([r[2], r[3]]), 32768);
        assert_eq!(u16::from_le_bytes([r[4], r[5]]), 65535);
        assert_eq!(u16::from_le_bytes([r[8], r[9]]), 1023);
        assert_eq!(r[12], 2, "up-right");
        assert_eq!(u16::from_le_bytes([r[13], r[14]]), 1 | 1 << 4 | 1 << 12);
        assert_eq!(report(&Pad::default())[12], 0, "centered hat is null");
    }

    #[test]
    fn report_map_sizes() {
        // walk the items: collections must close, report 1 input = 16 bytes,
        // report 3 output = 8 bytes
        let (mut i, mut depth, mut size, mut count, mut id) = (0, 0i32, 0u32, 0u32, 0u8);
        let (mut input1, mut output3) = (0u32, 0u32);
        while i < REPORT_MAP.len() {
            let b = REPORT_MAP[i];
            let n = match b & 3 { 3 => 4, k => k as usize };
            let mut data = 0u32;
            for k in 0..n {
                data |= (REPORT_MAP[i + 1 + k] as u32) << (8 * k);
            }
            match b & 0xFC {
                0xA0 => depth += 1,
                0xC0 => depth -= 1,
                0x74 => size = data,
                0x94 => count = data,
                0x84 => id = data as u8,
                0x80 if id == 1 => input1 += size * count,
                0x90 if id == 3 => output3 += size * count,
                _ => {}
            }
            i += 1 + n;
        }
        assert_eq!(depth, 0);
        assert_eq!(input1, 16 * 8);
        assert_eq!(output3, 8 * 8);
    }

    #[test]
    fn extras_map_sizes() {
        let (mut i, mut depth, mut size, mut count, mut id) = (0, 0i32, 0u32, 0u32, 0u8);
        let mut bits = std::collections::HashMap::new();
        while i < EXTRAS_MAP.len() {
            let b = EXTRAS_MAP[i];
            let n = match b & 3 { 3 => 4, k => k as usize };
            let mut data = 0u32;
            for k in 0..n {
                data |= (EXTRAS_MAP[i + 1 + k] as u32) << (8 * k);
            }
            match b & 0xFC {
                0xA0 => depth += 1,
                0xC0 => depth -= 1,
                0x74 => size = data,
                0x94 => count = data,
                0x84 => id = data as u8,
                0x80 => *bits.entry(id).or_insert(0) += size * count,
                _ => {}
            }
            i += 1 + n;
        }
        assert_eq!(depth, 0);
        assert_eq!(bits[&2], 7 * 8, "mouse");
        assert_eq!(bits[&4], 8, "media");
        assert_eq!(bits[&5], 8 * 8, "keyboard");
    }

    #[test]
    fn rumble_parse() {
        let r = parse_rumble(&[0x03, 0, 0, 100, 50, 20, 0, 0]).unwrap();
        assert_eq!((r.strong, r.weak, r.ms), (65500, 32750, 200));
        assert_eq!(parse_rumble(&[0x03, 0, 0, 100, 50, 20, 0, 0][..5]), None);
    }
}
