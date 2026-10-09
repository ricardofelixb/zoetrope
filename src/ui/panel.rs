//! Detail panel for the selected agent.
//!
//! When an agent node is selected, the main area splits (in half until its left
//! edge is dragged, [`App::panel_share`]) and this panel
//! shows the agent: a short header (name, status, model, timing, what it was
//! for), then its conversation, scrollable, which follows the newest line until
//! scrolled up (see [`crate::ui::talk`]), and all its tool calls as one line at
//! the bottom. All data comes from the `SessionModel`, keyed by the node id.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};

use chrono::{DateTime, Utc};

use crate::state::App;
use crate::state::session::AgentInfo;
use crate::ui::{talk, truncate};

/// Render the detail panel for `agent_id` into `area`.
///
/// `agent_id` is copied out of the flow before this call to avoid borrowing
/// `app` both immutably (selection) and the panel state. Uses
/// `app.detail_scroll` for the conversation's scroll offset.
pub fn render(frame: &mut Frame, area: Rect, app: &mut App, agent_id: &str) {
    let palette = app.flow.theme.palette();
    let App {
        session,
        detail_scroll,
        detail_follow,
        detail_down,
        detail_seen,
        whole_prompts,
        panel_drag,
        timeline,
        ..
    } = app;
    let now = timeline.now_reference();
    let bg = Style::default().bg(palette.surface);
    // The left edge is a handle: drag it to resize; it lights up while held.
    let edge = if *panel_drag {
        palette.accent
    } else {
        palette.muted
    };

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(edge).bg(palette.surface))
        .style(bg)
        .padding(Padding::horizontal(1))
        // Affordance: the way out is visible, not tribal knowledge.
        .title_top(
            Line::from(" esc ✕ ")
                .right_aligned()
                .style(bg.fg(palette.subtle)),
        );
    let inner = block.inner(area);

    let Some(agent) = session.agent(agent_id) else {
        frame.render_widget(block, area);
        // Selected node has no model entry (stale selection) — show a hint.
        let para = Paragraph::new(Line::from(Span::styled(
            "no detail for this agent",
            Style::default().fg(palette.muted),
        )))
        .style(bg);
        frame.render_widget(para, inner);
        return;
    };

    let tools = talk::tools(session, agent_id, inner.width as usize, now, &palette);
    let [header_area, talk_area, tools_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Fill(1),
        Constraint::Length(u16::from(tools.is_some())),
    ])
    .areas(inner);
    let talk_block = Block::default()
        .borders(Borders::TOP)
        .border_style(bg.fg(palette.muted))
        .style(bg);
    let talk_inner = talk_block.inner(talk_area);
    let lines = talk::lines(
        session,
        agent_id,
        talk_inner.width as usize,
        *whole_prompts,
        &palette,
    );

    // The conversation tails the newest line by default; scrolling up detaches
    // it. The renderer clamps the offset to the real maximum and writes it (and
    // the re-attach) back, so the indicator and the next keypress agree.
    let total = lines.len().min(u16::MAX as usize) as u16;
    let (scroll, follow) = resolve_scroll(
        total,
        talk_inner.height,
        *detail_scroll,
        *detail_follow,
        std::mem::take(detail_down),
    );
    *detail_scroll = scroll;
    *detail_follow = follow;
    // Scrolled up, what has been said since is counted, as a chat would.
    let said = talk::said(session, agent_id);
    let seen = *detail_seen.get_or_insert(said);
    if follow {
        *detail_seen = None;
    }
    if total > talk_inner.height {
        let label = match said.saturating_sub(seen) {
            _ if follow => " ↕ wheel · j/k ".to_string(),
            0 => format!(" ↕ {scroll}/{total} "),
            new => format!(" ↓ {new} new "),
        };
        block = block.title_bottom(
            Line::from(label)
                .right_aligned()
                .style(bg.fg(palette.subtle)),
        );
    }
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    render_header(frame, header_area, agent, now, &palette);
    frame.render_widget(talk_block, talk_area);
    let shown: Vec<Line> = if lines.is_empty() {
        vec![Line::styled("nothing said yet", bg.fg(palette.subtle))]
    } else {
        lines
            .into_iter()
            .skip(scroll as usize)
            .take(talk_inner.height as usize)
            .collect()
    };
    frame.render_widget(Paragraph::new(shown).style(bg), talk_inner);
    if let Some(tools) = tools {
        frame.render_widget(Paragraph::new(tools).style(bg), tools_area);
    }
}

fn render_header(
    frame: &mut Frame,
    area: Rect,
    agent: &AgentInfo,
    now: Option<DateTime<Utc>>,
    palette: &rataflow::Palette,
) {
    let bg = Style::default().bg(palette.surface);
    let width = area.width as usize;

    // Title: agent type, bold.
    let title = agent
        .agent_type
        .as_deref()
        .unwrap_or(agent.kind.default_label());
    let mut lines = vec![Line::from(Span::styled(
        truncate(title, width),
        bg.fg(palette.text).add_modifier(Modifier::BOLD),
    ))];

    // Status, model and timing, on one line. Single-source vocabulary and
    // presence colors, shared with cards and inspect.
    let mut status = vec![Span::styled(
        agent.status_word(),
        bg.fg(crate::ui::status_color(agent.status, palette)),
    )];
    for part in [agent.model.clone(), fmt_timing(agent, now)]
        .into_iter()
        .flatten()
    {
        status.push(Span::styled(format!("  {part}"), bg.fg(palette.subtle)));
    }
    lines.push(Line::from(status));

    // What it is for, in one line: the conversation below has the rest.
    if let Some(desc) = agent.description.as_ref().filter(|d| !d.is_empty()) {
        let desc = desc.split_whitespace().collect::<Vec<_>>().join(" ");
        lines.push(Line::from(Span::styled(
            truncate(&desc, width),
            bg.fg(palette.subtle),
        )));
    }

    frame.render_widget(Paragraph::new(lines).style(bg), area);
}

/// Resolve the panel's scroll offset for one render: clamp to the reachable
/// maximum (keep the last screenful in view — no over-scroll into blank) and
/// reconcile the tail. Following pins to the bottom; scrolling back down to the
/// bottom (or content that fits) re-attaches, but only on a scroll `down`: content
/// that shrinks under a detached reader leaves them detached. Returns
/// `(offset, tailing)`.
fn resolve_scroll(total: u16, height: u16, scroll: u16, follow: bool, down: bool) -> (u16, bool) {
    let max = total.saturating_sub(height);
    let offset = if follow { max } else { scroll.min(max) };
    (offset, follow || down && offset >= max)
}

/// Format a timing line from an agent's first/last timestamps; a running
/// agent's counts up to `now` (the timeline's `now_reference`).
fn fmt_timing(agent: &AgentInfo, now: Option<DateTime<Utc>>) -> Option<String> {
    let running = agent.status == crate::state::session::AgentStatus::Running;
    let last = if running {
        now.or(agent.last_ts)
    } else {
        agent.last_ts
    };
    match (agent.first_ts, last) {
        (Some(first), Some(last)) => {
            let secs = (last - first).num_seconds().max(0);
            if secs >= 60 {
                Some(format!("⏱ {}m {}s", secs / 60, secs % 60))
            } else {
                Some(format!("⏱ {secs}s"))
            }
        }
        (Some(first), None) => Some(format!(
            "⏱ started {}",
            first.with_timezone(&chrono::Local).format("%H:%M:%S")
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_scroll_clamps_and_reconciles_tail() {
        // Content shorter than the viewport → offset 0; a detached reader stays
        // detached until they scroll down, which re-attaches.
        assert_eq!(resolve_scroll(5, 10, 3, false, false), (0, false));
        assert_eq!(resolve_scroll(5, 10, 3, false, true), (0, true));
        // Following → pinned to the bottom (max = 20 - 8 = 12).
        assert_eq!(resolve_scroll(20, 8, 0, true, false), (12, true));
        // Detached and scrolled up → keep the offset, stay detached.
        assert_eq!(resolve_scroll(20, 8, 5, false, false), (5, false));
        // Detached but (over-)scrolled to the bottom → clamp + re-attach.
        assert_eq!(resolve_scroll(20, 8, 99, false, true), (12, true));
        // Content that shrank under a detached reader leaves them detached.
        assert_eq!(resolve_scroll(20, 8, 31, false, false), (12, false));
    }

    use chrono::TimeZone;

    fn agent_with_ts(first: Option<i64>, last: Option<i64>) -> AgentInfo {
        let mut a = AgentInfo::new(crate::state::session::AgentKind::Subagent);
        a.first_ts = first.map(|s| Utc.timestamp_opt(s, 0).unwrap());
        a.last_ts = last.map(|s| Utc.timestamp_opt(s, 0).unwrap());
        a
    }

    #[test]
    fn timing_duration_under_a_minute() {
        let a = agent_with_ts(Some(100), Some(142));
        assert_eq!(fmt_timing(&a, None).as_deref(), Some("⏱ 42s"));
    }

    #[test]
    fn timing_duration_over_a_minute() {
        let a = agent_with_ts(Some(0), Some(125));
        assert_eq!(fmt_timing(&a, None).as_deref(), Some("⏱ 2m 5s"));
    }

    #[test]
    fn timing_negative_clamped() {
        let a = agent_with_ts(Some(100), Some(50));
        assert_eq!(fmt_timing(&a, None).as_deref(), Some("⏱ 0s"));
    }

    #[test]
    fn timing_counts_up_while_running() {
        let mut a = agent_with_ts(Some(0), Some(10));
        let now = Utc.timestamp_opt(70, 0).single();
        assert_eq!(fmt_timing(&a, now).as_deref(), Some("⏱ 1m 10s"));
        a.status = crate::state::session::AgentStatus::Done;
        assert_eq!(
            fmt_timing(&a, now).as_deref(),
            Some("⏱ 10s"),
            "a finished agent stops"
        );
    }

    #[test]
    fn timing_none_when_no_first() {
        let a = agent_with_ts(None, None);
        assert!(fmt_timing(&a, None).is_none());
    }
}
