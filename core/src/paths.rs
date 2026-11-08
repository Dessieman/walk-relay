//! Where Big Walk keeps its saves on each OS.
//!
//! Windows: `%USERPROFILE%\AppData\LocalLow\House House\Big Walk\user_data\save_games`
//! macOS:   `~/Library/Application Support/House House/Big Walk/user_data/save_games`
//!          (Unity sometimes uses `unity.House House.Big Walk` instead, so both are watched)
//! Linux/Steam Deck (Proton): inside the compatdata prefix for app 1478500.

#[allow(unused_imports)]
use std::path::{Path, PathBuf};

pub const STEAM_APP_ID: u32 = 1478500;
const COMPANY: &str = "House House";
const PRODUCT: &str = "Big Walk";
const SUBDIR: &[&str] = &["user_data", "save_games"];

fn with_subdir(base: PathBuf) -> PathBuf {
    SUBDIR.iter().fold(base, |p, s| p.join(s))
}

/// Every place Big Walk might keep saves on this OS, most likely first.
pub fn candidate_save_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let home = dirs::home_dir();

    #[cfg(target_os = "windows")]
    if let Some(home) = &home {
        out.push(with_subdir(home.join("AppData").join("LocalLow").join(COMPANY).join(PRODUCT)));
    }

    #[cfg(target_os = "macos")]
    if let Some(home) = &home {
        let support = home.join("Library").join("Application Support");
        out.push(with_subdir(support.join(COMPANY).join(PRODUCT)));
        out.push(with_subdir(support.join(format!("unity.{COMPANY}.{PRODUCT}"))));
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    if let Some(home) = &home {
        let proton_suffix = Path::new("steamapps/compatdata")
            .join(STEAM_APP_ID.to_string())
            .join("pfx/drive_c/users/steamuser/AppData/LocalLow")
            .join(COMPANY)
            .join(PRODUCT);
        for steam_root in [".local/share/Steam", ".steam/steam", ".steam/root"] {
            out.push(with_subdir(home.join(steam_root).join(&proton_suffix)));
        }
        out.push(with_subdir(home.join(".config/unity3d").join(COMPANY).join(PRODUCT)));
    }

    let _ = &home; // unused on some OSes
    out
}

/// The candidate folders (plus any configured extras) that exist right now,
/// with duplicates (e.g. symlinked Steam roots) removed.
pub fn existing_save_dirs(extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = Vec::<PathBuf>::new();
    let mut out = Vec::new();
    for dir in candidate_save_dirs().into_iter().chain(extra.iter().cloned()) {
        if !dir.is_dir() {
            continue;
        }
        let canon = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if seen.contains(&canon) {
            continue;
        }
        seen.push(canon);
        out.push(dir);
    }
    out
}

/// What a journey file is called, if `file_name` is one.
/// Returns the journey stem (`save_<walk name>_<id>`) for `.sav` and `.sav.bak` files.
pub fn journey_stem(file_name: &str) -> Option<&str> {
    let stem = file_name
        .strip_suffix(".sav.bak")
        .or_else(|| file_name.strip_suffix(".sav"))?;
    if stem.is_empty() || stem.starts_with('.') {
        return None;
    }
    Some(stem)
}

/// Human-friendly walk name: `save_a nice view_52057433e2c14fbb` -> `a nice view`.
pub fn display_name(stem: &str) -> String {
    let s = stem.strip_prefix("save_").unwrap_or(stem);
    if let Some((name, id)) = s.rsplit_once('_') {
        if id.len() >= 8 && id.chars().all(|c| c.is_ascii_hexdigit()) && !name.is_empty() {
            return name.to_string();
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_and_names() {
        assert_eq!(journey_stem("save_a nice view_52057433e2c14fbb.sav"), Some("save_a nice view_52057433e2c14fbb"));
        assert_eq!(journey_stem("save_a nice view_52057433e2c14fbb.sav.bak"), Some("save_a nice view_52057433e2c14fbb"));
        assert_eq!(journey_stem("settings.json"), None);
        assert_eq!(journey_stem(".sav"), None);
        assert_eq!(display_name("save_a nice view_52057433e2c14fbb"), "a nice view");
        assert_eq!(display_name("save_under_score walk_401fe41c4c9041c6"), "under_score walk");
        assert_eq!(display_name("weird"), "weird");
    }

    #[test]
    fn candidates_end_in_save_games() {
        for d in candidate_save_dirs() {
            assert!(d.ends_with(Path::new("user_data").join("save_games")), "{}", d.display());
        }
    }
}
