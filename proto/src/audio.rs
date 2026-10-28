//! Audio over RTP using PipeWire's own modules. Each direction runs as a
//! small `pipewire -c <conf>` client process that loads `module-rtp-sink`
//! (sends) or `module-rtp-source` (receives); dropping the handle stops it.
//! Both ends use uncompressed 16-bit 48 kHz, so no codec latency is added.

use std::net::IpAddr;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

pub const RATE: u32 = 48000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Channels {
    Stereo,
    Mono,
}

impl Channels {
    fn props(self) -> &'static str {
        match self {
            Channels::Stereo => "audio.channels = 2 audio.position = [ FL FR ]",
            Channels::Mono => "audio.channels = 1 audio.position = [ MONO ]",
        }
    }
}

/// Node properties for the stream the module creates, as `key = value` pairs.
pub type Props<'a> = &'a [(&'a str, &'a str)];

/// Asks the PipeWire graph for 256-frame (~5 ms) cycles while we stream.
/// With the usual 1024-frame default the device alone adds ~30 ms, more than
/// the jitter buffer, and playback underruns.
const LOW_LATENCY: &str = "node.latency = \"256/48000\"";

fn render_props(props: Props) -> String {
    let mut s: Vec<String> = props.iter().map(|(k, v)| format!("{k} = \"{}\"", v.replace('"', "'"))).collect();
    s.push(LOW_LATENCY.into());
    s.join(" ")
}

fn conf(module: &str, args: &str) -> String {
    format!(
        "context.properties = {{ log.level = 1 }}\n\
         context.spa-libs = {{ audio.convert.* = audioconvert/libspa-audioconvert support.* = support/libspa-support }}\n\
         context.modules = [\n\
         {{ name = libpipewire-module-rt args = {{ nice.level = -11 }} flags = [ ifexists nofail ] }}\n\
         {{ name = libpipewire-module-protocol-native }}\n\
         {{ name = libpipewire-module-client-node }}\n\
         {{ name = libpipewire-module-adapter }}\n\
         {{ name = {module} args = {{ {args} }} }}\n\
         ]\n"
    )
}

/// Config that sends what is played into (or captured by) the stream to `dest`.
pub fn sender_conf(dest: IpAddr, port: u16, ch: Channels, props: Props) -> String {
    let args = format!(
        "destination.ip = \"{dest}\" destination.port = {port} net.mtu = 1280 net.dscp = 46 \
         sess.min-ptime = 2 sess.max-ptime = 4 sess.media = audio \
         audio.format = S16BE audio.rate = {RATE} {} stream.props = {{ {} }}",
        ch.props(),
        render_props(props)
    );
    conf("libpipewire-module-rtp-sink", &args)
}

/// Config that receives RTP on `port` with a `latency_ms` jitter buffer.
pub fn receiver_conf(port: u16, latency_ms: u32, ch: Channels, props: Props) -> String {
    let args = format!(
        "source.ip = \"0.0.0.0\" source.port = {port} sess.latency.msec = {latency_ms} \
         sess.ignore-ssrc = true sess.media = audio \
         audio.format = S16BE audio.rate = {RATE} {} stream.props = {{ {} }}",
        ch.props(),
        render_props(props)
    );
    conf("libpipewire-module-rtp-source", &args)
}

/// A running `pipewire -c` process; killed on drop and if we die.
pub struct PwProcess {
    child: Child,
    conf: PathBuf,
}

impl PwProcess {
    pub fn spawn(name: &str, conf_text: &str) -> std::io::Result<Self> {
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        let conf = PathBuf::from(dir).join(format!("{name}.conf"));
        std::fs::write(&conf, conf_text)?;
        let mut cmd = Command::new("pipewire");
        cmd.arg("-c").arg(&conf).stdin(Stdio::null()).stdout(Stdio::null());
        unsafe {
            cmd.pre_exec(|| {
                // take the child down with us, even on SIGKILL
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        Ok(Self { child: cmd.spawn()?, conf })
    }

    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for PwProcess {
    fn drop(&mut self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.conf);
    }
}

/// Runs `pactl` and returns trimmed stdout on success.
pub fn pactl(args: &[&str]) -> Option<String> {
    let out = Command::new("pactl").args(args).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Node names from `pactl list short sinks|sources`.
pub fn node_names(kind: &str) -> Vec<String> {
    pactl(&["list", "short", kind])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split('\t').nth(1).map(String::from))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configs_carry_addresses_and_props() {
        let s = sender_conf("192.0.2.10".parse().unwrap(), 47830, Channels::Stereo, &[("node.name", "x")]);
        assert!(s.contains("libpipewire-module-rtp-sink"));
        assert!(s.contains("destination.ip = \"192.0.2.10\" destination.port = 47830"));
        assert!(s.contains("audio.channels = 2"));
        assert!(s.contains("node.name = \"x\""));
        let r = receiver_conf(47831, 30, Channels::Mono, &[("node.description", "a \"b\"")]);
        assert!(r.contains("source.port = 47831 sess.latency.msec = 30"));
        assert!(r.contains("[ MONO ]"));
        assert!(r.contains("node.description = \"a 'b'\""));
    }
}
