//! Where pi keeps its sessions on disk, and how to tell what a file is.
//!
//! One file per session: `<agent dir>/sessions/--<cwd>--/<timestamp>_<id>.jsonl`,
//! the directory named by the working directory with `/`, `\` and `:` replaced
//! by `-`. The first line is the header, `{"type":"session","id":…,"cwd":…}`,
//! which names the session. pi has no subagents: a file is a whole session.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::wire::parse_line;
use crate::provider::{FileRole, Provider, ReadMode, Scope, SessionFile};

/// pi's agent directory: `$PI_CODING_AGENT_DIR` (empty is unset, a leading `~`
/// is the home directory), else `~/.pi/agent`.
fn agent_dir() -> Option<PathBuf> {
    let home = crate::provider::home_dir()?;
    let Some(dir) = std::env::var("PI_CODING_AGENT_DIR")
        .ok()
        .filter(|d| !d.is_empty())
    else {
        return Some(home.join(".pi/agent"));
    };
    Some(match dir.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            home.join(rest.trim_start_matches(['/', '\\']))
        }
        _ => PathBuf::from(dir),
    })
}

/// Every session file under `<agent dir>/sessions`. The file name ends in the
/// session id, so an id prefix prunes without reading.
pub fn all_paths(scope: &Scope) -> Vec<PathBuf> {
    let Some(dir) = agent_dir().and_then(|d| std::fs::read_dir(d.join("sessions")).ok()) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = dir
        .flatten()
        .filter_map(|d| std::fs::read_dir(d.path()).ok())
        .flat_map(|files| files.flatten().map(|f| f.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    if let Some(prefix) = &scope.id_prefix {
        out.retain(|p| {
            let stem = p.file_stem().and_then(|s| s.to_str());
            stem.and_then(|s| s.split_once('_'))
                .is_some_and(|(_, id)| id.starts_with(prefix.as_str()))
        });
    }
    out
}

/// What a file is, by its header.
pub fn session_file(path: &Path) -> Option<SessionFile> {
    classify_head(
        path,
        &crate::provider::read_head(path)?,
        crate::provider::modified(path),
    )
}

/// [`session_file`] given the first line already read: the browser's way.
pub fn classify_head(path: &Path, head: &str, modified: SystemTime) -> Option<SessionFile> {
    let header = parse_line(head.lines().find(|l| !l.trim().is_empty())?)?;
    if header.kind != "session" {
        return None;
    }
    Some(SessionFile {
        provider: Provider::Pi,
        path: path.to_path_buf(),
        session: header.id?,
        role: FileRole::Root,
        read: ReadMode::Tail,
        project_key: header.cwd.unwrap_or_default(),
        modified,
    })
}

/// pi records the working directory as it is.
pub fn project_key(cwd: &Path) -> String {
    cwd.to_string_lossy().into_owned()
}
