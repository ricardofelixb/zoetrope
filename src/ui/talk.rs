//! An agent's conversation, as the detail panel shows it: what it was asked,
//! what a person told it while it ran, and what it said, with the agents it
//! spawned indented under it. The root of a session, or a job, shows everyone's.
//! A conductor's card (a group of a job's manifest) shows only its dealings with
//! its agents, see [`log`]. Its tool calls are one line, see [`tools`].

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::state::session::{
    AgentKind, AgentStatus, Entry, EntryKind, MAIN_ID, SessionModel, ToolState,
};
use crate::ui::{truncate, wrap};

/// Wrapped lines of a prompt shown before it folds; `x` shows prompts whole.
const PROMPT_LINES: usize = 3;
/// The time column, `HH:MM` and two spaces, which bodies are indented past.
const TIME_COLS: usize = 7;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// What the panel calls an agent: the name its session or its spawner gave it.
fn name(model: &SessionModel, id: &str) -> String {
    model
        .agent(id)
        .and_then(|a| a.agent_type.clone().or_else(|| a.description.clone()))
        .unwrap_or_else(|| id.to_string())
}

/// One thing the conversation shows: an entry, or an agent starting (one
/// spawned by another, which has no prompt of its own in the format).
enum Item<'a> {
    Entry(&'a Entry),
    Started(&'a str, Option<DateTime<Utc>>),
}

impl Item<'_> {
    fn agent(&self) -> &str {
        match self {
            Item::Entry(e) => &e.agent,
            Item::Started(id, _) => id,
        }
    }

    fn ts(&self) -> Option<DateTime<Utc>> {
        match self {
            Item::Entry(e) => e.ts,
            Item::Started(_, ts) => *ts,
        }
    }
}

/// Every line of `scope`'s conversation, wrapped to `width`; `whole` shows long
/// prompts unfolded.
pub(crate) fn lines(
    model: &SessionModel,
    scope: &str,
    width: usize,
    whole: bool,
    palette: &rataflow::Palette,
) -> Vec<Line<'static>> {
    let job = crate::job::is_job_id(&model.session_id);
    let base =
        depth(model, scope) + usize::from(job && scope == MAIN_ID || conductor(model, scope));
    let subtle = Style::default().fg(palette.subtle);
    let code = Style::default().fg(palette.accent);
    let mut out = Vec::new();
    // The minute shown last: a time is shown only when it changes.
    let mut minute = String::new();
    for item in items(model, scope) {
        let id = item.agent();
        let depth = depth(model, id).saturating_sub(base);
        let indent = TIME_COLS + 2 * depth;
        let text_w = width.saturating_sub(indent + 1).max(8);
        let time = match item.ts() {
            Some(t) => {
                let at = t.with_timezone(&chrono::Local).format("%H:%M").to_string();
                let shown = if at == minute {
                    " ".repeat(5)
                } else {
                    at.clone()
                };
                minute = at;
                format!("{shown}  ")
            }
            None => " ".repeat(TIME_COLS),
        };
        let who = name(model, id);
        // The panel's own agent goes unnamed: its header says who it is. A
        // message of its own is just its time and its words.
        let own = id == scope;
        let to = |from: &str| {
            if own {
                from.to_string()
            } else {
                format!("{from} → {who}")
            }
        };
        let accent = Style::default().fg(palette.accent);
        let (header, head_style, body, body_style, fold) = match item {
            Item::Entry(e) => match e.kind {
                EntryKind::Prompt if job && id == MAIN_ID => (
                    Some("task".to_string()),
                    subtle,
                    e.text.as_str(),
                    subtle,
                    true,
                ),
                EntryKind::Prompt => {
                    let from = to(if job { "conductor" } else { "you" });
                    (Some(from), subtle, e.text.as_str(), subtle, true)
                }
                EntryKind::Told => (
                    Some(to("you")),
                    accent.add_modifier(Modifier::BOLD),
                    e.text.as_str(),
                    accent,
                    false,
                ),
                EntryKind::Waiting => continue,
                EntryKind::Thought => (
                    (!own).then(|| who.clone()),
                    subtle,
                    e.text.as_str(),
                    subtle.add_modifier(Modifier::ITALIC),
                    false,
                ),
                EntryKind::Message => (
                    (!own).then(|| who.clone()),
                    Style::default()
                        .fg(palette.text)
                        .add_modifier(Modifier::BOLD),
                    e.text.as_str(),
                    Style::default().fg(palette.text),
                    false,
                ),
            },
            Item::Started(..) => {
                let about = model.agent(id).and_then(|a| a.description.clone());
                let head = match about {
                    Some(about) if !about.is_empty() && about != who => {
                        format!("{who} started · {about}")
                    }
                    _ => format!("{who} started"),
                };
                (Some(head), subtle, "", subtle, false)
            }
        };
        if !out.is_empty() {
            out.push(Line::default());
        }
        let pad = " ".repeat(2 * depth);
        let mut time = Some(time);
        if let Some(header) = header {
            out.push(Line::from(vec![
                Span::styled(time.take().unwrap_or_default(), subtle),
                Span::raw(pad.clone()),
                Span::styled(truncate(&header, width.saturating_sub(indent)), head_style),
            ]));
        }

        // Each line, and whether it is in a code block: those keep their
        // indentation and lose their fences.
        let mut wrapped: Vec<(String, bool)> = Vec::new();
        let mut fenced = false;
        for para in plain(body).lines() {
            if para.trim_start().starts_with("```") {
                fenced = !fenced;
            } else if fenced {
                wrapped.push((truncate(para, text_w), true));
            } else if para.trim().is_empty() {
                wrapped.push((String::new(), false));
            } else {
                wrapped.extend(
                    wrap(para, text_w, usize::MAX)
                        .into_iter()
                        .map(|l| (l, false)),
                );
            }
        }
        while wrapped.last().is_some_and(|(l, _)| l.is_empty()) {
            wrapped.pop();
        }
        let hidden = if fold && !whole {
            wrapped.len().saturating_sub(PROMPT_LINES)
        } else {
            0
        };
        wrapped.truncate(wrapped.len() - hidden);
        while hidden > 0 && wrapped.last().is_some_and(|(l, _)| l.is_empty()) {
            wrapped.pop();
        }
        let margin = " ".repeat(indent);
        for (text, fenced) in wrapped {
            // Unheaded, the first line carries the time.
            let lead = match time.take() {
                Some(time) => Span::styled(format!("{time}{pad}"), subtle),
                None => Span::raw(margin.clone()),
            };
            let mut line = vec![lead];
            if fenced {
                line.push(Span::styled(text, code));
            } else {
                line.extend(inline_code(&text, body_style, code));
            }
            out.push(Line::from(line));
        }
        if hidden > 0 {
            out.push(Line::from(vec![
                Span::raw(margin.clone()),
                Span::styled(format!("… {hidden} more lines · x"), subtle),
            ]));
        }
    }
    out
}

/// A line's spans, its `inline code` in `code` without the backticks; a line
/// with an unpaired backtick reads as it is.
fn inline_code(text: &str, style: Style, code: Style) -> Vec<Span<'static>> {
    if text.matches('`').count() % 2 == 1 {
        return vec![Span::styled(text.to_string(), style)];
    }
    text.split('`')
        .enumerate()
        .filter(|(_, part)| !part.is_empty())
        .map(|(i, part)| Span::styled(part.to_string(), if i % 2 == 1 { code } else { style }))
        .collect()
}

/// How many things `scope`'s conversation shows, to count the new ones.
pub(crate) fn said(model: &SessionModel, scope: &str) -> usize {
    items(model, scope).len()
}

/// Whether `id` is `scope` or hangs somewhere below it.
fn within(model: &SessionModel, id: &str, scope: &str) -> bool {
    let mut at = Some(id.to_string());
    for _ in 0..32 {
        match at {
            Some(a) if a == scope => return true,
            Some(a) => at = model.agent(&a).and_then(|a| a.parent.clone()),
            None => return false,
        }
    }
    false
}

/// Whether `scope` is a conductor: a group of a job's manifest, which hangs
/// under the job's root (a member's own groups hang under the member).
fn conductor(model: &SessionModel, scope: &str) -> bool {
    crate::job::is_job_id(&model.session_id)
        && model
            .agent(scope)
            .is_some_and(|a| a.kind == AgentKind::Group && a.parent.as_deref() == Some(MAIN_ID))
}

/// A conductor's dealings with its agents, in time order: each brief it sent a
/// member (a member root's prompt), each message a person told one, and each
/// member's final report, its latest message at or before each of its turn
/// ends and not one already reported. Nothing else of theirs is shown.
fn log<'a>(model: &'a SessionModel, scope: &str) -> Vec<Item<'a>> {
    let feed: Vec<&Entry> = model
        .feed()
        .filter(|e| e.agent != scope && within(model, &e.agent, scope))
        .collect();
    let member = |id: &str| {
        model
            .agent(id)
            .is_some_and(|a| a.parent.as_deref() == Some(scope))
    };
    let mut latest: HashMap<&str, &Entry> = HashMap::new();
    let mut reported: HashMap<&str, &Entry> = HashMap::new();
    let mut items = Vec::new();
    for e in &feed {
        match e.kind {
            // The session holds a told message as a prompt that starts with it.
            EntryKind::Prompt
                if member(&e.agent)
                    && !feed.iter().any(|t| {
                        t.kind == EntryKind::Told
                            && t.agent == e.agent
                            && t.ts <= e.ts
                            && e.text.starts_with(t.text.as_str())
                    }) =>
            {
                items.push(Item::Entry(e));
            }
            EntryKind::Told => items.push(Item::Entry(e)),
            EntryKind::Message => {
                latest.insert(&e.agent, e);
            }
            EntryKind::Waiting if member(&e.agent) => {
                if let Some(&report) = latest.get(e.agent.as_str())
                    && reported.insert(&e.agent, report) != Some(report)
                {
                    items.push(Item::Entry(report));
                }
            }
            _ => {}
        }
    }
    items.sort_by_key(Item::ts);
    items
}

/// What the conversation shows, in time order. A job's root repeats each
/// member's prompts as its own chapters, and a message told while an agent ran
/// is in its session as a prompt that starts with it: neither is shown twice.
fn items<'a>(model: &'a SessionModel, scope: &str) -> Vec<Item<'a>> {
    if conductor(model, scope) {
        return log(model, scope);
    }
    let job = crate::job::is_job_id(&model.session_id);
    let feed: Vec<&Entry> = model
        .feed()
        .filter(|e| e.kind != EntryKind::Waiting && within(model, &e.agent, scope))
        .collect();
    let member_prompts: HashSet<(Option<DateTime<Utc>>, &str)> = feed
        .iter()
        .filter(|e| e.kind == EntryKind::Prompt && e.agent != MAIN_ID)
        .map(|e| (e.ts, e.text.as_str()))
        .collect();
    let told: Vec<&Entry> = feed
        .iter()
        .copied()
        .filter(|e| e.kind == EntryKind::Told)
        .collect();
    let prompted: HashSet<&str> = feed
        .iter()
        .filter(|e| !matches!(e.kind, EntryKind::Message | EntryKind::Thought))
        .map(|e| e.agent.as_str())
        .collect();
    let mut items: Vec<Item> = feed
        .iter()
        .copied()
        .filter(|e| {
            e.kind != EntryKind::Prompt
                || !(job && e.agent == MAIN_ID && member_prompts.contains(&(e.ts, e.text.as_str())))
                    && !told
                        .iter()
                        .any(|t| t.agent == e.agent && e.text.starts_with(t.text.as_str()))
        })
        .map(Item::Entry)
        .collect();
    for id in model.spawn_order() {
        let Some(agent) = model.agent(id) else {
            continue;
        };
        if id != MAIN_ID
            && agent.kind == AgentKind::Subagent
            && !prompted.contains(id)
            && within(model, id, scope)
        {
            items.push(Item::Started(id, agent.first_ts));
        }
    }
    items.sort_by_key(Item::ts);
    items
}

/// How far below the root an agent hangs.
fn depth(model: &SessionModel, id: &str) -> usize {
    let mut depth = 0;
    let mut at = model.agent(id).and_then(|a| a.parent.clone());
    while let Some(parent) = at {
        depth += 1;
        if depth > 32 {
            break;
        }
        at = model.agent(&parent).and_then(|a| a.parent.clone());
    }
    depth
}

/// All of `scope`'s tool calls, its subagents' included, as one line, animated
/// while any of them runs: the call in flight and how long it has taken (`now`
/// is the timeline's `now_reference`), or `thinking` between calls; the count
/// once all have stopped. `None` before anything happened.
pub(crate) fn tools(
    model: &SessionModel,
    scope: &str,
    width: usize,
    now: Option<DateTime<Utc>>,
    palette: &rataflow::Palette,
) -> Option<Line<'static>> {
    let mut count = 0usize;
    let mut failed = 0usize;
    let mut alive = false;
    let mut running: Option<&crate::state::session::ToolCallInfo> = None;
    for id in model.spawn_order().filter(|id| within(model, id, scope)) {
        let Some(agent) = model.agent(id) else {
            continue;
        };
        alive |= agent.status == AgentStatus::Running;
        for call in agent.tool_calls() {
            count += 1;
            failed += usize::from(call.state == ToolState::Err);
            // A call still pending in an agent that has stopped never ends.
            if call.state == ToolState::Pending
                && agent.status == AgentStatus::Running
                && running.is_none_or(|r| r.ts <= call.ts)
            {
                running = Some(call);
            }
        }
    }
    if count == 0 && !alive {
        return None;
    }
    let subtle = Style::default().fg(palette.subtle);
    let total = match count {
        0 => String::new(),
        1 => " · 1 tool".to_string(),
        _ => format!(" · {count} tools"),
    };
    let mut spans = Vec::new();
    if alive {
        let frame = web_time::SystemTime::now()
            .duration_since(web_time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() / 80) as usize;
        spans.push(Span::styled(
            format!("{} ", SPINNER[frame % SPINNER.len()]),
            Style::default().fg(palette.accent),
        ));
    }
    if let Some(call) = running {
        // How long it has run, once that is worth reading.
        let took = call
            .duration(now)
            .filter(|d| d.num_seconds() >= 1)
            .map(|d| format!(" · {}", crate::ui::chips::fmt_dur(d)))
            .unwrap_or_default();
        let what = describe(&call.name, call.summary.as_deref());
        let room = width.saturating_sub(total.len() + took.len() + 4);
        spans.push(Span::styled(
            truncate(&what, room),
            Style::default().fg(palette.text),
        ));
        spans.push(Span::styled(took, Style::default().fg(palette.accent)));
        spans.push(Span::styled(total, subtle));
    } else if alive {
        spans.push(Span::styled(format!("thinking{total}"), subtle));
    } else {
        spans.push(Span::styled(
            format!("✓ {}", total.trim_start_matches(" · ")),
            subtle,
        ));
    }
    if failed > 0 {
        spans.push(Span::styled(
            format!(" · ✗ {failed}"),
            Style::default().fg(palette.error),
        ));
    }
    Some(Line::from(spans))
}

/// A call as a verb and its object: `run cargo test`, `edit feed.rs`.
fn describe(tool: &str, summary: Option<&str>) -> String {
    // Codex runs its tools from one `exec` program, which its summary names as
    // `tool: argument` unless the program runs a command.
    if tool == "exec" {
        return match summary.and_then(|s| s.split_once(": ")) {
            Some((inner, rest)) if !inner.contains(' ') => format!("{} {rest}", verb(inner)),
            _ => format!("run {}", summary.unwrap_or_default()),
        };
    }
    match summary {
        Some(s) => format!("{} {s}", verb(tool)),
        None => verb(tool),
    }
}

/// A tool's name as the verb of a line: `run`, `read`, `edit`, `search`, or its
/// own name for the rest.
fn verb(tool: &str) -> String {
    match tool {
        "Bash" | "exec_command" | "shell" | "local_shell" | "PowerShell" => "run",
        "Read" | "NotebookRead" => "read",
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "apply_patch" => "edit",
        "Grep" | "Glob" | "web_search" | "WebSearch" => "search",
        "WebFetch" => "fetch",
        other => return other.to_lowercase(),
    }
    .to_string()
}

/// Markdown as it reads: a link as its text, emphasis without its stars.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let link = rest[open..].find("](").and_then(|mid| {
            let close = rest[open + mid..].find(')')?;
            Some((open + mid, open + mid + close))
        });
        match link {
            Some((mid, close)) if !rest[open + 1..mid].contains(['[', '\n']) => {
                out.push_str(&rest[..open]);
                out.push_str(&rest[open + 1..mid]);
                rest = &rest[close + 1..];
            }
            _ => {
                out.push_str(&rest[..=open]);
                rest = &rest[open + 1..];
            }
        }
    }
    out.push_str(rest);
    out.replace("**", "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::{Fact, FactKind, Outcome};

    fn at(minute: u32) -> Option<DateTime<Utc>> {
        Some(
            DateTime::parse_from_rfc3339(&format!("2026-10-06T10:{minute:02}:00Z"))
                .unwrap()
                .with_timezone(&Utc),
        )
    }

    /// `at(minute)` as the panel shows it, in local time.
    fn hm(minute: u32) -> String {
        at(minute)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%H:%M")
            .to_string()
    }

    fn apply(model: &mut SessionModel, agent: &str, minute: u32, kind: FactKind) {
        model.apply_fact(&Fact {
            agent: Some(agent.into()),
            ts: at(minute),
            kind,
        });
    }

    fn text(lines: impl IntoIterator<Item = Line<'static>>) -> String {
        lines
            .into_iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    fn job() -> SessionModel {
        let mut model = SessionModel::new("job:j".into());
        apply(
            &mut model,
            MAIN_ID,
            0,
            FactKind::Prompt("Fix the bug".into()),
        );
        for member in ["review", "plan"] {
            let birth = FactKind::Agent {
                kind: AgentKind::Subagent,
                parent: Some(MAIN_ID.into()),
                agent_type: Some(format!("{member}: someone")),
                description: None,
                spawned_by: None,
                interactive: false,
            };
            apply(&mut model, member, 1, birth);
        }
        let brief = "Review this change.\none\ntwo\nthree\nfour";
        apply(&mut model, "review", 1, FactKind::Prompt(brief.into()));
        apply(&mut model, MAIN_ID, 1, FactKind::Prompt(brief.into()));
        apply(
            &mut model,
            "plan",
            1,
            FactKind::Message("Here is the plan.".into()),
        );
        let call = |name: &str, summary: &str| FactKind::ToolStart {
            call: format!("c-{summary}"),
            name: name.into(),
            summary: Some(summary.into()),
        };
        apply(&mut model, "review", 2, call("Bash", "git diff"));
        apply(
            &mut model,
            "review",
            2,
            FactKind::ToolEnd {
                call: "c-git diff".into(),
                outcome: Outcome::Err,
            },
        );
        apply(
            &mut model,
            "review",
            3,
            FactKind::Told("that finding is intended".into()),
        );
        let delivered = "that finding is intended\n\n(A message from the user.)";
        apply(&mut model, "review", 3, FactKind::Prompt(delivered.into()));
        apply(
            &mut model,
            "review",
            4,
            FactKind::Message("VERDICT: **clean**".into()),
        );
        apply(
            &mut model,
            "review",
            5,
            call("exec", "exec_command: cargo test"),
        );
        model
    }

    /// A member's panel: its brief (folded), what the user told it and what it
    /// said, each once; nothing of the other member's.
    #[test]
    fn a_member_reads_as_its_conversation() {
        let model = job();
        let shown = text(lines(
            &model,
            "review",
            100,
            false,
            &rataflow::Palette::DARK,
        ));
        assert!(shown.contains("  conductor\n"), "{shown}");
        assert!(
            !shown.contains("review: someone"),
            "its own name, once, is the header's: {shown}"
        );
        assert!(shown.contains("… 2 more lines · x"), "{shown}");
        assert!(shown.contains("  you\n"), "{shown}");
        assert_eq!(
            shown.matches("that finding is intended").count(),
            1,
            "{shown}"
        );
        assert!(shown.contains("VERDICT: clean"), "{shown}");
        assert!(
            !shown.contains("Here is the plan") && !shown.contains("Fix the bug"),
            "{shown}"
        );
        assert_eq!(
            model.agent_count(),
            2,
            "told makes no agent, and the job's root has no card"
        );
    }

    /// The job's root shows everyone's, and its own copy of a prompt once.
    #[test]
    fn the_root_reads_as_the_whole_job() {
        let shown = text(lines(&job(), MAIN_ID, 100, true, &rataflow::Palette::DARK));
        assert!(
            shown.contains("task\n") && shown.contains("Fix the bug"),
            "{shown}"
        );
        assert_eq!(shown.matches("Review this change.").count(), 1, "{shown}");
        assert!(
            shown.contains("conductor → review: someone"),
            "others are named: {shown}"
        );
        assert!(shown.contains("  plan: someone\n"), "{shown}");
        assert!(
            shown.contains("four") && shown.contains("Here is the plan."),
            "{shown}"
        );
    }

    /// Every tool call is one line: the one in flight and how long it has run,
    /// `thinking` between calls, the count once all have stopped.
    #[test]
    fn all_the_tools_are_one_line() {
        let mut model = job();
        let line = |model: &SessionModel, scope| {
            text(tools(model, scope, 100, at(7), &rataflow::Palette::DARK))
        };
        let running = line(&model, "review");
        assert!(
            running.ends_with("run cargo test · 2m0s · 2 tools · ✗ 1\n"),
            "{running}"
        );
        assert!(SPINNER.iter().any(|s| running.starts_with(s)), "{running}");
        // Past the spinner, whose frame is the clock's.
        let still = |line: String| line.chars().skip(1).collect::<String>();
        assert_eq!(
            still(line(&model, MAIN_ID)),
            still(running),
            "the root counts its members'"
        );
        let plan = line(&model, "plan");
        assert!(
            plan.ends_with(" thinking\n"),
            "no calls, but working: {plan}"
        );
        apply(&mut model, "plan", 6, FactKind::Ended(AgentStatus::Done));
        assert_eq!(line(&model, "plan"), "", "no calls, done: no line");
        apply(&mut model, "review", 6, FactKind::Ended(AgentStatus::Done));
        assert_eq!(line(&model, "review"), "✓ 2 tools · ✗ 1\n");
    }

    /// Code reads apart from prose: blocks without their fences, inline code
    /// without its backticks.
    #[test]
    fn code_reads_apart() {
        let mut model = SessionModel::new("s".into());
        let said = "Run `cargo test` now:\n```sh\ncargo   test\n```";
        apply(&mut model, MAIN_ID, 0, FactKind::Message(said.into()));
        let palette = rataflow::Palette::DARK;
        let shown = lines(&model, MAIN_ID, 100, false, &palette);
        assert_eq!(
            text(shown.clone()),
            format!("{}  Run cargo test now:\n       cargo   test\n", hm(0))
        );
        let code = Style::default().fg(palette.accent);
        assert_eq!(shown[0].spans[2].content, "cargo test");
        assert_eq!(shown[0].spans[2].style, code);
        assert_eq!(shown[1].spans[1].style, code);
    }

    /// A time is shown when the minute changes, not on every message.
    #[test]
    fn a_time_once_a_minute() {
        let mut model = SessionModel::new("s".into());
        for (minute, said) in [(0, "one"), (0, "two"), (1, "three")] {
            apply(&mut model, MAIN_ID, minute, FactKind::Message(said.into()));
        }
        let shown = text(lines(&model, MAIN_ID, 100, false, &rataflow::Palette::DARK));
        assert_eq!(
            shown,
            format!("{}  one\n\n       two\n\n{}  three\n", hm(0), hm(1))
        );
    }

    #[test]
    fn markdown_reads_plain() {
        assert_eq!(
            plain("see [run.py:3](C:/r/run.py:3) **now**"),
            "see run.py:3 now"
        );
        assert_eq!(plain("a [b] c [d](e"), "a [b] c [d](e");
    }

    /// A conductor's card: its briefs, what the user told its agents and their
    /// final reports, in time order, and nothing else of theirs.
    #[test]
    fn a_conductor_reads_as_its_dealings_with_its_agents() {
        let mut model = SessionModel::new("job:j".into());
        let group = |parent: &str| FactKind::Agent {
            kind: AgentKind::Group,
            parent: Some(parent.into()),
            agent_type: None,
            description: None,
            spawned_by: None,
            interactive: false,
        };
        apply(&mut model, "c1", 0, group(MAIN_ID));
        apply(
            &mut model,
            "c1",
            0,
            FactKind::Label {
                agent_type: Some("Claude (conductor)".into()),
                description: None,
            },
        );
        let member = FactKind::Agent {
            kind: AgentKind::Subagent,
            parent: Some("c1".into()),
            agent_type: Some("review: Sol".into()),
            description: None,
            spawned_by: None,
            interactive: false,
        };
        apply(&mut model, "review", 1, member);
        apply(
            &mut model,
            "review",
            1,
            FactKind::Prompt("Review this change.".into()),
        );
        apply(
            &mut model,
            "review",
            2,
            FactKind::Message("Looking.".into()),
        );
        apply(&mut model, "review", 2, FactKind::Reasoning("Hmm.".into()));
        apply(
            &mut model,
            "review",
            2,
            FactKind::ToolStart {
                call: "t".into(),
                name: "Bash".into(),
                summary: Some("git diff".into()),
            },
        );
        apply(
            &mut model,
            "review",
            3,
            FactKind::Told("that is intended".into()),
        );
        apply(
            &mut model,
            "review",
            3,
            FactKind::Prompt("that is intended\n\n(wrapped)".into()),
        );
        apply(
            &mut model,
            "review",
            4,
            FactKind::Message("VERDICT: clean".into()),
        );
        apply(&mut model, "review", 5, FactKind::Waiting);
        // A subagent of the member, and a turn end of its own: not reports.
        let sub = FactKind::Agent {
            kind: AgentKind::Subagent,
            parent: Some("review".into()),
            agent_type: Some("helper".into()),
            description: None,
            spawned_by: None,
            interactive: false,
        };
        apply(&mut model, "review/a1", 4, sub);
        apply(
            &mut model,
            "review/a1",
            4,
            FactKind::Message("sub said".into()),
        );
        apply(&mut model, "review/a1", 5, FactKind::Waiting);

        assert!(conductor(&model, "c1"));
        assert!(!conductor(&model, MAIN_ID) && !conductor(&model, "review"));
        let shown = text(lines(&model, "c1", 100, false, &rataflow::Palette::DARK));
        assert!(shown.contains("conductor → review: Sol\n"), "{shown}");
        assert!(shown.contains("Review this change."), "{shown}");
        assert!(shown.contains("you → review: Sol\n"), "{shown}");
        assert_eq!(shown.matches("that is intended").count(), 1, "{shown}");
        assert!(shown.contains("VERDICT: clean"), "{shown}");
        for left_out in ["Looking.", "Hmm.", "sub said", "wrapped", "started"] {
            assert!(!shown.contains(left_out), "{left_out} in {shown}");
        }
        // The report is shown once however many turns end after it.
        apply(&mut model, "review", 6, FactKind::Waiting);
        let again = text(lines(&model, "c1", 100, false, &rataflow::Palette::DARK));
        assert_eq!(again.matches("VERDICT: clean").count(), 1, "{again}");
        // The tool line still counts every call under the group.
        assert!(tools(&model, "c1", 80, None, &rataflow::Palette::DARK).is_some());
    }

    /// A tell sent after a brief does not take the brief away, however much of
    /// it the tell repeats.
    #[test]
    fn a_later_tell_leaves_the_earlier_brief() {
        let mut model = SessionModel::new("job:j".into());
        let group = FactKind::Agent {
            kind: AgentKind::Group,
            parent: Some(MAIN_ID.into()),
            agent_type: Some("c".into()),
            description: None,
            spawned_by: None,
            interactive: false,
        };
        apply(&mut model, "c1", 0, group);
        let member = FactKind::Agent {
            kind: AgentKind::Subagent,
            parent: Some("c1".into()),
            agent_type: Some("review".into()),
            description: None,
            spawned_by: None,
            interactive: false,
        };
        apply(&mut model, "review", 1, member);
        let brief = FactKind::Prompt("Review this change.".into());
        apply(&mut model, "review", 1, brief);
        apply(&mut model, "review", 2, FactKind::Told("Review".into()));
        let shown = text(lines(&model, "c1", 100, false, &rataflow::Palette::DARK));
        assert!(shown.contains("Review this change."), "{shown}");
        assert!(shown.contains("you → review"), "{shown}");
    }
}
