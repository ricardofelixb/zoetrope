//! An agent's conversation, as the detail panel shows it: what it was asked,
//! what a person told it while it ran, and what it said, with the agents it
//! spawned indented under it. The root of a session, or a job, shows everyone's.
//! A conductor's card (a group of a job's manifest) shows only its dealings with
//! its agents, see [`log`]. Its tool calls are one line, see [`tools`].

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::state::DetailView;
use crate::state::session::{
    AgentKind, AgentStatus, Entry, EntryKind, MAIN_ID, SessionModel, ToolCallInfo, ToolState,
};
use crate::ui::{split_at_width, truncate, wrap};

/// Wrapped lines of a prompt shown before it folds; `x` shows prompts whole.
const PROMPT_LINES: usize = 3;
/// A report of more wrapped lines than this is previewed in the activity view:
/// its first `REPORT_HEAD` lines and its last `REPORT_TAIL`, where its outcome,
/// or a question for the reader, ends up. Shorter ones are whole.
const REPORT_LINES: usize = 8;
const REPORT_HEAD: usize = 4;
const REPORT_TAIL: usize = 2;
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

/// One thing the conversation shows: an entry, an agent starting (one spawned
/// by another, which has no prompt of its own in the format), or, in the
/// activity view, an agent's tool calls: one call, or a run of successful ones.
enum Item<'a> {
    Entry(&'a Entry),
    Started(&'a str, Option<DateTime<Utc>>),
    Calls(&'a str, Vec<&'a ToolCallInfo>),
}

impl Item<'_> {
    fn agent(&self) -> &str {
        match self {
            Item::Entry(e) => &e.agent,
            Item::Started(id, _) | Item::Calls(id, _) => id,
        }
    }

    fn ts(&self) -> Option<DateTime<Utc>> {
        match self {
            Item::Entry(e) => e.ts,
            Item::Started(_, ts) => *ts,
            Item::Calls(_, calls) => calls.first().and_then(|c| c.ts),
        }
    }
}

/// How much of an item's text shows before the rest folds, until `x`.
enum Fold {
    /// All of it.
    None,
    /// Its first `PROMPT_LINES`.
    Head,
    /// A long brief is one line, under this title; a short one is `Head`.
    Task(String),
    /// A long report is its head and its tail.
    Report,
}

/// What a line of text is.
enum Row {
    Text,
    Code,
    /// Where lines were folded away.
    Fold,
}

/// How a row of tool calls reads: failures and running calls stand out.
enum Tone {
    Quiet,
    Failed,
    Running,
}

/// Every line of `scope`'s conversation, wrapped to `width`; `whole` shows long
/// prompts unfolded.
pub(crate) fn lines(
    model: &SessionModel,
    scope: &str,
    width: usize,
    whole: bool,
    view: DetailView,
    now: Option<DateTime<Utc>>,
    palette: &rataflow::Palette,
) -> Vec<Line<'static>> {
    let activity = view == DetailView::Activity;
    let job = crate::job::is_job_id(&model.session_id);
    let base =
        depth(model, scope) + usize::from(job && scope == MAIN_ID || conductor(model, scope));
    let subtle = Style::default().fg(palette.subtle);
    let code = Style::default().fg(palette.accent);
    let mut out = Vec::new();
    // The minute shown last: a time is shown only when it changes.
    let mut minute = String::new();
    // Whether the last thing shown was tool calls, which sit close together.
    let mut calls_before = false;
    for item in items(model, scope, view) {
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
        let pad = " ".repeat(2 * depth);
        let margin = " ".repeat(indent);
        // A brief gives way to one line in the activity view; a person's own
        // words, and the transcript, fold at their first lines.
        let brief = |title: String| {
            if activity {
                Fold::Task(title)
            } else {
                Fold::Head
            }
        };
        let (header, head_style, body, body_style, fold) = match &item {
            Item::Entry(e) => match e.kind {
                EntryKind::Prompt if job && id == MAIN_ID => (
                    Some("task".to_string()),
                    subtle,
                    e.text.as_str(),
                    subtle,
                    brief("Task updated".to_string()),
                ),
                EntryKind::Prompt => {
                    let from = to(if job { "conductor" } else { "you" });
                    let fold = if job || id != MAIN_ID {
                        brief(format!("Task updated · {from}"))
                    } else {
                        Fold::Head
                    };
                    (Some(from), subtle, e.text.as_str(), subtle, fold)
                }
                EntryKind::Told => (
                    Some(to("you")),
                    accent.add_modifier(Modifier::BOLD),
                    e.text.as_str(),
                    accent,
                    Fold::None,
                ),
                EntryKind::Waiting => continue,
                EntryKind::Thought => (
                    (!own).then(|| who.clone()),
                    subtle,
                    e.text.as_str(),
                    subtle.add_modifier(Modifier::ITALIC),
                    Fold::None,
                ),
                EntryKind::Message => (
                    (!own).then(|| who.clone()),
                    Style::default()
                        .fg(palette.text)
                        .add_modifier(Modifier::BOLD),
                    e.text.as_str(),
                    Style::default().fg(palette.text),
                    if activity { Fold::Report } else { Fold::None },
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
                (Some(head), subtle, "", subtle, Fold::None)
            }
            // Tool calls are rows of their own, close together.
            Item::Calls(_, calls) => {
                if !out.is_empty() && !calls_before {
                    out.push(Line::default());
                }
                calls_before = true;
                let by = (!own).then_some(who.as_str());
                let rows = call_rows(model, id, by, calls, whole, text_w, now);
                let mut time = Some(time);
                for (row, kind) in rows {
                    let lead = match time.take() {
                        Some(time) => format!("{time}{pad}"),
                        None => margin.clone(),
                    };
                    let style = match kind {
                        Tone::Quiet => subtle,
                        Tone::Failed => Style::default()
                            .fg(palette.error)
                            .add_modifier(Modifier::BOLD),
                        Tone::Running => accent,
                    };
                    out.push(Line::from(vec![
                        Span::styled(lead, subtle),
                        Span::styled(truncate(&row, text_w), style),
                    ]));
                }
                continue;
            }
        };
        if !out.is_empty() {
            out.push(Line::default());
        }
        calls_before = false;
        let mut time = Some(time);

        // Each line, and whether it is in a code block: those keep their
        // indentation and lose their fences.
        let mut rows: Vec<(String, Row)> = Vec::new();
        let mut fenced = false;
        for para in plain(body).lines() {
            if para.trim_start().starts_with("```") {
                fenced = !fenced;
            } else if fenced {
                rows.push((truncate(para, text_w), Row::Code));
            } else if para.trim().is_empty() {
                rows.push((String::new(), Row::Text));
            } else {
                rows.extend(
                    wrap(para, text_w, usize::MAX)
                        .into_iter()
                        .map(|l| (l, Row::Text)),
                );
            }
        }
        while rows.last().is_some_and(|(l, _)| l.is_empty()) {
            rows.pop();
        }
        let mut header = header;
        let total = rows.len();
        // How much of it shows before the rest folds: the lines kept from its
        // start and from its end.
        let (head, tail) = match fold {
            _ if whole => (total, 0),
            Fold::None => (total, 0),
            Fold::Head => (PROMPT_LINES, 0),
            Fold::Task(title) if total > PROMPT_LINES => {
                header = Some(format!("{title} · {total} lines · x"));
                rows.clear();
                (0, 0)
            }
            Fold::Task(_) => (PROMPT_LINES, 0),
            Fold::Report if total > REPORT_LINES => (REPORT_HEAD, REPORT_TAIL),
            Fold::Report => (total, 0),
        };
        let hidden = rows.len().saturating_sub(head + tail);
        if hidden > 0 {
            let mut after = rows.split_off(rows.len() - tail);
            rows.truncate(head);
            while rows.last().is_some_and(|(l, _)| l.is_empty()) {
                rows.pop();
            }
            rows.push((format!("… {hidden} more lines · x"), Row::Fold));
            let blank = after.iter().take_while(|(l, _)| l.is_empty()).count();
            rows.extend(after.drain(blank..));
        }
        if let Some(header) = header {
            out.push(Line::from(vec![
                Span::styled(time.take().unwrap_or_default(), subtle),
                Span::raw(pad.clone()),
                Span::styled(truncate(&header, width.saturating_sub(indent)), head_style),
            ]));
        }
        for (text, row) in rows {
            // Unheaded, the first line carries the time.
            let lead = match time.take() {
                Some(time) => Span::styled(format!("{time}{pad}"), subtle),
                None => Span::raw(margin.clone()),
            };
            let mut line = vec![lead];
            match row {
                Row::Code => line.push(Span::styled(text, code)),
                Row::Fold => line.push(Span::styled(text, subtle)),
                Row::Text => line.extend(inline_code(&text, body_style, code)),
            }
            out.push(Line::from(line));
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

/// How many things `scope`'s conversation shows, to count the new ones. A
/// call is one however it is grouped: a call that ends joins the group before
/// it, which must not take a place from a message that came after.
pub(crate) fn said(model: &SessionModel, scope: &str, view: DetailView) -> usize {
    items(model, scope, view)
        .iter()
        .map(|item| match item {
            Item::Entry(e) if e.kind == EntryKind::Thought => 0,
            Item::Calls(_, calls) => calls.len(),
            _ => 1,
        })
        .sum()
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
/// Tool calls are items in both views, each its own in the transcript; the
/// activity view leaves out thinking and makes runs of successful ones one item.
fn items<'a>(model: &'a SessionModel, scope: &str, view: DetailView) -> Vec<Item<'a>> {
    if conductor(model, scope) {
        return log(model, scope);
    }
    let activity = view == DetailView::Activity;
    let job = crate::job::is_job_id(&model.session_id);
    let feed: Vec<&Entry> = model
        .feed()
        .filter(|e| {
            e.kind != EntryKind::Waiting
                && !(activity && e.kind == EntryKind::Thought)
                && within(model, &e.agent, scope)
        })
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
        if within(model, id, scope) {
            items.extend(agent.tool_calls().map(|call| Item::Calls(id, vec![call])));
        }
    }
    items.sort_by_key(Item::ts);
    if activity {
        items = grouped(items);
    }
    items
}

/// `items` with each run of one agent's successful tool calls made one item.
/// Anything else between two calls, another agent's included, splits the run.
fn grouped(items: Vec<Item<'_>>) -> Vec<Item<'_>> {
    let mut out: Vec<Item> = Vec::with_capacity(items.len());
    for item in items {
        if let Item::Calls(id, calls) = &item
            && calls[0].state == ToolState::Ok
            && let Some(Item::Calls(last, run)) = out.last_mut()
            && last == id
            && run[0].state == ToolState::Ok
        {
            run.extend(calls.iter().copied());
        } else {
            out.push(item);
        }
    }
    out
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

/// The rows a run of `id`'s tool calls shows, each with how it reads, `by` the
/// agent's name when it is not the panel's own. One call is a row of its own;
/// successful ones, several, are a count of what they did, unfolded by `whole`
/// into a row each. Only what the model recorded is said: a call in an agent
/// that is no longer running has no result, which is not a failure or a success.
/// `whole` also shows what each call was given and what it returned, wrapped to
/// `width`.
fn call_rows(
    model: &SessionModel,
    id: &str,
    by: Option<&str>,
    calls: &[&ToolCallInfo],
    whole: bool,
    width: usize,
    now: Option<DateTime<Utc>>,
) -> Vec<(String, Tone)> {
    let running = model
        .agent(id)
        .is_some_and(|a| a.status == AgentStatus::Running);
    let row = |call: &ToolCallInfo, by: &str| {
        let what = describe(&call.name, call.summary.as_deref());
        let took = call
            .duration(now)
            .filter(|d| d.num_seconds() >= 1)
            .map(|d| format!(" · {}", crate::ui::chips::fmt_dur(d)))
            .unwrap_or_default();
        match call.state {
            ToolState::Ok => (
                format!("{} {by}{what}{took}", AgentStatus::Done.glyph()),
                Tone::Quiet,
            ),
            ToolState::Err => (
                format!("{} {by}{what}{took}", AgentStatus::Failed.glyph()),
                Tone::Failed,
            ),
            ToolState::Pending if running => (
                format!("{} {by}{what}{took}", AgentStatus::Running.glyph()),
                Tone::Running,
            ),
            ToolState::Pending => (
                format!("{} {by}{what} · no result", AgentStatus::Idle.glyph()),
                Tone::Quiet,
            ),
        }
    };
    let by = by.map(|by| format!("{by} · ")).unwrap_or_default();
    // A call's row at `indent`, and under it, when `whole`, what it held.
    let shown = |call: &ToolCallInfo, by: &str, indent: usize| {
        let (head, tone) = row(call, by);
        let mut rows = vec![(format!("{}{head}", " ".repeat(indent)), tone)];
        if whole {
            rows.extend(detail(call, indent + 2, width));
        }
        rows
    };
    if calls.len() < 2 {
        return calls.iter().flat_map(|call| shown(call, &by, 0)).collect();
    }
    // Tools by what they did, in the order they first did it.
    let mut tally: Vec<(String, usize)> = Vec::new();
    for call in calls {
        let what = describe(&call.name, call.summary.as_deref());
        let did = what.split(' ').next().unwrap_or_default().to_string();
        match tally.iter_mut().find(|(d, _)| *d == did) {
            Some((_, n)) => *n += 1,
            None => tally.push((did, 1)),
        }
    }
    let tally: Vec<String> = tally.iter().map(|(d, n)| format!("{d} ×{n}")).collect();
    let fold = if whole { "" } else { " · x" };
    let head = format!(
        "{} {by}{} tools · {}{fold}",
        AgentStatus::Done.glyph(),
        calls.len(),
        tally.join(" · "),
    );
    let mut rows = vec![(head, Tone::Quiet)];
    if whole {
        rows.extend(calls.iter().flat_map(|call| shown(call, "", 2)));
    }
    rows
}

/// What a call was given and what it returned, whole, under its row: each line
/// as it was written, indented by `indent`, and wrapped at `width` columns
/// where it would run past them. Control characters are not drawn, so output
/// that colors itself cannot reach the terminal.
fn detail(call: &ToolCallInfo, indent: usize, width: usize) -> Vec<(String, Tone)> {
    let pad = " ".repeat(indent);
    let room = width.saturating_sub(indent + 2).max(8);
    [("input", &call.input), ("output", &call.output)]
        .into_iter()
        .filter_map(|(label, text)| Some((label, text.as_deref()?)))
        .flat_map(|(label, text)| {
            let lines = text.lines().flat_map(|line| cut(&drawable(line), room));
            let lines = lines.map(|line| (format!("{pad}  {line}"), Tone::Quiet));
            std::iter::once((format!("{pad}{label}"), Tone::Quiet)).chain(lines)
        })
        .collect()
}

/// `line` without the characters a terminal would act on, tabs as spaces.
fn drawable(line: &str) -> String {
    line.replace('\t', "    ")
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

/// `line` in pieces of at most `width` columns, cut where the width ends.
fn cut(line: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;
    let mut pieces = Vec::new();
    let mut rest = line;
    while rest.width() > width {
        let at = split_at_width(rest, width);
        pieces.push(rest[..at].to_string());
        rest = &rest[at..];
    }
    pieces.push(rest.to_string());
    pieces
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
            input: None,
        };
        apply(&mut model, "review", 2, call("Bash", "git diff"));
        apply(
            &mut model,
            "review",
            2,
            FactKind::ToolEnd {
                call: "c-git diff".into(),
                outcome: Outcome::Err,
                output: None,
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
            DetailView::Transcript,
            None,
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
        let shown = text(lines(
            &job(),
            MAIN_ID,
            100,
            true,
            DetailView::Transcript,
            None,
            &rataflow::Palette::DARK,
        ));
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
        let shown = lines(
            &model,
            MAIN_ID,
            100,
            false,
            DetailView::Transcript,
            None,
            &palette,
        );
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
        let shown = text(lines(
            &model,
            MAIN_ID,
            100,
            false,
            DetailView::Transcript,
            None,
            &rataflow::Palette::DARK,
        ));
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
                input: None,
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
        let shown = text(lines(
            &model,
            "c1",
            100,
            false,
            DetailView::Transcript,
            None,
            &rataflow::Palette::DARK,
        ));
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
        let again = text(lines(
            &model,
            "c1",
            100,
            false,
            DetailView::Transcript,
            None,
            &rataflow::Palette::DARK,
        ));
        assert_eq!(again.matches("VERDICT: clean").count(), 1, "{again}");
        // The tool line still counts every call under the group.
        assert!(tools(&model, "c1", 80, None, &rataflow::Palette::DARK).is_some());
    }

    /// `scope`'s panel text in `view`.
    fn show(
        model: &SessionModel,
        scope: &str,
        whole: bool,
        view: DetailView,
        now: Option<DateTime<Utc>>,
    ) -> String {
        text(lines(
            model,
            scope,
            100,
            whole,
            view,
            now,
            &rataflow::Palette::DARK,
        ))
    }

    fn start(model: &mut SessionModel, minute: u32, call: &str, name: &str, summary: &str) {
        let start = FactKind::ToolStart {
            call: call.into(),
            name: name.into(),
            summary: Some(summary.into()),
            input: None,
        };
        apply(model, MAIN_ID, minute, start);
    }

    fn finish(model: &mut SessionModel, minute: u32, call: &str, outcome: Outcome) {
        let end = FactKind::ToolEnd {
            call: call.into(),
            outcome,
            output: None,
        };
        apply(model, MAIN_ID, minute, end);
    }

    /// A session worked in the order: read, read, build, a failed test, two
    /// edits, a message, a read, and a check still running.
    fn work() -> SessionModel {
        let mut model = SessionModel::new("s".into());
        apply(&mut model, MAIN_ID, 0, FactKind::Message("On it.".into()));
        let calls = [
            (1, "Read", "a.rs", Outcome::Ok),
            (2, "Read", "b.rs", Outcome::Ok),
            (3, "Bash", "cargo build", Outcome::Ok),
            (4, "Bash", "cargo test", Outcome::Err),
            (6, "Edit", "a.rs", Outcome::Ok),
            (7, "Edit", "b.rs", Outcome::Ok),
        ];
        for (minute, name, summary, outcome) in calls {
            let call = format!("c{minute}");
            start(&mut model, minute, &call, name, summary);
            // The failed test took a minute; the rest were over at once.
            let took = u32::from(outcome == Outcome::Err);
            finish(&mut model, minute + took, &call, outcome);
        }
        let said = FactKind::Message("Fixed the test.".into());
        apply(&mut model, MAIN_ID, 8, said);
        start(&mut model, 9, "c9", "Read", "c.rs");
        finish(&mut model, 9, "c9", Outcome::Ok);
        start(&mut model, 10, "c10", "Bash", "cargo clippy");
        model
    }

    /// Activity runs of successful calls are one entry each, split by anything
    /// else; a failure and a running call stand alone, at their place in time;
    /// `x` lists every call. The transcript has none of it.
    #[test]
    fn activity_groups_successful_calls_and_keeps_the_rest_apart() {
        let model = work();
        let now = at(12);
        let activity = show(&model, MAIN_ID, false, DetailView::Activity, now);
        let order = [
            "On it.",
            "✓ 3 tools · read ×2 · run ×1 · x\n",
            "✗ run cargo test · 1m0s\n",
            "✓ 2 tools · edit ×2 · x\n",
            "Fixed the test.",
            "✓ read c.rs\n",
            "● run cargo clippy · 2m0s\n",
        ];
        let mut from = 0;
        for want in order {
            let found = activity[from..].find(want);
            assert!(found.is_some(), "{want:?} after {from} in {activity}");
            from += found.unwrap() + want.len();
        }
        assert_eq!(activity.matches('✗').count(), 1, "{activity}");
        assert!(!activity.contains("cargo build"), "{activity}");

        let whole = show(&model, MAIN_ID, true, DetailView::Activity, now);
        for call in ["read a.rs", "read b.rs", "run cargo build", "edit b.rs"] {
            assert!(whole.contains(call), "{call} in {whole}");
        }

        // The transcript has the same calls, each on a row of its own and none
        // folded, whether or not `x` is on.
        for whole in [false, true] {
            let transcript = show(&model, MAIN_ID, whole, DetailView::Transcript, now);
            for call in [
                "\u{2713} read a.rs\n",
                "\u{2713} read b.rs\n",
                "\u{2713} run cargo build\n",
                "\u{2717} run cargo test \u{b7} 1m0s\n",
                "\u{2713} edit b.rs\n",
                "\u{25cf} run cargo clippy \u{b7} 2m0s\n",
            ] {
                assert!(transcript.contains(call), "{call:?} in {transcript}");
            }
            assert!(!transcript.contains(" tools \u{b7} "), "{transcript}");
            assert!(transcript.contains("Fixed the test."), "{transcript}");
        }
    }

    /// What is new while scrolled up is counted by call, not by group: a call
    /// that ends joins the group before it, and a question after it is new.
    #[test]
    fn a_call_ending_into_a_group_does_not_hide_a_new_message() {
        for view in [DetailView::Activity, DetailView::Transcript] {
            let mut model = work();
            let seen = said(&model, MAIN_ID, view);
            finish(&mut model, 11, "c10", Outcome::Ok);
            assert_eq!(said(&model, MAIN_ID, view), seen, "{view:?}: nothing new");
            let ask = FactKind::Message("Shall I merge?".into());
            apply(&mut model, MAIN_ID, 12, ask);
            assert_eq!(said(&model, MAIN_ID, view), seen + 1, "{view:?}");
        }
    }

    /// A call is running only while its agent is: after that it has no result
    /// on record, which is not a failure and not a success.
    #[test]
    fn a_call_that_never_ended_is_not_shown_running() {
        let mut model = work();
        apply(&mut model, MAIN_ID, 11, FactKind::Ended(AgentStatus::Done));
        let shown = show(&model, MAIN_ID, false, DetailView::Activity, at(12));
        assert!(
            shown.contains("◌ run cargo clippy · no result\n"),
            "{shown}"
        );
        assert!(!shown.contains('●'), "{shown}");
    }

    /// Thinking and a long brief give way to a line; the brief and the user's
    /// words are whole when asked for or told, and the transcript keeps all.
    #[test]
    fn activity_leaves_out_thinking_and_folds_the_brief() {
        let mut model = job();
        let think = FactKind::Reasoning("Let me think.".into());
        apply(&mut model, "review", 6, think);
        let activity = show(&model, "review", false, DetailView::Activity, None);
        assert!(
            activity.contains("Task updated · conductor · 5 lines · x\n"),
            "{activity}"
        );
        assert!(!activity.contains("Review this change."), "{activity}");
        assert!(!activity.contains("Let me think."), "{activity}");
        assert!(activity.contains("that finding is intended"), "{activity}");
        assert!(activity.contains("VERDICT: clean"), "{activity}");

        let root = show(&model, MAIN_ID, false, DetailView::Activity, None);
        assert!(
            root.contains("Task updated · conductor → review: someone · 5 lines"),
            "{root}"
        );
        assert!(
            root.contains("Fix the bug"),
            "a short task reads as it is: {root}"
        );

        let whole = show(&model, "review", true, DetailView::Activity, None);
        assert!(whole.contains("Review this change.") && whole.contains("four"));
        assert!(!whole.contains("Task updated"), "{whole}");

        let full = show(&model, "review", false, DetailView::Transcript, None);
        assert!(full.contains("Let me think."), "{full}");
        assert!(full.contains("… 2 more lines · x"), "{full}");
        assert!(!full.contains("Task updated"), "{full}");
    }

    /// A long report is its first lines and its last, which is where an outcome
    /// or a question to the reader ends up; a short one is whole.
    #[test]
    fn activity_previews_a_long_report_by_its_head_and_tail() {
        let mut model = SessionModel::new("s".into());
        let report: Vec<String> = (1..=19)
            .map(|n| format!("step {n:02}"))
            .chain(["Shall I merge?".to_string()])
            .collect();
        let short = report[..8].join("\n");
        apply(&mut model, MAIN_ID, 0, FactKind::Message(short));
        apply(&mut model, MAIN_ID, 1, FactKind::Message(report.join("\n")));
        let activity = show(&model, MAIN_ID, false, DetailView::Activity, None);
        assert!(activity.contains("step 08"), "short is whole: {activity}");
        assert!(activity.contains("… 14 more lines · x"), "{activity}");
        for kept in ["step 01", "step 04", "step 19", "Shall I merge?"] {
            assert!(activity.contains(kept), "{kept} in {activity}");
        }
        assert!(!activity.contains("step 10"), "{activity}");

        for whole in [
            show(&model, MAIN_ID, true, DetailView::Activity, None),
            show(&model, MAIN_ID, false, DetailView::Transcript, None),
        ] {
            assert!(whole.contains("step 10") && !whole.contains("more lines"));
        }
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
        let shown = text(lines(
            &model,
            "c1",
            100,
            false,
            DetailView::Transcript,
            None,
            &rataflow::Palette::DARK,
        ));
        assert!(shown.contains("Review this change."), "{shown}");
        assert!(shown.contains("you → review"), "{shown}");
    }

    /// A model whose one call failed: a command too long for a row, and a
    /// diagnostic that is indented and several lines.
    fn failed_test() -> (SessionModel, String) {
        let mut model = SessionModel::new("s".into());
        let command = format!("cargo test {}", "--quiet ".repeat(30));
        let start = FactKind::ToolStart {
            call: "t".into(),
            name: "Bash".into(),
            summary: Some("cargo test".into()),
            input: Some(format!("command: {command}")),
        };
        apply(&mut model, MAIN_ID, 1, start);
        let output = "thread 'main' panicked at src/lib.rs:3\n    assertion failed: left == right\n\u{1b}[31m  left: 1\u{1b}[0m\tright: 2";
        let end = FactKind::ToolEnd {
            call: "t".into(),
            outcome: Outcome::Err,
            output: Some(output.into()),
        };
        apply(&mut model, MAIN_ID, 2, end);
        (model, command)
    }

    /// The whole of what a call was given and what came back is there on `x`,
    /// in either view, wrapped and not cut; without it the failure is the row.
    #[test]
    fn x_shows_a_calls_whole_input_and_output() {
        let (model, command) = failed_test();
        let squash = |s: &str| s.split_whitespace().collect::<String>();
        for view in [DetailView::Activity, DetailView::Transcript] {
            let folded = show(&model, MAIN_ID, false, view, None);
            assert!(folded.contains("\u{2717} run cargo test"), "{folded}");
            assert!(!folded.contains("panicked"), "{view:?}: {folded}");
            let whole = show(&model, MAIN_ID, true, view, None);
            assert!(whole.contains("\u{2717} run cargo test"), "{whole}");
            assert!(
                squash(&whole).contains(&squash(&command)),
                "{view:?}: {whole}"
            );
            assert!(
                whole.contains("thread 'main' panicked at src/lib.rs:3"),
                "{whole}"
            );
            // Its indentation is kept; the escape codes are not drawn.
            let indent = |needle: &str| {
                let line = whole.lines().find(|l| l.contains(needle)).unwrap();
                line.len() - line.trim_start().len()
            };
            assert_eq!(
                indent("assertion failed"),
                indent("thread 'main'") + 4,
                "{whole}"
            );
            assert!(!whole.contains('\u{1b}'), "{whole}");
            assert!(whole.contains("left: 1"), "{whole}");
        }
    }

    /// Each call of an unfolded group has its own, under its own row.
    #[test]
    fn x_shows_each_call_of_a_group_its_own_output() {
        let mut model = SessionModel::new("s".into());
        for (minute, name, out) in [(1, "a.rs", "alpha text"), (2, "b.rs", "beta text")] {
            let call = format!("c{minute}");
            let start = FactKind::ToolStart {
                call: call.clone(),
                name: "Read".into(),
                summary: Some(name.into()),
                input: None,
            };
            apply(&mut model, MAIN_ID, minute, start);
            let end = FactKind::ToolEnd {
                call,
                outcome: Outcome::Ok,
                output: Some(out.into()),
            };
            apply(&mut model, MAIN_ID, minute, end);
        }
        let folded = show(&model, MAIN_ID, false, DetailView::Activity, None);
        assert!(
            folded.contains("2 tools") && !folded.contains("alpha"),
            "{folded}"
        );
        let whole = show(&model, MAIN_ID, true, DetailView::Activity, None);
        let (a, b) = (whole.find("read a.rs"), whole.find("read b.rs"));
        let (alpha, beta) = (whole.find("alpha text"), whole.find("beta text"));
        assert!(a < alpha && alpha < b && b < beta, "{whole}");
    }
}
