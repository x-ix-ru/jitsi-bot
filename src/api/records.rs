use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};

use axum::{
    body::Body,
    extract::{Form, Path, Query, State},
    http::{header, HeaderMap, Request, StatusCode},
    response::{Html, IntoResponse, Response},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use tokio::fs;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use uuid::Uuid;

use super::AppState;

struct RecordRow {
    id: Uuid,
    room: String,
    status: &'static str,
    modified: SystemTime,
}

#[derive(Default)]
struct ParticipantFiles {
    audio: Option<u64>,
    video: Option<u64>,
}

#[derive(Default, Deserialize)]
pub(super) struct LoginQuery {
    next: Option<String>,
    invalid: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct LoginForm {
    password: String,
    next: Option<String>,
}

fn valid_basic_auth(headers: &HeaderMap, state: &AppState) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Basic "))
        .and_then(|value| STANDARD.decode(value).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|value| value.strip_prefix("viewer:").map(str::to_owned))
        .is_some_and(|password| password == state.token.as_ref())
}

async fn authenticated(headers: &HeaderMap, state: &AppState) -> bool {
    if valid_basic_auth(headers, state) {
        return true;
    }
    let Some(id) = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies
                .split(';')
                .map(str::trim)
                .find_map(|cookie| cookie.strip_prefix("jitsi_records_session="))
        })
        .and_then(|value| Uuid::parse_str(value).ok())
    else {
        return false;
    };
    state
        .web_sessions
        .lock()
        .await
        .get(&id)
        .is_some_and(|expires| *expires > Instant::now())
}

fn login_redirect(next: &str) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    let location = format!("/records/login?next={next}");
    response.headers_mut().insert(
        header::LOCATION,
        header::HeaderValue::from_str(&location).expect("safe path"),
    );
    response
}

fn safe_next(next: Option<String>) -> String {
    match next {
        Some(next) if next == "/records" => next,
        Some(next)
            if next
                .strip_prefix("/records/")
                .is_some_and(|id| Uuid::parse_str(id).is_ok()) =>
        {
            next
        }
        _ => "/records".into(),
    }
}

pub(super) async fn login_page(Query(query): Query<LoginQuery>) -> Response {
    let next = safe_next(query.next);
    let error = if query.invalid.is_some() {
        "<p class=\"error\">Неверный токен. Попробуйте ещё раз.</p>"
    } else {
        ""
    };
    let html = format!(
        r#"<!doctype html><html lang="ru"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Вход · Записи Jitsi</title><style>
*{{box-sizing:border-box}}body{{min-height:100vh;margin:0;display:grid;place-items:center;background:#0b1220;color:#eaf0fa;font:16px/1.5 system-ui,-apple-system,sans-serif}}main{{width:min(420px,calc(100% - 32px));padding:32px;background:#172438;border:1px solid #2b3d55;border-radius:18px}}h1{{margin:0 0 8px;font-size:27px}}p{{color:#a9bad0}}label{{display:block;margin:26px 0 8px}}input{{width:100%;padding:13px;border:1px solid #506780;border-radius:10px;background:#0b1220;color:#fff;font-size:16px}}button{{width:100%;margin-top:20px;padding:13px;border:0;border-radius:10px;background:#7bbaf2;color:#071120;font-size:16px;font-weight:700;cursor:pointer}}.error{{color:#ffb8af}}
</style></head><body><main><h1>Записи Jitsi</h1><p>Введите API_TOKEN, чтобы открыть записи участников.</p>{error}<form method="post" action="/records/login"><input type="hidden" name="next" value="{next}"><label for="password">API_TOKEN</label><input id="password" name="password" type="password" autocomplete="current-password" required autofocus><button type="submit">Открыть записи</button></form></main></body></html>"#
    );
    ([(header::CACHE_CONTROL, "no-store")], Html(html)).into_response()
}

pub(super) async fn login(State(state): State<AppState>, Form(form): Form<LoginForm>) -> Response {
    let next = safe_next(form.next);
    if form.password != state.token.as_ref() {
        let mut response = StatusCode::SEE_OTHER.into_response();
        let location = format!("/records/login?next={next}&invalid=1");
        response.headers_mut().insert(
            header::LOCATION,
            header::HeaderValue::from_str(&location).expect("safe path"),
        );
        return response;
    }
    let id = Uuid::new_v4();
    let mut sessions = state.web_sessions.lock().await;
    sessions.retain(|_, expires| *expires > Instant::now());
    sessions.insert(id, Instant::now() + Duration::from_secs(43_200));
    let mut response = StatusCode::SEE_OTHER.into_response();
    response.headers_mut().insert(
        header::LOCATION,
        header::HeaderValue::from_str(&next).expect("safe path"),
    );
    response.headers_mut().insert(
        header::SET_COOKIE,
        header::HeaderValue::from_str(&format!(
            "jitsi_records_session={id}; HttpOnly; SameSite=Strict; Path=/records; Max-Age=43200"
        ))
        .expect("safe cookie"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn status_label(status: &str) -> &'static str {
    match status {
        "starting" => "Подключается",
        "joined" => "Идёт запись",
        "stopping" => "Завершается",
        "stopped" => "Завершена",
        "exited" => "Отключена",
        _ => "Сохранена",
    }
}

fn file_size(size: u64) -> String {
    if size >= 1_048_576 {
        format!("{:.1} МБ", size as f64 / 1_048_576.0)
    } else if size >= 1024 {
        format!("{:.0} КБ", size as f64 / 1024.0)
    } else {
        format!("{size} Б")
    }
}

async fn list(state: &AppState) -> Result<Vec<RecordRow>, String> {
    let mut rows = HashMap::<Uuid, RecordRow>::new();
    let sessions = state.sessions.lock().await;
    for entry in sessions.values() {
        rows.insert(
            entry.info.session_id,
            RecordRow {
                id: entry.info.session_id,
                room: entry.info.room.clone(),
                status: entry.info.status,
                modified: SystemTime::UNIX_EPOCH,
            },
        );
    }
    drop(sessions);

    let mut dirs = fs::read_dir(state.recordings_dir.as_ref())
        .await
        .map_err(|error| format!("Не удалось прочитать каталог записей: {error}"))?;
    while let Some(dir) = dirs.next_entry().await.map_err(|error| error.to_string())? {
        let Some(id) = dir
            .file_name()
            .to_str()
            .and_then(|name| Uuid::parse_str(name).ok())
        else {
            continue;
        };
        if !dir
            .file_type()
            .await
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            continue;
        }
        let modified = dir
            .metadata()
            .await
            .ok()
            .and_then(|meta| meta.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if let Some(row) = rows.get_mut(&id) {
            row.modified = modified;
        } else {
            let room = fs::read_to_string(dir.path().join("room.txt"))
                .await
                .unwrap_or_else(|_| "Комната неизвестна".into());
            rows.insert(
                id,
                RecordRow {
                    id,
                    room,
                    status: "archived",
                    modified,
                },
            );
        }
    }
    let mut rows: Vec<_> = rows.into_values().collect();
    rows.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| b.id.cmp(&a.id)));
    Ok(rows)
}

async fn participants(
    state: &AppState,
    id: Uuid,
) -> Result<Vec<(String, ParticipantFiles)>, String> {
    let dir = PathBuf::from(state.recordings_dir.as_ref()).join(id.to_string());
    let mut files = fs::read_dir(dir).await.map_err(|error| error.to_string())?;
    let mut participants = HashMap::<String, ParticipantFiles>::new();
    while let Some(file) = files
        .next_entry()
        .await
        .map_err(|error| error.to_string())?
    {
        let name = file.file_name();
        let Some(name) = name.to_str() else { continue };
        let (participant, kind) = if let Some(value) = name.strip_suffix("-video.webm") {
            (value, "video")
        } else if let Some(value) = name.strip_suffix("-audio.webm") {
            (value, "audio")
        } else {
            continue;
        };
        if !safe_participant(participant)
            || !file.file_type().await.map_err(|e| e.to_string())?.is_file()
        {
            continue;
        }
        let size = file.metadata().await.map_err(|e| e.to_string())?.len();
        let entry = participants.entry(participant.to_string()).or_default();
        if kind == "video" {
            entry.video = Some(size);
        } else {
            entry.audio = Some(size);
        }
    }
    let mut participants: Vec<_> = participants.into_iter().collect();
    participants.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(participants)
}

fn safe_participant(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

async fn page(state: &AppState, selected: Option<Uuid>) -> Response {
    let rows = match list(state).await {
        Ok(rows) => rows,
        Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    };
    if selected.is_some_and(|id| !rows.iter().any(|row| row.id == id)) {
        return (StatusCode::NOT_FOUND, "Сессия не найдена").into_response();
    }
    let mut nav = String::new();
    for row in &rows {
        let selected_class = if Some(row.id) == selected {
            " selected"
        } else {
            ""
        };
        nav.push_str(&format!(
            "<a class=\"session{selected_class}\" href=\"/records/{id}\"><strong>{room}</strong><small>{status} · {short_id}</small></a>",
            id = row.id,
            room = escape(row.room.trim()),
            status = status_label(row.status),
            short_id = &row.id.to_string()[..8],
        ));
    }
    if rows.is_empty() {
        nav.push_str("<p class=\"empty\">Записей пока нет.</p>");
    }

    let content = if let Some(id) = selected {
        let row = rows.iter().find(|row| row.id == id).expect("checked above");
        let files = match participants(state, id).await {
            Ok(files) => files,
            Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
        };
        let mut cards = String::new();
        for (participant, files) in files {
            let safe_id = escape(&participant);
            let video = match files.video {
                Some(size) if size > 0 => format!(
                    "<video controls preload=\"metadata\" playsinline src=\"/records/{id}/media/{safe_id}/video\"></video><a class=\"download\" href=\"/records/{id}/media/{safe_id}/video\" download>Скачать видео · {}</a>",
                    file_size(size)
                ),
                _ => "<p class=\"hint\">Видеопоток не записан.</p>".into(),
            };
            let audio = match files.audio {
                Some(size) if size > 0 => format!(
                    "<audio controls preload=\"metadata\" src=\"/records/{id}/media/{safe_id}/audio\"></audio><a class=\"download\" href=\"/records/{id}/media/{safe_id}/audio\" download>Скачать аудио · {}</a>",
                    file_size(size)
                ),
                _ => "<p class=\"hint\">Аудиопоток не записан.</p>".into(),
            };
            cards.push_str(&format!(
                "<section class=\"card\"><h2>Участник {safe_id}</h2>{video}{audio}</section>"
            ));
        }
        if cards.is_empty() {
            cards.push_str("<section class=\"card\"><p>Файлов пока нет. Записи появятся, когда участник включит камеру или микрофон.</p></section>");
        }
        format!(
            "<div class=\"eyebrow\">Сессия {id}</div><h1>{room}</h1><div class=\"badge\">{status}</div><p class=\"hint\">Видео и аудио сохраняются отдельно для каждого участника. Для обновления списка файлов перезагрузите страницу.</p>{cards}",
            room = escape(row.room.trim()),
            status = status_label(row.status),
        )
    } else {
        "<h1>Записи Jitsi</h1><p class=\"hint\">Выберите сессию слева, чтобы открыть запись участника. Завершённые записи остаются здесь после перезапуска сервиса.</p>".into()
    };
    let html = format!(
        r#"<!doctype html><html lang="ru"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Записи Jitsi</title><style>
*{{box-sizing:border-box}}body{{margin:0;background:#0b1220;color:#eaf0fa;font:16px/1.5 system-ui,-apple-system,sans-serif}}a{{color:inherit}}.layout{{display:grid;grid-template-columns:280px minmax(0,1fr);min-height:100vh}}aside{{background:#111c2e;border-right:1px solid #26364b;padding:28px 18px}}aside header{{display:flex;align-items:center;justify-content:space-between;padding:0 10px 22px}}aside header a{{color:#8fc6ff;font-size:13px;text-decoration:none}}aside h2{{font-size:18px;margin:0}}.session{{display:block;padding:14px 15px;margin:5px 0;border-radius:12px;text-decoration:none;color:#d5dfec}}.session:hover,.selected{{background:#203149}}.session strong{{display:block;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}}.session small{{display:block;color:#96a9bf;margin-top:3px}}main{{max-width:1100px;width:100%;margin:0 auto;padding:46px 42px}}h1{{font-size:32px;margin:5px 0 12px}}.eyebrow{{font-size:13px;color:#8fb6d9;word-break:break-all}}.badge{{display:inline-block;background:#173c3a;color:#a4edce;border-radius:999px;padding:5px 12px;font-size:13px;margin-bottom:16px}}.hint,.empty{{color:#a9bad0}}.card{{background:#172438;border:1px solid #2b3d55;border-radius:16px;padding:24px;margin-top:22px}}.card h2{{font-size:19px;margin:0 0 16px}}video{{display:block;width:100%;max-height:70vh;background:#080d15;border-radius:10px}}audio{{display:block;width:100%;margin-top:18px}}.download{{display:inline-block;color:#9bd0ff;font-size:14px;margin-top:8px}}@media(max-width:760px){{.layout{{grid-template-columns:1fr}}aside{{border-right:0;border-bottom:1px solid #26364b;padding:16px}}aside header{{padding:0 5px 8px}}main{{padding:25px 18px}}.session{{display:inline-block;min-width:170px;max-width:240px}}}}
</style></head><body><div class="layout"><aside><header><h2><a href="/records">Записи</a></h2><a href="">Обновить ↻</a></header>{nav}</aside><main>{content}</main></div></body></html>"#
    );
    ([(header::CACHE_CONTROL, "no-store")], Html(html)).into_response()
}

pub(super) async fn index(State(state): State<AppState>, request: Request<Body>) -> Response {
    if !authenticated(request.headers(), &state).await {
        return login_redirect("/records");
    }
    page(&state, None).await
}

pub(super) async fn show(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    request: Request<Body>,
) -> Response {
    if !authenticated(request.headers(), &state).await {
        return login_redirect(&format!("/records/{id}"));
    }
    page(&state, Some(id)).await
}

pub(super) async fn media(
    State(state): State<AppState>,
    Path((id, participant, kind)): Path<(Uuid, String, String)>,
    request: Request<Body>,
) -> Response {
    if !authenticated(request.headers(), &state).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !safe_participant(&participant) || !matches!(kind.as_str(), "video" | "audio") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = PathBuf::from(state.recordings_dir.as_ref())
        .join(id.to_string())
        .join(format!("{participant}-{kind}.webm"));
    let Ok(metadata) = fs::symlink_metadata(&path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let service = ServeFile::new(path);
    match service.oneshot(request).await {
        Ok(response) => {
            let mut response = response.map(Body::new);
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("private, no-store"),
            );
            response
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Не удалось открыть запись: {error}"),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::{escape, safe_participant};

    #[test]
    fn escapes_room_names_and_rejects_media_path_traversal() {
        assert_eq!(escape("<room>&\""), "&lt;room&gt;&amp;&quot;");
        assert!(!safe_participant("../secret"));
        assert!(safe_participant("307571a5"));
    }
}
