//! Jobs: several sessions, of any provider, shown as one tree.
//!
//! An orchestrator that runs one agent after another (an explorer, a planner,
//! an implementer, a reviewer, each its own CLI process and its own session)
//! writes a manifest naming them. `zoe <manifest>` then draws the job as the
//! root and each session under it, with everything that session spawned below
//! that. The manifest is append-only JSONL, so a job is followed live like any
//! transcript: a member appears when its line is written, and each member's
//! files are tailed as they grow.
//!
//! ```text
//! {"zoe":"job","v":1,"id":"auth-fix","title":"Fix token refresh","task":"…","cwd":"/src/app","ts":"2026-10-06T09:00:00Z"}
//! {"member":"plan","label":"plan: Opus (claude)","provider":"claude","session":"6f1c2a9e-…","ts":"…"}
//! {"member":"review","label":"review: Sol (codex)","provider":"codex","path":"/home/me/.codex/sessions/…/rollout-….jsonl"}
//! ```
//!
//! The first line is the header (`"zoe":"job"`). Every other line names a
//! member: its key, an optional label and task, and its session by `path` (any
//! file of it; relative to the manifest's directory) or by `session` id, or
//! both (the path wins). `provider` is optional but saves reading the file to
//! find out, and is needed for a file that is still empty. Unknown keys and
//! unreadable lines are skipped.
//!
//! The manifest is the core's, not a provider's: it names sessions, and each
//! is opened and read by its own provider unchanged. What this module adds is
//! the one translation a shared tree needs, [`Member::rewrite`]: every member
//! calls its own root `"main"`, so a member's ids are renamed into its own
//! namespace and its root is hung under the job's.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::fact::{AgentId, AgentKind, Fact, FactKind, Statement};
use crate::provider::Provider;
use crate::state::session::MAIN_ID;

/// The header line's marker: `{"zoe":"job", …}`.
const MARKER: &str = "job";

/// The manifest's header, if the file's first non-blank line is one: how a
/// path is told apart from a transcript, by content like everything else.
pub fn read_header(manifest: &Path) -> Option<Header> {
    match parse_line(&crate::provider::read_head(manifest)?)? {
        Line::Header(header) => Some(header),
        Line::Member(_) => None,
    }
}

/// The manifest's first line: what the job is.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Header {
    zoe: String,
    /// Names the job; the manifest's file stem when absent.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// What the job was asked to do. The root's prompt.
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub ts: Option<DateTime<Utc>>,
}

/// A member line: one session of the job.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Entry {
    /// Stable key, e.g. the role (`"review"`). A resumed step names the same
    /// session again under the same key and changes nothing.
    pub member: String,
    #[serde(default)]
    pub label: Option<String>,
    /// What the member was asked to do.
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default, deserialize_with = "provider_name")]
    pub provider: Option<Provider>,
    /// When the member was launched.
    #[serde(default)]
    pub ts: Option<DateTime<Utc>>,
}

fn provider_name<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Provider>, D::Error> {
    Ok(Option::<String>::deserialize(d)?.and_then(|n| Provider::parse(&n)))
}

impl Entry {
    /// The member's file, relative paths taken from the manifest's directory.
    pub fn path_from(&self, manifest: &Path) -> Option<PathBuf> {
        let path = self.path.as_ref()?;
        Some(match manifest.parent() {
            Some(dir) if path.is_relative() => dir.join(path),
            _ => path.clone(),
        })
    }
}

/// One manifest line.
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    Header(Header),
    Member(Entry),
}

/// Parse one manifest line. `None` for a blank, unreadable or unknown line,
/// and for a member that names no session or has an unusable key.
pub fn parse_line(line: &str) -> Option<Line> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if value.get("zoe").is_some() {
        let header: Header = serde_json::from_value(value).ok()?;
        return (header.zoe == MARKER).then_some(Line::Header(header));
    }
    let entry: Entry = serde_json::from_value(value).ok()?;
    let usable = valid_key(&entry.member) && (entry.path.is_some() || entry.session.is_some());
    usable.then_some(Line::Member(entry))
}

/// A key names a node and prefixes its ids, so it is kept to characters that
/// cannot be mistaken for the separators (`/`, `~`) and is never the root's.
fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key != MAIN_ID
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// The id a job's model and events are stamped with.
pub fn session_id(header: &Header, manifest: &Path) -> String {
    let name = header.id.clone().unwrap_or_else(|| {
        manifest
            .file_stem()
            .map_or_else(String::new, |s| s.to_string_lossy().into_owned())
    });
    format!("job:{name}")
}

/// What the header states: the job as the root agent, its title, and its
/// task as the root's prompt.
pub fn header_statements(header: &Header, manifest: &Path) -> Vec<Statement> {
    let fact = |agent: Option<&str>, kind| Fact {
        agent: agent.map(str::to_string),
        ts: header.ts,
        kind,
    };
    let mut root = vec![fact(
        Some(MAIN_ID),
        FactKind::Agent {
            kind: AgentKind::Main,
            parent: None,
            agent_type: Some("job".into()),
            description: header.title.clone(),
            spawned_by: None,
            interactive: true,
        },
    )];
    if let Some(task) = &header.task {
        root.push(fact(Some(MAIN_ID), FactKind::Prompt(task.clone())));
    }
    let mut meta = vec![fact(
        None,
        FactKind::Session {
            label: "manifest".into(),
            value: manifest.display().to_string(),
        },
    )];
    if let Some(title) = &header.title {
        meta.push(fact(None, FactKind::Title(title.clone())));
    }
    if let Some(cwd) = &header.cwd {
        meta.push(fact(
            None,
            FactKind::Session {
                label: "cwd".into(),
                value: cwd.clone(),
            },
        ));
    }
    vec![
        Statement {
            at: header.ts,
            facts: root,
        },
        Statement {
            at: None,
            facts: meta,
        },
    ]
}

/// An overview: every session that ran under a folder, found as they appear
/// rather than named by a manifest, and grouped by the repository each ran in.
/// The folder is the root; each repository is a group under it, so its status
/// rolls up from its sessions.
pub struct Overview;

impl Overview {
    /// The id an overview's model and events are stamped with.
    pub fn session_id(folder: &Path) -> String {
        format!("folder:{}", folder.display())
    }

    /// The folder as the root agent, and its name as the title.
    pub fn statements(folder: &Path) -> Vec<Statement> {
        let name = folder_name(folder);
        let root = Fact {
            agent: Some(MAIN_ID.into()),
            ts: None,
            kind: FactKind::Agent {
                kind: AgentKind::Main,
                parent: None,
                agent_type: Some("overview".into()),
                description: Some(folder.display().to_string()),
                spawned_by: None,
                interactive: true,
            },
        };
        let meta = |kind| Fact {
            agent: None,
            ts: None,
            kind,
        };
        vec![
            root.into(),
            Statement {
                at: None,
                facts: vec![
                    meta(FactKind::Title(name)),
                    meta(FactKind::Session {
                        label: "folder".into(),
                        value: folder.display().to_string(),
                    }),
                ],
            },
        ]
    }

    /// Which repository under `folder` a session that ran in `cwd` belongs to,
    /// as the group it hangs under: the nearest folder holding a `.git` (a
    /// repository or a worktree), else the first folder below `folder`.
    /// `None` when `cwd` is not under `folder` at all.
    pub fn repository(folder: &Path, cwd: &Path) -> Option<String> {
        let below = relative(folder, cwd)?;
        let repository = cwd
            .ancestors()
            .take_while(|dir| relative(folder, dir).is_some())
            .find(|dir| dir.join(".git").exists())
            .and_then(|dir| relative(folder, dir));
        let name = match repository {
            Some(rel) => rel,
            None => below
                .components()
                .next()
                .map(|c| PathBuf::from(c.as_os_str()))
                .unwrap_or_default(),
        };
        let name = name.to_string_lossy().replace('\\', "/");
        Some(if name.is_empty() {
            folder_name(folder)
        } else {
            name
        })
    }

    /// A repository's group: born under the root, named after the repository.
    pub fn group(name: &str) -> (AgentId, Statement) {
        let id = format!("@{name}");
        let facts = vec![
            Fact {
                agent: Some(id.clone()),
                ts: None,
                kind: FactKind::Agent {
                    kind: AgentKind::Group,
                    parent: Some(MAIN_ID.into()),
                    agent_type: None,
                    description: None,
                    spawned_by: None,
                    interactive: false,
                },
            },
            Fact {
                agent: Some(id.clone()),
                ts: None,
                kind: FactKind::Label {
                    agent_type: Some(name.into()),
                    description: None,
                },
            },
        ];
        (id, Statement { at: None, facts })
    }
}

fn folder_name(folder: &Path) -> String {
    folder.file_name().map_or_else(
        || folder.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// `path` below `folder`, or `None` when it is not under it. Windows paths
/// compare without case, as the filesystem does.
fn relative(folder: &Path, path: &Path) -> Option<PathBuf> {
    if let Ok(rel) = path.strip_prefix(folder) {
        return Some(rel.to_path_buf());
    }
    if !cfg!(windows) {
        return None;
    }
    let norm = |p: &Path| {
        p.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    let (f, p) = (norm(folder), norm(path));
    let rest = p.strip_prefix(&f)?;
    (rest.is_empty() || rest.starts_with('\\'))
        .then(|| PathBuf::from(rest.trim_start_matches('\\')))
}

/// One session of a job, and how its facts are renamed into the job's tree.
///
/// The member's root becomes `root` (its key, or `key~2` for a second session
/// under the same key), any other agent `root/id`, and every call `root/call`,
/// so neither agent nor call ids collide between members. The root is born as
/// a subagent of the job's root (or of a group under it): a `claude -p` or
/// `codex exec` run is a batch process, so it is running while active and
/// done when it goes quiet.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub root: AgentId,
    parent: AgentId,
    label: String,
    task: Option<String>,
    ts: Option<DateTime<Utc>>,
    /// Found under a folder rather than named by a manifest, so nobody named
    /// or described it: its session does (see [`Member::found`]).
    found: bool,
}

impl Member {
    pub fn new(root: AgentId, entry: &Entry) -> Self {
        Member {
            label: entry.label.clone().unwrap_or_else(|| root.clone()),
            root,
            parent: MAIN_ID.into(),
            task: entry.task.clone(),
            ts: entry.ts,
            found: false,
        }
    }

    /// A session found under a folder rather than named by a manifest
    /// ([`Overview`]): hung under its repository's group, named by its title
    /// if its format records one and by its provider otherwise, and described
    /// by what it was first asked. Its session rows are left out: an overview
    /// of many sessions is not about any one's mode or version.
    pub fn found(root: AgentId, group: AgentId, provider: crate::provider::Provider) -> Self {
        Member {
            root,
            parent: group,
            label: provider.name().into(),
            task: None,
            ts: None,
            found: true,
        }
    }

    /// The member's node, stated when its manifest line is read (or its
    /// session is found), before its session has said anything.
    pub fn birth(&self) -> Statement {
        let mut facts = vec![Fact {
            agent: Some(self.root.clone()),
            ts: self.ts,
            kind: self.birth_kind(None),
        }];
        // A label only names an unnamed node, so the session's own title, an
        // agent's statement, wins in either order.
        if self.found {
            facts.push(Fact {
                agent: Some(self.root.clone()),
                ts: self.ts,
                kind: FactKind::Label {
                    agent_type: Some(self.label.clone()),
                    description: None,
                },
            });
        }
        Statement { at: self.ts, facts }
    }

    /// Identical wherever it is stated (the manifest line and the session's own
    /// root birth), so the two fold to the same node in either order. A found
    /// member's name comes from `title`, when its session states one.
    fn birth_kind(&self, title: Option<String>) -> FactKind {
        FactKind::Agent {
            kind: AgentKind::Subagent,
            parent: Some(self.parent.clone()),
            agent_type: if self.found {
                title
            } else {
                Some(self.label.clone())
            },
            description: self.task.clone(),
            spawned_by: None,
            interactive: false,
        }
    }

    fn agent(&self, id: &str) -> AgentId {
        if id == MAIN_ID {
            self.root.clone()
        } else {
            format!("{}/{id}", self.root)
        }
    }

    fn call(&self, id: &str) -> String {
        format!("{}/{id}", self.root)
    }

    /// The statement as the job's tree states it. Exhaustive over
    /// [`FactKind`], so a new kind has to decide what it means here.
    pub fn rewrite(&self, statement: Statement) -> Statement {
        let mut facts = Vec::with_capacity(statement.facts.len());
        for fact in statement.facts {
            let by_root = fact.agent.as_deref() == Some(MAIN_ID);
            let agent = fact.agent.as_deref().map(|a| self.agent(a));
            let ts = fact.ts;
            let kind = match fact.kind {
                FactKind::Agent {
                    kind: AgentKind::Main,
                    ..
                } => self.birth_kind(None),
                FactKind::Agent {
                    kind,
                    parent,
                    agent_type,
                    description,
                    spawned_by,
                    interactive,
                } => {
                    let group = parent
                        .as_deref()
                        .filter(|p| *p != MAIN_ID)
                        .map(|p| self.agent(p));
                    let rewritten = FactKind::Agent {
                        kind,
                        parent: parent.as_deref().map(|p| self.agent(p)),
                        agent_type,
                        description,
                        spawned_by: spawned_by.as_deref().map(|c| self.call(c)),
                        interactive,
                    };
                    facts.push(Fact {
                        agent,
                        ts,
                        kind: rewritten,
                    });
                    // A parent other than the root may be a group born from this
                    // child, which hangs under the job's root unless told
                    // otherwise. After the child, so the statement is still
                    // dated by the child's facts.
                    if let Some(group) = group {
                        facts.push(Fact {
                            agent: Some(group),
                            ts: None,
                            kind: FactKind::Agent {
                                kind: AgentKind::Group,
                                parent: Some(self.root.clone()),
                                agent_type: None,
                                description: None,
                                spawned_by: None,
                                interactive: false,
                            },
                        });
                    }
                    continue;
                }
                FactKind::ToolStart {
                    call,
                    name,
                    summary,
                } => FactKind::ToolStart {
                    call: self.call(&call),
                    name,
                    summary,
                },
                FactKind::ToolEnd { call, outcome } => FactKind::ToolEnd {
                    call: self.call(&call),
                    outcome,
                },
                FactKind::Spawn { call } => FactKind::Spawn {
                    call: self.call(&call),
                },
                // The member's prompts are the job's chapters too: the job's
                // spine is its root's prompts.
                FactKind::Prompt(text) if by_root => {
                    facts.push(Fact {
                        agent: Some(MAIN_ID.into()),
                        ts,
                        kind: FactKind::Prompt(text.clone()),
                    });
                    // A description only fills an empty one, so the first
                    // prompt describes the member.
                    if self.found {
                        facts.push(Fact {
                            agent: agent.clone(),
                            ts,
                            kind: FactKind::Label {
                                agent_type: None,
                                description: Some(text.clone()),
                            },
                        });
                    }
                    FactKind::Prompt(text)
                }
                // A found member is named by its title.
                FactKind::Title(title) if self.found => {
                    facts.push(Fact {
                        agent: Some(self.root.clone()),
                        ts,
                        kind: self.birth_kind(Some(title)),
                    });
                    continue;
                }
                FactKind::Session { .. } if self.found => continue,
                // Session metadata is the job's: a member's is kept, under the
                // member's name, so the job's own title and rows stay put.
                FactKind::Title(title) => FactKind::Session {
                    label: format!("{} title", self.root),
                    value: title,
                },
                FactKind::Session { label, value } => FactKind::Session {
                    label: format!("{} {label}", self.root),
                    value,
                },
                kind @ (FactKind::Activity
                | FactKind::Label { .. }
                | FactKind::Model(_)
                | FactKind::Tokens { .. }
                | FactKind::Prompt(_)
                | FactKind::Reasoning(_)
                | FactKind::Ended(_)
                | FactKind::Tally(_)) => kind,
            };
            facts.push(Fact { agent, ts, kind });
        }
        Statement {
            at: statement.at,
            facts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::Outcome;

    fn entry(member: &str) -> Entry {
        Entry {
            member: member.into(),
            label: Some(format!("{member}: someone")),
            task: Some("do it".into()),
            path: None,
            session: Some("s".into()),
            provider: None,
            ts: None,
        }
    }

    fn fact(agent: &str, kind: FactKind) -> Fact {
        Fact {
            agent: Some(agent.into()),
            ts: None,
            kind,
        }
    }

    fn rewrite(member: &Member, facts: Vec<Fact>) -> Vec<Fact> {
        member.rewrite(Statement { at: None, facts }).facts
    }

    #[test]
    fn a_manifest_is_told_by_its_header() {
        let dir = std::env::temp_dir().join(format!("zoetrope_job_header_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = |name: &str, text: &str| {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            path
        };
        let job = file(
            "job.jsonl",
            "\n{\"zoe\":\"job\",\"v\":1,\"id\":\"j\"}\n{\"member\":\"a\"}\n",
        );
        assert_eq!(read_header(&job).and_then(|h| h.id).as_deref(), Some("j"));
        assert!(read_header(&file("other.jsonl", r#"{"zoe":"other"}"#)).is_none());
        assert!(read_header(&file("claude.jsonl", r#"{"type":"user","message":{}}"#)).is_none());
        assert!(read_header(&file("empty.jsonl", "")).is_none());
        // No provider claims the header by content.
        assert_eq!(crate::provider::provider_of(r#"{"zoe":"job","v":1}"#), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn member_lines_need_a_session_and_a_usable_key() {
        let ok = parse_line(r#"{"member":"review","session":"abc","provider":"Codex","x":1}"#);
        let Some(Line::Member(e)) = ok else {
            panic!("expected a member, got {ok:?}");
        };
        assert_eq!(e.provider, Some(Provider::Codex));
        assert_eq!(e.session.as_deref(), Some("abc"));
        assert!(parse_line(r#"{"member":"review"}"#).is_none(), "no session");
        assert!(parse_line(r#"{"member":"main","session":"a"}"#).is_none());
        assert!(parse_line(r#"{"member":"a/b","session":"a"}"#).is_none());
        assert!(parse_line(r#"{"member":"a~2","session":"a"}"#).is_none());
        assert!(parse_line("not json").is_none());
    }

    #[test]
    fn relative_paths_resolve_from_the_manifest() {
        let mut e = entry("a");
        e.path = Some(PathBuf::from("sub/s.jsonl"));
        let manifest = Path::new("/jobs/j/live.jsonl");
        assert_eq!(
            e.path_from(manifest),
            Some(PathBuf::from("/jobs/j/sub/s.jsonl"))
        );
    }

    #[test]
    fn two_roots_named_main_become_two_members() {
        let main = || {
            fact(
                MAIN_ID,
                FactKind::Agent {
                    kind: AgentKind::Main,
                    parent: None,
                    agent_type: Some("claude".into()),
                    description: None,
                    spawned_by: None,
                    interactive: true,
                },
            )
        };
        let plan = Member::new("plan".into(), &entry("plan"));
        let review = Member::new("review".into(), &entry("review"));
        let a = rewrite(&plan, vec![main()]);
        let b = rewrite(&review, vec![main()]);
        assert_eq!(a[0].agent.as_deref(), Some("plan"));
        assert_eq!(b[0].agent.as_deref(), Some("review"));
        // The session's own root birth folds to the same node as the manifest's.
        assert_eq!(a[0].kind, plan.birth().facts[0].kind);
        assert!(matches!(
            &a[0].kind,
            FactKind::Agent { kind: AgentKind::Subagent, parent: Some(p), interactive: false, .. } if p == MAIN_ID
        ));
    }

    #[test]
    fn calls_and_agents_are_prefixed() {
        let m = Member::new("impl".into(), &entry("impl"));
        let out = rewrite(
            &m,
            vec![
                fact(
                    MAIN_ID,
                    FactKind::ToolStart {
                        call: "c1".into(),
                        name: "Task".into(),
                        summary: None,
                    },
                ),
                fact(MAIN_ID, FactKind::Spawn { call: "c1".into() }),
                fact(
                    "a1",
                    FactKind::ToolEnd {
                        call: "c2".into(),
                        outcome: Outcome::Ok,
                    },
                ),
                fact(
                    "a1",
                    FactKind::Agent {
                        kind: AgentKind::Subagent,
                        parent: Some(MAIN_ID.into()),
                        agent_type: None,
                        description: None,
                        spawned_by: Some("c1".into()),
                        interactive: false,
                    },
                ),
            ],
        );
        assert!(matches!(&out[0].kind, FactKind::ToolStart { call, .. } if call == "impl/c1"));
        assert!(matches!(&out[1].kind, FactKind::Spawn { call } if call == "impl/c1"));
        assert_eq!(out[2].agent.as_deref(), Some("impl/a1"));
        assert!(matches!(&out[2].kind, FactKind::ToolEnd { call, .. } if call == "impl/c2"));
        assert!(matches!(
            &out[3].kind,
            FactKind::Agent { parent: Some(p), spawned_by: Some(c), .. } if p == "impl" && c == "impl/c1"
        ));
        assert_eq!(out.len(), 4, "a child of the root names no group");
    }

    #[test]
    fn a_group_parent_is_hung_under_the_member() {
        let m = Member::new("impl".into(), &entry("impl"));
        let out = rewrite(
            &m,
            vec![fact(
                "a1",
                FactKind::Agent {
                    kind: AgentKind::Subagent,
                    parent: Some("wf1".into()),
                    agent_type: None,
                    description: None,
                    spawned_by: None,
                    interactive: false,
                },
            )],
        );
        assert_eq!(out[0].agent.as_deref(), Some("impl/a1"));
        assert_eq!(out[1].agent.as_deref(), Some("impl/wf1"));
        assert!(matches!(
            &out[1].kind,
            FactKind::Agent { kind: AgentKind::Group, parent: Some(p), .. } if p == "impl"
        ));

        // Folded in either order, the group hangs under the member.
        let birth = m.birth().facts;
        for order in [[0, 1, 2], [2, 0, 1]] {
            let facts: Vec<&Fact> = order
                .iter()
                .map(|&i| if i == 2 { &birth[0] } else { &out[i] })
                .collect();
            let mut model = crate::state::session::SessionModel::new("j".into());
            for f in facts {
                model.apply_fact(f);
            }
            let group = model.agent("impl/wf1").unwrap();
            assert_eq!(group.kind, AgentKind::Group);
            assert_eq!(group.parent.as_deref(), Some("impl"));
        }
    }

    #[test]
    fn a_group_birth_never_reparents_an_agent() {
        let mut model = crate::state::session::SessionModel::new("j".into());
        model.apply_fact(&fact(
            "x",
            FactKind::Agent {
                kind: AgentKind::Subagent,
                parent: Some("p".into()),
                agent_type: None,
                description: None,
                spawned_by: None,
                interactive: false,
            },
        ));
        model.apply_fact(&fact("p", FactKind::Activity));
        let before = model.agent("p").unwrap().clone();
        model.apply_fact(&fact(
            "x",
            FactKind::Agent {
                kind: AgentKind::Group,
                parent: Some("elsewhere".into()),
                agent_type: None,
                description: None,
                spawned_by: None,
                interactive: false,
            },
        ));
        assert_eq!(model.agent("x").unwrap().kind, AgentKind::Subagent);
        assert_eq!(model.agent("x").unwrap().parent.as_deref(), Some("p"));
        assert_eq!(model.agent("p").unwrap().kind, before.kind);
    }

    #[test]
    fn metadata_is_demoted_and_prompts_are_chapters() {
        let m = Member::new("plan".into(), &entry("plan"));
        let out = rewrite(
            &m,
            vec![
                Fact {
                    agent: None,
                    ts: None,
                    kind: FactKind::Title("my session".into()),
                },
                Fact {
                    agent: None,
                    ts: None,
                    kind: FactKind::Session {
                        label: "cwd".into(),
                        value: "/x".into(),
                    },
                },
                fact(MAIN_ID, FactKind::Prompt("plan this".into())),
                fact("sub", FactKind::Prompt("not a chapter".into())),
            ],
        );
        assert!(
            matches!(&out[0].kind, FactKind::Session { label, value } if label == "plan title" && value == "my session")
        );
        assert!(matches!(&out[1].kind, FactKind::Session { label, .. } if label == "plan cwd"));
        assert_eq!(out[2].agent.as_deref(), Some(MAIN_ID));
        assert_eq!(out[3].agent.as_deref(), Some("plan"));
        assert!(matches!(&out[2].kind, FactKind::Prompt(t) if t == "plan this"));
        assert_eq!(out[4].agent.as_deref(), Some("plan/sub"));
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn the_header_states_the_root_and_its_metadata() {
        let Some(Line::Header(h)) = parse_line(
            r#"{"zoe":"job","v":1,"id":"j1","title":"Fix it","task":"fix the bug","cwd":"/src"}"#,
        ) else {
            panic!("expected a header");
        };
        let manifest = Path::new("/jobs/j1/live.jsonl");
        assert_eq!(session_id(&h, manifest), "job:j1");
        assert_eq!(session_id(&Header::default(), manifest), "job:live");
        let st = header_statements(&h, manifest);
        assert!(matches!(
            &st[0].facts[0].kind,
            FactKind::Agent {
                kind: AgentKind::Main,
                ..
            }
        ));
        assert!(matches!(&st[0].facts[1].kind, FactKind::Prompt(t) if t == "fix the bug"));
        assert!(st[1].is_session_meta());
    }

    #[test]
    fn a_session_hangs_under_the_repository_it_ran_in() {
        let folder = std::env::temp_dir().join(format!("zoetrope_overview_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        for dir in ["app/.git", "app/src/deep", "tools/cli/.git", "notes/drafts"] {
            std::fs::create_dir_all(folder.join(dir)).unwrap();
        }
        let at = |rel: &str| Overview::repository(&folder, &folder.join(rel));
        assert_eq!(at("app/src/deep").as_deref(), Some("app"));
        assert_eq!(at("app").as_deref(), Some("app"));
        assert_eq!(
            at("tools/cli").as_deref(),
            Some("tools/cli"),
            "a nested repository"
        );
        assert_eq!(
            at("notes/drafts").as_deref(),
            Some("notes"),
            "no repository: the first folder"
        );
        let name = folder.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(at("").as_deref(), Some(name.as_str()), "the folder itself");
        assert!(Overview::repository(&folder, &std::env::temp_dir()).is_none());
        if cfg!(windows) {
            let upper = PathBuf::from(folder.to_string_lossy().to_uppercase()).join("app");
            assert_eq!(
                Overview::repository(&folder, &upper).as_deref(),
                Some("app")
            );
        }
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_found_session_is_named_by_its_title_else_its_provider() {
        let m = Member::found("claude-s1".into(), "@app".into(), Provider::Claude);
        let birth = m.birth().facts;
        let titled = rewrite(
            &m,
            vec![
                Fact {
                    agent: None,
                    ts: None,
                    kind: FactKind::Title("Fix the login".into()),
                },
                Fact {
                    agent: None,
                    ts: None,
                    kind: FactKind::Session {
                        label: "mode".into(),
                        value: "normal".into(),
                    },
                },
                fact(MAIN_ID, FactKind::Prompt("the login is broken".into())),
            ],
        );
        assert!(
            titled.iter().all(|f| !f.is_session_meta()),
            "no session rows"
        );
        for facts in [
            [&birth[..], &titled[..]].concat(),
            [&titled[..], &birth[..]].concat(),
        ] {
            let mut model = crate::state::session::SessionModel::new("o".into());
            for f in &facts {
                model.apply_fact(f);
            }
            let node = model.agent("claude-s1").unwrap();
            assert_eq!(node.agent_type.as_deref(), Some("Fix the login"));
            assert_eq!(node.description.as_deref(), Some("the login is broken"));
            assert_eq!(node.parent.as_deref(), Some("@app"));
        }
        let mut model = crate::state::session::SessionModel::new("o".into());
        for f in &birth {
            model.apply_fact(f);
        }
        let untitled = model.agent("claude-s1").unwrap();
        assert_eq!(untitled.agent_type.as_deref(), Some("claude"));
    }
}
