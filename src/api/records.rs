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
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use uuid::Uuid;

use super::{meeting_room, start_session, transcription, AppState};

#[derive(Deserialize)]
pub(super) struct MeetingForm {
    url: String,
}

#[derive(Serialize)]
struct JournalSnapshot {
    entries: Vec<transcription::JournalLine>,
    status: &'static str,
    error: Option<String>,
}

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
    audio_meta: Option<TrackMetadata>,
    video_meta: Option<TrackMetadata>,
}

#[derive(Clone, Deserialize)]
struct TrackMetadata {
    participant_id: String,
    participant_name: Option<String>,
    source_type: Option<String>,
    first_buffer_session_offset_ns: u64,
}

#[derive(Deserialize)]
struct ParticipantName {
    participant_id: String,
    display_name: String,
}

impl TrackMetadata {
    fn offset_secs(&self) -> f64 {
        self.first_buffer_session_offset_ns as f64 / 1_000_000_000.0
    }
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
    let mut files = fs::read_dir(&dir)
        .await
        .map_err(|error| error.to_string())?;
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
            entry.video_meta = fs::read_to_string(file.path().with_extension("track.json"))
                .await
                .ok()
                .and_then(|value| serde_json::from_str(&value).ok());
        } else {
            entry.audio = Some(size);
            entry.audio_meta = fs::read_to_string(file.path().with_extension("track.json"))
                .await
                .ok()
                .and_then(|value| serde_json::from_str(&value).ok());
        }
    }
    for entry in participants.values_mut() {
        let id = entry
            .video_meta
            .as_ref()
            .or(entry.audio_meta.as_ref())
            .map(|meta| meta.participant_id.clone());
        let Some(id) = id.filter(|id| safe_participant(id)) else {
            continue;
        };
        let name = fs::read_to_string(dir.join(format!("participant-{id}.json")))
            .await
            .ok()
            .and_then(|value| serde_json::from_str::<ParticipantName>(&value).ok())
            .filter(|info| info.participant_id == id && !info.display_name.trim().is_empty())
            .map(|info| info.display_name);
        if let Some(name) = name {
            if let Some(meta) = entry.video_meta.as_mut() {
                meta.participant_name = Some(name.clone());
            }
            if let Some(meta) = entry.audio_meta.as_mut() {
                meta.participant_name = Some(name);
            }
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

async fn page(state: &AppState, selected: Option<Uuid>, error: Option<&str>) -> Response {
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
            let label = files
                .video_meta
                .as_ref()
                .or(files.audio_meta.as_ref())
                .map(|meta| {
                    let name = meta
                        .participant_name
                        .as_deref()
                        .filter(|name| !name.trim().is_empty())
                        .map(|name| name.chars().take(128).collect::<String>())
                        .unwrap_or_else(|| format!("Участник {}", meta.participant_id));
                    format!(
                        "{} · {}",
                        escape(&name),
                        match meta.source_type.as_deref() {
                            Some("desktop") => "Экран",
                            Some("camera") => "Камера",
                            _ if files.audio.is_some() => "Звук",
                            _ => "Источник",
                        }
                    )
                })
                .unwrap_or_else(|| format!("Участник {safe_id}"));
            let video_offset = files
                .video_meta
                .as_ref()
                .map_or(0.0, TrackMetadata::offset_secs);
            let audio_offset = files
                .audio_meta
                .as_ref()
                .map_or(0.0, TrackMetadata::offset_secs);
            let video = match files.video {
                Some(size) if size > 0 => format!(
                    "<video controls preload=\"metadata\" playsinline data-offset=\"{video_offset}\" src=\"/records/{id}/media/{safe_id}/video\"></video><a class=\"download\" href=\"/records/{id}/media/{safe_id}/video\" download>Скачать видео · {}</a>",
                    file_size(size)
                ),
                _ if files.audio_meta.is_some() => String::new(),
                _ => "<p class=\"hint\">Видеопоток не записан.</p>".into(),
            };
            let audio = match files.audio {
                Some(size) if size > 0 => format!(
                    "<audio controls preload=\"metadata\" data-offset=\"{audio_offset}\" src=\"/records/{id}/media/{safe_id}/audio\"></audio><a class=\"download\" href=\"/records/{id}/media/{safe_id}/audio\" download>Скачать аудио · {}</a>",
                    file_size(size)
                ),
                _ if files.video_meta.is_some() => String::new(),
                _ => "<p class=\"hint\">Аудиопоток не записан.</p>".into(),
            };
            let mut starts = Vec::new();
            if files.video.is_some() {
                starts.push(format!("Видео: +{video_offset:.3} с"));
            }
            if files.audio.is_some() {
                starts.push(format!("Звук: +{audio_offset:.3} с"));
            }
            cards.push_str(&format!(
                "<section class=\"card\"><h2>{label}</h2><p class=\"hint\">{}</p>{video}{audio}</section>", starts.join(" · ")
            ));
        }
        if cards.is_empty() {
            cards.push_str("<section class=\"card\"><p>Файлов пока нет. Записи появятся, когда участник включит камеру или микрофон.</p></section>");
        }
        let journal_lines = transcription::read_journal(
            &PathBuf::from(state.recordings_dir.as_ref()).join(id.to_string()),
        );
        let mut journal = String::new();
        for line in journal_lines {
            journal.push_str(&format!("<li><button type=\"button\" class=\"journal-line\" data-seek=\"{:.2}\"><span>+{:.0} с · {}</span><strong>{}</strong></button></li>", line.offset_seconds, line.offset_seconds, escape(&line.participant), escape(&line.text)));
        }
        let journal_hint = if state.transcription.is_some() {
            "Журнал обновляется во время встречи. Автоматическая расшифровка может содержать ошибки."
        } else {
            "Для новых встреч задайте TRANSCRIPTION_URL, чтобы включить живую расшифровку."
        };
        format!(
            "<div class=\"eyebrow\">Сессия {id}</div><h1>{room}</h1><div class=\"badge\" id=\"session-status\">{status}</div><p class=\"hint\">Каждая дорожка имеет начало на общей шкале встречи. Камера, экран и звук отображаются отдельно.</p><section class=\"journal card\" data-session=\"{id}\"><h2>Журнал встречи</h2><p class=\"hint\">{journal_hint}</p><p id=\"journal-error\" class=\"form-error\" hidden></p><ol id=\"journal-lines\">{journal}</ol></section><div class=\"sync\"><button type=\"button\" id=\"play-all\">▶ Воспроизвести все</button><button type=\"button\" id=\"pause-all\">Пауза</button><input id=\"timeline\" type=\"range\" min=\"0\" max=\"1\" step=\"0.01\" value=\"0\" aria-label=\"Время встречи\"><span id=\"time-label\">0:00</span></div>{cards}",
            room = escape(row.room.trim()),
            status = status_label(row.status),
        )
    } else {
        "<h1>Записи Jitsi</h1><p class=\"hint\">Выберите сессию слева, чтобы открыть запись участника. Завершённые записи остаются здесь после перезапуска сервиса.</p>".into()
    };
    let error_html = error
        .map(|message| format!("<p class=\"form-error\">{}</p>", escape(message)))
        .unwrap_or_default();
    let html = format!(
        r#"<!doctype html><html lang="ru"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Записи Jitsi</title><style>
*{{box-sizing:border-box}}body{{margin:0;background:#0b1220;color:#eaf0fa;font:16px/1.5 system-ui,-apple-system,sans-serif}}a{{color:inherit}}.layout{{display:grid;grid-template-columns:280px minmax(0,1fr);min-height:100vh}}aside{{background:#111c2e;border-right:1px solid #26364b;padding:28px 18px}}aside header{{display:flex;align-items:center;justify-content:space-between;padding:0 10px 22px}}aside header a{{color:#8fc6ff;font-size:13px;text-decoration:none}}aside h2{{font-size:18px;margin:0}}.add-meeting,.dialog-submit{{width:100%;background:#7bbaf2;color:#071120;border:0;border-radius:10px;padding:11px 12px;font:inherit;font-weight:700;cursor:pointer}}.add-meeting{{margin:0 0 15px}}.session{{display:block;padding:14px 15px;margin:5px 0;border-radius:12px;text-decoration:none;color:#d5dfec}}.session:hover,.selected{{background:#203149}}.session strong{{display:block;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}}.session small{{display:block;color:#96a9bf;margin-top:3px}}main{{max-width:1100px;width:100%;margin:0 auto;padding:46px 42px}}h1{{font-size:32px;margin:5px 0 12px}}.eyebrow{{font-size:13px;color:#8fb6d9;word-break:break-all}}.badge{{display:inline-block;background:#173c3a;color:#a4edce;border-radius:999px;padding:5px 12px;font-size:13px;margin-bottom:16px}}.hint,.empty{{color:#a9bad0}}.sync{{display:flex;align-items:center;gap:10px;flex-wrap:wrap;padding:14px;background:#172438;border-radius:12px}}.sync button{{background:#7bbaf2;color:#071120;border:0;border-radius:8px;padding:8px 12px;cursor:pointer}}.sync input{{flex:1;min-width:150px}}.card{{background:#172438;border:1px solid #2b3d55;border-radius:16px;padding:24px;margin-top:22px}}.card h2{{font-size:19px;margin:0 0 16px}}video{{display:block;width:100%;max-height:70vh;background:#080d15;border-radius:10px}}audio{{display:block;width:100%;margin-top:18px}}.download{{display:inline-block;color:#9bd0ff;font-size:14px;margin-top:8px}}.journal ol{{padding-left:20px;max-height:320px;overflow:auto}}.journal li{{margin:10px 0}}.journal-line{{display:block;text-align:left;width:100%;background:#203149;color:#eaf0fa;border:0;border-radius:8px;padding:10px;cursor:pointer}}.journal-line span{{display:block;color:#9bc8ee;font-size:13px}}.journal-line strong{{font-weight:500}}dialog{{width:min(530px,calc(100% - 30px));border:1px solid #506780;border-radius:16px;padding:24px;background:#172438;color:#eaf0fa}}dialog::backdrop{{background:#000a}}dialog h2{{margin-top:0}}dialog input{{display:block;width:100%;margin:8px 0 18px;padding:12px;border:1px solid #506780;border-radius:8px;background:#0b1220;color:#fff;font:inherit}}.dialog-actions{{display:flex;gap:10px}}.dialog-actions button{{flex:1}}.dialog-cancel{{background:transparent;color:#d5dfec;border:1px solid #506780;border-radius:10px;font:inherit;cursor:pointer}}.form-error{{color:#ffb8af}}@media(max-width:760px){{.layout{{grid-template-columns:1fr}}aside{{border-right:0;border-bottom:1px solid #26364b;padding:16px}}aside header{{padding:0 5px 8px}}main{{padding:25px 18px}}.session{{display:inline-block;min-width:170px;max-width:240px}}}}
</style></head><body><div class="layout"><aside><header><h2><a href="/records">Записи</a></h2><a href="">Обновить ↻</a></header><button class="add-meeting" type="button" id="add-meeting">＋ Добавить встречу</button>{nav}</aside><main>{error_html}{content}</main></div><dialog id="meeting-dialog"><h2>Добавить встречу</h2><p class="hint">Вставьте ссылку Jitsi. Бот войдёт в комнату и начнёт запись.</p><form method="post" action="/records/new"><label for="meeting-url">Ссылка на встречу</label><input id="meeting-url" name="url" type="url" placeholder="https://meet.example.org/комната" required autofocus><div class="dialog-actions"><button class="dialog-cancel" type="button" id="cancel-meeting">Отмена</button><button class="dialog-submit" type="submit">Подключить бота</button></div></form></dialog><script>
const meetingDialog=document.getElementById('meeting-dialog');document.getElementById('add-meeting').onclick=()=>meetingDialog.showModal();document.getElementById('cancel-meeting').onclick=()=>meetingDialog.close();
const journal=document.querySelector('.journal[data-session]');if(journal){{const lines=document.getElementById('journal-lines');const session=journal.dataset.session;const fmt=t=>`${{Math.floor(t/60)}}:${{String(Math.floor(t%60)).padStart(2,'0')}}`;const labels={{starting:'Подключается',joined:'Идёт запись',stopping:'Завершается',stopped:'Завершена',exited:'Отключена',archived:'Сохранена'}};async function refreshJournal(){{try{{const r=await fetch(`/records/${{session}}/journal`,{{cache:'no-store'}});if(!r.ok)return;const data=await r.json();document.getElementById('session-status').textContent=labels[data.status]||data.status;const error=document.getElementById('journal-error');error.hidden=!data.error;error.textContent=data.error?`Ошибка транскрибации: ${{data.error}}`:'';lines.replaceChildren(...data.entries.map(item=>{{const li=document.createElement('li');const button=document.createElement('button');button.type='button';button.className='journal-line';button.dataset.seek=item.offset_seconds;const time=document.createElement('span');time.textContent=`${{fmt(item.offset_seconds)}} · ${{item.participant}}`;const text=document.createElement('strong');text.textContent=item.text;button.append(time,text);li.append(button);return li;}}));}}catch(_){{}}}}refreshJournal();setInterval(refreshJournal,5000);}}
document.addEventListener('click',event=>{{const button=event.target.closest('.journal-line');const slider=document.getElementById('timeline');if(button&&slider){{slider.value=button.dataset.seek;slider.dispatchEvent(new Event('input'));}}}});
(() => {{ const slider=document.getElementById('timeline'); if(!slider)return;
const media=[...document.querySelectorAll('video[data-offset],audio[data-offset]')];
const label=document.getElementById('time-label'); let playing=false, at=0, last=performance.now();
const fmt=t=>`${{Math.floor(t/60)}}:${{String(Math.floor(t%60)).padStart(2,'0')}}`;
function sync(force=false){{ const now=performance.now(); if(playing)at+=(now-last)/1000; last=now;
let end=0; for(const item of media){{ const offset=Number(item.dataset.offset)||0;
if(Number.isFinite(item.duration))end=Math.max(end,offset+item.duration);
if(!playing&&!force)continue;
const target=at-offset; if(target<0 || (Number.isFinite(item.duration)&&target>=item.duration)){{item.pause();continue;}}
if(Math.abs(item.currentTime-target)>0.25){{try{{item.currentTime=Math.max(0,target)}}catch(_){{}}}}
if(playing&&item.paused)item.play().catch(()=>{{}}); if(!playing&&!item.paused)item.pause();}}
slider.max=String(Math.max(1,end)); if(at>end&&end>0){{at=end;playing=false;}} slider.value=String(at);label.textContent=fmt(at);}}
document.getElementById('play-all').onclick=()=>{{playing=true;last=performance.now();sync();}};
document.getElementById('pause-all').onclick=()=>{{playing=false;media.forEach(item=>item.pause());sync();}};
slider.oninput=()=>{{at=Number(slider.value);last=performance.now();sync(true);}};
setInterval(sync,100); media.forEach(item=>item.addEventListener('loadedmetadata',sync)); sync();
}})();</script></body></html>"#
    );
    ([(header::CACHE_CONTROL, "no-store")], Html(html)).into_response()
}

pub(super) async fn index(State(state): State<AppState>, request: Request<Body>) -> Response {
    if !authenticated(request.headers(), &state).await {
        return login_redirect("/records");
    }
    page(&state, None, None).await
}

pub(super) async fn show(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    request: Request<Body>,
) -> Response {
    if !authenticated(request.headers(), &state).await {
        return login_redirect(&format!("/records/{id}"));
    }
    page(&state, Some(id), None).await
}

pub(super) async fn add_meeting(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<MeetingForm>,
) -> Response {
    if !authenticated(&headers, &state).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let room = match meeting_room(&form.url, &state.bosh_url) {
        Ok(room) => room,
        Err(error) => return page(&state, None, Some(&error)).await,
    };
    match start_session(state.clone(), room, state.default_nick.to_string()).await {
        Ok(info) => {
            let mut response = StatusCode::SEE_OTHER.into_response();
            response.headers_mut().insert(
                header::LOCATION,
                header::HeaderValue::from_str(&format!("/records/{}", info.session_id))
                    .expect("safe id"),
            );
            response
        }
        Err((_, Json(error))) => page(&state, None, Some(&error.error)).await,
    }
}

pub(super) async fn journal(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    request: Request<Body>,
) -> Response {
    if !authenticated(request.headers(), &state).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let dir = PathBuf::from(state.recordings_dir.as_ref()).join(id.to_string());
    if !dir.is_dir() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let status = state
        .sessions
        .lock()
        .await
        .get(&id)
        .map(|entry| entry.info.status)
        .unwrap_or("archived");
    let error = fs::read_to_string(dir.join("transcription-error.txt"))
        .await
        .ok();
    Json(JournalSnapshot {
        entries: transcription::read_journal(&dir),
        status,
        error,
    })
    .into_response()
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
