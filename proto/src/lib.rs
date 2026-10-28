//! Wire protocol between the handheld (client) and the PC (host).
//!
//! Every datagram starts with `MAGIC` and a kind byte; integers are little
//! endian. The client sends its full controller state on every change and
//! re-sends it periodically, so a lost packet is repaired by the next one and
//! the host never needs acknowledgements.

pub const MAGIC: [u8; 4] = *b"FPD1";
pub const PORT: u16 = 47820;
pub const TOKEN_LEN: usize = 16;
/// RTP audio from the host to the handheld's headphone jack.
pub const AUDIO_PORT: u16 = PORT + 10;
/// RTP audio from the handheld's headset microphone to the host.
pub const MIC_PORT: u16 = PORT + 11;

pub mod audio;

/// Bits of `Pad::buttons`.
pub mod button {
    pub const SOUTH: u32 = 1 << 0;
    pub const EAST: u32 = 1 << 1;
    pub const NORTH: u32 = 1 << 2;
    pub const WEST: u32 = 1 << 3;
    pub const TL: u32 = 1 << 4;
    pub const TR: u32 = 1 << 5;
    pub const SELECT: u32 = 1 << 6;
    pub const START: u32 = 1 << 7;
    pub const MODE: u32 = 1 << 8;
    pub const THUMBL: u32 = 1 << 9;
    pub const THUMBR: u32 = 1 << 10;
    pub const DPAD_UP: u32 = 1 << 11;
    pub const DPAD_DOWN: u32 = 1 << 12;
    pub const DPAD_LEFT: u32 = 1 << 13;
    pub const DPAD_RIGHT: u32 = 1 << 14;
}

/// Controller state in Xbox 360 ranges.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pad {
    pub buttons: u32,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
    pub lt: u8,
    pub rt: u8,
}

/// Shortcuts the handheld can trigger on the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum HostAction {
    VolumeDown = 1,
    VolumeUp,
    Mute,
    PlayPause,
    Previous,
    Next,
    /// F12: Steam's in-game screenshot.
    SteamScreenshot,
    /// Print Screen: the desktop's screenshot tool.
    Screenshot,
    AltTab,
    Escape,
    ShowDesktop,
    BigPicture,
}

impl HostAction {
    pub const ALL: [HostAction; 12] = [
        HostAction::VolumeDown,
        HostAction::VolumeUp,
        HostAction::Mute,
        HostAction::PlayPause,
        HostAction::Previous,
        HostAction::Next,
        HostAction::SteamScreenshot,
        HostAction::Screenshot,
        HostAction::AltTab,
        HostAction::Escape,
        HostAction::ShowDesktop,
        HostAction::BigPicture,
    ];

    pub fn from_u8(x: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|a| *a as u8 == x)
    }
}

/// Host machine readings for the handheld's status screen. `None` = unknown.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub cpu_pct: Option<u8>,
    pub cpu_temp: Option<f32>,
    pub gpu_pct: Option<u8>,
    pub gpu_temp: Option<f32>,
    pub mem_pct: Option<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    /// client → broadcast: looking for hosts.
    Discover,
    /// host → client: answer to `Discover`.
    Offer { name: String },
    /// client → host: start a session.
    Hello { token: [u8; TOKEN_LEN], name: String },
    /// host → client: session accepted.
    Welcome { session: u32, name: String },
    /// host → client: wrong token or another handheld is connected.
    Reject { reason: String },
    State { session: u32, seq: u32, pad: Pad },
    Ping { session: u32, id: u32, micros: u64 },
    Pong { id: u32, micros: u64 },
    /// host → client: magnitudes 0..=65535; `ms == 0` with zero magnitudes stops.
    Rumble { strong: u16, weak: u16, ms: u16 },
    Status(Status),
    Bye { session: u32 },
    /// client → host: what is plugged into the handheld's 3.5mm jack.
    Jack { session: u32, headphones: bool, mic: bool },
    /// client → host: run a shortcut. `id` lets the host drop repeats.
    Action { session: u32, id: u32, action: HostAction },
}

mod kind {
    pub const DISCOVER: u8 = 1;
    pub const OFFER: u8 = 2;
    pub const HELLO: u8 = 3;
    pub const WELCOME: u8 = 4;
    pub const REJECT: u8 = 5;
    pub const STATE: u8 = 6;
    pub const PING: u8 = 7;
    pub const PONG: u8 = 8;
    pub const RUMBLE: u8 = 9;
    pub const STATUS: u8 = 10;
    pub const BYE: u8 = 11;
    pub const JACK: u8 = 12;
    pub const ACTION: u8 = 13;
}

struct W(Vec<u8>);

impl W {
    fn new(k: u8) -> Self {
        let mut v = Vec::with_capacity(32);
        v.extend_from_slice(&MAGIC);
        v.push(k);
        W(v)
    }
    fn u8(mut self, x: u8) -> Self {
        self.0.push(x);
        self
    }
    fn u16(mut self, x: u16) -> Self {
        self.0.extend_from_slice(&x.to_le_bytes());
        self
    }
    fn i16(mut self, x: i16) -> Self {
        self.0.extend_from_slice(&x.to_le_bytes());
        self
    }
    fn u32(mut self, x: u32) -> Self {
        self.0.extend_from_slice(&x.to_le_bytes());
        self
    }
    fn u64(mut self, x: u64) -> Self {
        self.0.extend_from_slice(&x.to_le_bytes());
        self
    }
    fn bytes(mut self, b: &[u8]) -> Self {
        self.0.extend_from_slice(b);
        self
    }
    fn str(self, s: &str) -> Self {
        let b = &s.as_bytes()[..s.len().min(255)];
        self.u8(b.len() as u8).bytes(b)
    }
    /// Optional byte: 0xFF means unknown.
    fn opt_u8(self, x: Option<u8>) -> Self {
        self.u8(x.unwrap_or(0xFF).min(0xFF))
    }
    /// Optional temperature in tenths of a degree; i16::MIN means unknown.
    fn opt_temp(self, x: Option<f32>) -> Self {
        self.i16(x.map_or(i16::MIN, |t| (t * 10.0).round().clamp(-3000.0, 3000.0) as i16))
    }
}

struct R<'a>(&'a [u8]);

impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn i16(&mut self) -> Option<i16> {
        Some(i16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u8()? as usize;
        Some(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
    fn opt_u8(&mut self) -> Option<Option<u8>> {
        let x = self.u8()?;
        Some((x != 0xFF).then_some(x))
    }
    fn opt_temp(&mut self) -> Option<Option<f32>> {
        let x = self.i16()?;
        Some((x != i16::MIN).then(|| x as f32 / 10.0))
    }
}

impl Msg {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Msg::Discover => W::new(kind::DISCOVER),
            Msg::Offer { name } => W::new(kind::OFFER).str(name),
            Msg::Hello { token, name } => W::new(kind::HELLO).bytes(token).str(name),
            Msg::Welcome { session, name } => W::new(kind::WELCOME).u32(*session).str(name),
            Msg::Reject { reason } => W::new(kind::REJECT).str(reason),
            Msg::State { session, seq, pad } => W::new(kind::STATE)
                .u32(*session)
                .u32(*seq)
                .u32(pad.buttons)
                .i16(pad.lx)
                .i16(pad.ly)
                .i16(pad.rx)
                .i16(pad.ry)
                .u8(pad.lt)
                .u8(pad.rt),
            Msg::Ping { session, id, micros } => W::new(kind::PING).u32(*session).u32(*id).u64(*micros),
            Msg::Pong { id, micros } => W::new(kind::PONG).u32(*id).u64(*micros),
            Msg::Rumble { strong, weak, ms } => W::new(kind::RUMBLE).u16(*strong).u16(*weak).u16(*ms),
            Msg::Status(s) => W::new(kind::STATUS)
                .opt_u8(s.cpu_pct)
                .opt_temp(s.cpu_temp)
                .opt_u8(s.gpu_pct)
                .opt_temp(s.gpu_temp)
                .opt_u8(s.mem_pct),
            Msg::Bye { session } => W::new(kind::BYE).u32(*session),
            Msg::Jack { session, headphones, mic } => {
                W::new(kind::JACK).u32(*session).u8(*headphones as u8 | (*mic as u8) << 1)
            }
            Msg::Action { session, id, action } => W::new(kind::ACTION).u32(*session).u32(*id).u8(*action as u8),
        }
        .0
    }

    pub fn decode(buf: &[u8]) -> Option<Msg> {
        if buf.len() < 5 || buf[..4] != MAGIC {
            return None;
        }
        let mut r = R(&buf[5..]);
        Some(match buf[4] {
            kind::DISCOVER => Msg::Discover,
            kind::OFFER => Msg::Offer { name: r.str()? },
            kind::HELLO => Msg::Hello { token: r.take(TOKEN_LEN)?.try_into().ok()?, name: r.str()? },
            kind::WELCOME => Msg::Welcome { session: r.u32()?, name: r.str()? },
            kind::REJECT => Msg::Reject { reason: r.str()? },
            kind::STATE => Msg::State {
                session: r.u32()?,
                seq: r.u32()?,
                pad: Pad {
                    buttons: r.u32()?,
                    lx: r.i16()?,
                    ly: r.i16()?,
                    rx: r.i16()?,
                    ry: r.i16()?,
                    lt: r.u8()?,
                    rt: r.u8()?,
                },
            },
            kind::PING => Msg::Ping { session: r.u32()?, id: r.u32()?, micros: r.u64()? },
            kind::PONG => Msg::Pong { id: r.u32()?, micros: r.u64()? },
            kind::RUMBLE => Msg::Rumble { strong: r.u16()?, weak: r.u16()?, ms: r.u16()? },
            kind::STATUS => Msg::Status(Status {
                cpu_pct: r.opt_u8()?,
                cpu_temp: r.opt_temp()?,
                gpu_pct: r.opt_u8()?,
                gpu_temp: r.opt_temp()?,
                mem_pct: r.opt_u8()?,
            }),
            kind::BYE => Msg::Bye { session: r.u32()? },
            kind::ACTION => Msg::Action { session: r.u32()?, id: r.u32()?, action: HostAction::from_u8(r.u8()?)? },
            kind::JACK => {
                let session = r.u32()?;
                let bits = r.u8()?;
                Msg::Jack { session, headphones: bits & 1 != 0, mic: bits & 2 != 0 }
            }
            _ => return None,
        })
    }
}

/// True if sequence number `a` is newer than `b`, tolerating wraparound.
pub fn seq_newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// Parses a token written as 32 hex digits.
pub fn parse_token(s: &str) -> Option<[u8; TOKEN_LEN]> {
    let s = s.trim();
    if s.len() != TOKEN_LEN * 2 {
        return None;
    }
    let mut t = [0u8; TOKEN_LEN];
    for (i, b) in t.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_kinds() {
        let pad = Pad { buttons: button::SOUTH | button::DPAD_LEFT, lx: -32768, ly: 32767, rx: 5, ry: -5, lt: 255, rt: 1 };
        let msgs = [
            Msg::Discover,
            Msg::Offer { name: "desk".into() },
            Msg::Hello { token: [7; TOKEN_LEN], name: "flip2".into() },
            Msg::Welcome { session: 42, name: "桌面".into() },
            Msg::Reject { reason: "busy".into() },
            Msg::State { session: 1, seq: u32::MAX, pad },
            Msg::Ping { session: 1, id: 9, micros: 123456789 },
            Msg::Pong { id: 9, micros: 123456789 },
            Msg::Rumble { strong: 65535, weak: 0, ms: 200 },
            Msg::Status(Status { cpu_pct: Some(37), cpu_temp: Some(61.5), gpu_pct: None, gpu_temp: Some(-1.2), mem_pct: Some(0) }),
            Msg::Bye { session: 3 },
            Msg::Jack { session: 3, headphones: true, mic: false },
            Msg::Jack { session: 4, headphones: false, mic: true },
            Msg::Action { session: 4, id: 7, action: HostAction::BigPicture },
        ];
        for m in msgs {
            assert_eq!(Msg::decode(&m.encode()), Some(m.clone()), "{m:?}");
        }
    }

    #[test]
    fn rejects_garbage_and_truncation() {
        assert_eq!(Msg::decode(b"nope!"), None);
        let full = Msg::State { session: 1, seq: 2, pad: Pad::default() }.encode();
        assert_eq!(Msg::decode(&full[..full.len() - 1]), None);
    }

    #[test]
    fn sequence_wraps() {
        assert!(seq_newer(1, 0));
        assert!(seq_newer(0, u32::MAX));
        assert!(!seq_newer(5, 5));
        assert!(!seq_newer(u32::MAX, 0));
    }

    #[test]
    fn token_hex() {
        assert_eq!(parse_token("000102030405060708090a0b0c0d0e0f").unwrap()[15], 15);
        assert!(parse_token("xyz").is_none());
    }
}
