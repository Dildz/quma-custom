//! Headless monitoring.
//!
//! quma does NOT own the headless clients — the compose stack does. This module
//! only *observes* them (Fika API for status/players, Docker for container state)
//! and offers lifecycle control (start/stop/restart) over the one compose-managed
//! headless container named by `headless_container` in config.
//!
//! The old converge/supervisor subsystem that created and scaled quma-owned
//! headless containers is gone; there is deliberately no way to spawn one here.

use std::collections::HashMap;
use std::sync::Arc;

use actix_session::Session;
use actix_web::web::{Data, Form, Path};
use actix_web::{HttpRequest, HttpResponse};
use askama::Template;

use crate::container::ContainerManager;
use crate::db::rbac::Permission;
use crate::spt::headless::EHeadlessStatus;
use crate::web::auth::{require_auth, require_permission, SessionUser};
use crate::web::error::WebError;
use crate::web::flash::{set_flash, take_flash, FlashMessage, FlashType};
use crate::web::nav::NavContext;
use crate::web::state::AppState;

#[allow(unused_imports)]
mod filters {
    pub use crate::web::template_filters::*;
}

#[derive(Clone)]
pub struct PlayerInfo {
    pub name: String,
}

/// A headless client as reported by the Fika server (`/fika/headless/get`),
/// keyed by its SPT profile ID. This is the source of truth for headless status
/// now that the supervisor is gone.
pub struct HeadlessClientView {
    pub profile_id: String,
    pub alias: Option<String>,
    pub status: String,
    pub ready: bool,
    pub in_raid: bool,
    pub level: i32,
    pub players: Vec<PlayerInfo>,
}

/// The compose-managed headless container, as seen by Docker.
pub struct HeadlessContainerView {
    pub name: String,
    pub running: bool,
    pub started_at: Option<String>,
}

#[derive(Template)]
#[template(path = "headless.html")]
struct HeadlessPageTemplate {
    user: SessionUser,
    flash: Option<FlashMessage>,
    csrf_token: String,
    nav: NavContext,
    container: Option<HeadlessContainerView>,
    clients: Vec<HeadlessClientView>,
    fika_available: bool,
}

#[derive(Template)]
#[template(path = "clients/partials/status.html")]
struct HeadlessStatusPartialTemplate {
    user: SessionUser,
    csrf_token: String,
    container: Option<HeadlessContainerView>,
    clients: Vec<HeadlessClientView>,
    fika_available: bool,
}

#[derive(Template)]
#[template(path = "partials/dashboard_clients_status.html")]
struct DashboardClientsStatusTemplate {
    container: Option<HeadlessContainerView>,
    clients: Vec<HeadlessClientView>,
}

#[derive(serde::Deserialize)]
pub struct StartRaidForm {
    csrf_token: String,
    location_id: String,
    time: i32,
    use_event: Option<String>,
}

fn build_profile_names(spt_dir: &std::path::Path) -> HashMap<String, String> {
    crate::spt::profiles::list_profiles(spt_dir)
        .unwrap_or_default()
        .into_iter()
        .map(|p| (p.aid, p.username))
        .collect()
}

fn build_client_aliases(spt_dir: &std::path::Path) -> HashMap<String, String> {
    let path = crate::fika::config::fika_config_path(spt_dir);
    crate::fika::config::read_fika_config(&path)
        .map(|c| c.headless.profiles.aliases)
        .unwrap_or_default()
}

fn redirect_headless() -> HttpResponse {
    HttpResponse::SeeOther()
        .insert_header(("Location", "/quma/headless"))
        .finish()
}

fn require_container_mgr<'a>(
    state: &'a AppState,
    session: &Session,
) -> Result<&'a Arc<ContainerManager>, HttpResponse> {
    state.container_mgr.as_ref().ok_or_else(|| {
        set_flash(
            session,
            "Docker socket not available — quma cannot control the headless container.",
            FlashType::Error,
        );
        redirect_headless()
    })
}

/// Docker's view of the compose-managed headless container. `None` when no
/// `headless_container` is configured — a valid setup: plenty of servers
/// (ARM hosts, solo servers) run no headless at all.
async fn fetch_container(state: &AppState) -> Option<HeadlessContainerView> {
    let name = state.config().headless_container.clone()?;
    let mgr = state.container_mgr.as_ref()?;

    let running = mgr.is_running(&name).await.unwrap_or(false);
    let started_at = if running {
        mgr.container_started_at(&name)
            .await
            .ok()
            .flatten()
            .and_then(|s| crate::container::filter_started_at(Some(s)))
    } else {
        None
    };

    Some(HeadlessContainerView {
        name,
        running,
        started_at,
    })
}

/// Fika's view of every connected headless client. An empty vec means either no
/// headless is connected or the SPT server is unreachable — both render the same.
async fn fetch_clients(state: &AppState) -> Vec<HeadlessClientView> {
    let (host, port) = {
        let config = state.config();
        crate::server_detect::resolve_server_addr(&config, &state.spt_dir)
    };

    let spt = match crate::spt::server::SptClient::new(&host, port) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(err = %e, "failed to build SPT client for headless status");
            return Vec::new();
        }
    };

    let resp = match spt.headless_clients().await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(err = %e, "Fika headless endpoint unreachable");
            return Vec::new();
        }
    };

    let names = build_profile_names(&state.spt_dir);
    let aliases = build_client_aliases(&state.spt_dir);

    let mut clients: Vec<HeadlessClientView> = resp
        .headlesses
        .into_iter()
        .map(|(profile_id, info)| {
            let (status, ready, in_raid) = match info.state {
                EHeadlessStatus::Ready => ("Ready".to_string(), true, false),
                EHeadlessStatus::InRaid => ("In Raid".to_string(), false, true),
                EHeadlessStatus::Unknown(ref v) => (format!("Unknown ({v})"), false, false),
            };
            let players = info
                .players
                .iter()
                .map(|id| PlayerInfo {
                    name: names.get(id).cloned().unwrap_or_else(|| id.clone()),
                })
                .collect();
            HeadlessClientView {
                alias: aliases.get(&profile_id).cloned(),
                profile_id,
                status,
                ready,
                in_raid,
                level: info.level,
                players,
            }
        })
        .collect();

    // Stable ordering — HashMap iteration order is not.
    clients.sort_by(|a, b| a.profile_id.cmp(&b.profile_id));
    clients
}

pub async fn headless_page(
    state: Data<AppState>,
    req: HttpRequest,
    session: Session,
) -> actix_web::Result<HttpResponse> {
    let user = require_auth(&req)?;
    require_permission(&user, Permission::SettingsManage)?;
    let flash = take_flash(&session);
    let csrf_token = crate::web::csrf::get_or_create_token(&session);

    let tmpl = HeadlessPageTemplate {
        user,
        flash,
        csrf_token,
        nav: NavContext::from_state(&state),
        container: fetch_container(&state).await,
        clients: fetch_clients(&state).await,
        fika_available: state.fika_client.is_some(),
    };
    Ok(HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(tmpl.render().map_err(WebError::from)?))
}

pub async fn headless_status_partial(
    state: Data<AppState>,
    req: HttpRequest,
    session: Session,
) -> actix_web::Result<HttpResponse> {
    let user = require_auth(&req)?;
    require_permission(&user, Permission::SettingsManage)?;
    let csrf_token = crate::web::csrf::get_or_create_token(&session);

    let tmpl = HeadlessStatusPartialTemplate {
        user,
        csrf_token,
        container: fetch_container(&state).await,
        clients: fetch_clients(&state).await,
        fika_available: state.fika_client.is_some(),
    };
    Ok(HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(tmpl.render().map_err(WebError::from)?))
}

pub async fn dashboard_headless_status(
    state: Data<AppState>,
    req: HttpRequest,
) -> actix_web::Result<HttpResponse> {
    require_auth(&req)?;

    let tmpl = DashboardClientsStatusTemplate {
        container: fetch_container(&state).await,
        clients: fetch_clients(&state).await,
    };
    Ok(HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(tmpl.render().map_err(WebError::from)?))
}

/// Start/stop/restart the compose-managed headless container: plain Docker by
/// container name. No supervisor, no convergence, no container creation.
async fn headless_lifecycle(
    state: &Data<AppState>,
    req: &HttpRequest,
    session: &Session,
    csrf_token: &str,
    action: &str,
) -> actix_web::Result<HttpResponse> {
    let user = require_auth(req)?;
    require_permission(&user, Permission::HeadlessManage)?;

    if !crate::web::csrf::validate_token(session, csrf_token) {
        return Err(WebError::Forbidden.into());
    }

    let mgr = match require_container_mgr(state, session) {
        Ok(m) => m,
        Err(resp) => return Ok(resp),
    };

    let Some(name) = state.config().headless_container.clone() else {
        set_flash(
            session,
            "No headless container configured (set headless_container / QUMA_HEADLESS_CONTAINER).",
            FlashType::Error,
        );
        return Ok(redirect_headless());
    };

    let result = match action {
        "start" => mgr.start(&name).await,
        "stop" => mgr.stop(&name).await,
        "restart" => mgr.restart(&name).await,
        _ => unreachable!("unknown headless lifecycle action"),
    };

    match result {
        Err(e) => {
            tracing::error!(container = %name, action, err = %e, "headless lifecycle action failed");
            set_flash(
                session,
                &format!("Failed to {action} headless container: {e}"),
                FlashType::Error,
            );
        }
        Ok(()) => {
            tracing::info!(container = %name, action, "headless lifecycle action succeeded");
            let verb = match action {
                "start" => "starting",
                "stop" => "stopped",
                "restart" => "restarting",
                _ => action,
            };
            set_flash(
                session,
                &format!("Headless container {verb}"),
                FlashType::Success,
            );
        }
    }

    Ok(redirect_headless())
}

pub async fn headless_start(
    state: Data<AppState>,
    req: HttpRequest,
    session: Session,
    form: Form<crate::web::csrf::CsrfForm>,
) -> actix_web::Result<HttpResponse> {
    headless_lifecycle(&state, &req, &session, &form.csrf_token, "start").await
}

pub async fn headless_stop(
    state: Data<AppState>,
    req: HttpRequest,
    session: Session,
    form: Form<crate::web::csrf::CsrfForm>,
) -> actix_web::Result<HttpResponse> {
    headless_lifecycle(&state, &req, &session, &form.csrf_token, "stop").await
}

pub async fn headless_restart(
    state: Data<AppState>,
    req: HttpRequest,
    session: Session,
    form: Form<crate::web::csrf::CsrfForm>,
) -> actix_web::Result<HttpResponse> {
    headless_lifecycle(&state, &req, &session, &form.csrf_token, "restart").await
}

/// Ask a headless client to shut down cleanly via the Fika API. Fika brings it
/// back on its own — we deliberately do not touch the container here.
pub async fn headless_graceful_restart(
    state: Data<AppState>,
    req: HttpRequest,
    session: Session,
    path: Path<String>,
    form: Form<crate::web::csrf::CsrfForm>,
) -> actix_web::Result<HttpResponse> {
    let user = require_auth(&req)?;
    require_permission(&user, Permission::HeadlessManage)?;

    if !crate::web::csrf::validate_token(&session, &form.csrf_token) {
        return Err(WebError::Forbidden.into());
    }

    let profile_id = path.into_inner();

    let Some(fika_client) = state.fika_client.clone() else {
        set_flash(&session, "Fika integration not available", FlashType::Error);
        return Ok(redirect_headless());
    };

    match fika_client.shutdown_headless(&profile_id).await {
        Ok(()) => {
            tracing::info!(profile_id = %profile_id, "requested graceful headless restart");
            set_flash(
                &session,
                "Graceful restart requested — the headless will reconnect shortly.",
                FlashType::Success,
            );
        }
        Err(e) => {
            tracing::error!(profile_id = %profile_id, err = %e, "graceful headless restart failed");
            set_flash(
                &session,
                &format!("Graceful restart failed: {e}"),
                FlashType::Error,
            );
        }
    }

    Ok(redirect_headless())
}

/// Trigger a raid on a READY headless client via the Fika API.
pub async fn headless_start_raid(
    state: Data<AppState>,
    req: HttpRequest,
    session: Session,
    path: Path<String>,
    form: Form<StartRaidForm>,
) -> actix_web::Result<HttpResponse> {
    let user = require_auth(&req)?;
    require_permission(&user, Permission::HeadlessManage)?;

    if !crate::web::csrf::validate_token(&session, &form.csrf_token) {
        return Err(WebError::Forbidden.into());
    }

    let profile_id = path.into_inner();

    let Some(fika_client) = state.fika_client.clone() else {
        set_flash(&session, "Fika integration not available", FlashType::Error);
        return Ok(redirect_headless());
    };

    let req_body = crate::fika::client::StartHeadlessRaidRequest {
        headless_session_id: profile_id.clone(),
        location_id: form.location_id.clone(),
        time: form.time,
        time_and_weather_settings: None,
        use_event: form.use_event.is_some(),
        side: 0,
        spawn_place: 0,
        metabolism_disabled: false,
        bot_settings: None,
        waves_settings: None,
        custom_raid_settings: None,
    };

    match fika_client.start_headless_raid(&req_body).await {
        Ok(resp) => match resp.error {
            Some(err) => set_flash(
                &session,
                &format!("Start raid failed: {err}"),
                FlashType::Error,
            ),
            None => set_flash(&session, "Raid starting", FlashType::Success),
        },
        Err(e) => set_flash(
            &session,
            &format!("Start raid failed: {e}"),
            FlashType::Error,
        ),
    }

    Ok(redirect_headless())
}
