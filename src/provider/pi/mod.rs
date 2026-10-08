//! The pi provider: session lines in, [`Fact`]s out.
//!
//! One file is one session with one agent, `main`. pi has no subagents, so
//! there are no spawns; an assistant message ends the turn unless it
//! called tools (pi then asks again), and a failed or aborted turn is still a
//! turn that ended. The `toolResult` message is a tool call's only record of its
//! outcome.

use serde_json::Value;

pub mod discovery;
pub mod wire;

use crate::fact::{AgentKind, Fact, FactKind, Outcome, Statement};
use crate::provider::summary::{short_path, truncate_summary};
use crate::state::session::MAIN_ID;
use wire::{Block, Line, parse_line};

/// One session file being read.
#[derive(Debug, Clone, Default)]
pub struct Stream {
    /// The header's working directory, for relativising paths in summaries.
    cwd: Option<String>,
}

impl Stream {
    pub fn new() -> Self {
        Stream::default()
    }

    /// Parse one line and state what it says. `None` for a blank or
    /// unparsable line, or one that states nothing.
    pub fn push(&mut self, line: &str) -> Option<Statement> {
        let line = parse_line(line)?;
        let facts = self.facts(&line);
        (!facts.is_empty()).then_some(Statement {
            at: line.timestamp,
            facts,
        })
    }

    fn facts(&mut self, line: &Line) -> Vec<Fact> {
        let ts = line.timestamp;
        let by = |kind| Fact {
            agent: Some(MAIN_ID.to_string()),
            ts,
            kind,
        };
        let mut out = Vec::new();
        match line.kind.as_str() {
            "session" => {
                self.cwd = line.cwd.clone();
                out.push(by(FactKind::Agent {
                    kind: AgentKind::Main,
                    parent: None,
                    agent_type: Some("pi".into()),
                    description: None,
                    spawned_by: None,
                    interactive: true,
                }));
                out.extend(line.cwd.clone().map(|value| Fact {
                    agent: None,
                    ts: None,
                    kind: FactKind::Session {
                        label: "cwd".into(),
                        value,
                    },
                }));
            }
            "model_change" => out.extend(line.model_id.clone().map(|m| by(FactKind::Model(m)))),
            "message" => {
                let Some(m) = &line.message else { return out };
                match m.role.as_str() {
                    "user" => {
                        // Even an image-only message: it starts a turn.
                        out.push(by(FactKind::Prompt(m.content.text())));
                    }
                    "assistant" => {
                        out.extend(m.model.clone().map(|m| by(FactKind::Model(m))));
                        out.extend(m.usage.as_ref().and_then(|u| u.output).map(|output| {
                            by(FactKind::Tokens {
                                output,
                                dedup: None,
                            })
                        }));
                        let text = m.content.text();
                        if !text.trim().is_empty() {
                            out.push(by(FactKind::Message(text)));
                        }
                        let mut calls = false;
                        if let wire::Content::Blocks(blocks) = &m.content {
                            for block in blocks {
                                match block {
                                    Block::Thinking { thinking } if !thinking.trim().is_empty() => {
                                        out.push(by(FactKind::Reasoning(thinking.clone())));
                                    }
                                    Block::ToolCall {
                                        id,
                                        name,
                                        arguments,
                                    } => {
                                        calls = true;
                                        out.push(by(FactKind::ToolStart {
                                            call: id.clone(),
                                            name: name.clone(),
                                            summary: summarize(arguments, self.cwd.as_deref()),
                                        }));
                                    }
                                    _ => {}
                                }
                            }
                        }
                        // Tool calls make pi ask again, unless the turn failed.
                        let failed = matches!(m.stop_reason.as_deref(), Some("error" | "aborted"));
                        if failed || !calls {
                            out.push(by(FactKind::Waiting));
                        }
                    }
                    "toolResult" => out.extend(m.tool_call_id.clone().map(|call| {
                        by(FactKind::ToolEnd {
                            call,
                            outcome: if m.is_error {
                                Outcome::Err
                            } else {
                                Outcome::Ok
                            },
                        })
                    })),
                    _ => {}
                }
            }
            _ => {}
        }
        out
    }
}

/// A tool call's one-liner: its command, else its path. pi's tools name them
/// `command` (bash) and `path` (read, write, edit).
fn summarize(arguments: &Value, cwd: Option<&str>) -> Option<String> {
    let arg = |key| arguments.get(key)?.as_str();
    arg("command")
        .map(truncate_summary)
        .or_else(|| arg("path").map(|p| short_path(p, cwd)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::Statement;

    /// The shipped capture, found the way a live session is, and run through
    /// the whole conformance check.
    #[test]
    fn capture_conforms() {
        let Some(root) = crate::provider::harness::fixture_dir("pi") else {
            return;
        };
        let file = root.join(
            "demo/--tmp-demo--/2026-10-08T22-48-57-536Z_01a11db4-99be-739e-9013-3d9a028a768d.jsonl",
        );
        let found = discovery::session_file(&file).unwrap();
        assert_eq!(found.session, "01a11db4-99be-739e-9013-3d9a028a768d");
        let streams = || {
            let mut stream = Stream::new();
            let text = std::fs::read_to_string(&file).unwrap();
            vec![
                text.lines()
                    .filter_map(|l| stream.push(l))
                    .collect::<Vec<Statement>>(),
            ]
        };
        crate::provider::harness::conform("pi", "demo", streams);
    }
}
