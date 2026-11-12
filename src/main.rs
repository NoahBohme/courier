use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, Method as HttpMethod, StatusCode, Uri, header},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{any, get, post},
};
use reqwest::{Method, header::HeaderName, header::HeaderValue};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast};
use tokio_stream::{Stream, StreamExt, wrappers::BroadcastStream};

const INDEX_HTML: &str = include_str!("../static/index.html");

/// Captured requests kept per webhook bin.
const MAX_CAPTURES_PER_BIN: usize = 100;
/// Webhook bins kept in memory; the least recently used is dropped beyond this.
const MAX_BINS: usize = 100;
/// Bodies larger than this are truncated when captured.
const MAX_CAPTURED_BODY: usize = 512 * 1024;

struct AppState {
    client: reqwest::Client,
    data_path: PathBuf,
    // Serialises writes to the data file.
    write_lock: Mutex<()>,
    bins: std::sync::Mutex<HashMap<String, Bin>>,
    next_capture_id: AtomicU64,
}

struct Bin {
    captures: VecDeque<Capture>,
    tx: broadcast::Sender<Capture>,
    touched: Instant,
}

impl Bin {
    fn new() -> Self {
        Self {
            captures: VecDeque::new(),
            tx: broadcast::channel(64).0,
            touched: Instant::now(),
        }
    }
}

#[derive(Clone, Serialize)]
struct Capture {
    id: u64,
    at: u128,
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: String,
    binary: bool,
    truncated: bool,
    size: usize,
    ip: String,
}

#[derive(Deserialize)]
struct SendRequest {
    method: String,
    url: String,
    #[serde(default)]
    headers: Vec<(String, String)>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Serialize)]
struct SendResponse {
    status: u16,
    status_text: String,
    headers: Vec<(String, String)>,
    body: String,
    binary: bool,
    size: usize,
    time_ms: u128,
    final_url: String,
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(ErrorBody { error: msg.into() })).into_response()
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn send(State(state): State<Arc<AppState>>, Json(req): Json<SendRequest>) -> Response {
    let method = match Method::from_bytes(req.method.trim().to_uppercase().as_bytes()) {
        Ok(m) => m,
        Err(_) => return err(StatusCode::BAD_REQUEST, format!("Invalid method: {}", req.method)),
    };

    let mut url = req.url.trim().to_string();
    if url.is_empty() {
        return err(StatusCode::BAD_REQUEST, "URL is empty");
    }
    if !url.contains("://") {
        url = format!("http://{url}");
    }

    let mut builder = state
        .client
        .request(method, &url)
        .timeout(Duration::from_millis(req.timeout_ms.unwrap_or(30_000)));

    for (k, v) in req.headers {
        let k = k.trim();
        if k.is_empty() {
            continue;
        }
        let name = match HeaderName::from_bytes(k.as_bytes()) {
            Ok(n) => n,
            Err(_) => return err(StatusCode::BAD_REQUEST, format!("Invalid header name: {k}")),
        };
        let value = match HeaderValue::from_str(&v) {
            Ok(v) => v,
            Err(_) => return err(StatusCode::BAD_REQUEST, format!("Invalid value for header {k}")),
        };
        builder = builder.header(name, value);
    }

    if let Some(body) = req.body {
        builder = builder.body(body);
    }

    let started = Instant::now();
    let resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_GATEWAY, describe_error(&e)),
    };

    let status = resp.status();
    let final_url = resp.url().to_string();
    let headers = header_pairs(resp.headers());

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_GATEWAY, describe_error(&e)),
    };
    let time_ms = started.elapsed().as_millis();

    let (body, binary) = match std::str::from_utf8(&bytes) {
        Ok(s) => (s.to_string(), false),
        Err(_) => (String::new(), true),
    };

    Json(SendResponse {
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or("").to_string(),
        headers,
        body,
        binary,
        size: bytes.len(),
        time_ms,
        final_url,
    })
    .into_response()
}

fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect()
}

fn describe_error(e: &reqwest::Error) -> String {
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        source = s.source();
    }
    msg
}

async fn get_state(State(state): State<Arc<AppState>>) -> Response {
    match tokio::fs::read_to_string(&state.data_path).await {
        Ok(s) => ([(header::CONTENT_TYPE, "application/json")], s).into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Json(serde_json::json!({})).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn put_state(
    State(state): State<Arc<AppState>>,
    Json(value): Json<serde_json::Value>,
) -> Response {
    let _guard = state.write_lock.lock().await;
    let pretty = match serde_json::to_string_pretty(&value) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let tmp = state.data_path.with_extension("json.tmp");
    if let Err(e) = tokio::fs::write(&tmp, pretty).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    if let Err(e) = tokio::fs::rename(&tmp, &state.data_path).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    StatusCode::NO_CONTENT.into_response()
}

// ---------- webhook debugger ----------

fn valid_bin_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Runs `f` against the bin, creating it (and evicting the stalest bin) if needed.
fn with_bin<T>(state: &AppState, id: &str, f: impl FnOnce(&mut Bin) -> T) -> T {
    let mut bins = state.bins.lock().unwrap();
    if !bins.contains_key(id) && bins.len() >= MAX_BINS {
        if let Some(oldest) = bins.iter().min_by_key(|(_, b)| b.touched).map(|(k, _)| k.clone()) {
            bins.remove(&oldest);
        }
    }
    let bin = bins.entry(id.to_string()).or_insert_with(Bin::new);
    bin.touched = Instant::now();
    f(bin)
}

async fn capture(
    State(state): State<Arc<AppState>>,
    Path(params): Path<HashMap<String, String>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    method: HttpMethod,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let id = params.get("id").map(String::as_str).unwrap_or_default();
    if !valid_bin_id(id) {
        return err(StatusCode::BAD_REQUEST, "Invalid webhook id");
    }

    let ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| peer.ip().to_string());

    let truncated = body.len() > MAX_CAPTURED_BODY;
    let stored = &body[..body.len().min(MAX_CAPTURED_BODY)];
    let (text, binary) = match std::str::from_utf8(stored) {
        Ok(s) => (s.to_string(), false),
        // A truncation can split a multi-byte character; keep the valid prefix.
        Err(e) if truncated && e.error_len().is_none() => {
            (String::from_utf8_lossy(&stored[..e.valid_up_to()]).into_owned(), false)
        }
        Err(_) => (String::new(), true),
    };

    let cap = Capture {
        id: state.next_capture_id.fetch_add(1, Ordering::Relaxed),
        at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        method: method.to_string(),
        path: uri.path().to_string(),
        query: uri.query().unwrap_or("").to_string(),
        headers: header_pairs(&headers),
        body: text,
        binary,
        truncated,
        size: body.len(),
        ip,
    };

    with_bin(&state, id, |bin| {
        bin.captures.push_front(cap.clone());
        bin.captures.truncate(MAX_CAPTURES_PER_BIN);
        let _ = bin.tx.send(cap);
    });

    (
        [
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
            (header::ACCESS_CONTROL_ALLOW_HEADERS, "*"),
            (header::ACCESS_CONTROL_ALLOW_METHODS, "*"),
        ],
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

async fn list_captures(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    if !valid_bin_id(&id) {
        return err(StatusCode::BAD_REQUEST, "Invalid webhook id");
    }
    let caps: Vec<Capture> = with_bin(&state, &id, |bin| bin.captures.iter().cloned().collect());
    Json(caps).into_response()
}

async fn clear_captures(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    if !valid_bin_id(&id) {
        return err(StatusCode::BAD_REQUEST, "Invalid webhook id");
    }
    with_bin(&state, &id, |bin| bin.captures.clear());
    StatusCode::NO_CONTENT.into_response()
}

async fn stream_captures(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Response> {
    if !valid_bin_id(&id) {
        return Err(err(StatusCode::BAD_REQUEST, "Invalid webhook id"));
    }
    let rx = with_bin(&state, &id, |bin| bin.tx.subscribe());
    let stream = BroadcastStream::new(rx).filter_map(|msg| {
        msg.ok()
            .and_then(|cap| Event::default().json_data(cap).ok())
            .map(Ok)
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[tokio::main]
async fn main() {
    let host = std::env::var("COURIER_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port = std::env::var("COURIER_PORT").unwrap_or_else(|_| "7878".into());
    let data_path = PathBuf::from(
        std::env::var("COURIER_DATA").unwrap_or_else(|_| "courier-data.json".into()),
    );

    let client = reqwest::Client::builder()
        .user_agent(concat!("courier/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .expect("failed to build HTTP client");

    let state = Arc::new(AppState {
        client,
        data_path,
        write_lock: Mutex::new(()),
        bins: std::sync::Mutex::new(HashMap::new()),
        next_capture_id: AtomicU64::new(1),
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/send", post(send))
        .route("/api/state", get(get_state).put(put_state))
        .route("/api/hooks/{id}", get(list_captures).delete(clear_captures))
        .route("/api/hooks/{id}/stream", get(stream_captures))
        .route("/hook/{id}", any(capture))
        .route("/hook/{id}/{*rest}", any(capture))
        .layer(axum::extract::DefaultBodyLimit::max(50 * 1024 * 1024))
        .with_state(state.clone());

    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .expect("invalid COURIER_HOST/COURIER_PORT");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));

    println!("Courier running at http://{addr}");
    println!("Data file: {}", state.data_path.display());
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .expect("server error");
}
