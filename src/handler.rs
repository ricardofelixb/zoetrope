//! Input routing.
//!
//! App-level keys (`q`/`ctrl-c` quit, `space` play/pause, `[`/`]` step + `End`/`g`
//! go-live transport, `s` pacing, `o`/`f`/`r` camera, `i`/`?` overlays) mutate `app`
//! directly, and `enter` hands the selected agent's session to `--on-enter`;
//! everything else is
//! forwarded to the flow — first `handle_controls_key_event` (zoom/fit), then
//! `handle_key_event` (selection nav), and mouse to `handle_mouse_event`. Flow
//! events are consumed via `into_events`; the graph is read-only so only
//! selection state matters (read during render), but events are still drained.

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use rataflow::EventResponse;

use crate::state::{App, Camera};

/// Route one crossterm event. Returns `true` if the app should quit. Mutates
/// `app` only — every key path is in-process state (no channel).
pub fn handle_event(event: &Event, app: &mut App) -> bool {
    match event {
        Event::Key(key) if app.draft.is_some() => {
            write_key(key, app);
            false
        }
        Event::Paste(text) => {
            if let Some(draft) = app.draft.as_mut() {
                draft.insert(text);
            }
            false
        }
        Event::Key(key) => handle_key(key, app),
        Event::Mouse(mouse) => {
            // A press/drag on the scrubber row seeks the playhead — intercept it
            // before the flow sees it (else it reads as a pane drag → pan).
            if let Some(bar) = app.scrubber_area
                && mouse.row >= bar.y
                && mouse.row < bar.y + bar.height
                && bar.width > 1
                && matches!(
                    mouse.kind,
                    MouseEventKind::Down(MouseButton::Left)
                        | MouseEventKind::Drag(MouseButton::Left)
                )
            {
                let rel = mouse.column.saturating_sub(bar.x).min(bar.width - 1);
                // Queue rather than seek now: a drag delivers many events per
                // frame and a backward seek rebuilds the whole model — the tick
                // applies only the latest target, once per frame.
                app.pending_seek = Some(rel as f64 / (bar.width - 1) as f64);
                return false;
            }
            // The panel's left edge, held, resizes it.
            let held = match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => app.grab_panel(mouse.column, mouse.row),
                MouseEventKind::Drag(MouseButton::Left) => app.drag_panel(mouse.column),
                MouseEventKind::Up(MouseButton::Left) => std::mem::take(&mut app.panel_drag),
                _ => false,
            };
            if held {
                return false;
            }
            // The wheel over the panel scrolls its conversation.
            let wheel = match mouse.kind {
                MouseEventKind::ScrollUp => -WHEEL_SCROLL,
                MouseEventKind::ScrollDown => WHEEL_SCROLL,
                _ => 0,
            };
            if wheel != 0 && app.wheel_panel(mouse.column, mouse.row, wheel) {
                return false;
            }
            // rataflow re-exports ratatui's crossterm; types unify, so the
            // `From<crossterm::event::MouseEvent>` impl applies directly.
            let events: Vec<_> = app.flow.handle_mouse_event(*mouse).into_events().collect();
            // A click on the root's card runs `--on-root` instead of selecting it.
            let root = events.iter().any(|e| {
                matches!(e, rataflow::FlowEvent::SelectionChanged { node_ids, .. }
                    if node_ids.iter().any(|id| id == crate::state::session::MAIN_ID))
            });
            match app.on_root.clone() {
                Some(command) if root => {
                    app.flow.clear_selection();
                    spawn(
                        app,
                        "--on-root",
                        command.split_whitespace().map(str::to_string),
                    );
                }
                _ => process_flow_events(app, events.into_iter()),
            }
            false
        }
        _ => false,
    }
}

/// Handle a single key event, returning `true` to quit.
fn handle_key(key: &KeyEvent, app: &mut App) -> bool {
    // Some terminals emit both Press and Release; act on Press (and the legacy
    // empty kind) only, so a single keystroke isn't handled twice.
    if matches!(key.kind, KeyEventKind::Release) {
        return false;
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    match key.code {
        // Quit.
        KeyCode::Char('q') | KeyCode::Char('Q') => {
            app.should_quit = true;
            return true;
        }
        KeyCode::Char('c') | KeyCode::Char('C') if ctrl => {
            app.should_quit = true;
            return true;
        }

        // Detail-panel tool-call list scrolling (only meaningful when an agent
        // is selected). `j`/`k` and PageDown/PageUp move the offset; clamped to
        // the list length so long lists (30–190+ calls) are fully reachable.
        // Arrow keys are intentionally left to the flow for graph navigation.
        KeyCode::Char('j') => {
            if scroll_detail(app, 1) {
                return false;
            }
        }
        KeyCode::Char('k') => {
            if scroll_detail(app, -1) {
                return false;
            }
        }
        KeyCode::PageDown => {
            if scroll_detail(app, PAGE_SCROLL) {
                return false;
            }
        }
        KeyCode::PageUp => {
            if scroll_detail(app, -PAGE_SCROLL) {
                return false;
            }
        }

        // Camera keys name destinations, not toggles: `o` overview (auto-fit
        // everything), `f` follow (readable zoom, track the latest activity).
        // The only ways out of Manual.
        KeyCode::Char('o') | KeyCode::Char('O') => {
            app.camera = Camera::Overview;
            app.camera_glide = None; // fit-view owns the viewport now
            // Camera is orthogonal to layout: frame what's there, never reflow.
            crate::state::fit(&mut app.flow);
            return false;
        }
        KeyCode::Char('f') | KeyCode::Char('F') => {
            app.camera = Camera::Follow;
            // `track_activity` → `center_node` owns the readable-zoom bump.
            app.track_activity();
            return false;
        }

        // Tidy the graph: layout is user-driven (new nodes never auto-reflow,
        // which read as jumpy), so `r` runs Sugiyama on demand and reframes.
        KeyCode::Char('r') | KeyCode::Char('R') => {
            app.relayout_now();
            return false;
        }

        // Timeline scrubbing (DVR). `[`/`]` step the playhead to the prev/next
        // prompt-era boundary; `End`/`g`/`G` re-pin to the live/replay edge (`g`
        // is the letter alias, vim-style "go to end", that also works in the
        // browser where `End` can be unreliable).
        KeyCode::Char('[') => {
            app.seek_prompt(false);
            return false;
        }
        KeyCode::Char(']') => {
            app.seek_prompt(true);
            return false;
        }
        KeyCode::End | KeyCode::Char('g') | KeyCode::Char('G') => {
            app.go_live();
            return false;
        }

        // Toggle inactivity-skip: compress dead air (default) vs faithful
        // real-time pacing. Presentation-only — never touches content.
        KeyCode::Char('s') | KeyCode::Char('S') => {
            app.timeline.compress_gaps = !app.timeline.compress_gaps;
            return false;
        }

        // Help overlay.
        KeyCode::Char('?') => {
            app.show_help = !app.show_help;
            return false;
        }

        // Session-info overlay (untimed metadata: mode, perms, last prompt, …).
        KeyCode::Char('i') | KeyCode::Char('I') => {
            app.show_info = !app.show_info;
            return false;
        }

        // Close the help overlay, else the detail panel (clear selection).
        // Without this there is no way out of the panel: pane-clicks
        // deliberately don't deselect (drag-friendly) and the whitelist
        // blocks the library's own bindings.
        KeyCode::Esc => {
            if app.show_help {
                app.show_help = false;
            } else if app.show_info {
                app.show_info = false;
            } else if app.camera != Camera::Follow {
                // Close the detail panel. In Follow esc is a no-op: the panel is
                // part of Follow's contract (it auto-narrates the active agent),
                // and a user selection would have dropped Follow to Manual anyway.
                app.flow.clear_selection();
                app.detail_scroll = 0;
                app.detail_follow = true;
            }
            return false;
        }

        // Unified play/pause: freeze when playing, or resume from the current
        // cursor when paused or scrubbed into the past. Works in both intents.
        KeyCode::Char(' ') => {
            app.toggle_play_pause();
            return false;
        }

        // Write to the panel's agent, when it has a session to send to.
        KeyCode::Enter if app.on_send.is_some() => {
            if let Some(about) = app.panel_agent().filter(|id| app.session_of(id).is_some()) {
                app.draft = Some(crate::state::Draft {
                    about,
                    ..Default::default()
                });
            }
            return false;
        }

        // The panel's prompts, folded or whole.
        KeyCode::Char('x') | KeyCode::Char('X') if app.panel_agent().is_some() => {
            app.whole_prompts = !app.whole_prompts;
            return false;
        }

        KeyCode::Char('t') | KeyCode::Char('T') => {
            app.show_timeline = !app.show_timeline;
            return false;
        }

        _ => {}
    }

    // Whitelist policy: the graph is read-only, so only navigation and
    // viewport keys reach the flow. Falling through by default would expose
    // destructive/stateful library bindings — Delete/Backspace removes the
    // selected node (re-added on the next sync with a layout jump), 'i'
    // silently toggles the viewport lock, 'm' toggles multi-select — none of
    // which zoetrope surfaces or wants.
    let response = match key.code {
        // Selection navigation: sequential + spatial. The flow is configured with
        // `SelectionReveal::None` (see `graph::new_flow`), so selection changes
        // without the library moving the camera — zoetrope's center-glide
        // (pending_center → center_node) is the sole, smooth camera move.
        KeyCode::Tab
        | KeyCode::BackTab
        | KeyCode::Up
        | KeyCode::Down
        | KeyCode::Left
        | KeyCode::Right => app.flow.handle_key_event(*key),
        // Viewport: zoom in/out/reset (controls bindings).
        KeyCode::Char('+' | '=' | '-' | '_' | '0') => app.flow.handle_controls_key_event(*key),
        // Viewport: vim panning (j/k reach here only with no agent selected —
        // the detail-panel scroll consumed them above otherwise) and
        // center-on-selected.
        KeyCode::Char('h' | 'j' | 'k' | 'l' | 'c') => app.flow.handle_key_event(*key),
        _ => EventResponse::NotHandled,
    };
    process_flow_events(app, response.into_events());
    false
}

/// A key while a message is being written: it edits the draft, `enter` sends
/// it and `esc` drops it. No other key acts meanwhile, so typing a letter never
/// fires its shortcut.
fn write_key(key: &KeyEvent, app: &mut App) {
    if matches!(key.kind, KeyEventKind::Release) {
        return;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let Some(draft) = app.draft.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => app.draft = None,
        KeyCode::Char('c') if ctrl => app.draft = None,
        KeyCode::Char('u') if ctrl => {
            *draft = crate::state::Draft {
                about: std::mem::take(&mut draft.about),
                ..Default::default()
            }
        }
        KeyCode::Enter => send(app),
        KeyCode::Char(c) => draft.insert(c.encode_utf8(&mut [0; 4])),
        KeyCode::Backspace => draft.backspace(),
        KeyCode::Delete => draft.delete(),
        KeyCode::Left => draft.step(-1),
        KeyCode::Right => draft.step(1),
        KeyCode::Home => draft.home(),
        KeyCode::End => draft.end(),
        _ => {}
    }
}

/// Send the draft with the `--on-send` command, without waiting for it: where
/// it goes is the command's business, and zoe stays read-only. An empty draft
/// is dropped.
fn send(app: &mut App) {
    let Some(draft) = app.draft.take() else {
        return;
    };
    let (Some(command), false) = (app.on_send.clone(), draft.text.trim().is_empty()) else {
        return;
    };
    let Some(session) = app.session_of(&draft.about) else {
        return;
    };
    let (provider, id, cwd) = (
        session.provider.name().to_string(),
        session.id,
        session.cwd.unwrap_or_default(),
    );
    let text = draft.text.trim();
    let words = command.split_whitespace().map(|word| {
        word.replace("{provider}", &provider)
            .replace("{session}", &id)
            .replace("{cwd}", &cwd)
            .replace("{text}", text)
    });
    spawn(app, "--on-send", words);
}

/// Run a command (its program, then its arguments) without waiting for it;
/// a failure to start shows as `flag`'s error.
fn spawn(app: &mut App, flag: &str, mut words: impl Iterator<Item = String>) {
    let Some(program) = words.next() else {
        return;
    };
    let spawned = std::process::Command::new(program)
        .args(words)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        // Reaped off the input path, so it never lingers as a zombie.
        Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
        Err(e) => app.last_error = Some(format!("{flag}: {e}")),
    }
}

/// Rows moved per wheel notch over the detail panel.
const WHEEL_SCROLL: i32 = 3;

/// Rows moved per PageUp/PageDown in the detail panel's tool-call list.
const PAGE_SCROLL: i32 = 10;

/// Scroll the detail panel's tool-call list by `delta` rows, clamped to the
/// selected agent's tool-call count. Returns `true` if an agent was selected
/// (so the key is consumed and not forwarded to the flow); `false` lets the key
/// fall through to graph navigation when no panel is shown.
fn scroll_detail(app: &mut App, delta: i32) -> bool {
    if app.panel_agent().is_none() {
        return false;
    }
    // Scrolling up detaches the tail; scrolling down to the bottom re-attaches.
    // The renderer owns the upper clamp (it alone knows the panel height and the
    // true line count incl. era headers) and writes the resolved offset back.
    if delta < 0 {
        app.detail_follow = false;
    }
    app.detail_scroll = (app.detail_scroll as i32 + delta).max(0) as u16;
    true
}

/// Drain and react to the flow events produced by an input handler call.
///
/// Intentionally minimal — selection is read during render, so only side
/// effects that aren't render-derived are acted on here: the
/// detail-panel scroll resets when the selection changes, and any user-driven
/// viewport change (pan keys, wheel zoom, drag pan) hands the camera to the
/// user.
pub fn process_flow_events(app: &mut App, events: impl Iterator<Item = rataflow::FlowEvent>) {
    // The logic lives on `App` so the browser frontend shares it; this stays as
    // the native handler's call surface.
    app.process_flow_events(events);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Mode;
    use crossterm::event::MouseEvent;
    use rataflow::FlowEvent;

    /// An App with a 20-column scrubber at (x=2, rows 5..8) over a 4-item replay.
    fn scrubber_app() -> App {
        use crate::provider::claude::{Record, Source};
        use crate::tailer::{ReplayItem, UiEvent};
        let item = |uuid: &str, t: &str| {
            let line = format!(
                r#"{{"type":"user","uuid":"{uuid}","parentUuid":null,"timestamp":"{t}","message":{{"role":"user","content":"x"}}}}"#
            );
            ReplayItem::new(
                Record::Entry {
                    source: Source::Main,
                    entry: crate::provider::claude::wire::parse_line(&line).unwrap(),
                }
                .statement()
                .unwrap(),
            )
        };
        let mut app = App::new("s".into(), Mode::Replay);
        app.handle_ui_event(UiEvent::ReplayLoaded {
            session_id: "s".into(),
            items: vec![
                item("u1", "2026-06-05T10:00:00.000Z"),
                item("u2", "2026-06-05T10:00:01.000Z"),
                item("u3", "2026-06-05T10:00:02.000Z"),
                item("u4", "2026-06-05T10:00:03.000Z"),
            ],
            speed: 8.0,
            info: Default::default(),
        });
        app.scrubber_area = Some(ratatui::layout::Rect::new(2, 5, 20, 3));
        app
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn scrubber_click_maps_columns_to_fractions() {
        let mut app = scrubber_app();
        let down = |col, row| mouse(MouseEventKind::Down(MouseButton::Left), col, row);

        // Leftmost column → fraction 0.0.
        handle_event(&down(2, 5), &mut app);
        assert_eq!(app.pending_seek, Some(0.0));

        // Rightmost column (x + width - 1 = 21) → fraction 1.0.
        handle_event(&down(21, 6), &mut app);
        assert_eq!(app.pending_seek, Some(1.0));

        // Past the right edge clamps to 1.0 (no overshoot, no panic).
        handle_event(&down(60, 7), &mut app);
        assert_eq!(app.pending_seek, Some(1.0));

        // A row BELOW the bar is not a seek — it forwards to the flow.
        app.pending_seek = None;
        handle_event(&down(10, 8), &mut app);
        assert_eq!(app.pending_seek, None, "off-bar clicks must not seek");
    }

    #[test]
    fn scrubber_drag_burst_coalesces_to_one_seek_per_tick() {
        let mut app = scrubber_app();
        let drag = |col| mouse(MouseEventKind::Drag(MouseButton::Left), col, 6);

        // Ride to the edge first so a backward drag exercises the rebuild path.
        app.go_live();
        assert_eq!(app.timeline.folded, 4);

        // A burst of drag events within one frame: only the LAST target is
        // queued; nothing rebuilds until the tick.
        handle_event(&drag(15), &mut app);
        handle_event(&drag(9), &mut app);
        handle_event(&drag(2), &mut app);
        assert_eq!(app.pending_seek, Some(0.0));
        assert_eq!(app.timeline.folded, 4, "no rebuild before the tick");

        // The tick applies exactly one seek — to the latest target.
        app.tick_timeline(std::time::Duration::ZERO);
        assert_eq!(app.pending_seek, None);
        assert_eq!(
            app.timeline.folded,
            app.timeline.fold_at_fraction(0.0),
            "one coalesced seek lands on the last drag target"
        );
    }

    #[test]
    fn viewport_change_hands_camera_to_user() {
        let mut app = App::new("s".into(), Mode::Live);
        assert_eq!(app.camera, Camera::Overview);
        process_flow_events(
            &mut app,
            vec![FlowEvent::ViewportChanged {
                x: 1.0,
                y: 2.0,
                zoom: 1.5,
            }]
            .into_iter(),
        );
        assert_eq!(app.camera, Camera::Manual);
    }

    #[test]
    fn selection_nav_leaves_the_camera_to_our_glide() {
        use ratatui::widgets::Widget;
        let mk = |id: &str, x: f64| {
            rataflow::Node::new(
                id,
                (x, 0.0),
                (10.0, 5.0),
                crate::ui::nodes::AgentNode {
                    title: id.into(),
                    description: None,
                    said: None,
                    status: crate::state::session::AgentStatus::Running,
                    tool_count: 0,
                    last_tool: None,
                    output_tokens: 0,
                    interactive: false,
                },
            )
        };
        let mut app = App::new("s".into(), Mode::Live);
        // Two nodes far apart so the second is off-screen when we focus the first.
        app.flow.add_node(mk("a", 0.0)).unwrap();
        app.flow.add_node(mk("b", 500.0)).unwrap();
        // Render so the flow has a canvas, then zoom/focus tightly on "a" so "b"
        // is well off-screen (the library WOULD pan to reveal it).
        let area = ratatui::layout::Rect::new(0, 0, 60, 20);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        (&mut app.flow).render(area, &mut buf);
        app.flow.zoom_to(5.0);
        app.flow.center_on((5.0, 2.5));
        app.flow.select_node("a");
        let before = (app.flow.viewport.x, app.flow.viewport.y);

        // Tab to "b": the flow is `SelectionReveal::None`, so the selection moves
        // WITHOUT the library touching the camera — zoetrope's center-glide (queued
        // via pending_center) is the sole, smooth camera move.
        handle_event(
            &Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            &mut app,
        );

        assert_eq!(
            (app.flow.viewport.x, app.flow.viewport.y),
            before,
            "a selection key must leave the viewport to our glide (library reveal is off)"
        );
        assert_eq!(
            app.pending_center.as_deref(),
            Some("b"),
            "the newly-selected node is queued for a smooth center-glide instead"
        );
    }

    #[test]
    fn destructive_and_stateful_library_keys_are_inert() {
        let mut app = App::new("s".into(), Mode::Live);
        // A selected, default-flags node — deletable=true in the library.
        let node = rataflow::Node::new(
            "a",
            (0.0, 0.0),
            (10.0, 5.0),
            crate::ui::nodes::AgentNode {
                title: "a".into(),
                description: None,
                said: None,
                status: crate::state::session::AgentStatus::Running,
                tool_count: 0,
                last_tool: None,
                output_tokens: 0,
                interactive: false,
            },
        );
        app.flow.add_node(node).unwrap();
        app.flow.select_node("a");

        // Delete must NOT remove the selected node (read-only graph).
        let del = Event::Key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        handle_event(&del, &mut app);
        assert!(app.flow.node("a").is_some(), "Delete must be inert");

        // 'i' must NOT toggle the viewport lock.
        let i = Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        handle_event(&i, &mut app);
        assert!(!app.flow.locked, "'i' must not silently lock the viewport");
    }

    #[test]
    fn esc_closes_the_detail_panel() {
        let mut app = App::new("s".into(), Mode::Live);
        let node = rataflow::Node::new(
            "a",
            (0.0, 0.0),
            (10.0, 5.0),
            crate::ui::nodes::AgentNode {
                title: "a".into(),
                description: None,
                said: None,
                status: crate::state::session::AgentStatus::Running,
                tool_count: 0,
                last_tool: None,
                output_tokens: 0,
                interactive: false,
            },
        );
        app.flow.add_node(node).unwrap();
        app.flow.select_node("a");
        assert!(app.selected_agent_id().is_some());

        let esc = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        handle_event(&esc, &mut app);
        assert!(
            app.selected_agent_id().is_none(),
            "esc must clear selection (close the panel)"
        );
    }

    #[test]
    fn dragging_a_node_drops_follow_to_manual() {
        // Regression: node drag emits NodeDragged (not SelectionChanged /
        // ViewportChanged), which earlier left Follow engaged. Any gesture drops.
        let mut app = App::new("s".into(), Mode::Live);
        app.camera = Camera::Follow;
        process_flow_events(
            &mut app,
            vec![FlowEvent::NodeDragged {
                node_id: "a".into(),
            }]
            .into_iter(),
        );
        assert_eq!(app.camera, Camera::Manual, "node drag must drop Follow");
    }

    #[test]
    fn selecting_in_overview_stays_in_overview() {
        // Overview yields only to viewport changes — selecting a node to inspect
        // it shouldn't break auto-framing.
        let mut app = App::new("s".into(), Mode::Live);
        assert_eq!(app.camera, Camera::Overview);
        process_flow_events(
            &mut app,
            vec![FlowEvent::SelectionChanged {
                node_ids: vec!["a".into()],
                edge_ids: vec![],
            }]
            .into_iter(),
        );
        assert_eq!(
            app.camera,
            Camera::Overview,
            "selection must not drop Overview"
        );
    }

    #[test]
    fn user_selection_drops_follow_to_manual() {
        let mut app = App::new("s".into(), Mode::Live);
        app.camera = Camera::Follow;
        app.camera_glide = Some(crate::state::CameraGlide {
            from: (0.0, 0.0),
            to: (10.0, 0.0),
            t: 0.1,
        });

        // A USER selection gesture (click or spatial nav) drops Follow so the
        // camera stops chasing activity and gliding back over the selection.
        process_flow_events(
            &mut app,
            vec![FlowEvent::SelectionChanged {
                node_ids: vec!["a".into()],
                edge_ids: vec![],
            }]
            .into_iter(),
        );
        assert_eq!(app.camera, Camera::Manual, "user selection drops Follow");
        assert!(
            app.camera_glide.is_none(),
            "the glide-back is cancelled too"
        );
        assert_eq!(
            app.pending_center.as_deref(),
            Some("a"),
            "the selected node is queued for a center-glide on the next draw"
        );
    }

    #[test]
    fn deselection_clears_a_pending_center() {
        let mut app = App::new("s".into(), Mode::Live);
        app.pending_center = Some("a".into());
        process_flow_events(
            &mut app,
            vec![FlowEvent::SelectionChanged {
                node_ids: vec![],
                edge_ids: vec![],
            }]
            .into_iter(),
        );
        assert!(
            app.pending_center.is_none(),
            "deselecting must not center a stale node"
        );
    }

    #[test]
    fn r_key_tidies_layout_on_demand() {
        let mut app = App::new("s".into(), Mode::Live);
        // Growth has accumulated but layout is never automatic now.
        app.layout_dirty = true;

        let r = Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(!handle_event(&r, &mut app));
        assert!(
            !app.layout_dirty,
            "r must apply the pending relayout (user-driven tidy)"
        );
    }

    #[test]
    fn camera_keys_name_destinations() {
        let mut app = App::new("s".into(), Mode::Live);
        app.camera = Camera::Manual;

        let f = Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        assert!(!handle_event(&f, &mut app));
        assert_eq!(app.camera, Camera::Follow);

        let o = Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        app.layout_dirty = true; // growth accumulated
        assert!(!handle_event(&o, &mut app));
        assert_eq!(app.camera, Camera::Overview);
        assert!(
            app.layout_dirty,
            "camera keys are orthogonal to layout: o must NOT relayout (only r does)"
        );
    }

    fn press(app: &mut App, code: KeyCode) {
        handle_event(&Event::Key(KeyEvent::new(code, KeyModifiers::NONE)), app);
    }

    /// `enter` writes, every key then edits the message, not the graph, and
    /// `esc` drops it; nothing is written without `--on-send`.
    #[test]
    fn enter_writes_a_message_and_keys_edit_it() {
        let mut app = App::new("s".into(), Mode::Live);
        press(&mut app, KeyCode::Enter);
        assert!(app.draft.is_none(), "no --on-send, no message");
        app.on_send = Some("say {text}".into());
        press(&mut app, KeyCode::Enter);
        assert!(app.draft.is_none(), "no agent, no message");
        app.draft = Some(crate::state::Draft::default());
        for c in "hq!".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert!(!app.should_quit, "q is a letter while writing");
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Backspace);
        handle_event(&Event::Paste("ey\nyou".into()), &mut app);
        let draft = app.draft.clone().unwrap();
        assert_eq!((draft.text.as_str(), draft.cursor), ("hey you!", 7));
        press(&mut app, KeyCode::Esc);
        assert!(app.draft.is_none());
    }

    #[test]
    fn the_wheel_over_the_panel_scrolls_it() {
        let mut app = App::new("s".into(), Mode::Live);
        assert!(!app.wheel_panel(5, 5, 3), "no panel");
        app.panel_area = Some(ratatui::layout::Rect::new(10, 0, 20, 10));
        assert!(!app.wheel_panel(5, 5, 3), "outside it");
        assert!(app.wheel_panel(12, 5, -3));
        assert!(!app.detail_follow, "up stops following");
    }

    #[test]
    fn dragging_the_panel_edge_resizes_it() {
        let mut app = App::new("s".into(), Mode::Live);
        app.panel_area = Some(ratatui::layout::Rect::new(30, 0, 70, 10));
        handle_event(
            &mouse(MouseEventKind::Down(MouseButton::Left), 30, 5),
            &mut app,
        );
        assert!(app.panel_drag, "the edge is held");
        handle_event(
            &mouse(MouseEventKind::Drag(MouseButton::Left), 50, 5),
            &mut app,
        );
        assert_eq!(app.panel_share, 50);
        handle_event(
            &mouse(MouseEventKind::Drag(MouseButton::Left), 99, 5),
            &mut app,
        );
        assert_eq!(app.panel_share, 20, "never thinner than a fifth");
        handle_event(
            &mouse(MouseEventKind::Up(MouseButton::Left), 99, 5),
            &mut app,
        );
        assert!(!app.panel_drag);
        handle_event(
            &mouse(MouseEventKind::Down(MouseButton::Left), 60, 5),
            &mut app,
        );
        assert!(!app.panel_drag, "inside the panel is not the edge");
    }

    /// Watching only the agents, the root's card opens no panel: its keys
    /// are the graph's, and there is nothing to write to.
    #[test]
    fn agents_only_leaves_the_root_without_a_panel() {
        let mut app = App::new("s".into(), Mode::Live);
        app.agents_only = true;
        app.on_send = Some("say {text}".into());
        let app_node = || crate::ui::nodes::AgentNode {
            title: "root".into(),
            description: None,
            said: None,
            status: crate::state::session::AgentStatus::Running,
            tool_count: 0,
            last_tool: None,
            output_tokens: 0,
            interactive: false,
        };
        let node = rataflow::Node::new(
            crate::state::session::MAIN_ID,
            (0.0, 0.0),
            (10.0, 5.0),
            app_node(),
        );
        app.flow.add_node(node).unwrap();
        app.flow.select_node(crate::state::session::MAIN_ID);
        assert!(app.selected_agent_id().is_some());
        assert!(app.panel_agent().is_none());
        press(&mut app, KeyCode::Enter);
        assert!(app.draft.is_none());
        // A conductor's card (a group) does have one.
        let group = rataflow::Node::new("c1", (0.0, 20.0), (10.0, 5.0), app_node());
        app.flow.add_node(group).unwrap();
        app.flow.select_node("c1");
        assert_eq!(app.panel_agent().as_deref(), Some("c1"));
        app.flow.select_node(crate::state::session::MAIN_ID);
        app.agents_only = false;
        assert!(app.panel_agent().is_some(), "otherwise the root has one");
        press(&mut app, KeyCode::Char('t'));
        assert!(!app.show_timeline, "t hides the timeline");
    }
}
