//! Local control panel on http://127.0.0.1:<control_port>: screen layout editor,
//! connected machines, file sending. Also the API the `multipc send` command uses.
//!
//! Only reachable from this PC. Every API call needs the token stored in the
//! config folder (the page gets it embedded), and the Host header must be local,
//! so other websites open in the browser cannot drive it.

use crate::daemon::{ApiRequest, Event};
use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use mpc_core::geometry::Placement;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

const PAGE: &str = include_str!("ui.html");
pub const TOKEN_FILE: &str = "api-token";

#[derive(Clone)]
struct AppState {
    events: mpsc::UnboundedSender<Event>,
    token: Arc<String>,
    port: u16,
}

pub async fn start(port: u16, cfg_dir: PathBuf, events: mpsc::UnboundedSender<Event>) -> Result<()> {
    let token = crate::transfer::random_id().to_string() + &crate::transfer::random_id().to_string();
    write_token(&cfg_dir, &token)?;
    let state = AppState { events, token: Arc::new(token), port };
    let app = Router::new()
        .route("/", get(page))
        .route("/api/state", get(api_state))
        .route("/api/layout", post(api_layout))
        .route("/api/send", post(api_send))
        .with_state(state);
    let listener =
        tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await.with_context(|| format!("control panel port {port} is busy"))?;
    tracing::info!("control panel: http://127.0.0.1:{port}");
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("control panel stopped: {e}");
        }
    });
    Ok(())
}

fn write_token(dir: &Path, token: &str) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(TOKEN_FILE), token)?;
    Ok(())
}

fn local_host(headers: &HeaderMap, port: u16) -> bool {
    let host = headers.get("host").and_then(|h| h.to_str().ok()).unwrap_or("");
    host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
}

fn check(state: &AppState, headers: &HeaderMap) -> Result<(), (StatusCode, &'static str)> {
    let token_ok = headers.get("x-token").and_then(|t| t.to_str().ok()) == Some(state.token.as_str());
    if local_host(headers, state.port) && token_ok {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "forbidden"))
    }
}

async fn ask<T>(state: &AppState, make: impl FnOnce(oneshot::Sender<T>) -> ApiRequest) -> Result<T, (StatusCode, &'static str)> {
    let (tx, rx) = oneshot::channel();
    let _ = state.events.send(Event::Api(make(tx)));
    rx.await.map_err(|_| (StatusCode::SERVICE_UNAVAILABLE, "shutting down"))
}

async fn page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !local_host(&headers, state.port) {
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    Html(PAGE.replace("__TOKEN__", &state.token)).into_response()
}

async fn api_state(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = check(&state, &headers) {
        return r.into_response();
    }
    match ask(&state, ApiRequest::State).await {
        Ok(view) => Json(view).into_response(),
        Err(r) => r.into_response(),
    }
}

async fn api_layout(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<BTreeMap<String, Placement>>) -> Response {
    if let Err(r) = check(&state, &headers) {
        return r.into_response();
    }
    match ask(&state, |tx| ApiRequest::SetPlacements(body, tx)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(r) => r.into_response(),
    }
}

#[derive(Deserialize)]
struct SendBody {
    peer: String,
    paths: Vec<PathBuf>,
}

async fn api_send(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<SendBody>) -> Response {
    if let Err(r) = check(&state, &headers) {
        return r.into_response();
    }
    if let Some(bad) = body.paths.iter().find(|p| !p.exists()) {
        return (StatusCode::BAD_REQUEST, format!("not found: {}", bad.display())).into_response();
    }
    match ask(&state, |reply| ApiRequest::SendFiles { peer: body.peer, paths: body.paths, reply }).await {
        Ok(Ok(())) => StatusCode::ACCEPTED.into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, e).into_response(),
        Err(r) => r.into_response(),
    }
}

/// Minimal HTTP client for the CLI talking to the running daemon.
pub mod client {
    use super::TOKEN_FILE;
    use anyhow::{bail, Context, Result};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::path::Path;

    pub fn request(cfg_dir: &Path, port: u16, method: &str, path: &str, body: Option<&str>) -> Result<String> {
        let token = std::fs::read_to_string(cfg_dir.join(TOKEN_FILE)).context("MultiPC is not running (no api token)")?;
        let mut s = TcpStream::connect(("127.0.0.1", port)).context("MultiPC is not running")?;
        let body = body.unwrap_or("");
        write!(
            s,
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Token: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            token.trim(),
            body.len()
        )?;
        let mut resp = Vec::new();
        s.read_to_end(&mut resp)?;
        let split = resp.windows(4).position(|w| w == b"\r\n\r\n").map_or(resp.len(), |i| i + 4);
        let head = String::from_utf8_lossy(&resp[..split]).to_string();
        let raw = &resp[split..];
        let status: u16 = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { dechunk(raw) } else { raw.to_vec() };
        let body = String::from_utf8_lossy(&body).into_owned();
        if !(200..300).contains(&status) {
            bail!("{status}: {body}");
        }
        Ok(body)
    }

    fn dechunk(mut s: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(i) = s.windows(2).position(|w| w == b"\r\n") {
            let size = String::from_utf8_lossy(&s[..i]);
            let Ok(n) = usize::from_str_radix(size.trim(), 16) else { break };
            let rest = &s[i + 2..];
            if n == 0 || rest.len() < n {
                break;
            }
            out.extend_from_slice(&rest[..n]);
            s = rest[n..].strip_prefix(b"\r\n").unwrap_or(&rest[n..]);
        }
        out
    }
}
