use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use tokio::sync::mpsc;

use super::Cli;
use crate::config::{self, is_fika_installed, Config};
use crate::db::Database;
use crate::forge::client::ForgeClient;
use crate::logging;
use crate::spt::detect::{detect_spt_dir, read_spt_version};

pub async fn run(bind: Option<&str>, port: Option<u16>, cli: &Cli) -> Result<()> {
    let spt_dir = detect_spt_dir(cli.spt_dir.as_deref(), None)?;
    let spt_info = read_spt_version(&spt_dir)?;

    let config_path = Config::resolve_path(cli.config.as_deref(), Some(&spt_dir));
    let mut config = Config::load_with_env(&config_path)
        .with_context(|| format!("failed to load config from {}", config_path.display()))?;

    if let Some(b) = bind {
        config.web_bind = b.to_string();
    }
    if let Some(p) = port {
        config.web_port = p;
    }

    // Create LogBroadcast with configured buffer size
    let log_broadcast = Arc::new(logging::LogBroadcast::new(config.logging.web.buffer_size));

    // Step 1: create mpsc channel before subscriber
    let (log_tx, log_rx) = mpsc::unbounded_channel();

    // Step 2: init subscriber with sender
    let reload_handles = logging::init_subscriber(&log_broadcast, Some(log_tx));

    // Reconfigure logging now that config is loaded
    let filter =
        logging::resolve_log_filter(&config.logging, cli.verbose, cli.log_level.as_deref());

    let mut logging_config = config.logging.clone();
    if let Some(ref fmt) = cli.log_format {
        if let Ok(format) = fmt.parse::<config::ConsoleFormat>() {
            logging_config.console.format = format;
        }
    }

    reload_handles.reconfigure(&logging_config, &filter, Some(&spt_dir));

    config.ensure_session_secret();
    config
        .save(&config_path)
        .context("failed to save config with session secret")?;

    let db_path = spt_dir.join("quartermaster.db");
    let db = Database::open(&db_path)
        .with_context(|| format!("failed to open database at {}", db_path.display()))?;

    // Step 3: spawn LogWriter after DB is available
    let db_arc = Arc::new(Mutex::new(db));

    // Initialize log level counts cache from DB
    let log_level_counts: crate::logging::writer::LogLevelCounts = Arc::new(
        parking_lot::RwLock::new(db_arc.lock().log_counts_by_level().unwrap_or_default()),
    );

    let (_log_writer_handle, log_writer_shutdown) = crate::logging::writer::spawn(
        Arc::clone(&db_arc),
        log_rx,
        config.logging.web.retention_days,
        config.logging.web.max_entries,
        Arc::clone(&log_level_counts),
    );

    if !db_arc.lock().has_user_manager()? {
        anyhow::bail!("No admin user exists. Run `quma setup` first to create an admin account.");
    }

    // Take ownership of the core mods the compose stack handed over (QUMA_MANAGE_*).
    if let Err(e) = crate::adopt::adopt_core_mods(&db_arc.lock(), &spt_dir) {
        tracing::warn!(err = %e, "core mod adoption failed — mods stay unmanaged");
    }

    // Create ContainerManager if available
    let container_mgr = match crate::container::ContainerManager::new(config.container_stop_timeout)
    {
        Ok(mgr) => {
            let mgr = Arc::new(mgr);

            // Auto-start server container if configured
            if config.auto_start_server {
                if let Some(ref container) = config.server_container {
                    match mgr.is_running(container).await {
                        Ok(true) => {
                            tracing::info!(container, "server container already running");
                        }
                        Ok(false) => {
                            tracing::info!(container, "auto-starting server container");
                            if let Err(e) = mgr.start(container).await {
                                tracing::warn!(container, err = %e, "failed to auto-start server container — web UI will start anyway");
                            }
                        }
                        Err(e) => {
                            tracing::warn!(container, err = %e, "failed to check container status — skipping auto-start");
                        }
                    }
                }
            }

            Some(mgr)
        }
        Err(e) => {
            tracing::warn!(err = %e, "failed to connect to Podman — container features disabled");
            None
        }
    };

    let forge = ForgeClient::new()?;

    let fika_installed = is_fika_installed(&spt_dir);
    let modsync_installed = crate::config::is_modsync_installed(&spt_dir);
    let config_arc = Arc::new(parking_lot::RwLock::new(config));

    // Headless clients are owned by the compose stack, not by quma. There is no
    // supervisor and no convergence loop — the web layer reads their status
    // straight from the Fika API (see web::handlers::clients).
    let config = config_arc.read().clone();

    let on_exit = config.on_exit.clone();
    let teardown_mgr = container_mgr.clone();

    let server_future = crate::web::start_server(crate::web::ServerContext {
        config,
        config_handle: config_arc,
        config_path,
        db: db_arc,
        forge,
        spt_dir,
        spt_info,
        log_broadcast: Arc::clone(&log_broadcast),
        reload_handles: Arc::new(reload_handles),
        container_mgr,
        fika_installed,
        modsync_installed,
        log_level_counts: Arc::clone(&log_level_counts),
    });

    // Actix-web handles SIGINT/SIGTERM internally. For SIGHUP, we race
    // the server future against the signal — dropping the future triggers
    // actix's cleanup. When on_exit is Nothing, skip the signal listener
    // entirely so there's zero overhead.
    let server_result = if on_exit != crate::config::OnExit::Nothing {
        let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .expect("failed to register SIGHUP handler");
        tokio::select! {
            result = server_future => result,
            _ = sighup.recv() => {
                tracing::info!("received SIGHUP, shutting down");
                Ok(())
            }
        }
    } else {
        server_future.await
    };

    // Shutdown log writer before tearing down containers
    log_writer_shutdown.shutdown().await;

    if let Some(ref mgr) = teardown_mgr {
        teardown_containers(mgr, &on_exit).await;
    }

    server_result
}

async fn teardown_containers(
    container_mgr: &crate::container::ContainerManager,
    on_exit: &crate::config::OnExit,
) {
    use crate::config::OnExit;

    if *on_exit == OnExit::Nothing {
        return;
    }

    tracing::info!(mode = %on_exit, "tearing down managed containers");

    let containers = match container_mgr
        .detect_containers_by_label("managed-by", "quma")
        .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(err = %e, "failed to discover managed containers for teardown");
            return;
        }
    };

    if containers.is_empty() {
        tracing::debug!("no managed containers found");
        return;
    }

    for name in &containers {
        let result = match on_exit {
            OnExit::Stop => {
                tracing::info!(container = %name, "stopping container");
                container_mgr.stop(name).await
            }
            OnExit::Remove => {
                tracing::info!(container = %name, "removing container");
                container_mgr.remove_container(name).await
            }
            OnExit::Nothing => return,
        };
        if let Err(e) = result {
            tracing::warn!(container = %name, err = %e, "container teardown failed");
        }
    }

    tracing::info!(count = containers.len(), "container teardown complete");
}
