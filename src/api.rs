use std::{
    collections::HashMap, env, fs, path::PathBuf, process::Stdio, sync::Arc, time::Duration,
};

use axum::{
    extract::{Path, State},
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::{oneshot, Mutex},
    time::timeout,
};
use uuid::Uuid;

use crate::{encode_room, recording_pipeline};

#[derive(Clone)]
struct AppState {
    token: Arc<str>,
    bosh_url: Arc<str>,
    default_room: Arc<str>,
    default_nick: Arc<str>,
    recordings_dir: Arc<str>,
    gst_meet_bin: Arc<str>,
    sessions: Arc<Mutex<HashMap<Uuid, SessionEntry>>>,
}

struct SessionEntry {
    info: SessionInfo,
    stop: Option<oneshot::Sender<()>>,
}

#[derive(Clone, Serialize)]
struct SessionInfo {
    session_id: Uuid,
    room: String,
    status: &'static str,
    recording_dir: String,
    exit_code: Option<i32>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct JoinRequest {
    room: Option<String>,
    nick: Option<String>,
}

#[derive(Serialize)]
struct ApiError {
    error: String,
}

type ApiResult<T> = Result<T, (StatusCode, Json<ApiError>)>;

fn api_error(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (
        status,
        Json(ApiError {
            error: message.into(),
        }),
    )
}

fn authorized(headers: &HeaderMap, state: &AppState) -> ApiResult<()> {
    let Some(value) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return Err(api_error(StatusCode::UNAUTHORIZED, "Bearer token required"));
    };
    if value.strip_prefix("Bearer ") != Some(&state.token) {
        return Err(api_error(StatusCode::UNAUTHORIZED, "Invalid Bearer token"));
    }
    Ok(())
}

fn safe_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '@'))
}

fn safe_pipeline_path(value: &str) -> bool {
    !value.is_empty()
        && !value
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '!' | '"' | '\\' | '{' | '}'))
}

async fn join(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<JoinRequest>,
) -> ApiResult<(StatusCode, Json<SessionInfo>)> {
    authorized(&headers, &state)?;
    let room = request
        .room
        .unwrap_or_else(|| state.default_room.to_string());
    let nick = request
        .nick
        .unwrap_or_else(|| state.default_nick.to_string());
    if !safe_name(&room) || !safe_name(&nick) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "room and nick must be nonempty single names up to 256 bytes",
        ));
    }
    let mut sessions = state.sessions.lock().await;
    if sessions
        .values()
        .filter(|entry| matches!(entry.info.status, "starting" | "joined" | "stopping"))
        .count()
        >= 10
    {
        return Err(api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "maximum of 10 active sessions reached",
        ));
    }
    let id = Uuid::new_v4();
    let dir = format!("{}/{id}", state.recordings_dir.trim_end_matches('/'));
    fs::create_dir_all(&dir).map_err(|error| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot create recording directory: {error}"),
        )
    })?;
    let pipeline = recording_pipeline(&dir);
    let mut command = Command::new(&*state.gst_meet_bin);
    command
        .arg(format!("--bosh-url={}", state.bosh_url))
        .arg(format!("--room-name={}", encode_room(&room)))
        .arg(format!("--nick={nick}"))
        .arg(format!("--recv-pipeline-participant-template={pipeline}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn().map_err(|error| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot start gst-meet: {error}"),
        )
    })?;
    let info = SessionInfo {
        session_id: id,
        room,
        status: "starting",
        recording_dir: dir,
        exit_code: None,
    };
    let (stop_tx, stop_rx) = oneshot::channel();
    sessions.insert(
        id,
        SessionEntry {
            info: info.clone(),
            stop: Some(stop_tx),
        },
    );
    drop(sessions);
    tokio::spawn(monitor(state, id, child, stop_rx));
    Ok((StatusCode::ACCEPTED, Json(info)))
}

async fn session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<SessionInfo>> {
    authorized(&headers, &state)?;
    let sessions = state.sessions.lock().await;
    sessions
        .get(&id)
        .map(|entry| Json(entry.info.clone()))
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "session not found"))
}

async fn leave(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<SessionInfo>> {
    authorized(&headers, &state)?;
    let mut sessions = state.sessions.lock().await;
    let entry = sessions
        .get_mut(&id)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "session not found"))?;
    if let Some(stop) = entry.stop.take() {
        entry.info.status = "stopping";
        let _ = stop.send(());
    }
    Ok(Json(entry.info.clone()))
}

async fn read_log<R: tokio::io::AsyncRead + Unpin>(
    stream: R,
    state: AppState,
    id: Uuid,
    source: &'static str,
) {
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        eprintln!("[{id} {source}] {line}");
        if line.contains("Connected Colibri") {
            if let Some(entry) = state.sessions.lock().await.get_mut(&id) {
                if entry.info.status == "starting" {
                    entry.info.status = "joined";
                }
            }
        }
    }
}

async fn monitor(state: AppState, id: Uuid, mut child: Child, mut stop_rx: oneshot::Receiver<()>) {
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(read_log(stdout, state.clone(), id, "stdout"));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(read_log(stderr, state.clone(), id, "stderr"));
    }
    let mut was_stopped = false;
    let result = tokio::select! {
        result = child.wait() => result,
        _ = &mut stop_rx => {
            was_stopped = true;
            if let Some(pid) = child.id() {
                // gst-meet handles SIGINT and closes its GStreamer pipelines.
                unsafe { libc::kill(pid as i32, libc::SIGINT) };
            }
            match timeout(Duration::from_secs(10), child.wait()).await {
                Ok(result) => result,
                Err(_) => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            }
        }
    };
    let mut sessions = state.sessions.lock().await;
    if let Some(entry) = sessions.get_mut(&id) {
        entry.stop = None;
        entry.info.exit_code = result.ok().and_then(|status| status.code());
        entry.info.status = if was_stopped { "stopped" } else { "exited" };
    }
}

async fn shutdown(state: AppState) {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    let terminate = async {
        let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        signal.recv().await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
    {
        let mut sessions = state.sessions.lock().await;
        for entry in sessions.values_mut() {
            if let Some(stop) = entry.stop.take() {
                entry.info.status = "stopping";
                let _ = stop.send(());
            }
        }
    }
    for _ in 0..60 {
        if state
            .sessions
            .lock()
            .await
            .values()
            .all(|entry| !matches!(entry.info.status, "starting" | "joined" | "stopping"))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

pub(crate) async fn serve() -> Result<(), String> {
    let token = env::var("API_TOKEN").map_err(|_| "API_TOKEN is required".to_string())?;
    if token.len() < 16 {
        return Err("API_TOKEN must contain at least 16 bytes".into());
    }
    let bosh_url = env::var("JITSI_BOSH_URL").map_err(|_| "JITSI_BOSH_URL is required")?;
    if !bosh_url.starts_with("https://") {
        return Err("JITSI_BOSH_URL must start with https://".into());
    }
    let recordings_dir = env::var("RECORDINGS_DIR").unwrap_or_else(|_| "/data/recordings".into());
    if !safe_pipeline_path(&recordings_dir) {
        return Err("RECORDINGS_DIR contains unsupported pipeline characters".into());
    }
    fs::create_dir_all(PathBuf::from(&recordings_dir))
        .map_err(|error| format!("cannot create recordings directory: {error}"))?;
    let state = AppState {
        token: token.into(),
        bosh_url: bosh_url.into(),
        default_room: env::var("JITSI_ROOM").unwrap_or_default().into(),
        default_nick: env::var("JITSI_NICK")
            .unwrap_or_else(|_| "rust-recording-bot".into())
            .into(),
        recordings_dir: recordings_dir.into(),
        gst_meet_bin: env::var("GST_MEET_BIN")
            .unwrap_or_else(|_| "gst-meet".into())
            .into(),
        sessions: Arc::new(Mutex::new(HashMap::new())),
    };
    let bind = env::var("API_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|error| format!("cannot bind API to {bind}: {error}"))?;
    let app = Router::new()
        .route("/api/jitsi-bot/join", post(join))
        .route("/api/jitsi-bot/sessions/{id}", get(session))
        .route("/api/jitsi-bot/sessions/{id}/leave", post(leave))
        .with_state(state.clone());
    eprintln!("Jitsi bot API listening on {bind}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown(state))
        .await
        .map_err(|error| format!("API server failed: {error}"))
}
