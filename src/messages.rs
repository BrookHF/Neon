//! Message, tool, and request vocabulary. The model-facing shape lives here.

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    /// Gemini names the model side `model`, not `assistant`.
    pub fn wire(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "model",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    /// Internal id. Gemini keys tool results by name, so this is ours alone.
    pub id: String,
    pub name: String,
    pub args: Value,
}

#[derive(Clone, Debug)]
pub enum Part {
    Text(String),
    Call(ToolCall),
    Result {
        call_id: String,
        name: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Clone, Debug)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

impl Message {
    pub fn user(text: &str) -> Self {
        Message {
            role: Role::User,
            parts: vec![Part::Text(text.to_string())],
        }
    }

    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Debug)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub model: String,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

#[derive(Clone, Debug)]
pub enum Event {
    Delta(String),
    Call(ToolCall),
    Done { finish_reason: Option<String> },
    Error(String),
}

/// Rough token estimate: 4 characters per token. Good enough for a budget check.
pub fn estimate_tokens(messages: &[Message]) -> usize {
    messages.iter().map(|m| m.text().len() / 4).sum()
}
