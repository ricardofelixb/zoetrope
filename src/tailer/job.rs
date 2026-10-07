//! Tailing a job (native): several sessions read as one tree.
//!
//! A job's members come from a manifest naming them, tailed like any file, or
//! from a folder: every session that ran under it, found as they appear (an
//! [`Overview`]). Each member is opened by its own provider and read by an
//! ordinary [`LiveSession`], so its subagents and workflows are found exactly
//! as they are for that session alone. What the members state goes through
//! [`Member::rewrite`] into the job's namespace, and every tick's statements go
//! out as one batch stamped with the job's id.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::sync::mpsc;

use crate::fact::Statement;
use crate::job::{self, Entry, Line, Member, Overview};
use crate::provider::{Provider, Scope, Session, Target, open, sweep};

use super::bytes::{ReadResult, TailState, read_appended};
use super::item::ReplayItem;
use super::live::{Feed, LiveSession, read_session, tail_loop};
use super::replay::settle;
use super::{Flow, TailRequest, UiEvent};

/// Look up what is not in the tree yet every N ticks (~2s at the 200ms poll):
/// a member named only by session id, or an overview's new sessions. Either
/// sweeps every provider's sessions.
const LOOKUP_EVERY: u32 = 10;

/// How far back an overview looks when it opens. Sessions that start later
/// join as they appear, and a session in the tree stays.
const OVERVIEW_LOOKBACK: Duration = Duration::from_secs(60 * 60);

/// Where a job's members come from.
enum Source {
    Manifest {
        path: PathBuf,
        state: TailState,
        /// Member lines whose session cannot be opened yet: a file not
        /// written yet, or an id no provider lists yet.
        pending: Vec<Entry>,
    },
    Folder(Folder),
}

/// Everything one job's poll loop owns.
pub(crate) struct JobFeed {
    source: Source,
    /// Whether the root has been stated: a manifest's header, or the folder.
    /// A later header line is ignored.
    announced: bool,
    members: Vec<(Member, LiveSession)>,
    /// Sessions already in the tree, by provider and id, so a line naming one
    /// again (a resumed step) changes nothing.
    claimed: HashSet<(Provider, String)>,
    /// How many sessions each key has, so a second one becomes `key~2`.
    per_key: HashMap<String, usize>,
    ticks: u32,
}

impl JobFeed {
    pub(crate) fn new(manifest: PathBuf) -> Self {
        Self::with(Source::Manifest {
            path: manifest,
            state: TailState::default(),
            pending: Vec::new(),
        })
    }

    /// An overview of every session under `folder`.
    pub(crate) fn overview(folder: PathBuf) -> Self {
        Self::with(Source::Folder(Folder {
            path: folder,
            since: SystemTime::now()
                .checked_sub(OVERVIEW_LOOKBACK)
                .unwrap_or(SystemTime::UNIX_EPOCH),
            placed: HashSet::new(),
            groups: HashSet::new(),
        }))
    }

    fn with(source: Source) -> Self {
        JobFeed {
            source,
            announced: false,
            members: Vec::new(),
            claimed: HashSet::new(),
            per_key: HashMap::new(),
            ticks: 0,
        }
    }

    /// What re-attaches this job from scratch.
    fn target(&self) -> Target {
        match &self.source {
            Source::Manifest { path, .. } => Target::Job(path.clone()),
            Source::Folder(folder) => Target::Overview(folder.path.clone()),
        }
    }

    /// Read what the source and every member gained since the last call.
    /// Returns `true` if any file was truncated or rotated: the job must be
    /// re-attached, as a session would be.
    pub(crate) fn read(&mut self, out: &mut Vec<Statement>) -> bool {
        let lookup = self.ticks.is_multiple_of(LOOKUP_EVERY);
        self.ticks = self.ticks.wrapping_add(1);
        let joined = match &mut self.source {
            Source::Manifest {
                path,
                state,
                pending,
            } => {
                match read_appended(path, state) {
                    ReadResult::Reset => return true,
                    ReadResult::Lines(lines) => {
                        for line in lines.iter().filter_map(|l| job::parse_line(l)) {
                            match line {
                                Line::Header(header) if !self.announced => {
                                    self.announced = true;
                                    out.extend(job::header_statements(&header, path));
                                }
                                Line::Header(_) => {}
                                Line::Member(entry) => pending.push(entry),
                            }
                        }
                    }
                    ReadResult::NoChange | ReadResult::Missing => {}
                }
                let mut joined = Vec::new();
                pending.retain(|entry| match resolve(path, entry, lookup) {
                    Some(session) => {
                        joined.push((entry.clone(), session));
                        false
                    }
                    None => true,
                });
                joined
            }
            Source::Folder(folder) => {
                if !self.announced {
                    self.announced = true;
                    out.extend(Overview::statements(&folder.path));
                }
                if lookup {
                    let recent = sweep(&Scope::ALL.since(folder.since), None);
                    folder.place(recent, &mut self.members, out);
                }
                Vec::new()
            }
        };
        for (entry, session) in joined {
            self.join(&entry, session, out);
        }

        let mut said = Vec::new();
        for (member, session) in &mut self.members {
            if read_session(session, &mut said) {
                return true;
            }
            out.extend(said.drain(..).map(|s| member.rewrite(s)));
        }
        false
    }

    /// Add a session a manifest names to the tree, unless it is in it already.
    fn join(&mut self, entry: &Entry, session: Session, out: &mut Vec<Statement>) {
        if !self.claimed.insert((session.provider, session.id.clone())) {
            return;
        }
        let count = self.per_key.entry(entry.member.clone()).or_default();
        *count += 1;
        let root = match *count {
            1 => entry.member.clone(),
            n => format!("{}~{n}", entry.member),
        };
        let member = Member::new(root, entry);
        out.push(member.birth());
        self.members
            .push((member, LiveSession::new(session, None, entry.provider)));
    }

    /// One poll tick: emit what was read as one batch, or re-attach.
    pub(crate) async fn poll(
        &mut self,
        session_id: &str,
        ui_tx: &mpsc::Sender<UiEvent>,
    ) -> Option<Target> {
        let mut statements = Vec::new();
        if self.read(&mut statements) {
            let _ = ui_tx
                .send(UiEvent::SessionReset {
                    session_id: session_id.to_string(),
                })
                .await;
            return Some(self.target());
        }
        if !statements.is_empty() {
            let _ = ui_tx
                .send(UiEvent::Batch {
                    session_id: session_id.to_string(),
                    statements,
                })
                .await;
        }
        None
    }
}

/// Open the session a member line names, if it can be opened now.
/// `by_id`: also look up a member named only by its session id.
fn resolve(manifest: &Path, entry: &Entry, by_id: bool) -> Option<Session> {
    if let Some(path) = entry.path_from(manifest) {
        // An empty file is claimed by layout, and Claude's layout claims any
        // `.jsonl`: without a provider, wait until the content says.
        if entry.provider.is_none() && std::fs::metadata(&path).is_ok_and(|m| m.len() == 0) {
            return None;
        }
        return open(&Target::Path(path), entry.provider).ok();
    }
    let id = entry.session.as_ref().filter(|_| by_id)?;
    open(&Target::Id(id.clone()), entry.provider).ok()
}

/// An overview's own state: the folder, and what has been placed in it.
struct Folder {
    path: PathBuf,
    /// Sessions last written before this are not looked at.
    since: SystemTime,
    /// Sessions already placed, in the tree or not under the folder, so each
    /// root's head is read once.
    placed: HashSet<(Provider, String)>,
    /// Repository groups already stated.
    groups: HashSet<String>,
}

impl Folder {
    /// Place every session not placed yet: under its repository's group if it
    /// ran under the folder, nowhere otherwise.
    fn place(
        &mut self,
        sessions: impl IntoIterator<Item = Session>,
        members: &mut Vec<(Member, LiveSession)>,
        out: &mut Vec<Statement>,
    ) {
        for session in sessions {
            let key = (session.provider, session.id.clone());
            if self.placed.contains(&key) {
                continue;
            }
            // A root that names no directory yet (just created) is retried.
            let Some(cwd) = session.provider.cwd(&session.root) else {
                continue;
            };
            self.placed.insert(key);
            let Some(repository) = Overview::repository(&self.path, &cwd) else {
                continue;
            };
            let (group, statement) = Overview::group(&repository);
            if self.groups.insert(group.clone()) {
                out.push(statement);
            }
            let provider = session.provider;
            let root = format!("{}-{}", provider.name(), session.id);
            let member = Member::found(root, group, provider);
            out.push(member.birth());
            members.push((member, LiveSession::new(session, None, Some(provider))));
        }
    }
}

/// The id a job's events are stamped with, from its manifest.
pub fn session_id(manifest: &Path) -> String {
    job::session_id(&job::read_header(manifest).unwrap_or_default(), manifest)
}

/// Follow or replay a job until a switch or exit. Replay reads everything on
/// disk up front and hands it over whole, as a session's replay does; either
/// way the job keeps tailing afterwards, so new members and appends flow in.
pub(crate) async fn run_job(
    manifest: &Path,
    replay: bool,
    speed: f64,
    ui_tx: &mpsc::Sender<UiEvent>,
    req_rx: &mut mpsc::Receiver<TailRequest>,
) -> Flow {
    let session_id = session_id(manifest);
    let mut feed = JobFeed::new(manifest.to_path_buf());
    if replay {
        // The first read is the whole backfill: run it off the runtime, as the
        // session replay does, and keep the feed (it holds every offset).
        let loaded = tokio::task::spawn_blocking(move || {
            let mut statements = Vec::new();
            feed.read(&mut statements);
            (feed, statements)
        })
        .await;
        let (loaded, statements) = match loaded {
            Ok(loaded) => loaded,
            Err(e) => {
                let _ = ui_tx
                    .send(UiEvent::Error(format!("failed to load job: {e}")))
                    .await;
                return Flow::Exit;
            }
        };
        feed = loaded;
        let (items, info) = settle(statements.into_iter().map(ReplayItem::new).collect());
        let speed = if speed > 0.0 { speed } else { 1.0 };
        let loaded = UiEvent::ReplayLoaded {
            session_id: session_id.clone(),
            items,
            speed,
            info,
        };
        if ui_tx.send(loaded).await.is_err() {
            return Flow::Exit;
        }
    } else {
        // Live: announce the id before any batch (see `run_live`); the first
        // poll backfills.
        let _ = ui_tx
            .send(UiEvent::SessionReset {
                session_id: session_id.clone(),
            })
            .await;
    }
    tail_loop(Feed::Job(Box::new(feed)), session_id, ui_tx, req_rx).await
}

/// Follow an overview of `folder` until a switch or exit. Always live: what is
/// running now is the point, and the first poll backfills the last hour.
pub(crate) async fn run_overview(
    folder: &Path,
    ui_tx: &mpsc::Sender<UiEvent>,
    req_rx: &mut mpsc::Receiver<TailRequest>,
) -> Flow {
    let session_id = Overview::session_id(folder);
    let _ = ui_tx
        .send(UiEvent::SessionReset {
            session_id: session_id.clone(),
        })
        .await;
    let feed = JobFeed::overview(folder.to_path_buf());
    tail_loop(Feed::Job(Box::new(feed)), session_id, ui_tx, req_rx).await
}

/// Read a whole job, every member's every file, as the statements its tree is
/// folded from. `inspect`'s way in.
pub fn read_job(manifest: &Path) -> Vec<Statement> {
    read_all(JobFeed::new(manifest.to_path_buf()))
}

/// Read an overview of `folder` as it stands: the sessions of the last hour.
pub fn read_overview(folder: &Path) -> Vec<Statement> {
    read_all(JobFeed::overview(folder.to_path_buf()))
}

fn read_all(mut feed: JobFeed) -> Vec<Statement> {
    let mut statements = Vec::new();
    feed.read(&mut statements);
    statements
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::{AgentKind, FactKind};
    use crate::state::session::{MAIN_ID, SessionModel};
    use std::io::Write;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zoetrope_job_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn append(path: &Path, line: &str) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }

    fn claude_line(text: &str, minute: u32) -> String {
        format!(
            r#"{{"type":"user","uuid":"u{minute}","parentUuid":null,"timestamp":"2026-10-06T10:{minute:02}:00.000Z","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    fn fold(statements: &[Statement]) -> SessionModel {
        let mut model = SessionModel::new("job".into());
        for f in statements.iter().flat_map(|s| &s.facts) {
            model.apply_fact(f);
        }
        model
    }

    #[test]
    fn members_join_when_their_session_appears_and_once() {
        let dir = temp_dir("live");
        let manifest = dir.join("live.jsonl");
        let plan = dir
            .join("project")
            .join("11111111-1111-1111-1111-111111111111.jsonl");
        let review = dir
            .join("project")
            .join("22222222-2222-2222-2222-222222222222.jsonl");
        std::fs::create_dir_all(plan.parent().unwrap()).unwrap();
        append(
            &manifest,
            r#"{"zoe":"job","v":1,"id":"j","title":"Fix it","task":"fix"}"#,
        );
        append(&plan, &claude_line("plan this", 0));

        let mut feed = JobFeed::new(manifest.clone());
        let mut out = Vec::new();
        assert!(!feed.read(&mut out));
        assert!(
            out.iter().any(|s| s.is_session_meta()),
            "the header's metadata"
        );

        // A member whose file exists joins at once; one whose file does not
        // yet waits. Paths are relative to the manifest.
        append(
            &manifest,
            r#"{"member":"plan","label":"plan: Opus (claude)","path":"project/11111111-1111-1111-1111-111111111111.jsonl"}"#,
        );
        append(
            &manifest,
            &format!(
                r#"{{"member":"review","provider":"claude","path":{}}}"#,
                serde_json::to_string(&review).unwrap()
            ),
        );
        assert!(!feed.read(&mut out));
        let model = fold(&out);
        let plan_node = model.agent("plan").expect("plan joined");
        assert_eq!(plan_node.parent.as_deref(), Some(MAIN_ID));
        assert_eq!(plan_node.kind, AgentKind::Subagent);
        assert_eq!(plan_node.agent_type.as_deref(), Some("plan: Opus (claude)"));
        assert!(model.agent("review").is_none(), "no file yet");
        assert!(model.first_prompt().is_some_and(|p| p.contains("fix")));

        // The second session's file appears; a resumed step names the first
        // one again, which changes nothing.
        append(&review, &claude_line("review this", 5));
        append(
            &manifest,
            r#"{"member":"plan","path":"project/11111111-1111-1111-1111-111111111111.jsonl"}"#,
        );
        assert!(!feed.read(&mut out));
        let model = fold(&out);
        assert!(model.agent("review").is_some());
        assert!(
            model.agent("plan~2").is_none(),
            "the same session is not a re-run"
        );

        // A step that starts a new session under the same key is a sibling.
        let rerun = dir
            .join("project")
            .join("33333333-3333-3333-3333-333333333333.jsonl");
        append(&rerun, &claude_line("plan again", 9));
        append(
            &manifest,
            r#"{"member":"plan","path":"project/33333333-3333-3333-3333-333333333333.jsonl"}"#,
        );
        assert!(!feed.read(&mut out));
        let model = fold(&out);
        assert_eq!(
            model.agent("plan~2").and_then(|a| a.parent.as_deref()),
            Some(MAIN_ID)
        );

        // Appends to a member's file arrive as that member's.
        let mut tick = Vec::new();
        append(&plan, &claude_line("more", 12));
        assert!(!feed.read(&mut tick));
        assert!(
            tick.iter()
                .flat_map(|s| &s.facts)
                .all(|f| f.agent.as_deref() == Some("plan") || f.agent.as_deref() == Some(MAIN_ID))
        );
        assert!(
            tick.iter()
                .flat_map(|s| &s.facts)
                .any(|f| matches!(&f.kind, FactKind::Prompt(t) if t == "more"))
        );

        // Truncating a member's file re-attaches the job.
        std::fs::write(&plan, "{}\n").unwrap();
        assert!(feed.read(&mut Vec::new()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_overview_places_sessions_under_their_repositories_once() {
        let dir = temp_dir("overview");
        let folder = dir.join("projects");
        for repo in ["app/.git", "app/src", "site/.git"] {
            std::fs::create_dir_all(folder.join(repo)).unwrap();
        }
        // Claude keeps sessions by project elsewhere; each records where it ran.
        let session = |uuid: &str, cwd: &Path| {
            let path = dir.join("claude").join("p").join(format!("{uuid}.jsonl"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let cwd = serde_json::to_string(&cwd.display().to_string()).unwrap();
            let line = format!(
                r#"{{"type":"user","uuid":"u1","parentUuid":null,"cwd":{cwd},"timestamp":"2026-10-06T10:00:00.000Z","message":{{"role":"user","content":"fix the login"}}}}"#
            );
            std::fs::write(&path, format!("{line}\n")).unwrap();
            open(&Target::Path(path), Some(Provider::Claude)).unwrap()
        };
        let sessions = || {
            vec![
                session(
                    "11111111-1111-1111-1111-111111111111",
                    &folder.join("app/src"),
                ),
                session("22222222-2222-2222-2222-222222222222", &folder.join("site")),
                session(
                    "33333333-3333-3333-3333-333333333333",
                    &dir.join("elsewhere"),
                ),
            ]
        };

        let mut feed = JobFeed::overview(folder.clone());
        let Source::Folder(found) = &mut feed.source else {
            unreachable!()
        };
        let mut out = Overview::statements(&folder);
        found.place(sessions(), &mut feed.members, &mut out);
        // Seen again on the next sweep, nothing is placed twice.
        found.place(sessions(), &mut feed.members, &mut out);
        assert_eq!(
            feed.members.len(),
            2,
            "the session outside the folder is left out"
        );
        assert!(!feed.read(&mut out));

        let model = fold(&out);
        let app = "claude-11111111-1111-1111-1111-111111111111";
        assert_eq!(
            model.agent(app).and_then(|a| a.parent.as_deref()),
            Some("@app")
        );
        assert_eq!(
            model
                .agent("@app")
                .map(|g| (g.kind, g.parent.as_deref(), g.agent_type.as_deref())),
            Some((AgentKind::Group, Some(MAIN_ID), Some("app")))
        );
        assert_eq!(
            model
                .agent("claude-22222222-2222-2222-2222-222222222222")
                .and_then(|a| a.parent.as_deref()),
            Some("@site")
        );
        let node = model.agent(app).unwrap();
        assert_eq!(node.agent_type.as_deref(), Some("claude"));
        assert_eq!(node.description.as_deref(), Some("fix the login"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fixture job mixes a Claude session (with subagents and a workflow)
    /// and a Codex session (with child threads) in one tree.
    #[test]
    fn mixed_providers_conform() {
        let Some(dir) = crate::provider::harness::fixture_dir("job") else {
            return;
        };
        let manifest = dir.join("demo.live.jsonl");
        assert_eq!(session_id(&manifest), "job:demo");
        let statements = read_job(&manifest);
        let model = fold(&statements);
        for (member, provider) in [("explore", "claude"), ("review", "codex")] {
            let node = model
                .agent(member)
                .unwrap_or_else(|| panic!("{member} joined"));
            assert_eq!(node.parent.as_deref(), Some(MAIN_ID));
            assert!(
                node.agent_type
                    .as_deref()
                    .is_some_and(|t| t.contains(provider))
            );
        }
        // Every member's agents hang in its own namespace, groups included.
        for (id, agent) in &model.agents {
            if id != MAIN_ID && !["explore", "review"].contains(&id.as_str()) {
                let member = id.split('/').next().unwrap();
                assert!(id.contains('/'), "{id} is namespaced");
                assert!(
                    agent
                        .parent
                        .as_deref()
                        .is_some_and(|p| p.starts_with(member)),
                    "{id} hangs under {member}: {:?}",
                    agent.parent
                );
            }
        }
        assert!(model.agents.values().any(|a| a.kind == AgentKind::Group));

        crate::provider::harness::conform("job", "demo", || per_file(&manifest));
    }

    /// The job's statements one stream per file, as the harness interleaves
    /// them: the manifest's (header and births), then every file of every
    /// member through its own provider, rewritten.
    fn per_file(manifest: &Path) -> Vec<Vec<Statement>> {
        let text = std::fs::read_to_string(manifest).unwrap();
        let mut own = Vec::new();
        let mut streams = Vec::new();
        for line in text.lines().filter_map(job::parse_line) {
            let entry = match line {
                Line::Header(h) => {
                    own.extend(job::header_statements(&h, manifest));
                    continue;
                }
                Line::Member(entry) => entry,
            };
            let path = entry.path_from(manifest).unwrap();
            let session = open(&Target::Path(path), entry.provider).unwrap();
            let member = Member::new(entry.member.clone(), &entry);
            own.push(member.birth());
            for file in session.every_file() {
                let text = std::fs::read_to_string(&file.path).unwrap();
                let said: Vec<Statement> = match file.read {
                    crate::provider::ReadMode::Tail => {
                        let mut stream = session.provider.stream_for(file);
                        text.lines().filter_map(|l| stream.push(l)).collect()
                    }
                    crate::provider::ReadMode::Whole => {
                        session.provider.sidecar(file, &text).into_iter().collect()
                    }
                };
                streams.push(said.into_iter().map(|s| member.rewrite(s)).collect());
            }
        }
        streams.insert(0, own);
        streams
    }
}
