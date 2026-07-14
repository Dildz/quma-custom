//! Classification of mod files as client-side or server-side.
//!
//! quma does not distribute client files to the headless — ModSync owns that,
//! and doing it from here would fight ModSync's allowlist. What remains is the
//! classifier itself, used to show which of a mod's files are client-side.

const FIKA_MANAGED_PREFIXES: &[&str] = &["BepInEx/plugins/Fika/", "BepInEx/plugins/Fika.Headless/"];

/// A "client file" is anything that belongs in the game client install —
/// everything EXCEPT server-side mods (`SPT/user/mods/`), BepInEx config
/// (per-client overlay), and Fika-managed directories.
pub fn is_client_file(path: &str) -> bool {
    !path.starts_with("SPT/")
        && !path.starts_with("BepInEx/config/")
        && !FIKA_MANAGED_PREFIXES.iter().any(|p| path.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_client_vs_server_files() {
        assert!(is_client_file("BepInEx/plugins/SomeMod/mod.dll"));
        assert!(!is_client_file("SPT/user/mods/fika-server/package.json"));
        assert!(!is_client_file("BepInEx/config/SomeMod.cfg"));
        assert!(!is_client_file("BepInEx/plugins/Fika/Fika.Core.dll"));
    }
}
