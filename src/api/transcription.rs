use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{sleep, timeout},
};
use uuid::Uuid;

use super::AppState;

const SAMPLE_RATE: u32 = 16_000;
const CHUNK_BYTES: u64 = SAMPLE_RATE as u64 * 2 * 10;

#[derive(Clone)]
pub(super) struct Endpoint {
    host: String,
    port: u16,
    path: String,
}

impl Endpoint {
    pub(super) fn parse(url: &str) -> Result<Self, String> {
        let rest = url
            .strip_prefix("http://")
            .ok_or("TRANSCRIPTION_URL must start with http://")?;
        let (authority, base) = rest.split_once('/').unwrap_or((rest, ""));
        let (host, port) = authority
            .rsplit_once(':')
            .ok_or("TRANSCRIPTION_URL must include a port")?;
        let port = port
            .parse::<u16>()
            .map_err(|_| "invalid TRANSCRIPTION_URL port")?;
        if host.is_empty()
            || host.contains(['@', '/', '?', '#', ' '])
            || base.contains(['?', '#', '\r', '\n'])
        {
            return Err("invalid TRANSCRIPTION_URL".into());
        }
        let prefix = base.trim_end_matches('/');
        Ok(Self {
            host: host.into(),
            port,
            path: format!("/{prefix}/v1/audio/transcriptions").replace("//", "/"),
        })
    }

    async fn transcribe(&self, wav: &[u8]) -> Result<String, String> {
        let boundary = format!("jitsi-{}", Uuid::new_v4());
        let start = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nnemotron-bolt\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"chunk.wav\"\r\nContent-Type: audio/wav\r\n\r\n");
        let end = format!("\r\n--{boundary}--\r\n");
        let length = start.len() + wav.len() + end.len();
        let request = format!("POST {} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: multipart/form-data; boundary={}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", self.path, self.host, self.port, boundary, length);
        let operation = async {
            let mut stream = TcpStream::connect((self.host.as_str(), self.port))
                .await
                .map_err(|e| e.to_string())?;
            stream
                .write_all(request.as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            stream
                .write_all(start.as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            stream.write_all(wav).await.map_err(|e| e.to_string())?;
            stream
                .write_all(end.as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            let mut response = Vec::new();
            stream
                .read_to_end(&mut response)
                .await
                .map_err(|e| e.to_string())?;
            let header_end = response
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .ok_or("invalid transcription response")?;
            let headers = String::from_utf8_lossy(&response[..header_end]);
            if !headers.starts_with("HTTP/1.1 200 ") && !headers.starts_with("HTTP/1.0 200 ") {
                return Err(format!(
                    "transcription server: {}",
                    headers.lines().next().unwrap_or("unknown error")
                ));
            }
            let body: serde_json::Value =
                serde_json::from_slice(&response[header_end + 4..]).map_err(|e| e.to_string())?;
            body.get("text")
                .and_then(|text| text.as_str())
                .map(str::to_owned)
                .ok_or("transcription response has no text".into())
        };
        timeout(Duration::from_secs(90), operation)
            .await
            .map_err(|_| "transcription timed out".to_string())?
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct JournalLine {
    pub(super) participant: String,
    pub(super) participant_id: String,
    pub(super) offset_seconds: f64,
    pub(super) text: String,
}

#[derive(Deserialize)]
struct TrackMetadata {
    participant_id: String,
    participant_name: Option<String>,
    first_buffer_session_offset_ns: u64,
}

#[derive(Deserialize)]
struct ParticipantName {
    display_name: String,
}

fn wav(samples: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(44 + samples.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    out.extend_from_slice(samples);
    out
}

fn is_speech_result(text: &str) -> bool {
    let lower = text.trim().to_lowercase();
    !lower.is_empty()
        && ![
            "no speech",
            "cannot transcribe",
            "unable to transcribe",
            "please provide",
            "audio file",
            "no audio",
            "too noisy",
            "no clear speech",
            "mostly silent",
            "undetectable",
        ]
        .iter()
        .any(|phrase| lower.contains(phrase))
}

fn track_info(dir: &Path, path: &Path) -> Option<(String, String, f64)> {
    let stem = path.file_name()?.to_str()?.strip_suffix("-audio.s16le")?;
    let metadata: TrackMetadata =
        serde_json::from_slice(&fs::read(dir.join(format!("{stem}-audio.track.json"))).ok()?)
            .ok()?;
    let name = fs::read(dir.join(format!("participant-{}.json", metadata.participant_id)))
        .ok()
        .and_then(|data| serde_json::from_slice::<ParticipantName>(&data).ok())
        .map(|entry| entry.display_name)
        .or(metadata.participant_name)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| format!("Участник {}", metadata.participant_id));
    Some((
        metadata.participant_id,
        name,
        metadata.first_buffer_session_offset_ns as f64 / 1e9,
    ))
}

pub(super) fn read_journal(dir: &Path) -> Vec<JournalLine> {
    let mut lines: Vec<JournalLine> = fs::read_to_string(dir.join("journal.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<JournalLine>(line).ok())
        .collect();
    lines.sort_by(|a, b| a.offset_seconds.total_cmp(&b.offset_seconds));
    lines
}

fn append_journal(dir: &Path, entry: &JournalLine) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("journal.jsonl"))
        .map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut file, entry).map_err(|e| e.to_string())?;
    file.write_all(b"\n").map_err(|e| e.to_string())
}

pub(super) async fn run(state: AppState, session: Uuid, dir: PathBuf, endpoint: Endpoint) {
    let mut cursors = HashMap::<PathBuf, u64>::new();
    let mut failures = 0u32;
    loop {
        let active = state
            .sessions
            .lock()
            .await
            .get(&session)
            .is_some_and(|entry| matches!(entry.info.status, "starting" | "joined" | "stopping"));
        let mut pending = false;
        if let Ok(files) = fs::read_dir(&dir) {
            for file in files.flatten() {
                let path = file.path();
                if !path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with("-audio.s16le"))
                {
                    continue;
                }
                let Some((participant_id, participant, track_offset)) = track_info(&dir, &path)
                else {
                    continue;
                };
                let cursor = cursors.entry(path.clone()).or_default();
                let size = file.metadata().map(|m| m.len()).unwrap_or(0);
                let available = size.saturating_sub(*cursor) / 2 * 2;
                if available < CHUNK_BYTES && (active || available < SAMPLE_RATE as u64 * 2) {
                    continue;
                }
                pending = true;
                let length = available.min(CHUNK_BYTES) as usize;
                let mut samples = vec![0; length];
                let result = File::open(&path).and_then(|mut source| {
                    source.seek(SeekFrom::Start(*cursor))?;
                    source.read_exact(&mut samples)
                });
                if let Err(error) = result {
                    eprintln!("[{session} transcription] read failed: {error}");
                    continue;
                }
                let offset_seconds = track_offset + *cursor as f64 / (SAMPLE_RATE as f64 * 2.0);
                let rms = (samples
                    .chunks_exact(2)
                    .map(|bytes| {
                        let value = i16::from_le_bytes([bytes[0], bytes[1]]) as f64;
                        value * value
                    })
                    .sum::<f64>()
                    / (length / 2) as f64)
                    .sqrt();
                if rms < 8.0 {
                    *cursor += length as u64;
                    continue;
                }
                match endpoint.transcribe(&wav(&samples)).await {
                    Ok(text) => {
                        failures = 0;
                        let _ = fs::remove_file(dir.join("transcription-error.txt"));
                        *cursor += length as u64;
                        if is_speech_result(&text) {
                            let line = JournalLine {
                                participant: participant.clone(),
                                participant_id: participant_id.clone(),
                                offset_seconds,
                                text: text.trim().into(),
                            };
                            if let Err(error) = append_journal(&dir, &line) {
                                eprintln!("[{session} transcription] journal failed: {error}");
                            }
                        }
                    }
                    Err(error) => {
                        failures += 1;
                        eprintln!("[{session} transcription] {error}");
                        let _ = fs::write(dir.join("transcription-error.txt"), &error);
                    }
                }
            }
        }
        if !active && (!pending || failures >= 3) {
            break;
        }
        sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{is_speech_result, wav, Endpoint};

    #[test]
    fn validates_endpoint_and_wav() {
        assert!(Endpoint::parse("http://192.0.2.34:8099").is_ok());
        assert!(Endpoint::parse("https://other:8099").is_err());
        assert_eq!(&wav(&[0, 0])[..4], b"RIFF");
        assert!(!is_speech_result("[No speech detected]"));
    }
}
