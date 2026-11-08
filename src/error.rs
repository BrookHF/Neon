//! Error type shared by the adapter, the tools, and the loop.

use std::fmt;

#[derive(Debug)]
pub enum AgentError {
    Config(String),
    Http(String),
    Adapter(String),
    Tool(String),
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::Config(m) => write!(f, "config: {m}"),
            AgentError::Http(m) => write!(f, "http: {m}"),
            AgentError::Adapter(m) => write!(f, "adapter: {m}"),
            AgentError::Tool(m) => write!(f, "tool: {m}"),
        }
    }
}

impl From<std::io::Error> for AgentError {
    fn from(e: std::io::Error) -> Self {
        AgentError::Http(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, AgentError>;
