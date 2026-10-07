//! What the user left a view as, kept across runs: the selected agent, the
//! panel's width, the timeline and prompt toggles, and the camera: following,
//! framing everything, or where the user panned and zoomed it. Not the scroll:
//! a view reopens on the newest lines.
//!
//! Kept per view, keyed by the session or job watched, in one small file under
//! the user's cache (`~/.cache/zoetrope/views.json`), the oldest dropped past
//! `KEEP`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How many views are remembered.
const KEEP: usize = 64;

/// A view's state as kept. Missing fields read as a fresh view's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Kept {
    pub selected: Option<String>,
    pub panel_share: u16,
    pub show_timeline: bool,
    pub whole_prompts: bool,
    pub follow: bool,
    /// Where the user took the camera, as pan x, pan y and zoom: `None` unless
    /// they did.
    pub camera: Option<(f64, f64, f64)>,
}

impl Default for Kept {
    fn default() -> Self {
        Self {
            selected: None,
            panel_share: 50,
            show_timeline: true,
            whole_prompts: false,
            follow: false,
            camera: None,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Entry {
    /// When it was kept (unix seconds), to drop the oldest.
    at: i64,
    #[serde(flatten)]
    kept: Kept,
}

/// The file views are kept in.
pub fn file() -> Option<PathBuf> {
    Some(
        crate::provider::home_dir()?
            .join(".cache")
            .join("zoetrope")
            .join("views.json"),
    )
}

fn read(path: &Path) -> BTreeMap<String, Entry> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// `view`'s kept state in `path`, if it was kept.
pub fn load(path: &Path, view: &str) -> Option<Kept> {
    read(path).remove(view).map(|e| e.kept)
}

/// Keep `view`'s state in `path`. Written whole and then moved into place, so
/// a view reading it never sees half of one.
pub fn save(path: &Path, view: &str, kept: &Kept) -> std::io::Result<()> {
    let mut all = read(path);
    let at = chrono::Utc::now().timestamp();
    all.insert(
        view.to_string(),
        Entry {
            at,
            kept: kept.clone(),
        },
    );
    while all.len() > KEEP {
        let oldest = all.iter().min_by_key(|(_, e)| e.at).map(|(k, _)| k.clone());
        all.remove(&oldest.unwrap_or_default());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let next = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&next, serde_json::to_string(&all)?)?;
    std::fs::rename(&next, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_view_is_kept_and_read_back() {
        let dir = std::env::temp_dir().join(format!("zoe-remember-{}", std::process::id()));
        let path = dir.join("views.json");
        assert_eq!(load(&path, "job:a"), None, "nothing kept yet");
        let kept = Kept {
            selected: Some("review".into()),
            panel_share: 40,
            ..Kept::default()
        };
        save(&path, "job:a", &kept).unwrap();
        save(&path, "job:b", &Kept::default()).unwrap();
        assert_eq!(load(&path, "job:a"), Some(kept));
        assert_eq!(load(&path, "job:b"), Some(Kept::default()));
        std::fs::write(&path, r#"{"job:a":{"at":1,"panel_share":30}}"#).unwrap();
        let old = load(&path, "job:a").unwrap();
        assert_eq!(
            (old.panel_share, old.show_timeline),
            (30, true),
            "missing fields are a fresh view's"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
