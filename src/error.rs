//! Error type shared by the adapter, the tools, and the loop.

use std::fmt;

#[derive(Debug)]
pub enum AgentError {
    Config(String),
    Http(String),
    /// A transient LLM failure worth waiting and retrying (429, 5xx, empty stream).
    Retryable(String),
    Adapter(String),
    Tool(String),
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::Config(m) => write!(f, "config: {m}"),
            AgentError::Http(m) => write!(f, "http: {m}"),
            AgentError::Retryable(m) => write!(f, "adapter: {m}"),
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

#[cfg(test)]
mod tests {
    use super::AgentError;

    #[test]
    fn retryable_reads_like_an_adapter_error_to_the_user() {
        assert_eq!(
            AgentError::Retryable("HTTP 429: rate limited".to_string()).to_string(),
            "adapter: HTTP 429: rate limited"
        );
    }
}
