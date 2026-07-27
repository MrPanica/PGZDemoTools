use crate::demo::{DemoInfo, read_demo, safe_name};
use crate::edit::{
    edit_demo, edit_demo_with_freecam, edit_demo_with_freecam_progress, edit_demo_with_progress,
};
use crate::voice::{
    DemoEvent, DemoPlayer, VoicePlayer, build_player_ogg, create_zip, extract_demo_index,
    unique_clients,
};
use main_error::MainError;
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, percent_encode};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::{self, BufWriter, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};
use uuid::Uuid;

const HTML: &str = include_str!("../web/index.html");
const LAME: &[u8] = include_bytes!("../lame.min.js");
const MAX_UPLOAD: u64 = 8 * 1024 * 1024 * 1024;
const MAX_JSON: usize = 2_000_000;

type HttpResponse = Response<Box<dyn Read + Send>>;
type PreparedEdit = (Arc<Session>, Vec<(u32, u32)>, PathBuf, bool);

#[derive(Clone)]
struct Session {
    directory: PathBuf,
    info: Arc<DemoInfo>,
    voice: PathBuf,
}

struct AppState {
    workspace: PathBuf,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    progress_timeout_seconds: u64,
}

struct Job {
    target: PathBuf,
    status: Mutex<JobStatus>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct JobStatus {
    state: String,
    progress: u8,
    stage: String,
    error: Option<String>,
}

impl Job {
    fn new(target: PathBuf) -> Self {
        Self {
            target,
            status: Mutex::new(JobStatus {
                state: "running".to_owned(),
                progress: 0,
                stage: "queued".to_owned(),
                error: None,
            }),
        }
    }

    fn update(&self, progress: u8, stage: &str) {
        if let Ok(mut status) = self.status.lock() {
            if status.progress == progress && status.stage == stage {
                return;
            }
            status.progress = progress;
            status.stage = stage.to_owned();
        }
    }

    fn ready(&self) {
        if let Ok(mut status) = self.status.lock() {
            status.state = "ready".to_owned();
            status.progress = 90;
            status.stage = "download".to_owned();
        }
    }

    fn fail(&self, error: impl Into<String>) {
        if let Ok(mut status) = self.status.lock() {
            status.state = "error".to_owned();
            status.stage = "error".to_owned();
            status.error = Some(error.into());
        }
    }
}

#[derive(Debug)]
struct WebError {
    status: u16,
    message: String,
}

impl WebError {
    fn bad(error: impl std::fmt::Debug) -> Self {
        Self {
            status: 400,
            message: format!("{error:?}"),
        }
    }

    fn message(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            message: message.into(),
        }
    }

    fn not_found() -> Self {
        Self {
            status: 404,
            message: "not found".to_owned(),
        }
    }
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid HTTP header")
}

fn response(
    status: u16,
    content_type: &str,
    data: Vec<u8>,
    mut headers: Vec<Header>,
) -> HttpResponse {
    let length = data.len();
    headers.push(header("Content-Type", content_type));
    headers.push(header("Cache-Control", "no-store"));
    Response::new(
        StatusCode(status),
        headers,
        Box::new(Cursor::new(data)),
        Some(length),
        None,
    )
}

fn json_response<T: Serialize>(status: u16, value: &T) -> HttpResponse {
    match serde_json::to_vec(value) {
        Ok(body) => response(status, "application/json; charset=utf-8", body, Vec::new()),
        Err(error) => response(
            500,
            "application/json; charset=utf-8",
            format!(r#"{{"error":"{}"}}"#, error).into_bytes(),
            Vec::new(),
        ),
    }
}

fn file_response(path: &Path) -> Result<HttpResponse, WebError> {
    let file = File::open(path).map_err(WebError::bad)?;
    let length: usize = file
        .metadata()
        .map_err(WebError::bad)?
        .len()
        .try_into()
        .map_err(WebError::bad)?;
    let name = path
        .file_name()
        .ok_or_else(|| WebError::message("output has no file name"))?
        .to_string_lossy();
    let quoted = percent_encode(name.as_bytes(), NON_ALPHANUMERIC).to_string();
    Ok(Response::new(
        StatusCode(200),
        vec![
            header("Content-Type", "application/octet-stream"),
            header("Cache-Control", "no-store"),
            header(
                "Content-Disposition",
                &format!("attachment; filename*=UTF-8''{quoted}"),
            ),
        ],
        Box::new(file),
        Some(length),
        None,
    ))
}

fn valid_session_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn progress_timeout_seconds(value: Option<&str>) -> u64 {
    value
        .and_then(|value| value.parse().ok())
        .map(|value: u64| value.clamp(30, 3600))
        .unwrap_or(300)
}

fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|part| {
        let (name, value) = part.split_once('=').unwrap_or((part, ""));
        (name == key).then(|| percent_decode_str(value).decode_utf8_lossy().into_owned())
    })
}

fn load_sessions(workspace: &Path) -> HashMap<String, Arc<Session>> {
    let mut sessions = HashMap::new();
    let Ok(entries) = fs::read_dir(workspace) else {
        return sessions;
    };
    for entry in entries.flatten() {
        let id = entry.file_name().to_string_lossy().into_owned();
        if !valid_session_id(&id) || !entry.path().is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(entry.path()) else {
            continue;
        };
        let mut demos: Vec<_> = files
            .flatten()
            .map(|file| file.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "dem"))
            .collect();
        demos.sort_by_key(|path| {
            path.metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
        });
        let Some(demo) = demos.first() else {
            continue;
        };
        let Ok(info) = read_demo(demo) else {
            continue;
        };
        let directory = entry.path();
        sessions.insert(
            id,
            Arc::new(Session {
                voice: directory.join("voice"),
                directory,
                info: Arc::new(info),
            }),
        );
    }
    sessions
}

fn session(state: &AppState, id: &str) -> Result<Arc<Session>, WebError> {
    if !valid_session_id(id) {
        return Err(WebError::message("invalid session"));
    }
    state
        .sessions
        .lock()
        .map_err(WebError::bad)?
        .get(id)
        .cloned()
        .ok_or_else(|| WebError::message("demo session expired"))
}

fn read_json<T: for<'de> Deserialize<'de>>(request: &mut Request) -> Result<T, WebError> {
    let length = request.body_length().unwrap_or(0);
    if length == 0 || length >= MAX_JSON {
        return Err(WebError::message("invalid request size"));
    }
    let mut body = Vec::with_capacity(length);
    request
        .as_reader()
        .take(length as u64 + 1)
        .read_to_end(&mut body)
        .map_err(WebError::bad)?;
    if body.len() != length {
        return Err(WebError::message("request ended early"));
    }
    serde_json::from_slice(&body).map_err(WebError::bad)
}

#[derive(Serialize)]
struct SessionResponse {
    id: String,
    meta: crate::demo::DemoMeta,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JobStartResponse {
    id: String,
    timeout_seconds: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VoicesResponse {
    players: Vec<VoicePlayer>,
    all_players: Vec<DemoPlayer>,
    events: Vec<DemoEvent>,
}

#[derive(Deserialize)]
struct EditRequest {
    id: String,
    #[serde(default)]
    ranges: Vec<Vec<u32>>,
    name: Option<String>,
    #[serde(default, rename = "unlockCamera")]
    unlock_camera: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VoiceRequest {
    id: String,
    #[serde(default)]
    clients: Vec<u8>,
    #[serde(default = "default_true")]
    keep_gaps: bool,
}

fn upload(request: &mut Request, state: &AppState, query: &str) -> Result<HttpResponse, WebError> {
    let length = request.body_length().unwrap_or(0) as u64;
    if length == 0 || length > MAX_UPLOAD {
        return Err(WebError::message("demo is empty or larger than 8 GiB"));
    }
    let original = query_value(query, "name").unwrap_or_else(|| "demo.dem".to_owned());
    if !original.to_lowercase().ends_with(".dem") {
        return Err(WebError::message("only .dem files are accepted"));
    }
    let id = Uuid::new_v4().simple().to_string();
    let directory = state.workspace.join(&id);
    fs::create_dir(&directory).map_err(WebError::bad)?;
    let original_name = Path::new(&original)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let demo_name = safe_name(&original_name, "demo.dem");
    let demo_path = directory.join(&demo_name);
    let partial = directory.join(format!("{demo_name}.part"));
    let result = (|| {
        let mut output = BufWriter::new(File::create(&partial).map_err(WebError::bad)?);
        let copied =
            io::copy(&mut request.as_reader().take(length), &mut output).map_err(WebError::bad)?;
        output.flush().map_err(WebError::bad)?;
        drop(output);
        if copied != length {
            return Err(WebError::message("upload ended early"));
        }
        fs::rename(&partial, &demo_path).map_err(WebError::bad)?;
        let info = Arc::new(read_demo(&demo_path).map_err(WebError::bad)?);
        let response = SessionResponse {
            id: id.clone(),
            meta: info.meta(),
        };
        let session = Arc::new(Session {
            voice: directory.join("voice"),
            directory: directory.clone(),
            info,
        });
        state
            .sessions
            .lock()
            .map_err(WebError::bad)?
            .insert(id, session);
        Ok(json_response(200, &response))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&partial);
        let _ = fs::remove_file(&demo_path);
        let _ = fs::remove_dir(&directory);
    }
    result
}

fn prepare_edit(payload: EditRequest, state: &AppState) -> Result<PreparedEdit, WebError> {
    let session = session(state, &payload.id)?;
    let ranges = payload
        .ranges
        .iter()
        .map(|range| match range.as_slice() {
            [start, end] => Ok((*start, *end)),
            _ => Err(WebError::message("invalid edit range")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let default = format!(
        "{}-edit",
        session
            .info
            .path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
    );
    let mut name = safe_name(payload.name.as_deref().unwrap_or(&default), &default);
    if name.to_lowercase().ends_with(".dem") {
        name.truncate(name.len() - 4);
    }
    name = name.replace('.', "-").trim_matches([' ', '-']).to_owned();
    if name.is_empty() {
        name = default;
    }
    let target = session.directory.join(format!("{name}.dem"));
    Ok((session, ranges, target, payload.unlock_camera))
}

fn run_edit(
    session: &Session,
    ranges: &[(u32, u32)],
    target: &Path,
    workspace: &Path,
    unlock_camera: bool,
) -> Result<PathBuf, MainError> {
    if unlock_camera {
        edit_demo_with_freecam(&session.info, ranges, target, workspace)
    } else {
        edit_demo(&session.info, ranges, target, workspace)
    }
}

fn run_edit_with_progress(
    session: &Session,
    ranges: &[(u32, u32)],
    target: &Path,
    workspace: &Path,
    unlock_camera: bool,
    progress: &mut dyn FnMut(u8),
) -> Result<PathBuf, MainError> {
    if unlock_camera {
        edit_demo_with_freecam_progress(&session.info, ranges, target, workspace, progress)
    } else {
        edit_demo_with_progress(&session.info, ranges, target, workspace, progress)
    }
}

const fn edit_job_progress(progress: u8) -> u8 {
    5 + ((progress as u16) * 85 / 100) as u8
}

fn edit(request: &mut Request, state: &AppState) -> Result<HttpResponse, WebError> {
    let payload: EditRequest = read_json(request)?;
    let (session, ranges, target, unlock_camera) = prepare_edit(payload, state)?;
    run_edit(&session, &ranges, &target, &state.workspace, unlock_camera).map_err(WebError::bad)?;
    file_response(&target)
}

fn start_edit_job(request: &mut Request, state: &AppState) -> Result<HttpResponse, WebError> {
    let payload: EditRequest = read_json(request)?;
    let (session, ranges, target, unlock_camera) = prepare_edit(payload, state)?;
    let id = Uuid::new_v4().simple().to_string();
    let job = Arc::new(Job::new(target.clone()));
    state
        .jobs
        .lock()
        .map_err(WebError::bad)?
        .insert(id.clone(), Arc::clone(&job));
    let workspace = state.workspace.clone();
    thread::spawn(move || {
        let mut report = |progress| {
            job.update(
                edit_job_progress(progress),
                if unlock_camera && progress >= 92 {
                    "freecam"
                } else {
                    "editing"
                },
            );
        };
        match run_edit_with_progress(
            &session,
            &ranges,
            &target,
            &workspace,
            unlock_camera,
            &mut report,
        ) {
            Ok(_) => job.ready(),
            Err(error) => job.fail(format!("{error:?}")),
        }
    });
    Ok(json_response(
        202,
        &JobStartResponse {
            id,
            timeout_seconds: state.progress_timeout_seconds,
        },
    ))
}

fn job(state: &AppState, id: &str) -> Result<Arc<Job>, WebError> {
    if !valid_session_id(id) {
        return Err(WebError::message("invalid job"));
    }
    state
        .jobs
        .lock()
        .map_err(WebError::bad)?
        .get(id)
        .cloned()
        .ok_or_else(WebError::not_found)
}

fn job_status(state: &AppState, query: &str) -> Result<HttpResponse, WebError> {
    let id = query_value(query, "id").unwrap_or_default();
    let status = job(state, &id)?
        .status
        .lock()
        .map_err(WebError::bad)?
        .clone();
    Ok(json_response(200, &status))
}

fn job_download(state: &AppState, query: &str) -> Result<HttpResponse, WebError> {
    let id = query_value(query, "id").unwrap_or_default();
    let job = job(state, &id)?;
    if job.status.lock().map_err(WebError::bad)?.state != "ready" {
        return Err(WebError::message("job is not ready"));
    }
    file_response(&job.target)
}

fn send_voices(request: &mut Request, state: &AppState) -> Result<HttpResponse, WebError> {
    let payload: VoiceRequest = read_json(request)?;
    let current = session(state, &payload.id)?;
    let index = extract_demo_index(&current.info, &current.voice).map_err(WebError::bad)?;
    let known: HashSet<_> = index.players.iter().map(|player| player.client).collect();
    let clients = unique_clients(&payload.clients);
    if clients.is_empty() || clients.iter().any(|client| !known.contains(client)) {
        return Err(WebError::message("invalid player selection"));
    }
    let by_client: HashMap<_, _> = index
        .players
        .into_iter()
        .map(|player| (player.client, player))
        .collect();
    let mut outputs = Vec::with_capacity(clients.len());
    for client in clients {
        let player = &by_client[&client];
        let name = safe_name(&player.name, &format!("client-{client}"));
        let suffix = if payload.keep_gaps {
            ".with-pauses"
        } else {
            ".compact"
        };
        let target = current.directory.join(format!("{name}{suffix}.ogg"));
        build_player_ogg(
            &current.voice.join("frames").join(format!("{client}.txt")),
            &target,
            current.info.tick_rate,
            payload.keep_gaps,
        )
        .map_err(WebError::bad)?;
        outputs.push(target);
    }
    if outputs.len() == 1 {
        return file_response(&outputs[0]);
    }
    let archive = current
        .directory
        .join(format!("voices-{}.zip", &payload.id[..8]));
    create_zip(&outputs, &archive).map_err(WebError::bad)?;
    file_response(&archive)
}

fn route(
    request: &mut Request,
    state: &AppState,
    method: Method,
    path: &str,
    query: &str,
) -> Result<HttpResponse, WebError> {
    match (method, path) {
        (Method::Get, "/") => Ok(response(
            200,
            "text/html; charset=utf-8",
            HTML.replace(
                "__PGZ_PROGRESS_TIMEOUT_SECONDS__",
                &state.progress_timeout_seconds.to_string(),
            )
            .into_bytes(),
            Vec::new(),
        )),
        (Method::Get, "/lame.min.js") => Ok(response(
            200,
            "application/javascript; charset=utf-8",
            LAME.to_vec(),
            Vec::new(),
        )),
        (Method::Get, "/api/session") => {
            let id = query_value(query, "id").unwrap_or_default();
            let current = session(state, &id)?;
            Ok(json_response(
                200,
                &SessionResponse {
                    id,
                    meta: current.info.meta(),
                },
            ))
        }
        (Method::Get, "/api/voices") => {
            let id = query_value(query, "id").unwrap_or_default();
            let current = session(state, &id)?;
            let index = extract_demo_index(&current.info, &current.voice).map_err(WebError::bad)?;
            Ok(json_response(
                200,
                &VoicesResponse {
                    players: index.players,
                    all_players: index.all_players,
                    events: index.events,
                },
            ))
        }
        (Method::Get, "/api/job") => job_status(state, query),
        (Method::Get, "/api/job/download") => job_download(state, query),
        (Method::Post, "/api/upload") => upload(request, state, query),
        (Method::Post, "/api/edit") => edit(request, state),
        (Method::Post, "/api/edit/job") => start_edit_job(request, state),
        (Method::Post, "/api/voice") => send_voices(request, state),
        _ => Err(WebError::not_found()),
    }
}

fn handle_request(mut request: Request, state: &AppState) {
    let url = request.url().to_owned();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let method = request.method().clone();
    let response = match route(&mut request, state, method, path, query) {
        Ok(response) => response,
        Err(error) => json_response(error.status, &serde_json::json!({ "error": error.message })),
    };
    let _ = request.respond(response);
}

fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = Command::new("cmd")
            .args(["/C", "start", "", url])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("xdg-open").arg(url).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg(url).spawn();
    }
}

pub fn serve(host: &str, port: u16, workspace: &Path, no_browser: bool) -> Result<(), MainError> {
    fs::create_dir_all(workspace)?;
    let progress_timeout_seconds = progress_timeout_seconds(
        env::var("PGZ_DEMO_PROGRESS_TIMEOUT_SECONDS")
            .ok()
            .as_deref(),
    );
    let state = Arc::new(AppState {
        workspace: workspace.to_path_buf(),
        sessions: Mutex::new(load_sessions(workspace)),
        jobs: Mutex::new(HashMap::new()),
        progress_timeout_seconds,
    });
    let server = Arc::new(
        Server::http(format!("{host}:{port}"))
            .map_err(|error| io::Error::other(error.to_string()))?,
    );
    let workers = thread::available_parallelism()
        .map(|value| value.get().clamp(2, 8))
        .unwrap_or(4);
    let url = format!("http://{host}:{port}");
    println!("TF2 Demo Tools: {url} (Ctrl+C to stop)");
    if !no_browser {
        let url = url.clone();
        thread::spawn(move || open_browser(&url));
    }
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let server = Arc::clone(&server);
        let state = Arc::clone(&state);
        handles.push(thread::spawn(move || {
            while let Ok(request) = server.recv() {
                handle_request(request, &state);
            }
        }));
    }
    for handle in handles {
        let _ = handle.join();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        EditRequest, edit_job_progress, progress_timeout_seconds, query_value, valid_session_id,
    };

    #[test]
    fn session_and_query_validation() {
        assert!(valid_session_id("0123456789abcdef0123456789abcdef"));
        assert!(!valid_session_id("0123456789ABCDEF0123456789ABCDEF"));
        assert_eq!(
            query_value("name=MEGA%20POTNAYA.dem", "name").as_deref(),
            Some("MEGA POTNAYA.dem")
        );
        let normal: EditRequest = serde_json::from_str(r#"{"id":"x","ranges":[[1,2]]}"#).unwrap();
        let freecam: EditRequest =
            serde_json::from_str(r#"{"id":"x","unlockCamera":true}"#).unwrap();
        assert!(!normal.unlock_camera);
        assert!(freecam.unlock_camera);
        assert_eq!(progress_timeout_seconds(None), 300);
        assert_eq!(progress_timeout_seconds(Some("5")), 30);
        assert_eq!(progress_timeout_seconds(Some("7200")), 3600);
        assert_eq!(edit_job_progress(0), 5);
        assert_eq!(edit_job_progress(100), 90);
    }
}
