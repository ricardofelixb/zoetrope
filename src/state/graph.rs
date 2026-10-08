//! Incremental projection of [`SessionModel`] onto a rataflow `Flow`.
//!
//! Never rebuilds: per agent, either mutate the existing node content in place
//! via `node_content_mut`, or `add_node` + `add_edge` (duplicate-id `Err` is an
//! idempotent no-op). Nodes are added before their edges. A structural change
//! (node/edge added) marks layout dirty; at sync end we run Sugiyama. Selection
//! survives because node ids are stable and we never clear-and-re-add.

use rataflow::{Edge, Flow, Handle, HandlePosition, Node, Reconnectable, Sugiyama, Theme};
use ratatui::style::Color;

use super::session::{AgentInfo, AgentKind, AgentStatus, SessionModel};
use crate::ui::edges::AgentEdge;
use crate::ui::nodes::{AgentNode, MAIN_NODE_DIMS, SUB_NODE_DIMS};

/// The concrete `Flow` type zoetrope uses: agent-card nodes, step-routed parent
/// edges (no labels — liveness reads from color alone).
pub type AgentFlow = Flow<AgentNode, AgentEdge>;

/// Build an empty, fully-configured `Flow` for zoetrope.
///
/// Config: `with_deselect_on_pane_click(false)`, `deselect_on_drag = false`
/// (detail panel persists), `with_min_zoom(0.1)` (Sugiyama trees outgrow the
/// default fit-view limit). Hidden source/target handles for a clean look.
pub fn new_flow() -> AgentFlow {
    // zoetrope identity palette: stock dark base, but `accent` becomes GOLD —
    // selection highlights, done medals, the REPLAY badge. Green stays
    // exclusively "alive" (status), red "failed". Every surface resolves from
    // flow.theme, so this one assignment brands the whole app.
    let mut palette = Theme::Dark.palette();
    palette.accent = Color::Indexed(178);
    let mut flow = Flow::new()
        .with_theme(Theme::Custom(palette))
        .with_deselect_on_pane_click(false)
        // We drive the camera on selection ourselves (a center-glide via
        // `pending_center`), so suppress the library's instant ensure-visible pan
        // — otherwise the two stack into a jump-then-glide on off-screen nodes.
        .with_selection_reveal(rataflow::SelectionReveal::None)
        .with_min_zoom(0.1);
    flow.deselect_on_drag = false;
    flow
}

/// The most a fit frames the graph at: the cards' own size. A few cards fill
/// the canvas no further, and zooming in past it stays the user's to do.
const FIT_ZOOM: f64 = 1.0;

/// Frame every card (deferred to the next render), no larger than their own
/// size (`FIT_ZOOM`).
pub fn fit(flow: &mut AgentFlow) {
    flow.request_fit_view_with_options(rataflow::FitViewOptions::default().with_max_zoom(FIT_ZOOM));
}

/// Title line for a node: the agent type the provider recorded, else the
/// generic label for its kind. No provider name appears here; the root
/// agent's is stated by its provider like any other agent's.
fn node_title(info: &AgentInfo) -> String {
    info.agent_type
        .clone()
        .unwrap_or_else(|| info.kind.default_label().to_string())
}

/// Fixed card dimensions for a node kind.
fn node_dims(kind: AgentKind) -> (f64, f64) {
    match kind {
        AgentKind::Main | AgentKind::Group => MAIN_NODE_DIMS,
        AgentKind::Subagent => SUB_NODE_DIMS,
    }
}

/// Whether a node's content already mirrors the agent — allocation-free
/// comparison so unchanged agents skip [`build_content`]'s String clones on
/// every sync (the steady state for almost all agents on almost all ticks).
fn content_matches(info: &AgentInfo, node: &AgentNode) -> bool {
    let title_ok = node.title
        == info
            .agent_type
            .as_deref()
            .unwrap_or(info.kind.default_label());
    title_ok
        && node.description.as_deref() == info.description.as_deref()
        && node.said.as_deref() == info.said.as_ref().map(|s| s.excerpt.as_str())
        && node.status == info.status
        && node.tool_count == info.tool_calls.len()
        && node.last_tool.as_deref() == info.last_tool()
        && node.output_tokens == info.output_tokens
        && node.interactive == info.is_interactive()
}

/// Build the [`AgentNode`] content mirrored from an [`AgentInfo`].
fn build_content(info: &AgentInfo) -> AgentNode {
    AgentNode {
        title: node_title(info),
        description: info.description.clone(),
        said: info.said.as_ref().map(|s| s.excerpt.clone()),
        status: info.status,
        tool_count: info.tool_calls.len(),
        last_tool: info.last_tool().map(str::to_string),
        output_tokens: info.output_tokens,
        interactive: info.is_interactive(),
    }
}

/// Horizontal gap between locally-placed siblings (world units).
const LOCAL_H_GAP: f64 = 4.0;
/// Vertical gap below a parent for locally-placed children (world units).
const LOCAL_V_GAP: f64 = 5.0;

/// Incrementally sync `flow` to `model`.
///
/// For each agent in spawn order: mutate the existing node content in place, or
/// add the node (then its parent edge). Updates edge `animated` from target
/// status. New nodes get LOCAL placement (below their parent, offset past
/// siblings) so they land somewhere sensible even without a relayout.
///
/// When `relayout` is true, any structural change ends with a full
/// `Sugiyama::vertical()` pass (which overwrites the local placements). When
/// false — Manual camera: the user owns the view — nothing existing moves;
/// the caller tracks dirtiness and relayouts when the camera re-engages. The
/// exception is the first cards onto an empty canvas, which are several trees
/// (a job's): placing them locally would stack the trees, and nothing is there
/// to move.
/// Agents the model hides (and what hangs under them) are left off the canvas,
/// and taken off it if they were there. So is a job's root, with no edge from
/// it: what hung under it is the root of its own tree. Returns `true` if structure changed.
pub fn sync(flow: &mut AgentFlow, model: &SessionModel, relayout: bool) -> bool {
    let mut structural = false;
    let empty = flow.nodes().next().is_none();

    let hidden: std::collections::HashSet<&String> = model
        .spawn_order
        .iter()
        .filter(|id| model.undrawn(id))
        .collect();
    let drawn: Vec<String> = hidden
        .iter()
        .filter(|id| flow.node(id).is_some())
        .map(|id| (*id).clone())
        .collect();
    structural |= !drawn.is_empty();
    remove_agents(flow, &drawn);

    // First pass: nodes (must exist before their edges).
    for id in &model.spawn_order {
        let Some(info) = model.agent(id) else {
            continue;
        };
        if hidden.contains(id) {
            continue;
        }
        if let Some(existing) = flow.node_content_mut(id) {
            // Steady state: only rebuild (String clones) when something
            // visible changed — the per-second status tick and per-batch
            // syncs walk every agent, and most are unchanged.
            if !content_matches(info, existing) {
                *existing = build_content(info);
            }
        } else {
            // Sibling index for local placement — computed only for the rare
            // new node; the no-new-nodes steady state skips it entirely.
            let siblings = info
                .parent
                .as_deref()
                .map(|p| {
                    model
                        .spawn_order
                        .iter()
                        .take_while(|x| *x != id)
                        .filter(|x| model.agent(x).and_then(|a| a.parent.as_deref()) == Some(p))
                        .count()
                })
                .unwrap_or(0);
            let content = build_content(info);
            let (w, h) = node_dims(info.kind);
            // Local placement: below the parent, fanned past prior siblings.
            // Overwritten by Sugiyama when `relayout` runs; kept verbatim in
            // Manual so existing nodes never move underneath the user.
            let pos = info
                .parent
                .as_deref()
                .and_then(|p| flow.node(p))
                .map(|parent| {
                    (
                        parent.position.x + siblings as f64 * (w + LOCAL_H_GAP),
                        parent.position.y + parent.height + LOCAL_V_GAP,
                    )
                })
                .unwrap_or_else(|| {
                    // A tree's root: past the trees already there.
                    let right = flow
                        .nodes()
                        .map(|n| n.position.x + n.width + LOCAL_H_GAP)
                        .fold(0.0, f64::max);
                    (right, 0.0)
                });
            // Read-only monitor: nodes are selectable (detail panel) and
            // draggable (manual arrangement) — but never deletable and never
            // connection sources. Enforced at the DTO level, not just the key
            // whitelist, so no input path can mutate the graph.
            let node = Node::new(id.clone(), pos, (w, h), content)
                .with_deletable(false)
                .with_connectable(false)
                .with_handles(vec![
                    Handle::source(HandlePosition::Bottom).with_hidden(true),
                    Handle::target(HandlePosition::Top).with_hidden(true),
                ]);
            // Duplicate-id is an idempotent no-op; a genuine add is structural.
            if flow.add_node(node).is_ok() {
                structural = true;
            }
        }
    }

    // Second pass: edges from each agent to its parent.
    for id in &model.spawn_order {
        let Some(info) = model.agent(id) else {
            continue;
        };
        if hidden.contains(id) {
            continue;
        }
        let Some(parent) = info.parent.as_ref().filter(|p| !hidden.contains(p)) else {
            continue;
        };
        let animated = info.status == AgentStatus::Running;
        let edge_id = edge_id(id);
        // Edge already present (the steady state on every sync): just refresh
        // animation — probing via `edge_content_mut` first avoids building a
        // throwaway Edge (three String clones) per agent per sync only for
        // `add_edge` to reject it as a duplicate. Edges carry no selectable
        // meaning in zoetrope (no edge panel), and a stray edge click would pin
        // Follow mode while closing the node panel — a dead state. Fully inert:
        // not selectable, deletable, or reconnectable. Liveness shows as the
        // running color + marching ants, NOT a label — the current tool already
        // shows in the child's chips and detail panel.
        if let Some(content) = flow.edge_content_mut(&edge_id) {
            content.running = animated;
            flow.set_edge_animated(&edge_id, animated);
        } else {
            let edge = Edge::new(edge_id.clone(), parent.clone(), id.clone())
                .with_animated(animated)
                .with_selectable(false)
                .with_deletable(false)
                .with_reconnectable(Reconnectable::None);
            if flow.add_edge(edge).is_ok() {
                structural = true;
            }
            if let Some(content) = flow.edge_content_mut(&edge_id) {
                content.running = animated;
            }
        }
    }

    let trees = || {
        let root = |id: &&String| {
            model
                .agent(id)
                .is_some_and(|a| a.parent.as_ref().is_none_or(|p| hidden.contains(p)))
        };
        model
            .spawn_order
            .iter()
            .filter(|id| !hidden.contains(id))
            .filter(root)
            .nth(1)
            .is_some()
    };
    if structural && (relayout || empty && trees()) {
        self::relayout(flow);
    }
    structural
}

/// Stable id for the (single) parent edge of `child`.
///
/// Keyed by the child alone: every agent has exactly one parent edge, and
/// `sync` never removes edges — so the id must never change once created.
/// Keying on `spawned_by` or the parent would orphan a stale edge if
/// either field were filled in after the edge existed (latent today, armed by
/// any future meta re-emission).
fn edge_id(child: &str) -> String {
    format!("e-{child}")
}

/// Remove several agents from the canvas in ONE pass.
///
/// Bulk, not a loop of single removals: `Flow::remove_node` costs O(nodes plus
/// edges) every time — it shifts every lookup index and rebuilds the edge
/// lookup per call — so removing k of them is quadratic. `retain_nodes` makes a
/// single pass over each vec and drops the connected edges itself.
pub fn remove_agents(flow: &mut AgentFlow, agents: &[String]) {
    if agents.is_empty() {
        return;
    }
    let doomed: std::collections::HashSet<&str> = agents.iter().map(String::as_str).collect();
    flow.retain_nodes(|n| !doomed.contains(n.id.as_str()));
}

/// Apply the Sugiyama vertical layout to `flow`.
///
/// Split out so it can be called explicitly and unit-tested independently of
/// the per-agent diffing in [`sync`].
pub fn relayout(flow: &mut AgentFlow) {
    flow.apply_layout(Sugiyama::vertical());
    side_by_side(flow);
}

/// Set the trees of `flow` next to each other, left to right in the order
/// their roots were added, tops level.
///
/// Sugiyama lays out each connected tree from the same origin and leaves a
/// card with no edge where it was, so several trees (a job's groups, whose
/// root has no card) would land on top of one another. One tree stays as laid.
fn side_by_side(flow: &mut AgentFlow) {
    let parent: std::collections::HashMap<&str, &str> = flow
        .edges()
        .iter()
        .map(|e| (e.target.as_str(), e.source.as_str()))
        .collect();
    let mut trees: Vec<Vec<&str>> = Vec::new();
    let mut tree_of: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for node in flow.nodes() {
        let mut root = node.id.as_str();
        while let Some(&up) = parent.get(root) {
            root = up;
        }
        let next = trees.len();
        let tree = *tree_of.entry(root).or_insert(next);
        if tree == next {
            trees.push(Vec::new());
        }
        trees[tree].push(node.id.as_str());
    }
    if trees.len() < 2 {
        return;
    }
    let mut moves = Vec::new();
    let (mut cursor, mut level) = (None, None);
    for ids in &trees {
        let cards: Vec<_> = ids.iter().filter_map(|id| flow.node(id)).collect();
        let left = cards.iter().map(|n| n.position.x).fold(f64::MAX, f64::min);
        let right = cards
            .iter()
            .map(|n| n.position.x + n.width)
            .fold(f64::MIN, f64::max);
        let up = cards.iter().map(|n| n.position.y).fold(f64::MAX, f64::min);
        let at = *cursor.get_or_insert(left);
        let (dx, dy) = (at - left, *level.get_or_insert(up) - up);
        cursor = Some(at + (right - left) + LOCAL_H_GAP);
        for n in cards {
            moves.push((n.id.clone(), (n.position.x + dx, n.position.y + dy)));
        }
    }
    flow.set_node_positions(moves);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::claude::wire::SubagentMeta;

    /// A model with main + one direct subagent (running).
    fn model_with_subagent() -> SessionModel {
        let mut m = SessionModel::new("s1".into());
        // The root's name is the provider's to state.
        m.apply_fact(&crate::fact::Fact {
            agent: Some(super::super::session::MAIN_ID.to_string()),
            ts: None,
            kind: crate::fact::FactKind::Agent {
                kind: AgentKind::Main,
                parent: None,
                agent_type: Some("claude".into()),
                description: None,
                spawned_by: None,
                interactive: true,
            },
        });
        let meta = SubagentMeta {
            agent_type: Some("guide".into()),
            description: Some("research".into()),
            tool_use_id: Some("ag1".into()),
            stopped_by_user: None,
        };
        m.apply_meta("abc123", None, &meta);
        m
    }

    /// A job's model: the header's root, and a group under it per name.
    fn job_with_groups(groups: &[&str]) -> SessionModel {
        use crate::fact::{Fact, FactKind};
        let mut m = SessionModel::new("job:j".into());
        let agent = |id: &str, kind, parent: Option<&str>| Fact {
            agent: Some(id.to_string()),
            ts: None,
            kind: FactKind::Agent {
                kind,
                parent: parent.map(str::to_string),
                agent_type: Some("job".into()),
                description: None,
                spawned_by: None,
                interactive: false,
            },
        };
        m.apply_fact(&agent("main", AgentKind::Main, None));
        for g in groups {
            m.apply_fact(&agent(g, AgentKind::Group, Some("main")));
        }
        m
    }

    #[test]
    fn a_jobs_root_has_no_card_and_its_groups_are_trees_of_their_own() {
        let model = job_with_groups(&["c1", "c2"]);
        let mut flow = new_flow();
        assert!(sync(&mut flow, &model, true));
        assert!(flow.node("main").is_none());
        assert!(flow.node("c1").is_some() && flow.node("c2").is_some());
        assert!(flow.edges().is_empty(), "no edge from a hidden root");
        assert_eq!(model.agent_count(), 2);
        let (a, b) = (flow.node("c1").unwrap(), flow.node("c2").unwrap());
        assert!(
            a.position.x + a.width <= b.position.x && a.position.y == b.position.y,
            "the trees stand side by side"
        );
        assert_eq!(model.last_active_agent_id().as_deref(), Some("c2"));
    }

    #[test]
    fn a_jobs_first_cards_stand_apart_without_a_relayout() {
        use crate::fact::{Fact, FactKind};
        let mut model = job_with_groups(&["g1", "g2"]);
        for (id, group) in [("a", "g1"), ("b", "g1"), ("c", "g2")] {
            model.apply_fact(&Fact {
                agent: Some(id.into()),
                ts: None,
                kind: FactKind::Agent {
                    kind: AgentKind::Subagent,
                    parent: Some(group.into()),
                    agent_type: None,
                    description: None,
                    spawned_by: None,
                    interactive: false,
                },
            });
        }
        let mut flow = new_flow();
        assert!(sync(&mut flow, &model, false));
        let cards: Vec<_> = ["g1", "g2", "a", "b", "c"]
            .iter()
            .map(|id| flow.node(id).unwrap())
            .collect();
        for (i, a) in cards.iter().enumerate() {
            for b in &cards[i + 1..] {
                let apart = a.position.x + a.width <= b.position.x
                    || b.position.x + b.width <= a.position.x
                    || a.position.y + a.height <= b.position.y
                    || b.position.y + b.height <= a.position.y;
                assert!(apart, "{} and {} overlap", a.id, b.id);
            }
        }
    }

    #[test]
    fn a_job_with_only_its_header_draws_nothing() {
        let model = job_with_groups(&[]);
        let mut flow = new_flow();
        assert!(!sync(&mut flow, &model, true));
        assert_eq!(flow.nodes().count(), 0);
        assert_eq!(model.agent_count(), 0);
        assert_eq!(model.last_active_agent_id(), None);
    }

    #[test]
    fn a_plain_session_keeps_its_root() {
        let mut flow = new_flow();
        sync(&mut flow, &model_with_subagent(), true);
        assert!(flow.node("main").is_some());
    }

    #[test]
    fn sync_creates_nodes_and_edge() {
        let model = model_with_subagent();
        let mut flow = new_flow();
        let structural = sync(&mut flow, &model, true);
        assert!(structural);
        assert!(flow.node_content_mut("main").is_some());
        assert!(flow.node_content_mut("abc123").is_some());
        // One edge main -> abc123.
        assert_eq!(flow.edges().len(), 1);
    }

    #[test]
    fn sync_idempotent() {
        let model = model_with_subagent();
        let mut flow = new_flow();
        let first = sync(&mut flow, &model, true);
        assert!(first);
        let node_count = flow.nodes().count();
        let edge_count = flow.edges().len();

        // Applying the same model again adds nothing structural.
        let second = sync(&mut flow, &model, true);
        assert!(!second);
        assert_eq!(flow.nodes().count(), node_count);
        assert_eq!(flow.edges().len(), edge_count);
    }

    #[test]
    fn sync_preserves_selection() {
        let model = model_with_subagent();
        let mut flow = new_flow();
        sync(&mut flow, &model, true);
        flow.select_node("abc123");
        assert_eq!(
            flow.selected_nodes().next().map(|n| n.id.clone()),
            Some("abc123".to_string())
        );

        // Re-sync after a non-structural change (e.g. a tool call added).
        let mut model2 = model;
        if let Some(a) = model2.agents.get_mut("abc123") {
            a.output_tokens += 100;
        }
        sync(&mut flow, &model2, true);
        assert_eq!(
            flow.selected_nodes().next().map(|n| n.id.clone()),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn edge_animation_follows_status() {
        let mut model = model_with_subagent();
        let mut flow = new_flow();
        sync(&mut flow, &model, true);
        // Running subagent -> animated edge.
        let edge_id = edge_id("abc123");
        let animated = flow
            .edges()
            .iter()
            .find(|e| e.id == edge_id)
            .map(|e| e.animated);
        assert_eq!(animated, Some(true));

        // The edge content mirrors running-ness (drives the distinct color).
        assert!(flow.edge_content_mut(&edge_id).unwrap().running);

        // Mark done, re-sync -> no longer animated, color back to default.
        if let Some(a) = model.agents.get_mut("abc123") {
            a.status = AgentStatus::Done;
        }
        sync(&mut flow, &model, true);
        let animated = flow
            .edges()
            .iter()
            .find(|e| e.id == edge_id)
            .map(|e| e.animated);
        assert_eq!(animated, Some(false));
        assert!(!flow.edge_content_mut(&edge_id).unwrap().running);
    }

    #[test]
    fn graph_is_structurally_read_only() {
        use rataflow::Reconnectable;

        let model = model_with_subagent();
        let mut flow = new_flow();
        sync(&mut flow, &model, true);

        for node in flow.nodes() {
            assert!(node.selectable, "nodes stay selectable (detail panel)");
            assert!(node.draggable, "nodes stay draggable (manual arranging)");
            assert!(!node.deletable, "nodes must not be deletable");
            assert!(!node.connectable, "nodes must not start connections");
        }
        for edge in flow.edges() {
            assert!(!edge.selectable, "edges carry no selectable meaning");
            assert!(!edge.deletable);
            assert_eq!(edge.reconnectable, Reconnectable::None);
        }
    }

    #[test]
    fn manual_mode_local_placement_moves_nothing_existing() {
        let mut model = model_with_subagent();
        let mut flow = new_flow();
        // Initial layout (camera engaged).
        sync(&mut flow, &model, true);
        let main_pos = flow.node("main").unwrap().position;
        let first_sub = flow.node("abc123").unwrap().position;

        // Camera now Manual: a second subagent arrives, relayout deferred.
        let meta2 = SubagentMeta {
            agent_type: Some("guide".into()),
            description: None,
            tool_use_id: Some("ag2".into()),
            stopped_by_user: None,
        };
        model.apply_meta("def456", None, &meta2);
        let structural = sync(&mut flow, &model, false);
        assert!(structural);

        // Nothing existing moved...
        assert_eq!(flow.node("main").unwrap().position, main_pos);
        assert_eq!(flow.node("abc123").unwrap().position, first_sub);
        // ...and the newcomer landed below its parent, not at the origin.
        let new_pos = flow.node("def456").unwrap().position;
        assert!(new_pos.y > main_pos.y, "child placed below parent");
        assert_ne!((new_pos.x, new_pos.y), (0.0, 0.0));
    }

    #[test]
    fn cards_render_into_buffer() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;

        let model = model_with_subagent();
        let mut flow = new_flow();
        sync(&mut flow, &model, true);
        flow.request_fit_view();

        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        (&mut flow).render(area, &mut buf);

        let mut text = String::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(
            text.contains("claude"),
            "main card title missing from render:\n{text}"
        );
        assert!(
            text.contains("guide"),
            "subagent card title missing from render:\n{text}"
        );
    }

    #[test]
    fn a_hidden_agent_leaves_the_canvas_with_its_subtree_and_returns() {
        use crate::fact::{Fact, FactKind};
        let t = |m: u32| {
            chrono::DateTime::parse_from_rfc3339(&format!("2026-10-06T10:{m:02}:00Z"))
                .ok()
                .map(|d| d.with_timezone(&chrono::Utc))
        };
        let agent = |id: &str, kind, parent: &str, m| Fact {
            agent: Some(id.into()),
            ts: t(m),
            kind: FactKind::Agent {
                kind,
                parent: Some(parent.into()),
                agent_type: None,
                description: None,
                spawned_by: None,
                interactive: false,
            },
        };
        let mut model = model_with_subagent();
        model.apply_fact(&agent("g", AgentKind::Group, "main", 1));
        model.apply_fact(&agent("m", AgentKind::Subagent, "g", 1));
        let mut flow = new_flow();
        sync(&mut flow, &model, false);
        flow.select_node("m");
        assert!(flow.node("m").is_some());

        let gone = Fact {
            agent: Some("g".into()),
            ts: t(2),
            kind: FactKind::Gone,
        };
        model.apply_fact(&gone);
        assert!(sync(&mut flow, &model, false));
        assert!(flow.node("g").is_none() && flow.node("m").is_none());
        assert!(flow.node("abc123").is_some(), "the rest stays");
        assert_eq!(flow.selected_nodes().count(), 0, "deselected");
        assert_eq!(model.agent_count(), 2);

        model.apply_fact(&Fact {
            agent: Some("g".into()),
            ts: t(3),
            kind: FactKind::Declared(None),
        });
        assert!(sync(&mut flow, &model, false));
        assert!(flow.node("g").is_some() && flow.node("m").is_some());
        assert_eq!(model.agent_count(), 4);
    }
}
