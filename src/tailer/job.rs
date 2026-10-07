//! Tailing a job (native): a manifest naming sessions, read as one tree.
//!
//! The manifest is tailed like any file. Each member line names a session,
//! which is opened by its own provider and read by an ordinary
//! [`LiveSession`], so a member's subagents and workflows are found exactly as
//! they are for that session alone. What the members state goes through
//! [`Member::rewrite`] into the job's namespace, and every tick's statements
//! go out as one batch stamped with the job's id.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use tokio::sync::mpsc;

use crate::fact::{Fact, FactKind, Statement};
use crate::job::{self, Entry, Line, Member};
use crate::provider::{Provider, Target, open};

use super::bytes::{ReadResult, TailState, read_appended};
use super::item::ReplayItem;
use super::live::{Feed, LiveSession, read_session, tail_loop};
use super::replay::settle;
use super::{Flow, TailRequest, UiEvent};

/// Retry a member named only by session id every N ticks (~2s at the 200ms
/// poll): finding an id sweeps every provider's sessions.
const ID_RETRY_EVERY: u32 = 10;

/// A manifest line about a key, as of its time: a removal when `true`.
type Event = (Option<DateTime<Utc>>, bool);

/// Everything one job's poll loop owns.
pub(crate) struct JobFeed {
    manifest: PathBuf,
    state: TailState,
    /// Whether the header has been stated. A later header line is ignored.
    announced: bool,
    /// Member lines whose session cannot be opened yet: a file not written
    /// yet, or an id no provider lists yet.
    pending: Vec<Entry>,
    members: Vec<(Member, LiveSession)>,
    /// Sessions already in the tree, by provider and id, so a line naming one
    /// again (a resumed step) changes nothing.
    claimed: HashMap<(Provider, String), String>,
    /// Every removal (`true`) and statement of each key, in the order read, to
    /// apply to the sessions that join under it afterwards.
    events: HashMap<String, Vec<Event>>,
    /// How many sessions each key has, so a second one becomes `key~2`.
    per_key: HashMap<String, usize>,
    ticks: u32,
}

impl JobFeed {
    pub(crate) fn new(manifest: PathBuf) -> Self {
        JobFeed {
            manifest,
            state: TailState::default(),
            announced: false,
            pending: Vec::new(),
            members: Vec::new(),
            claimed: HashMap::new(),
            events: HashMap::new(),
            per_key: HashMap::new(),
            ticks: 0,
        }
    }

    /// Read what the manifest and every member gained since the last call.
    /// Returns `true` if any file was truncated or rotated: the job must be
    /// re-attached, as a session would be.
    pub(crate) fn read(&mut self, out: &mut Vec<Statement>) -> bool {
        match read_appended(&self.manifest, &mut self.state) {
            ReadResult::Reset => return true,
            ReadResult::Lines(lines) => {
                for line in lines.iter().filter_map(|l| job::parse_line(l)) {
                    match line {
                        Line::Header(header) if !self.announced => {
                            self.announced = true;
                            out.extend(job::header_statements(&header, &self.manifest));
                        }
                        Line::Header(_) => {}
                        // Stated now, though its session may be found later.
                        Line::Member(entry) => {
                            self.declare(&entry.member, &entry, out);
                            self.pending.push(entry);
                        }
                        Line::Group(group) => out.push(job::group_statement(&group)),
                        Line::Gone(gone) => {
                            // A member key covers every session run under it.
                            for n in 2..=self.count(&gone.gone) {
                                out.push(job::gone_statement(&job::Gone {
                                    gone: root(&gone.gone, n),
                                    ts: gone.ts,
                                }));
                            }
                            self.events
                                .entry(gone.gone.clone())
                                .or_default()
                                .push((Some(gone.ts), true));
                            out.push(job::gone_statement(&gone));
                        }
                        // To the member's latest session under that key.
                        Line::Told(told) => out.push(
                            Fact {
                                agent: Some(root(&told.member, self.count(&told.member).max(1))),
                                ts: told.ts,
                                kind: FactKind::Told(told.told),
                            }
                            .into(),
                        ),
                    }
                }
            }
            ReadResult::NoChange | ReadResult::Missing => {}
        }

        let retry_ids = self.ticks.is_multiple_of(ID_RETRY_EVERY);
        self.ticks = self.ticks.wrapping_add(1);
        for entry in std::mem::take(&mut self.pending) {
            match self.resolve(&entry, retry_ids) {
                Some(session) => self.join(&entry, session, out),
                None => self.pending.push(entry),
            }
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

    /// Open the session a member line names, if it can be opened now.
    fn resolve(&self, entry: &Entry, retry_ids: bool) -> Option<crate::provider::Session> {
        if let Some(path) = entry.path_from(&self.manifest) {
            // An empty file is claimed by layout, and Claude's layout claims
            // any `.jsonl`: without a provider, wait until the content says.
            if entry.provider.is_none() && std::fs::metadata(&path).is_ok_and(|m| m.len() == 0) {
                return None;
            }
            return open(&Target::Path(path), entry.provider).ok();
        }
        let id = entry.session.as_ref().filter(|_| retry_ids)?;
        open(&Target::Id(id.clone()), entry.provider).ok()
    }

    /// Add an opened session to the tree, unless it is in it already.
    fn join(&mut self, entry: &Entry, session: crate::provider::Session, out: &mut Vec<Statement>) {
        let key = (session.provider, session.id.clone());
        if self.claimed.contains_key(&key) {
            return;
        }
        let count = self.per_key.entry(entry.member.clone()).or_default();
        *count += 1;
        let member = Member::new(root(&entry.member, *count), entry);
        self.claimed.insert(key, member.root.clone());
        out.push(member.birth());
        out.push(member.session(session.provider, &session.id));
        for &(ts, gone) in self.events.get(&entry.member).into_iter().flatten() {
            out.push(match ts {
                Some(ts) if gone => job::gone_statement(&job::Gone {
                    gone: member.root.clone(),
                    ts,
                }),
                _ => job::redeclared(&member.root, ts),
            });
        }
        self.members
            .push((member, LiveSession::new(session, None, entry.provider)));
    }

    /// State every session under `key` as of the line `entry`, and keep it for
    /// the sessions that join later.
    fn declare(&mut self, key: &str, entry: &Entry, out: &mut Vec<Statement>) {
        self.events
            .entry(key.to_string())
            .or_default()
            .push((entry.ts, false));
        for n in 1..=self.count(key) {
            out.push(job::redeclared(&root(key, n), entry.ts));
        }
    }

    /// How many sessions have joined under `key`.
    fn count(&self, key: &str) -> usize {
        self.per_key.get(key).copied().unwrap_or(0)
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
            return Some(Target::Job(self.manifest.clone()));
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

/// The node of the `n`th session to join under `key`: the key, then `key~2`.
fn root(key: &str, n: usize) -> String {
    match n {
        1 => key.to_string(),
        n => format!("{key}~{n}"),
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

/// Read a whole job, every member's every file, as the statements its tree is
/// folded from. `inspect`'s way in.
pub fn read_job(manifest: &Path) -> Vec<Statement> {
    let mut feed = JobFeed::new(manifest.to_path_buf());
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
    fn a_told_line_goes_to_the_members_latest_session() {
        let dir = temp_dir("told");
        let manifest = dir.join("told.jsonl");
        let plan = dir.join("11111111-1111-1111-1111-111111111111.jsonl");
        append(&plan, &claude_line("plan this", 0));
        append(&manifest, r#"{"zoe":"job","v":1,"id":"j"}"#);
        append(
            &manifest,
            r#"{"member":"plan","path":"11111111-1111-1111-1111-111111111111.jsonl"}"#,
        );
        append(
            &manifest,
            r#"{"member":"plan","told":"use the helper","ts":"2026-10-06T10:05:00Z"}"#,
        );
        append(&manifest, r#"{"member":"no/such","told":"dropped"}"#);
        let mut out = Vec::new();
        assert!(!JobFeed::new(manifest).read(&mut out));
        let told: Vec<_> = out
            .iter()
            .flat_map(|s| &s.facts)
            .filter_map(|f| match &f.kind {
                FactKind::Told(t) => Some((f.agent.as_deref(), t.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(told, [(Some("plan"), "use the helper")]);
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
                Line::Group(group) => {
                    own.push(job::group_statement(&group));
                    continue;
                }
                Line::Gone(_) | Line::Told(_) => continue,
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

    /// A key's removal covers every session run under it, and only a manifest
    /// line shows them again, not what the sessions go on writing.
    #[test]
    fn a_member_key_is_removed_and_shown_again_by_manifest_lines() {
        let dir = temp_dir("gone");
        let manifest = dir.join("gone.jsonl");
        let (a, b) = (
            "11111111-1111-1111-1111-111111111111.jsonl",
            "22222222-2222-2222-2222-222222222222.jsonl",
        );
        append(&dir.join(a), &claude_line("one", 2));
        append(&dir.join(b), &claude_line("two", 3));
        append(&manifest, r#"{"zoe":"job","v":1,"id":"j"}"#);
        let member = |path: &str, ts: &str| {
            format!(r#"{{"member":"m","path":"{path}","ts":"2026-10-06T{ts}:00Z"}}"#)
        };
        append(&manifest, &member(a, "10:01"));
        append(&manifest, &member(b, "10:02"));
        append(&manifest, r#"{"gone":"m","ts":"2026-10-06T10:05:00Z"}"#);
        // Session records written after the removal do not undo it.
        append(&dir.join(a), &claude_line("later", 7));
        let model = fold(&read_job(&manifest));
        assert!(model.hidden("m") && model.hidden("m~2"));

        // The same session named again later shows the key.
        append(&manifest, &member(a, "10:10"));
        let model = fold(&read_job(&manifest));
        assert!(!model.hidden("m") && !model.hidden("m~2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every removal and statement of a key reaches the sessions that join
    /// under it later, however late their line is read.
    #[test]
    fn late_sessions_see_every_removal_and_statement_of_their_key() {
        let dir = temp_dir("late");
        let manifest = dir.join("late.jsonl");
        let (a, b) = (
            "11111111-1111-1111-1111-111111111111.jsonl",
            "22222222-2222-2222-2222-222222222222.jsonl",
        );
        append(&dir.join(a), &claude_line("one", 2));
        append(&dir.join(b), &claude_line("two", 3));
        append(&manifest, r#"{"zoe":"job","v":1,"id":"j"}"#);
        let line = |path: &str, ts: &str| {
            append(
                &manifest,
                &format!(r#"{{"member":"m","path":"{path}","ts":"2026-10-06T{ts}:00Z"}}"#),
            );
        };
        let gone = |ts: &str| {
            append(
                &manifest,
                &format!(r#"{{"gone":"m","ts":"2026-10-06T{ts}:00Z"}}"#),
            );
        };
        line(a, "10:01");
        gone("10:05");
        line(a, "10:10");
        // B's older line is read after the revival: it is shown too.
        line(b, "10:02");
        let model = fold(&read_job(&manifest));
        assert!(!model.hidden("m") && !model.hidden("m~2"));

        // Every removal counts: as of 10:06 both are hidden, and again after
        // the second removal.
        gone("10:15");
        let statements = read_job(&manifest);
        let until = |minute: u32| {
            let cut = format!("2026-10-06T10:{minute:02}:00Z")
                .parse::<chrono::DateTime<chrono::Utc>>()
                .unwrap();
            let kept: Vec<Statement> = statements
                .iter()
                .filter_map(|s| {
                    let facts: Vec<Fact> = s
                        .facts
                        .iter()
                        .filter(|f| f.ts.is_none_or(|t| t <= cut))
                        .cloned()
                        .collect();
                    (!facts.is_empty()).then_some(Statement { at: s.at, facts })
                })
                .collect();
            fold(&kept)
        };
        let early = until(6);
        assert!(early.hidden("m") && early.hidden("m~2"));
        let revived = until(11);
        assert!(!revived.hidden("m") && !revived.hidden("m~2"));
        let last = until(16);
        assert!(last.hidden("m") && last.hidden("m~2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A line stating a member shows its key at once, whether or not the
    /// session it names can be opened yet.
    #[test]
    fn a_pending_session_does_not_delay_a_revival() {
        let dir = temp_dir("pending");
        let manifest = dir.join("pending.jsonl");
        let a = "11111111-1111-1111-1111-111111111111.jsonl";
        append(&dir.join(a), &claude_line("one", 2));
        append(&manifest, r#"{"zoe":"job","v":1,"id":"j"}"#);
        append(
            &manifest,
            &format!(r#"{{"member":"m","path":"{a}","ts":"2026-10-06T10:01:00Z"}}"#),
        );
        append(&manifest, r#"{"gone":"m","ts":"2026-10-06T10:05:00Z"}"#);
        append(
            &manifest,
            r#"{"member":"m","path":"missing.jsonl","provider":"claude","ts":"2026-10-06T10:10:00Z"}"#,
        );
        let model = fold(&read_job(&manifest));
        assert!(model.agent("m").is_some() && !model.hidden("m"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
