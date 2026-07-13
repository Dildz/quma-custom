//! Adopt mods that were installed outside quma.
//!
//! The Docker image installs the core mods (Fika, ModSync) at boot from env vars,
//! so their files are on disk but absent from quma's DB — the web UI listed
//! nothing and offered no way to manage them. Adopting a mod records the files it
//! already owns, after which the normal update/remove paths work on it.
//!
//! Ownership is decided by the compose stack, not by quma: the image reinstalls a
//! core mod on every boot when its `AUTO_UPDATE_*` is true, which would revert
//! anything quma did. So the configurator passes the inverse of those toggles as
//! `QUMA_MANAGE_FIKA` / `QUMA_MANAGE_MODSYNC`, and quma only adopts what it was
//! given ownership of.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::db::Database;
use crate::ops::ModSource;

/// A mod the Docker image installs, and the paths it owns in the game root.
struct CoreMod {
    /// Env suffix: `QUMA_MANAGE_{key}` gates adoption, `{key}_VERSION` supplies
    /// the installed version.
    key: &'static str,
    name: &'static str,
    /// Forge mods get Forge update checks for free. `None` = GitHub-sourced.
    forge_mod_id: Option<i64>,
    /// Files and directories, relative to the game root.
    paths: &'static [&'static str],
    /// Paths inside `paths` that stay unmanaged. An adopted mod's files are the
    /// set quma prunes on update, so anything the image owns separately must be
    /// left out or an update would delete it.
    exclude: &'static [&'static str],
}

/// Fika server and client are separate Forge mods; ModSync is GitHub-only.
///
/// `Fika.Headless.dll` lives in the client plugin directory but comes from a
/// different repo (`FIKA_HEADLESS_VERSION`) and is served to the headless by
/// ModSync — it is not part of the client release, so adopting it would make the
/// next client update delete it.
const CORE_MODS: &[CoreMod] = &[
    CoreMod {
        key: "FIKA",
        name: "Project Fika - Server",
        forge_mod_id: Some(crate::config::FIKA_SERVER_FORGE_ID),
        paths: &["SPT/user/mods/fika-server"],
        exclude: &[],
    },
    CoreMod {
        key: "FIKA",
        name: "Project Fika",
        forge_mod_id: Some(crate::config::FIKA_CLIENT_FORGE_ID),
        paths: &["BepInEx/plugins/Fika"],
        exclude: &["BepInEx/plugins/Fika/Fika.Headless.dll"],
    },
    CoreMod {
        key: "MODSYNC",
        name: "Corter-ModSync",
        forge_mod_id: None,
        paths: &[
            "SPT/user/mods/Corter-ModSync",
            "BepInEx/plugins/Corter-ModSync",
            "BepInEx/patchers/Corter-ModSync-Prepatch.dll",
            "ModSync.Updater.exe",
        ],
        exclude: &[],
    },
];

/// Adopt the core mods quma has been given ownership of.
///
/// Runs on every boot and is idempotent: a mod whose files are already tracked is
/// skipped, so this only fires on the first boot after ownership is handed over.
pub fn adopt_core_mods(db: &Database, spt_dir: &Path) -> Result<()> {
    let tracked = tracked_paths(db)?;

    for core in CORE_MODS {
        if !env_flag(&format!("QUMA_MANAGE_{}", core.key)) {
            continue;
        }
        let version = std::env::var(format!("{}_VERSION", core.key)).unwrap_or_default();
        let version = if version.is_empty() {
            "unknown".to_string()
        } else {
            version
        };
        let source_url = core.forge_mod_id.is_none().then(|| modsync_url(&version));

        match adopt(
            db,
            spt_dir,
            &tracked,
            core.name,
            core.forge_mod_id,
            &version,
            source_url.as_deref(),
            core.paths,
            core.exclude,
        ) {
            Ok(Some(count)) => tracing::info!(
                mod_name = core.name,
                version = version,
                files = count,
                "adopted a mod installed by the compose stack"
            ),
            Ok(None) => tracing::debug!(
                mod_name = core.name,
                "nothing to adopt — not on disk, or already tracked"
            ),
            // Adoption is a convenience, never a reason to refuse to boot.
            Err(e) => tracing::warn!(mod_name = core.name, err = %e, "failed to adopt mod"),
        }
    }
    Ok(())
}

/// The ModSync release the image installed. `MODSYNC_URL` overrides the default
/// in the image, so honour it here too.
fn modsync_url(version: &str) -> String {
    std::env::var("MODSYNC_URL").unwrap_or_else(|_| {
        format!(
            "https://github.com/Dildz/ModSync-for-SPT4.0/releases/download/v{version}/Corter-ModSync-v{version}.zip"
        )
    })
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v.eq_ignore_ascii_case("true") || v == "1")
}

fn tracked_paths(db: &Database) -> Result<HashSet<String>> {
    Ok(db
        .get_all_tracked_files()?
        .into_iter()
        .map(|f| f.file_path)
        .collect())
}

/// Record an on-disk mod in the DB. Returns the number of files adopted, or
/// `None` when there is nothing to do.
#[allow(clippy::too_many_arguments)]
pub fn adopt(
    db: &Database,
    spt_dir: &Path,
    tracked: &HashSet<String>,
    name: &str,
    forge_mod_id: Option<i64>,
    version: &str,
    source_url: Option<&str>,
    paths: &[&str],
    exclude: &[&str],
) -> Result<Option<usize>> {
    let mut files = Vec::new();
    for p in paths {
        collect_files(spt_dir, Path::new(p), &mut files)?;
    }
    files.retain(|f| !exclude.contains(&f.as_str()));

    if files.is_empty() {
        return Ok(None);
    }
    // Already adopted (or genuinely quma-installed) — leave it alone.
    if files.iter().any(|f| tracked.contains(f)) {
        return Ok(None);
    }

    let source = if forge_mod_id.is_some() {
        ModSource::Forge
    } else {
        ModSource::Url
    };

    let tx = db.begin_transaction()?;
    let mod_id = db.insert_mod(
        forge_mod_id,
        None,
        name,
        None,
        version,
        source.as_str(),
        source_url,
    )?;
    for rel in &files {
        let abs = spt_dir.join(rel);
        let bytes = std::fs::read(&abs)
            .with_context(|| format!("failed to read {} while adopting", abs.display()))?;
        let hash = crate::spt::mods::compute_hash_public(&bytes);
        db.insert_file(mod_id, rel, Some(&hash), Some(bytes.len() as i64))?;
    }
    tx.commit()?;

    Ok(Some(files.len()))
}

/// Collect files under `rel` (a file or a directory), as paths relative to the
/// game root. Missing paths are not an error: a stack without ModSync simply has
/// none of its directories.
fn collect_files(spt_dir: &Path, rel: &Path, out: &mut Vec<String>) -> Result<()> {
    let abs = spt_dir.join(rel);
    if abs.is_file() {
        out.push(rel.to_string_lossy().replace('\\', "/"));
        return Ok(());
    }
    if !abs.is_dir() {
        return Ok(());
    }
    let mut stack = vec![(abs, PathBuf::from(rel))];
    while let Some((dir, dir_rel)) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("failed to read {}", dir.display()))?
        {
            let entry = entry?;
            let child_rel = dir_rel.join(entry.file_name());
            // Symlinks are skipped, matching the mod scanner.
            let ft = entry.file_type()?;
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push((entry.path(), child_rel));
            } else if ft.is_file() {
                out.push(child_rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn collects_dirs_and_files_and_skips_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "SPT/user/mods/fika-server/package.json", "{}");
        write(root, "SPT/user/mods/fika-server/src/mod.js", "//");
        write(root, "ModSync.Updater.exe", "MZ");

        let mut out = Vec::new();
        collect_files(root, Path::new("SPT/user/mods/fika-server"), &mut out).unwrap();
        collect_files(root, Path::new("ModSync.Updater.exe"), &mut out).unwrap();
        collect_files(root, Path::new("BepInEx/plugins/Nope"), &mut out).unwrap();

        out.sort();
        assert_eq!(
            out,
            vec![
                "ModSync.Updater.exe",
                "SPT/user/mods/fika-server/package.json",
                "SPT/user/mods/fika-server/src/mod.js",
            ]
        );
    }

    #[test]
    fn adopt_records_files_excludes_the_headless_dll_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "BepInEx/plugins/Fika/Fika.Core.dll", "core");
        write(root, "BepInEx/plugins/Fika/Fika.Headless.dll", "headless");

        let db = Database::open(&root.join("test.db")).unwrap();
        let paths = ["BepInEx/plugins/Fika"];
        let exclude = ["BepInEx/plugins/Fika/Fika.Headless.dll"];

        let n = adopt(
            &db,
            root,
            &HashSet::new(),
            "Project Fika",
            Some(crate::config::FIKA_CLIENT_FORGE_ID),
            "2.3.2",
            None,
            &paths,
            &exclude,
        )
        .unwrap();
        assert_eq!(n, Some(1), "the headless dll must not be adopted");

        let tracked = tracked_paths(&db).unwrap();
        assert!(tracked.contains("BepInEx/plugins/Fika/Fika.Core.dll"));
        assert!(!tracked.contains("BepInEx/plugins/Fika/Fika.Headless.dll"));

        // Second pass: already tracked, so nothing happens.
        let again = adopt(
            &db,
            root,
            &tracked,
            "Project Fika",
            Some(crate::config::FIKA_CLIENT_FORGE_ID),
            "2.3.2",
            None,
            &paths,
            &exclude,
        )
        .unwrap();
        assert_eq!(again, None);
    }
}
