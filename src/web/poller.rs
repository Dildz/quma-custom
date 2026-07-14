//! Fika presence poller — the raid-stats collector.
//!
//! quma used to learn about raids by proxying the game client's
//! `POST /client/match/local/start|end`. That proxy is gone, so instead we ask
//! Fika who is online (`/fika/presence/get`) every few seconds and derive raid
//! start/end from players entering and leaving a map.
//!
//! Nothing else about raid stats changed — this feeds the same
//! `web::raid_tracker` handlers, which write the same tables.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::db::Database;
use crate::fika::client::FikaClient;
use crate::web::raid_tracker::{self, ProfileSnapshot};
use crate::web::sse::ServerEvent;

/// Fika's presence payload marks a scav raid with side 1 (0 = PMC).
const SIDE_SCAV: i32 = 1;

/// SPT writes the profile when the raid ends; the poller can notice the player
/// left before that write lands. Wait a beat so the "after" snapshot sees the
/// raid results rather than the pre-raid profile.
const PROFILE_SETTLE: std::time::Duration = std::time::Duration::from_secs(3);

/// A raid we saw start and are waiting to see end.
struct TrackedRaid {
    is_scav: bool,
    /// Profile as it was at raid start — the baseline the exit status and kills
    /// are diffed against.
    before: ProfileSnapshot,
}

pub struct PollerContext {
    pub fika: Arc<FikaClient>,
    pub db: Arc<parking_lot::Mutex<Database>>,
    pub events: tokio::sync::broadcast::Sender<ServerEvent>,
    pub spt_dir: PathBuf,
    pub snapshots_enabled: bool,
    pub interval_secs: u64,
}

/// Start the poller. No-op when Fika isn't available — raids simply aren't tracked.
pub fn spawn(ctx: Option<PollerContext>) {
    let Some(ctx) = ctx else {
        tracing::debug!("Fika not available — raid poller not started");
        return;
    };

    let interval = std::time::Duration::from_secs(ctx.interval_secs.max(1));
    tracing::info!(
        interval_secs = ctx.interval_secs,
        "raid poller started (Fika presence)"
    );

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut tracked: HashMap<String, TrackedRaid> = HashMap::new();

        loop {
            ticker.tick().await;
            poll_once(&ctx, &mut tracked).await;
        }
    });
}

/// One poll: diff who is in a raid now against who was in a raid last tick.
async fn poll_once(ctx: &PollerContext, tracked: &mut HashMap<String, TrackedRaid>) {
    let presence = match ctx.fika.presence().await {
        Ok(p) => p,
        Err(e) => {
            // The server being down is normal and not worth shouting about.
            tracing::debug!(err = %e, "Fika presence poll failed");
            return;
        }
    };

    // Who is on a map right now, and where.
    let mut in_raid_now: HashMap<String, (String, bool)> = HashMap::new();
    for player in &presence {
        if let Some(ref raid) = player.raid_information {
            let is_scav = raid.side == SIDE_SCAV;
            in_raid_now.insert(player.profile_id.clone(), (raid.location.clone(), is_scav));
        }
    }

    // Raids that ended: we were tracking them, and they are no longer on a map.
    // A player who disconnects mid-raid also lands here — they vanish from
    // presence entirely — which is what we want: close the raid out.
    let ended: Vec<String> = tracked
        .keys()
        .filter(|id| !in_raid_now.contains_key(*id))
        .cloned()
        .collect();

    if !ended.is_empty() {
        tokio::time::sleep(PROFILE_SETTLE).await;
    }

    for profile_id in ended {
        let Some(raid) = tracked.remove(&profile_id) else {
            continue;
        };
        let (db, events, spt_dir) = (ctx.db.clone(), ctx.events.clone(), ctx.spt_dir.clone());
        let snapshots_enabled = ctx.snapshots_enabled;
        let before = raid.before.clone();
        let is_scav = raid.is_scav;

        // Profile reads + DB writes are blocking.
        let _ = actix_web::web::block(move || {
            raid_tracker::handle_raid_end(
                &profile_id,
                is_scav,
                Some(&before),
                &spt_dir,
                &db,
                &events,
                snapshots_enabled,
            );
        })
        .await;
    }

    // Raids that started: on a map now, weren't last tick.
    for (profile_id, (map, is_scav)) in in_raid_now {
        if tracked.contains_key(&profile_id) {
            continue;
        }
        let (db, events, spt_dir) = (ctx.db.clone(), ctx.events.clone(), ctx.spt_dir.clone());
        let snapshots_enabled = ctx.snapshots_enabled;
        let id = profile_id.clone();

        let before = actix_web::web::block(move || {
            raid_tracker::handle_raid_start(
                &id,
                &map,
                is_scav,
                &spt_dir,
                &db,
                &events,
                snapshots_enabled,
            )
        })
        .await;

        // Only track raids we actually recorded — an unregistered player (or a
        // headless) returns None and is skipped, as it was under the proxy.
        if let Ok(Some(before)) = before {
            tracked.insert(profile_id, TrackedRaid { is_scav, before });
        }
    }
}
