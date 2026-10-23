use std::{
    env, fs,
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Command, ExitCode, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

mod api;

const REQUIRED_PLUGINS: &[&str] = &[
    "audiotestsrc",
    "audioconvert",
    "audioresample",
    "opusenc",
    "filesink",
    "nicesrc",
    "nicesink",
    "dtlssrtpenc",
    "dtlssrtpdec",
    "rtpopuspay",
    "rtpopusdepay",
];

#[derive(Debug)]
struct Config {
    websocket_url: String,
    room: String,
    nick: String,
    output_dir: PathBuf,
    tone_frequency: u32,
    tone_volume: f64,
    xmpp_domain: Option<String>,
    muc_domain: Option<String>,
    focus_jid: Option<String>,
    xmpp_username: Option<String>,
    xmpp_password: Option<String>,
    xmpp_jwt: Option<String>,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let required = |key| env::var(key).map_err(|_| format!("{key} is required"));
        let optional = |key| env::var(key).ok().filter(|value| !value.is_empty());
        let websocket_url = required("JITSI_WEBSOCKET_URL")?;
        if !websocket_url.starts_with("wss://") && !websocket_url.starts_with("ws://") {
            return Err("JITSI_WEBSOCKET_URL must start with wss:// or ws://".into());
        }
        let room = required("JITSI_ROOM")?;
        if room.is_empty() || room.contains(['/', '@', ' ', '\n']) {
            return Err("JITSI_ROOM must be a single room name without /, @ or spaces".into());
        }
        let xmpp_username = optional("JITSI_XMPP_USERNAME");
        let xmpp_password = optional("JITSI_XMPP_PASSWORD");
        let xmpp_jwt = optional("JITSI_XMPP_JWT");
        if xmpp_username.is_some() != xmpp_password.is_some() {
            return Err("JITSI_XMPP_USERNAME and JITSI_XMPP_PASSWORD must be set together".into());
        }
        if xmpp_username.is_some() && xmpp_jwt.is_some() {
            return Err("choose either XMPP username/password or JWT".into());
        }
        let tone_frequency = optional("TEST_TONE_HZ")
            .unwrap_or_else(|| "440".into())
            .parse::<u32>()
            .map_err(|_| "TEST_TONE_HZ must be an integer")?;
        if !(20..=20_000).contains(&tone_frequency) {
            return Err("TEST_TONE_HZ must be between 20 and 20000".into());
        }
        let tone_volume = optional("TEST_TONE_VOLUME")
            .unwrap_or_else(|| "0.05".into())
            .parse::<f64>()
            .map_err(|_| "TEST_TONE_VOLUME must be a number")?;
        if !tone_volume.is_finite() || !(0.0..=1.0).contains(&tone_volume) {
            return Err("TEST_TONE_VOLUME must be between 0 and 1".into());
        }
        Ok(Self {
            websocket_url,
            room,
            nick: optional("JITSI_NICK").unwrap_or_else(|| "hermes-transport-probe".into()),
            output_dir: PathBuf::from(
                optional("AUDIO_OUTPUT_DIR").unwrap_or_else(|| "./audio".into()),
            ),
            tone_frequency,
            tone_volume,
            xmpp_domain: optional("JITSI_XMPP_DOMAIN"),
            muc_domain: optional("JITSI_MUC_DOMAIN"),
            focus_jid: optional("JITSI_FOCUS_JID"),
            xmpp_username,
            xmpp_password,
            xmpp_jwt,
        })
    }

    fn gst_args(&self) -> Vec<String> {
        let mut args = vec![
            format!("--web-socket-url={}", self.websocket_url),
            format!("--room-name={}", self.room),
            format!("--nick={}", self.nick),
            "--last-n=0".into(),
            format!("--send-pipeline=audiotestsrc is-live=true wave=sine freq={} volume={} ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=1 ! opusenc name=audio", self.tone_frequency, self.tone_volume),
            format!("--recv-pipeline-participant-template=audioconvert name=audio ! audioresample ! audio/x-raw,format=S16LE,rate=16000,channels=1 ! filesink location={}/{{participant_id}}.s16le", self.output_dir.display()),
        ];
        for (flag, value) in [
            ("xmpp-domain", &self.xmpp_domain),
            ("muc-domain", &self.muc_domain),
            ("focus-jid", &self.focus_jid),
            ("xmpp-username", &self.xmpp_username),
            ("xmpp-password", &self.xmpp_password),
            ("xmpp-jwt", &self.xmpp_jwt),
        ] {
            if let Some(value) = value {
                args.push(format!("--{flag}={value}"));
            }
        }
        args
    }
}

fn run_status(program: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

fn check_compatibility() -> Result<(), String> {
    run_status("gst-meet", &["--help"])?;
    run_status("gst-inspect-1.0", &["--version"])?;
    for plugin in REQUIRED_PLUGINS {
        run_status("gst-inspect-1.0", &[plugin])?;
    }
    println!("Local gst-meet/GStreamer commands and audio elements: OK");
    Ok(())
}

fn main() -> ExitCode {
    if env::args().nth(1).as_deref() == Some("serve") {
        return match tokio::runtime::Runtime::new()
            .map_err(|error| error.to_string())
            .and_then(|runtime| runtime.block_on(api::serve()))
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let result = match env::args().nth(1).as_deref() {
        Some("check") => check_compatibility(),
        Some("record") => record(),
        Some("run") | None => run(),
        _ => Err("usage: hermes-jitsi-bridge [check|run|record|serve]".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn record() -> Result<(), String> {
    let bosh_url = env::var("JITSI_BOSH_URL").map_err(|_| "JITSI_BOSH_URL is required")?;
    if !bosh_url.starts_with("https://") {
        return Err("JITSI_BOSH_URL must start with https://".into());
    }
    let room = env::var("JITSI_ROOM").map_err(|_| "JITSI_ROOM is required")?;
    if room.is_empty() || room.contains(['/', '@']) {
        return Err("JITSI_ROOM must be a single room name".into());
    }
    let encoded_room = encode_room(&room);
    let nick = env::var("JITSI_NICK").unwrap_or_else(|_| "rust-recording-bot".into());
    let output_dir = env::var("RECORDINGS_DIR").unwrap_or_else(|_| "/data/recordings".into());
    if output_dir.is_empty()
        || output_dir
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '!' | '"' | '\\'))
    {
        return Err("RECORDINGS_DIR cannot contain spaces, !, quotes or backslashes".into());
    }
    fs::create_dir_all(&output_dir)
        .map_err(|e| format!("cannot create recordings directory: {e}"))?;
    let (audio_pipeline, video_pipeline) = recording_pipelines(&output_dir, video_bitrate()?);
    let started = session_started_unix_ns()?;
    write_session_timeline(&output_dir, started)?;
    println!("Recording room {room} (XMPP name {encoded_room}) to {output_dir}");
    let error = Command::new("gst-meet")
        .arg(format!("--bosh-url={bosh_url}"))
        .arg(format!("--room-name={encoded_room}"))
        .arg(format!("--nick={nick}"))
        .arg(format!("--recv-audio-source-template={audio_pipeline}"))
        .arg(format!("--recv-video-source-template={video_pipeline}"))
        .arg(format!("--recording-metadata-dir={output_dir}"))
        .arg(format!("--session-started-unix-ns={started}"))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .exec();
    Err(format!("cannot start gst-meet BOSH recorder: {error}"))
}

pub(crate) fn video_bitrate() -> Result<u32, String> {
    let kbps = env::var("VIDEO_BITRATE_KBPS")
        .unwrap_or_else(|_| "3000".into())
        .parse::<u32>()
        .map_err(|_| "VIDEO_BITRATE_KBPS must be an integer")?;
    if !(250..=20_000).contains(&kbps) {
        return Err("VIDEO_BITRATE_KBPS must be between 250 and 20000".into());
    }
    Ok(kbps * 1000)
}

pub(crate) fn recording_pipelines(output_dir: &str, video_bitrate: u32) -> (String, String) {
    (
        format!("audioconvert name=track ! audioresample ! opusenc ! webmmux ! filesink location={output_dir}/{{track_id}}-audio.webm"),
        format!("videoconvert name=track ! vp8enc target-bitrate={video_bitrate} deadline=1000000 cpu-used=4 threads=4 ! webmmux ! filesink location={output_dir}/{{track_id}}-video.webm"),
    )
}

pub(crate) fn session_started_unix_ns() -> Result<u64, String> {
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    u64::try_from(ns).map_err(|_| "system clock exceeds timestamp range".into())
}

pub(crate) fn write_session_timeline(dir: &str, started: u64) -> Result<(), String> {
    fs::write(format!("{dir}/timeline.json"), format!("{{\"schema_version\":1,\"session_started_unix_ns\":{started},\"clock\":\"system_unix_ns\"}}"))
        .map_err(|e| format!("cannot save session timeline: {e}"))
}

pub(crate) fn encode_room(room: &str) -> String {
    let mut encoded = String::new();
    for byte in room.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02x}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::encode_room;

    #[test]
    fn encodes_unicode_room_like_jitsi_web() {
        assert_eq!(
            encode_room("комната"),
            "%d0%ba%d0%be%d0%bc%d0%bd%d0%b0%d1%82%d0%b0"
        );
    }
}

fn run() -> Result<(), String> {
    let config = Config::from_env()?;
    check_compatibility()?;
    // GStreamer pipeline descriptions are parsed by gst-meet. Keep the path free of
    // GStreamer syntax until this probe gains a dedicated escaped pipeline builder.
    let path = config.output_dir.to_string_lossy();
    if path
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '!' | '"' | '\\'))
    {
        return Err("AUDIO_OUTPUT_DIR cannot contain spaces, !, quotes or backslashes".into());
    }
    fs::create_dir_all(&config.output_dir)
        .map_err(|e| format!("cannot create audio output directory: {e}"))?;
    println!(
        "Joining room {} as {}; PCM: {}/<participant_id>.s16le",
        config.room, config.nick, path
    );
    println!("PCM format: signed 16-bit little-endian, mono, 16000 Hz. Press Ctrl-C to leave.");
    let error = Command::new("gst-meet")
        .args(config.gst_args())
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .exec();
    Err(format!("cannot start gst-meet: {error}"))
}
