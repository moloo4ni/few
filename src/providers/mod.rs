pub mod openai;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments_text: String,
    pub arguments: Value,
}

impl ToolCall {
    pub fn parse(id: String, name: String, arguments_text: String) -> Self {
        let arguments = serde_json::from_str::<Value>(&arguments_text).unwrap_or(Value::Null);
        Self {
            id,
            name,
            arguments_text,
            arguments,
        }
    }

    pub fn primary_arg(&self) -> String {
        for key in ["path", "command"] {
            if let Some(v) = self.arguments.get(key).and_then(|v| v.as_str()) {
                return v.to_owned();
            }
        }
        self.arguments_text.chars().take(80).collect()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Msg {
    pub role: Role,
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
}

impl Msg {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            ..Default::default()
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            ..Default::default()
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            ..Default::default()
        }
    }

    pub fn tool_result(
        call_id: impl Into<String>,
        tool_name: &str,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(call_id.into()),
            name: Some(tool_name.to_owned()),
            ..Default::default()
        }
    }

    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub content: String,
    pub reasoning: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
}

#[derive(Debug, Clone)]
pub enum ProviderError {
    NoToolSupport(String),
    Http(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::NoToolSupport(m) => write!(f, "{m}"),
            ProviderError::Http(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ProviderError {}

#[derive(Debug, Clone)]
pub enum ProbeOutcome {
    Supported,
    Unsupported(String),
    Unavailable(String),
}

pub trait Provider {
    fn model_name(&self) -> String;

    fn complete_streaming<F>(
        &self,
        messages: &[Msg],
        tools: &[ToolDef],
        on_delta: F,
    ) -> impl Future<Output = Result<Reply, ProviderError>> + Send
    where
        F: FnMut(StreamDelta) + Send;
}

#[derive(Debug, Clone)]
pub enum StreamDelta {
    Text(String),
    Reasoning(String),
}
