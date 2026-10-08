//! pi's wire format: the serde model for one session line, and nothing else.
//! Defensive like the others: a line that does not fit parses to nothing.

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// One session line: the header (`session`) or an entry (`message`,
/// `model_change`, ...). Entries link by `id`/`parentId`; the file is
/// append-only, so the order read is the order written.
#[derive(Debug, Deserialize)]
pub struct Line {
    #[serde(rename = "type")]
    pub kind: String,
    pub timestamp: Option<DateTime<Utc>>,
    /// The header's session id.
    pub id: Option<String>,
    pub cwd: Option<String>,
    #[serde(rename = "modelId")]
    pub model_id: Option<String>,
    pub message: Option<Message>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    /// `user`, `assistant`, `toolResult`, or `system`.
    pub role: String,
    #[serde(default)]
    pub content: Content,
    pub model: Option<String>,
    pub usage: Option<Usage>,
    #[serde(rename = "stopReason")]
    pub stop_reason: Option<String>,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: Option<String>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

#[derive(Debug, Deserialize)]
pub struct Usage {
    pub output: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<Block>),
}

impl Default for Content {
    fn default() -> Self {
        Content::Blocks(Vec::new())
    }
}

impl Content {
    /// The visible text, blocks joined, empty when there is none.
    pub fn text(&self) -> String {
        match self {
            Content::Text(t) => t.clone(),
            Content::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    Block::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum Block {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
    #[serde(rename = "toolCall")]
    ToolCall {
        id: String,
        name: String,
        #[serde(default)]
        arguments: serde_json::Value,
    },
    #[serde(other)]
    Other,
}

pub fn parse_line(line: &str) -> Option<Line> {
    serde_json::from_str(line).ok()
}
