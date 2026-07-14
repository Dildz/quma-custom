//! Raid stats collection.
//!
//! Raids used to be recorded by intercepting `POST /client/match/local/start|end`
//! in quma's built-in proxy. That proxy was removed, so the collector is now
//! driven by the Fika presence poller (`crate::web::poller`), which detects raid
//! start/end by watching players enter and leave a map.
//!
//! Everything downstream — the `raids` tables, the `/raids` pages — is unchanged.
//! Only the data source swapped: instead of reading the client's request bodies,
//! we read the SPT profile from disk (quma has the mount) and diff it.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::db::raids::{compress_snapshot, NewRaidKill};
use crate::db::Database;
use crate::web::sse::ServerEvent;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct VictimEntry {
    pub name: Option<String>,
    pub side: Option<String>,
    pub role: Option<String>,
    pub weapon: Option<String>,
    pub distance: Option<f64>,
    pub body_part: Option<String>,
    pub time: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ProfileSnapshot {
    pub xp: i64,
    pub level: i64,
    pub victim_count: i64,
    pub faction: Option<String>,
    /// `Stats.Eft.OverallCounters` entries keyed as `ExitStatus/Survived/Pmc`.
    /// Diffing these across a raid is how we recover the exit status — the
    /// proxy used to get it handed to it in the raid-end request body.
    #[serde(skip)]
    pub exit_counters: HashMap<String, i64>,
    /// `Stats.Eft.Aggressor` — who killed the player, when they died.
    #[serde(skip)]
    pub killer: Option<Killer>,
}

#[derive(Debug, Clone)]
pub struct Killer {
    pub profile_id: Option<String>,
    pub account_id: Option<String>,
}

/// Read the on-disk SPT profile and pull out the raid-relevant bits.
/// Returns the parsed snapshot plus the raw bytes (for the stored snapshot blob).
pub fn snapshot_profile(
    spt_dir: &Path,
    profile_id: &str,
    is_scav: bool,
) -> Option<(ProfileSnapshot, Vec<u8>)> {
    let path = spt_dir
        .join("SPT/user/profiles")
        .join(format!("{profile_id}.json"));

    let contents = std::fs::read(&path).ok()?;
    let parsed: serde_json::Value = serde_json::from_slice(&contents).ok()?;

    let character = if is_scav {
        // SPT stores the scav character under `characters.scav`. The old code
        // looked for `savage`/`Savage`, which does not exist in any SPT profile
        // — so every scav raid failed to snapshot and was silently dropped.
        // `savage` is kept as a fallback purely in case an older SPT used it.
        parsed
            .pointer("/characters/scav")
            .or_else(|| parsed.pointer("/characters/savage"))
            .or_else(|| parsed.pointer("/characters/Savage"))?
    } else {
        parsed.pointer("/characters/pmc")?
    };

    let xp = character
        .pointer("/Info/Experience")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let level = character
        .pointer("/Info/Level")
        .and_then(|v| v.as_i64())
        .unwrap_or(1);

    let victims = character
        .pointer("/Stats/Eft/Victims")
        .and_then(|v| v.as_array())
        .map(|arr| arr.len() as i64)
        .unwrap_or(0);

    let faction = if is_scav {
        None
    } else {
        character
            .pointer("/Info/Side")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };

    let exit_counters = read_exit_counters(character);
    let killer = read_killer(character);

    Some((
        ProfileSnapshot {
            xp,
            level,
            victim_count: victims,
            faction,
            exit_counters,
            killer,
        },
        contents,
    ))
}

/// Pull `ExitStatus/*` counters out of `Stats.Eft.OverallCounters`.
/// Shape: `{"Items": [{"Key": ["ExitStatus","Survived","Pmc"], "Value": 49}, ...]}`
fn read_exit_counters(character: &serde_json::Value) -> HashMap<String, i64> {
    let mut counters = HashMap::new();
    let Some(items) = character
        .pointer("/Stats/Eft/OverallCounters/Items")
        .and_then(|v| v.as_array())
    else {
        return counters;
    };

    for item in items {
        let Some(key_parts) = item.get("Key").and_then(|v| v.as_array()) else {
            continue;
        };
        let parts: Vec<&str> = key_parts.iter().filter_map(|p| p.as_str()).collect();
        if parts.first() != Some(&"ExitStatus") {
            continue;
        }
        let value = item.get("Value").and_then(|v| v.as_i64()).unwrap_or(0);
        counters.insert(parts.join("/"), value);
    }
    counters
}

fn read_killer(character: &serde_json::Value) -> Option<Killer> {
    let aggressor = character.pointer("/Stats/Eft/Aggressor")?;
    if aggressor.is_null() {
        return None;
    }
    Some(Killer {
        profile_id: aggressor
            .get("ProfileId")
            .and_then(|v| v.as_str())
            .map(String::from),
        account_id: aggressor
            .get("AccountId")
            .and_then(|v| v.as_str())
            .map(String::from),
    })
}

/// Which `ExitStatus/*` counter went up over the raid? That is the exit status.
/// `None` when nothing moved (or we have no "before" counters — e.g. quma
/// restarted mid-raid), in which case the raid is recorded with an unknown result.
pub fn diff_exit_status(before: &HashMap<String, i64>, after: &HashMap<String, i64>) -> Option<String> {
    after
        .iter()
        .filter(|(key, after_val)| {
            let before_val = before.get(*key).copied().unwrap_or(0);
            **after_val > before_val
        })
        // Key is `ExitStatus/<status>/<side>` — the status is the middle segment.
        .filter_map(|(key, _)| key.split('/').nth(1))
        .next()
        .map(String::from)
}

/// Raid start, as detected by the poller: a player appeared on a map.
#[allow(clippy::too_many_arguments)]
pub fn handle_raid_start(
    spt_profile_id: &str,
    map: &str,
    is_scav: bool,
    spt_dir: &Path,
    db: &Arc<parking_lot::Mutex<Database>>,
    events: &tokio::sync::broadcast::Sender<ServerEvent>,
    snapshots_enabled: bool,
) -> Option<ProfileSnapshot> {
    let player_side = if is_scav { "Savage" } else { "Pmc" };

    let (snapshot, profile_bytes) = match snapshot_profile(spt_dir, spt_profile_id, is_scav) {
        Some(pair) => pair,
        None => {
            tracing::warn!(profile_id = %spt_profile_id, is_scav, "failed to snapshot profile for raid start");
            return None;
        }
    };

    let compressed_snapshot = if snapshots_enabled {
        compress_snapshot(&profile_bytes).ok()
    } else {
        None
    };

    let started_at = chrono::Utc::now().to_rfc3339();
    let db_lock = db.lock();

    let user = match db_lock.get_user_by_spt_profile_id(spt_profile_id) {
        Ok(Some(u)) => u,
        Ok(None) => {
            tracing::debug!(profile_id = %spt_profile_id, "raid start for unregistered user — not tracked");
            return None;
        }
        Err(e) => {
            tracing::warn!(err = %e, profile_id = %spt_profile_id, "failed to query user by profile ID");
            return None;
        }
    };

    if let Err(e) = db_lock.close_orphaned_raids(spt_profile_id) {
        tracing::warn!(err = %e, profile_id = %spt_profile_id, "failed to close orphaned raids");
    }

    let raid_id = match db_lock.insert_raid(
        user.id,
        spt_profile_id,
        None,
        player_side,
        snapshot.faction.as_deref(),
        map,
        None,
        &started_at,
        Some(snapshot.xp),
        Some(snapshot.level),
        Some(snapshot.victim_count),
    ) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(err = %e, profile_id = %spt_profile_id, "failed to insert raid");
            return None;
        }
    };

    if let Some(ref compressed) = compressed_snapshot {
        if let Err(e) = db_lock.insert_raid_snapshot(raid_id, "before", compressed) {
            tracing::warn!(err = %e, raid_id, "failed to store before profile snapshot");
        }
    }

    drop(db_lock);

    tracing::info!(
        raid_id,
        profile_id = %spt_profile_id,
        username = %user.username,
        map = %map,
        player_side,
        "raid started"
    );

    let _ = events.send(ServerEvent::RaidStarted);
    Some(snapshot)
}

/// Raid end, as detected by the poller: the player left the map. The profile on
/// disk has by now been written by SPT with the raid results, so we re-read it
/// and diff against the snapshot taken at raid start.
pub fn handle_raid_end(
    spt_profile_id: &str,
    is_scav: bool,
    before: Option<&ProfileSnapshot>,
    spt_dir: &Path,
    db: &Arc<parking_lot::Mutex<Database>>,
    events: &tokio::sync::broadcast::Sender<ServerEvent>,
    snapshots_enabled: bool,
) {
    let (after, profile_bytes) = match snapshot_profile(spt_dir, spt_profile_id, is_scav) {
        Some(pair) => pair,
        None => {
            tracing::warn!(profile_id = %spt_profile_id, "failed to read profile for raid end");
            return;
        }
    };

    let db_lock = db.lock();

    let open_raid = match db_lock.find_open_raid(spt_profile_id) {
        Ok(Some(raid)) => raid,
        Ok(None) => {
            tracing::debug!(profile_id = %spt_profile_id, "raid end with no open raid — ignoring");
            return;
        }
        Err(e) => {
            tracing::warn!(err = %e, profile_id = %spt_profile_id, "failed to query open raid");
            return;
        }
    };

    // Kills = the victims added since the raid started. victim_count_before is
    // read back from the raid row, so this survives a quma restart mid-raid.
    let victims = read_victims(spt_dir, spt_profile_id, is_scav);
    let victim_count_before = open_raid.victim_count_before.unwrap_or(0).max(0) as usize;
    let victim_count_before = victim_count_before.min(victims.len());
    let new_victims: Vec<NewRaidKill> = victims
        .iter()
        .skip(victim_count_before)
        .map(|v| NewRaidKill {
            victim_name: v.name.clone(),
            victim_side: v.side.clone(),
            victim_role: v.role.clone(),
            weapon: v.weapon.clone(),
            distance: v.distance,
            body_part: v.body_part.clone(),
            kill_time: v.time.clone(),
        })
        .collect();

    // Exit status from the counter diff. Without a "before" (quma restarted
    // mid-raid) we cannot tell what happened — record it as Unknown rather than
    // guessing.
    let exit_status = before
        .and_then(|b| diff_exit_status(&b.exit_counters, &after.exit_counters))
        .unwrap_or_else(|| "Unknown".to_string());

    let (killer_id, killer_aid) = match after.killer {
        Some(ref k) if exit_status == "Killed" => (k.profile_id.clone(), k.account_id.clone()),
        _ => (None, None),
    };

    let ended_at = chrono::Utc::now();
    let play_time = chrono::DateTime::parse_from_rfc3339(&open_raid.started_at)
        .ok()
        .map(|start| (ended_at - start.with_timezone(&chrono::Utc)).num_seconds())
        .filter(|secs| *secs >= 0);

    if let Err(e) = db_lock.finish_raid_with_kills(
        open_raid.id,
        &ended_at.to_rfc3339(),
        play_time,
        &exit_status,
        None,
        killer_id.as_deref(),
        killer_aid.as_deref(),
        Some(after.xp),
        Some(after.level),
        &new_victims,
    ) {
        tracing::warn!(err = %e, raid_id = open_raid.id, "failed to finish raid");
        return;
    }

    if snapshots_enabled {
        if let Ok(compressed) = compress_snapshot(&profile_bytes) {
            if let Err(e) = db_lock.insert_raid_snapshot(open_raid.id, "after", &compressed) {
                tracing::warn!(err = %e, raid_id = open_raid.id, "failed to store after profile snapshot");
            }
        }
    }

    drop(db_lock);

    tracing::info!(
        raid_id = open_raid.id,
        profile_id = %spt_profile_id,
        exit_status = %exit_status,
        kills = new_victims.len(),
        play_time_secs = play_time.unwrap_or(0),
        "raid ended"
    );

    let _ = events.send(ServerEvent::RaidEnded);
}

fn read_victims(spt_dir: &Path, profile_id: &str, is_scav: bool) -> Vec<VictimEntry> {
    let path = spt_dir
        .join("SPT/user/profiles")
        .join(format!("{profile_id}.json"));
    let Ok(contents) = std::fs::read(&path) else {
        return Vec::new();
    };
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&contents) else {
        return Vec::new();
    };
    let key = if is_scav { "scav" } else { "pmc" };
    parsed
        .pointer(&format!("/characters/{key}/Stats/Eft/Victims"))
        .and_then(|v| serde_json::from_value::<Vec<VictimEntry>>(v.clone()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn counters(pairs: &[(&str, i64)]) -> HashMap<String, i64> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), *v))
            .collect()
    }

    #[test]
    fn exit_status_diff_picks_the_incremented_counter() {
        let before = counters(&[
            ("ExitStatus/Survived/Pmc", 49),
            ("ExitStatus/Killed/Pmc", 40),
        ]);
        let after = counters(&[
            ("ExitStatus/Survived/Pmc", 49),
            ("ExitStatus/Killed/Pmc", 41),
        ]);
        assert_eq!(diff_exit_status(&before, &after), Some("Killed".to_string()));
    }

    #[test]
    fn exit_status_diff_handles_first_ever_of_a_status() {
        // A status the player has never had before is absent from `before`.
        let before = counters(&[("ExitStatus/Survived/Pmc", 3)]);
        let after = counters(&[("ExitStatus/Survived/Pmc", 3), ("ExitStatus/Runner/Pmc", 1)]);
        assert_eq!(diff_exit_status(&before, &after), Some("Runner".to_string()));
    }

    #[test]
    fn exit_status_diff_is_none_when_nothing_moved() {
        let same = counters(&[("ExitStatus/Survived/Pmc", 7)]);
        assert_eq!(diff_exit_status(&same, &same), None);
    }

    #[test]
    fn parses_exit_counters_and_killer_from_profile_shape() {
        // Mirrors the real SPT 4.0 profile layout.
        let character = serde_json::json!({
            "Stats": { "Eft": {
                "OverallCounters": { "Items": [
                    { "Key": ["ExitStatus", "Survived", "Pmc"], "Value": 49 },
                    { "Key": ["Exp", "ExpExitStatus"], "Value": 14700 },
                    { "Key": ["ExitStatus", "Killed", "Pmc"], "Value": 40 }
                ]},
                "Aggressor": { "ProfileId": "abc123", "AccountId": "1680590", "Name": "TheSunGod" }
            }}
        });

        let c = read_exit_counters(&character);
        assert_eq!(c.get("ExitStatus/Survived/Pmc"), Some(&49));
        assert_eq!(c.get("ExitStatus/Killed/Pmc"), Some(&40));
        // Non-ExitStatus counters are ignored.
        assert!(!c.contains_key("Exp/ExpExitStatus"));

        let killer = read_killer(&character).unwrap();
        assert_eq!(killer.profile_id.as_deref(), Some("abc123"));
        assert_eq!(killer.account_id.as_deref(), Some("1680590"));
    }

    #[test]
    fn snapshot_reads_scav_character_not_savage() {
        // Regression: the scav character lives at `characters.scav`. Looking for
        // `savage` returned None and silently dropped every scav raid.
        let tmp = tempfile::tempdir().unwrap();
        let profiles = tmp.path().join("SPT/user/profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        let profile = serde_json::json!({
            "characters": {
                "pmc":  { "Info": { "Experience": 616359, "Level": 25, "Side": "Usec" } },
                "scav": { "Info": { "Experience": 8006, "Level": 3 },
                          "Stats": { "Eft": { "Victims": [{}, {}] } } }
            }
        });
        std::fs::write(
            profiles.join("deadbeef.json"),
            serde_json::to_vec(&profile).unwrap(),
        )
        .unwrap();

        let (scav, _) = snapshot_profile(tmp.path(), "deadbeef", true).unwrap();
        assert_eq!(scav.level, 3);
        assert_eq!(scav.xp, 8006);
        assert_eq!(scav.victim_count, 2);
        assert!(scav.faction.is_none());

        let (pmc, _) = snapshot_profile(tmp.path(), "deadbeef", false).unwrap();
        assert_eq!(pmc.level, 25);
        assert_eq!(pmc.faction.as_deref(), Some("Usec"));
    }
}
