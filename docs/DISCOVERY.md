# Discovery: how a feeder finds a session

The input side of the provider boundary. [ARCHITECTURE.md](ARCHITECTURE.md) §0 fixed the output side: a provider turns records into `Fact`s and nothing past `src/fact.rs` knows the format. This document fixes the other direction, how a feeder gets from what the user pointed at to a set of files with a stream each, and how a session list gets from the disk to sessions. It was written with two providers in hand, Claude Code and Codex, and checked against the storage of the other agents we know of (§7). The code is `src/provider/mod.rs`.

The rule, one level above the fact rule:

> **A provider states what a file is. The core decides what a session is.**

A provider never assembles a session, never resolves an id, never diffs a rescan, never tails. It answers questions about single paths in its own layout, and one pure function in the core builds sessions out of the answers.

---

## 1. The two operations

Feeders see two operations, both in `src/provider/mod.rs`, both written once:

| Operation | Question | Callers |
|---|---|---|
| `open(target, only) -> Result<Session, OpenError>` | One session: its provider, id, and every file of it known right now | replay, follow, `inspect`, herdr (path or id) |
| `sweep(scope, only) -> Vec<Session>` | Every session under every provider's roots that the scope admits, newest first | `open` by id and by directory, the newer-session auto-switch, and later the rail and `zoe sessions` |

`open` is `sweep` narrowed to one session: both feed paths through the same provider question and the same core assembly. The only difference is which paths go in (§4). `only` forces one provider (`--provider`); `None` reads it off the content.

The browser build has no filesystem and uses neither. It receives every file of a session as `(path, text)` and hands them to `tailer::Bundle`, which classifies each with `Provider::session_file_from` (the same answers as `session_file`, from the path and the first line instead of the disk), keeps one stream per tailed file for later appends, and states each sidecar once. So the page never learns a format either.

---

## 2. Shared types

```rust
// src/provider/mod.rs

pub enum Provider { Claude, Codex }

/// What a provider states about one path. The input-side analogue of `Fact`.
pub struct SessionFile {
    pub provider: Provider,
    pub path: PathBuf,
    pub session: String,        // the session it belongs to
    pub role: FileRole,
    pub read: ReadMode,         // Tail (line by line as it grows) or Whole (once it parses)
    pub project_key: String,    // compare with Provider::project_key(cwd), never as a path
    pub modified: SystemTime,
}

pub enum FileRole {
    Root,                       // the session's own transcript
    Agent { parent: String },   // a spawned agent's transcript
    Sidecar,                    // session-level file that is nobody's transcript (meta, ledger)
}

/// What the core assembles. Never produced by a provider.
pub struct Session {
    pub provider: Provider,
    pub id: String,
    pub project_key: String,
    pub root: SessionFile,
    pub files: Vec<SessionFile>, // agents and sidecars, root excluded
    pub last_modified: SystemTime,
}

/// One per-file parser. `push` is the whole reading contract.
pub enum Stream { Claude(claude::Stream), Codex(codex::Stream) }
impl Stream {
    pub fn push(&mut self, line: &str) -> Option<Statement>;
}

pub enum Target { Path(PathBuf), Id(String), Here(PathBuf) }
pub struct Scope { pub project: Option<PathBuf>, pub since: Option<SystemTime>, pub id_prefix: Option<String> }
```

Enums, not traits. Providers arrive by pull request, never from outside the crate, and an exhaustive `match` makes the compiler list every site a new provider must touch. There is no `dyn` anywhere on this path.

`session` is known from any file. Every format seen names the root from every file of a session: Claude by directory, Codex by `session_meta.session_id` on a child's first line. So `assemble` groups by `(provider, session)` and never walks a parent chain. `FileRole::Agent { parent }` keeps the spawner for information; the fact stream states it again with more detail.

---

## 3. The primitives

Each provider implements these in `src/provider/<name>/discovery.rs`, about its own layout only. The core calls them through `impl Provider`, one `match` per method.

| Primitive | Question it answers | Claude Code | Codex |
|---|---|---|---|
| `provider_of(head: &str) -> Option<Provider>` (free function) | Which provider wrote this text? Reads the first record. | a top-level `type` and no `payload` | first record is `type: "session_meta"`, or any record with a `payload` |
| `all_paths(scope) -> Vec<PathBuf>` | Every path that could be a session file, across the provider's roots. Prunes by the scope where the layout lets it. | `~/.claude/projects/*/<uuid>.jsonl`, roots only; a project scope narrows to one directory, `since` filters by mtime, an id prefix by file stem | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, roots and children; `since` drops whole day directories, an id prefix keeps the rollouts whose name ends in a matching thread id (the root's name carries the session id); a project scope cannot prune, the project is inside the file |
| `session_file(path) -> Option<SessionFile>` | What is this file? Which session, which role, which project, how is it read. | by path: a `.jsonl` in a project directory is `Root`; `<uuid>/subagents/agent-*.jsonl` and `subagents/workflows/<wf>/agent-*.jsonl` are `Agent`; `*.meta.json` is `Sidecar` read `Whole`; `workflows/<wf>/journal.jsonl` is `Sidecar` read `Tail` | by content: the file's own `session_meta`. `thread_source: "user"` is `Root`; `"subagent"` is `Agent { parent: source.subagent.thread_spawn.parent_thread_id }` with `session = session_id` |
| `related_paths(file) -> Vec<PathBuf>` | Where can the rest of this file's session be, root included when `file` is not it? | the root beside the `<uuid>` directory and everything under `<uuid>/subagents/` | from a root: rollouts between its day and the day of its last write, since a child is spawned while the root runs and the root writes after every spawn; from a child: every rollout in the tree, the root may be in an earlier day |
| `project_key(cwd) -> String` | How does this provider name a project? | `sanitize_cwd(cwd)`, the directory name under `projects/` | the path itself, as `session_meta.cwd` records it |
| `stream_for(file) -> Stream` | A parser for a tailed file, with whatever cross-line state the format needs | `claude::Stream` over `Source::Main`, `Sub(agent)`, or `Ledger(wf)`, derived from the path; state: the inherited timestamp, and whether the root has been stated | `codex::Stream`; state: the thread id, the root id, the ordinal below which the file is replayed parent history |
| `sidecar(file, text) -> Option<Statement>` | What does a whole-read sidecar state, once its text parses? | `agent-<id>.meta.json`: the agent's birth, `Stream::meta` | none |
| `session_file_from(path, head) -> Option<SessionFile>` | `session_file` without a filesystem: the path a file came with and its first bytes | by path, as `session_file` | by the first line, as `session_file` |

Three things these answers show:

- **`project_key` is opaque.** Claude stores a lossy sanitized path, Codex the path, other agents a hash. The core never compares a key to a directory; it compares a key to `project_key(cwd)`.
- **`related_paths` may over-include.** A Codex date directory holds every session of that day; `session_file` on each path sorts them out. Over-include on layout, let content decide.
- **`ReadMode` exists because Claude's `meta.json` is one JSON document with no trailing newline.** A line tailer never sees a complete line of it. Whole-read files are read in full each tick until they parse (a mid-write read fails and is retried), then stated once.

---

## 4. The core

Four functions, provider-agnostic, in `src/provider/mod.rs`:

```rust
/// Group files into sessions by (provider, session). A group without a root
/// file is not a session and is dropped: an orphan is not something we can show.
pub fn assemble(files: Vec<SessionFile>) -> Vec<Session>

pub fn sweep(scope: &Scope, only: Option<Provider>) -> Vec<Session> {
    // every provider (or `only`): all_paths(scope) → session_file → assemble → scope.admits
}

pub fn open(target: &Target, only: Option<Provider>) -> Result<Session, OpenError> {
    let file = match target {
        Target::Path(path) => {
            // Content first; failing that (an empty, just-created file), the
            // first provider whose layout claims the path.
            let p = only.or_else(|| provider_of(head(path))).or_else(|| layout_claims(path))?;
            p.session_file(path)?          // any file of a session opens the session
        }
        Target::Id(prefix) => return sweep(Scope::id(prefix)).exactly_one(),  // providers prune by name where they can
        Target::Here(cwd)  => return sweep(project(cwd)).first(),
    };
    // related_paths(file) → session_file → keep file.session → assemble → the one session
}

impl Session {
    /// Files that belong to this session and were not known before: the live
    /// tailer's per-tick scan. related_paths(root) → session_file → diff.
    pub fn rescan(&mut self) -> Vec<SessionFile>
}
```

`Scope::project(cwd)` admits a session when `session.project_key == session.provider.project_key(cwd)`. Project mode and workspace mode (P2 and P3 in the multi-session plan) are one sweep with this filter. `since` bounds the auto-switch scan (§5).

What used to be five feeder sites naming `claude::` are these calls. The live tailer's subagent scan is `rescan`. Its newer-session auto-switch is `sweep(project(cwd).since(now - 24h))`: a session newer than the followed one is by definition recent, and the bound keeps a provider that classifies by content from reading every file it ever wrote, every two seconds.

---

## 5. The feeders after this

- **Replay and follow** (`tailer/replay.rs`, `tailer/live.rs`): `open`, then one `Stream` per tailed file and one `sidecar` statement per whole-read file. Every tick: read appended bytes through each stream, `rescan` for files that appeared, state sidecars that now parse. `Flow::Switch { target, follow }` carries the working directory being followed, so a re-attach after truncation keeps following and a named file or id stays pinned.
- **`inspect`** (`main.rs`): `open`, read every file, fold. Session-level facts go to the info header whichever record carried them (a Codex root names itself and its app on one line; `Statement::take_session_meta` splits it).
- **The browser** (`web/wasm`): no filesystem. The page reads files (a drop, an upload, or a directory it may keep re-reading) and passes `[{path, text}]` to `zoetrope_load`; `tailer::Bundle` does the rest through `session_file_from`, `stream_for` and `sidecar`, and `zoetrope_append` continues the same streams. The page's own job is finding files: Claude by the `<uuid>.jsonl` and `<uuid>/subagents/` layout, Codex by reading each rollout's first line, which is `session_file`'s logic written a second time in JavaScript because the page cannot call it before the files are read.
- **The herdr plugin** (`herdr-plugin/`): asks Herdr which session the focused
  pane is running, hands `zoe` that id, and `open` does the rest. It is the
  case `Target::Id` was written for: the id is known, the file is not. See
  [HERDR-PLUGIN.md](HERDR-PLUGIN.md).

---

## 6. The CLI

| Invocation | Resolves to |
|---|---|
| `zoe <file>` | `open(Path)`. Provider by content. Works for any provider's root or agent file. |
| `zoe <id-prefix>` | `open(Id)`. Across every provider's roots; an ambiguous prefix is an error listing the matches. |
| `zoe` | `open(Here)`: newest session in this project, any provider, then followed. |
| `zoe <dir>` | `open(Here(dir))`: the same for another project. |
| `zoe --provider codex ...` | Forces the provider for a path when `provider_of` cannot tell, or restricts an id or directory lookup to one provider. Never the default route. |
| `zoe inspect <file>` | `open(Path)` rendered as text. |
| `zoe <job.jsonl>` | `Target::Job`: a file whose first line is a job header. The core reads it (§8); `open` refuses it. |

Later, with the rail: `zoe sessions` as `sweep` rendered as a table.

---

## 7. Scope, and the test for changing it

The agents whose storage we know fall into three classes:

1. **Append-only JSONL per session.** Claude Code, Codex, Copilot CLI, older Goose. This document covers them fully.
2. **One JSON document rewritten per turn.** Gemini CLI, Cline and its forks, Amp. Discovery covers them (`related_paths` is empty or a sibling document, `Sidecar` read `Whole` holds the second document where there is one); reading does not, since there is no line to push. Support would add `Stream::push_document`, diffing against the last document, beside `push`. An addition, not a change.
3. **A database.** OpenCode, Cursor, current Goose. No file per session, nothing to tail. Out of scope. That is a different kind of feeder reading rows, not a wider `SessionFile`.

Aider keeps many sessions in one markdown file per project and fits none of the above. Not designed for.

The abstraction here is over **feeders**, not over agents. The parts shared are the tailer loop, `assemble`, id lookup, rescan diffing and the rail; the parts that differ per agent, the primitives, are not shared and were never going to be. So the test for any future "agent X needs a primitive changed" is: does a feeder actually behave differently for X? If not, the primitive was wrong and should change. If it does, X is a new class and gets a sibling, not a wider interface.

---

## 8. Jobs: several sessions as one tree

An orchestrator that runs agents one after another, each its own CLI process and its own session (an explorer, a planner, an implementer, a reviewer, any provider), writes a **manifest** naming them. `zoe <manifest>` draws the job as the root, each session under it, and everything a session spawned below that. The code is `src/job.rs` (portable: the format and the rewrite) and `src/tailer/job.rs` (native: the feeder).

The manifest is append-only JSONL, so a job is followed like a transcript. The first line is the header; every other line names a member:

```json
{"zoe":"job","v":1,"id":"auth-fix","title":"Fix token refresh","task":"Fix the token refresh race","cwd":"/src/app","ts":"2026-10-06T09:00:00Z"}
{"member":"plan","label":"plan: Opus (claude)","task":"Plan the fix","provider":"claude","session":"6f1c2a9e-…","ts":"2026-10-06T09:00:02Z"}
{"member":"review","label":"review: Sol (codex)","provider":"codex","path":"/home/me/.codex/sessions/2026/10/06/rollout-….jsonl"}
```

- A member names its session by `path` (any file of it, relative to the manifest's directory) or `session` id, or both; the path wins. `provider` is optional but saves the read, and is needed while the file is still empty, since Claude's layout claims any `.jsonl`. The key is `[A-Za-z0-9_.-]+` and never `main`. Unknown keys and unreadable lines are skipped. The header must be written first: a manifest is told apart from a transcript by its first line.
- **A manifest is not a session.** No provider reads it and `open` refuses it. The feeder opens each member with `open`, so a member's subagents and workflows are found by its own provider's `rescan`, exactly as for that session alone.
- **One rewrite** (`Member::rewrite`, exhaustive over `FactKind`) puts each member in its own namespace: its root `main` becomes the member's key, any other agent `key/id`, every call `key/call`. The member's root is born a subagent of the job's root (a `claude -p` or `codex exec` run is a batch process: running while active, done when quiet), identically from the manifest line and from the session's own first record, so either may come first. A group a member's child names is stated under the member, since a group born from its first child otherwise hangs under the root. A member's title and session rows are kept under its key, so the job's own stay put; its root's prompts are also the job's chapters. On joining, a member also states its provider and session id as rows under its key (`plan provider`, `plan session`), which is what `enter` resumes, from the member's own `cwd` row or else the job's.
- The same key naming the same session again (a resumed step) changes nothing; a new session under a used key joins as `key~2`; a session another key already claimed is ignored.
- A truncated manifest or member file re-attaches the whole job, as a truncated session file re-attaches the session. Replay reads everything up front and keeps tailing, as a session's replay does.

Two rules in the model make this order-independent and are not job-specific: a group placeholder that later speaks for itself takes the kind it states (a Codex thread read after its child is the other case), and the prompt spine is kept in time order rather than arrival order (one file's arrival order already is; several files' is not).
