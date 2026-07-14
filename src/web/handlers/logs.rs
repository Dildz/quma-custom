use std::convert::Infallible;
use std::time::Duration;

use actix_session::Session;
use actix_web::web::{Data, Html, Query};
use actix_web::{HttpRequest, HttpResponse};
use actix_web_lab::sse;
use askama::Template;
use serde::{Deserialize, Serialize};

use crate::db::logs::{LogQuery as DbLogQuery, StoredLogEntry};
use crate::db::rbac::Permission;
use crate::web::auth::{require_auth, require_permission, SessionUser};
use crate::web::error::WebError;
use crate::web::flash::{take_flash, FlashMessage};
use crate::web::nav::NavContext;
use crate::web::state::AppState;

// Query struct for server container logs — simple limit-only
#[derive(Deserialize)]
pub struct ServerLogQuery {
    limit: Option<usize>,
}

// Query struct for app logs — supports filtering and cursor pagination
#[derive(Deserialize)]
pub struct AppLogQuery {
    level: Option<String>,
    target: Option<String>,
    q: Option<String>,
    before: Option<i64>,
    limit: Option<usize>,
}

// Response for app logs JSON endpoint
#[derive(Serialize)]
pub struct AppLogResponse {
    entries: Vec<StoredLogEntry>,
    has_more: bool,
}

// Query structs for headless client logs

#[derive(Deserialize)]
pub struct HeadlessLogQuery {
    container: String,
    limit: Option<usize>,
}

#[derive(Deserialize)]
pub struct HeadlessStreamQuery {
    container: String,
}

// ---------------------------------------------------------------------------
// App log endpoints — backed by SQLite database with server-side filtering
// ---------------------------------------------------------------------------

pub async fn app_logs_json(
    state: Data<AppState>,
    req: HttpRequest,
    query: Query<AppLogQuery>,
) -> actix_web::Result<HttpResponse> {
    let user = require_auth(&req)?;
    require_permission(&user, Permission::ServerLogs)?;

    let limit = query.limit.unwrap_or(100).min(1000);
    let db_query = DbLogQuery {
        level: query.level.clone(),
        target: query.target.clone(),
        search: query.q.clone(),
        before: query.before,
        limit: limit + 1, // fetch one extra to detect has_more
    };

    let db = state.db.clone();
    let mut entries = actix_web::web::block(move || {
        let db = db.lock();
        db.query_logs(&db_query)
    })
    .await
    .map_err(|e| WebError::Internal(anyhow::anyhow!("{e}")))?
    .map_err(WebError::from)?;

    let has_more = entries.len() > limit;
    entries.truncate(limit);

    Ok(HttpResponse::Ok().json(AppLogResponse { entries, has_more }))
}

pub async fn app_logs_count(
    state: Data<AppState>,
    req: HttpRequest,
) -> actix_web::Result<HttpResponse> {
    let user = require_auth(&req)?;
    require_permission(&user, Permission::ServerLogs)?;

    let counts = state.log_level_counts.read().clone();
    Ok(HttpResponse::Ok().json(counts))
}

pub async fn app_logs_stream(
    state: Data<AppState>,
    req: HttpRequest,
) -> actix_web::Result<sse::Sse<impl futures_util::Stream<Item = Result<sse::Event, Infallible>>>> {
    let user = require_auth(&req)?;
    require_permission(&user, Permission::ServerLogs)?;
    let mut rx = state.log_broadcast.subscribe();

    let (tx, channel_rx) = tokio::sync::mpsc::channel::<sse::Event>(64);

    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            let entry = match rx.recv().await {
                Ok(e) => e,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            };

            let mut batch = vec![entry];
            while batch.len() < 50 {
                match rx.try_recv() {
                    Ok(e) => batch.push(e),
                    Err(_) => break,
                }
            }

            for entry in batch {
                if let Ok(json) = serde_json::to_string(&entry) {
                    if tx
                        .send(sse::Event::Data(sse::Data::new(json)))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(channel_rx);
    Ok(sse::Sse::from_infallible_stream(stream).with_keep_alive(Duration::from_secs(15)))
}

// ---------------------------------------------------------------------------
// Server (container) log endpoints — read through the Docker API (bollard)
// ---------------------------------------------------------------------------

/// Collect the last `tail` lines of a container's logs via the Docker API.
///
/// This used to shell out to `podman logs` — which never worked in the quma
/// container (no podman binary, and the stack is Docker anyway). We already
/// speak the Docker Engine API through bollard, so use it.
async fn collect_container_logs(
    mgr: &crate::container::ContainerManager,
    container: &str,
    tail: usize,
) -> Result<Vec<String>, WebError> {
    use futures_util::StreamExt;

    let mut stream = mgr.log_stream(container, tail, false);
    let mut lines = Vec::new();

    let collect = async {
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(out) => {
                    if let Some(line) = log_output_line(&out) {
                        lines.push(line);
                    }
                }
                Err(e) => return Err(WebError::Internal(anyhow::anyhow!("docker logs: {e}"))),
            }
        }
        Ok(())
    };

    tokio::time::timeout(Duration::from_secs(10), collect)
        .await
        .map_err(|_| {
            WebError::Internal(anyhow::anyhow!(
                "docker logs timed out (log may be very large)"
            ))
        })??;

    Ok(lines)
}

/// Follow a container's logs as an SSE stream via the Docker API.
fn stream_container_logs(
    mgr: std::sync::Arc<crate::container::ContainerManager>,
    container: String,
) -> sse::Sse<impl futures_util::Stream<Item = Result<sse::Event, Infallible>>> {
    use futures_util::StreamExt;

    let (tx, rx) = tokio::sync::mpsc::channel::<sse::Event>(64);

    tokio::spawn(async move {
        let mut stream = mgr.log_stream(&container, 0, true);
        loop {
            tokio::select! {
                chunk = stream.next() => {
                    let Some(chunk) = chunk else { break };
                    match chunk {
                        Ok(out) => {
                            if let Some(line) = log_output_line(&out) {
                                if tx.send(sse::Event::Data(sse::Data::new(line))).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx
                                .send(sse::Event::Data(
                                    sse::Data::new(format!("error: {e}")).event("error"),
                                ))
                                .await;
                            break;
                        }
                    }
                }
                // The client went away — stop following.
                _ = tx.closed() => break,
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    sse::Sse::from_infallible_stream(stream).with_keep_alive(Duration::from_secs(15))
}

fn log_output_line(out: &bollard::container::LogOutput) -> Option<String> {
    let bytes = match out {
        bollard::container::LogOutput::StdOut { message }
        | bollard::container::LogOutput::StdErr { message }
        | bollard::container::LogOutput::Console { message } => message,
        bollard::container::LogOutput::StdIn { .. } => return None,
    };
    let line = String::from_utf8_lossy(bytes).trim_end().to_string();
    (!line.is_empty()).then_some(line)
}

pub async fn server_logs_json(
    state: Data<AppState>,
    req: HttpRequest,
    query: Query<ServerLogQuery>,
) -> actix_web::Result<HttpResponse> {
    let user = require_auth(&req)?;
    require_permission(&user, Permission::ServerLogs)?;

    let mgr = state.container_mgr.as_ref().ok_or(WebError::NotFound)?;
    let container = state
        .config()
        .server_container
        .clone()
        .ok_or(WebError::NotFound)?;
    let tail = query.limit.unwrap_or(100).min(10000);

    let lines = collect_container_logs(mgr, &container, tail).await?;
    Ok(HttpResponse::Ok().json(lines))
}

pub async fn server_logs_stream(
    state: Data<AppState>,
    req: HttpRequest,
) -> actix_web::Result<sse::Sse<impl futures_util::Stream<Item = Result<sse::Event, Infallible>>>> {
    let user = require_auth(&req)?;
    require_permission(&user, Permission::ServerLogs)?;

    let mgr = state.container_mgr.clone().ok_or(WebError::NotFound)?;
    let container = state
        .config()
        .server_container
        .clone()
        .ok_or(WebError::NotFound)?;

    Ok(stream_container_logs(mgr, container))
}

// ---------------------------------------------------------------------------
// Headless container log endpoints
//
// The headless is a compose-managed container named in config. It used to be
// discovered by a Docker label that only quma-CREATED containers carried, so it
// could never find a compose-managed one; and the name had to match
// `fika-headless-<number>`, which "fika-headless-4.0" does not. Both are gone:
// the configured name is the only headless quma knows about, and requiring an
// exact match with it also removes the injection surface the old validator
// existed to guard.
// ---------------------------------------------------------------------------

fn headless_container(state: &AppState) -> Result<String, WebError> {
    state
        .config()
        .headless_container
        .clone()
        .ok_or(WebError::NotFound)
}

pub async fn headless_containers(
    state: Data<AppState>,
    req: HttpRequest,
) -> actix_web::Result<HttpResponse> {
    let user = require_auth(&req)?;
    require_permission(&user, Permission::ServerLogs)?;

    // Zero or one — quma monitors exactly the headless the compose stack names.
    let names: Vec<String> = state
        .config()
        .headless_container
        .clone()
        .into_iter()
        .collect();
    Ok(HttpResponse::Ok().json(names))
}

pub async fn headless_logs_json(
    state: Data<AppState>,
    req: HttpRequest,
    query: Query<HeadlessLogQuery>,
) -> actix_web::Result<HttpResponse> {
    let user = require_auth(&req)?;
    require_permission(&user, Permission::ServerLogs)?;

    let mgr = state.container_mgr.as_ref().ok_or(WebError::NotFound)?;
    let container = headless_container(&state)?;
    if query.container != container {
        return Err(WebError::BadRequest("unknown headless container".into()).into());
    }
    let tail = query.limit.unwrap_or(100).min(10000);

    let lines = collect_container_logs(mgr, &container, tail).await?;
    Ok(HttpResponse::Ok().json(lines))
}

pub async fn headless_logs_stream(
    state: Data<AppState>,
    req: HttpRequest,
    query: Query<HeadlessStreamQuery>,
) -> actix_web::Result<sse::Sse<impl futures_util::Stream<Item = Result<sse::Event, Infallible>>>> {
    let user = require_auth(&req)?;
    require_permission(&user, Permission::ServerLogs)?;

    let mgr = state.container_mgr.clone().ok_or(WebError::NotFound)?;
    let container = headless_container(&state)?;
    if query.container != container {
        return Err(WebError::BadRequest("unknown headless container".into()).into());
    }

    Ok(stream_container_logs(mgr, container))
}

#[derive(Template)]
#[template(path = "logs.html")]
struct LogsTemplate {
    user: SessionUser,
    flash: Option<FlashMessage>,
    csrf_token: String,
    nav: NavContext,
}

pub async fn logs_page(
    state: Data<AppState>,
    req: HttpRequest,
    session: Session,
) -> actix_web::Result<Html> {
    let user = require_auth(&req)?;
    require_permission(&user, Permission::ServerLogs)?;
    let flash = take_flash(&session);
    let csrf_token = crate::web::csrf::get_or_create_token(&session);

    let tmpl = LogsTemplate {
        user,
        flash,
        csrf_token,
        nav: NavContext::from_state(&state),
    };
    Ok(Html::new(tmpl.render().map_err(WebError::from)?))
}
