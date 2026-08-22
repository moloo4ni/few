use super::{Msg, Provider, ProviderError, Reply, Role, ToolCall, ToolDef, Usage};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::RwLock;
use std::time::Duration;

pub struct OpenAiProvider {
    client: Client,
    base_url: String,
    api_key: Option<String>,
    model: RwLock<String>,
}

impl OpenAiProvider {
    pub fn new(base_url: &str, api_key: Option<&str>, model: &str) -> anyhow::Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(600))
            .build()?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
            api_key: api_key.map(str::to_owned),
            model: RwLock::new(model.to_owned()),
        })
    }

    pub fn set_model(&self, model: &str) {
        *self.model.write().unwrap() = model.to_owned();
    }

    fn current_model(&self) -> String {
        self.model.read().unwrap().clone()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, ProviderError> {
        let mut req = self.client.post(self.url(path)).json(body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ProviderError::Http(e.to_string()))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(classify_status(status.as_u16(), &text));
        }
        serde_json::from_str::<Value>(&text).map_err(|e| {
            ProviderError::Http(format!(
                "invalid JSON from provider: {e}; body: {}",
                truncate(&text, 300)
            ))
        })
    }

    pub async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        let mut req = self.client.get(self.url("/models"));
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.timeout(Duration::from_secs(20)).send().await?;
        let v: Value = resp.error_for_status()?.json().await?;
        let mut ids = Vec::new();
        if let Some(data) = v.get("data").and_then(|d| d.as_array()) {
            for item in data {
                if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                    ids.push(id.to_owned());
                }
            }
        }
        Ok(ids)
    }

    async fn complete_with_choice(
        &self,
        messages: &[Msg],
        tools: &[ToolDef],
        tool_choice: Value,
    ) -> Result<Reply, ProviderError> {
        let wire_msgs: Vec<WireMsg> = messages.iter().map(WireMsg::from_msg).collect();
        let mut body = json!({
            "model": self.current_model(),
            "messages": wire_msgs,
        });
        if !tools.is_empty() {
            body["tools"] = json!(tools
                .iter()
                .map(|t| json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    },
                }))
                .collect::<Vec<_>>());
            body["tool_choice"] = tool_choice;
        }

        let value = self.post("/chat/completions", &body).await?;
        if let Some(errmsg) = extract_error(&value) {
            return Err(ProviderError::Http(errmsg));
        }

        let parsed: WireResponse = serde_json::from_value(value)
            .map_err(|e| ProviderError::Http(format!("unexpected provider response shape: {e}")))?;

        let choice = parsed
            .choices
            .as_ref()
            .and_then(|c| c.first())
            .ok_or_else(|| ProviderError::Http("provider returned no choices".into()))?;

        let msg = &choice.message;
        let tool_calls = msg
            .tool_calls
            .clone()
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(i, tc)| {
                let id = tc
                    .id
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("call_{i}"));
                let args_text = match tc.function.arguments {
                    Some(Value::String(s)) => s,
                    Some(other) => other.to_string(),
                    None => "{}".into(),
                };
                ToolCall::parse(id, tc.function.name, args_text)
            })
            .collect();

        Ok(Reply {
            content: msg.content.clone(),
            reasoning: msg
                .reasoning_content
                .clone()
                .or(msg.reasoning.clone())
                .filter(|s| !s.is_empty()),
            tool_calls,
            usage: parsed.usage.unwrap_or_default(),
        })
    }

    pub async fn probe_tool_calling(&self) -> Result<(), String> {
        let msgs = [Msg::user("Call the keiko_probe tool immediately.")];
        let tools = [ToolDef {
            name: "keiko_probe",
            description: "no-op probe used by Keiko at startup",
            parameters: json!({"type": "object", "properties": {}, "required": []}),
        }];

        match self
            .complete_with_choice(&msgs, &tools, json!("required"))
            .await
        {
            Ok(reply) => {
                if reply.tool_calls.is_empty() {
                    Err("model responded with plain text instead of a native tool call".into())
                } else {
                    Ok(())
                }
            }
            Err(e) => {
                let retry = self
                    .complete_with_choice(&msgs, &tools, json!("auto"))
                    .await;
                match retry {
                    Ok(reply) if !reply.tool_calls.is_empty() => Ok(()),
                    _ => Err(format!(
                        "provider/model does not provide native structured tool-calling ({e}); Keiko refuses prompt-based fallback - choose a tool-calling capable model"
                    )),
                }
            }
        }
    }
}

impl Provider for OpenAiProvider {
    fn model_name(&self) -> String {
        self.current_model()
    }

    async fn complete(&self, messages: &[Msg], tools: &[ToolDef]) -> Result<Reply, ProviderError> {
        self.complete_with_choice(messages, tools, json!("auto"))
            .await
    }
}

fn classify_status(status: u16, body: &str) -> ProviderError {
    let lowered = body.to_lowercase();
    let mentions_tools =
        lowered.contains("tool") || lowered.contains("function") || lowered.contains("support");
    if matches!(status, 400 | 404 | 422 | 501) && mentions_tools {
        ProviderError::NoToolSupport(truncate(body, 500))
    } else {
        ProviderError::Http(format!("HTTP {status}: {}", truncate(body, 500)))
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_owned()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

fn extract_error(v: &Value) -> Option<String> {
    if let Some(e) = v.get("error") {
        if let Some(m) = e.get("message").and_then(|m| m.as_str()) {
            return Some(m.to_owned());
        }
        if let Some(m) = e.as_str() {
            return Some(m.to_owned());
        }
        return Some(e.to_string());
    }
    if let Some(m) = v.get("message").and_then(|m| m.as_str()) {
        return Some(m.to_owned());
    }
    None
}

#[derive(Serialize)]
struct WireMsg {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WireTc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

impl WireMsg {
    fn from_msg(m: &Msg) -> Self {
        let role = match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let content = if m.content.is_empty() && m.role == Role::Assistant {
            None
        } else {
            Some(m.content.clone())
        };
        Self {
            role,
            content,
            tool_calls: if m.tool_calls.is_empty() {
                None
            } else {
                Some(
                    m.tool_calls
                        .iter()
                        .map(|tc| WireTc {
                            id: tc.id.clone(),
                            kind: "function",
                            function: WireFn {
                                name: tc.name.clone(),
                                arguments: if tc.arguments_text.is_empty() {
                                    "{}".to_owned()
                                } else {
                                    tc.arguments_text.clone()
                                },
                            },
                        })
                        .collect(),
                )
            },
            tool_call_id: m.tool_call_id.clone(),
            name: m.name.clone(),
        }
    }
}

#[derive(Serialize)]
struct WireTc {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFn,
}

#[derive(Serialize)]
struct WireFn {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct WireResponse {
    choices: Option<Vec<Choice>>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: RespMsg,
}

#[derive(Deserialize)]
struct RespMsg {
    #[serde(default, deserialize_with = "de_content_opt")]
    content: String,
    reasoning_content: Option<String>,
    reasoning: Option<String>,
    tool_calls: Option<Vec<RespTc>>,
}

fn de_content_opt<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: Option<Value> = Option::deserialize(deserializer)?;
    Ok(match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s,
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        Some(other) => other.to_string(),
    })
}

#[derive(Deserialize, Clone)]
struct RespTc {
    id: Option<String>,
    function: RespFn,
}

#[derive(Deserialize, Clone)]
struct RespFn {
    name: String,
    #[serde(default)]
    arguments: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_response() {
        let src = r#"{
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "thinking about it",
                    "reasoning_content": "let me look",
                    "tool_calls": [{
                        "id": "tc1",
                        "type": "function",
                        "function": {"name": "read", "arguments": "{\"path\": \"README.md\"}"}
                    }]
                }
            }],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20}
        }"#;
        let parsed: WireResponse = serde_json::from_str(src).unwrap();
        let msg = &parsed.choices.as_ref().unwrap()[0].message;
        assert_eq!(msg.content, "thinking about it");
        assert_eq!(msg.reasoning_content.as_deref(), Some("let me look"));
        let tcs = msg.tool_calls.as_ref().unwrap();
        assert_eq!(tcs[0].function.name, "read");
        let usage = parsed.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 100);
    }

    #[test]
    fn parses_parts_content_and_object_arguments() {
        let src = r#"{
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "hello "}, {"type": "text", "text": "world"}],
                    "tool_calls": [{
                        "function": {"name": "shell", "arguments": {"command": "ls"}}
                    }]
                }
            }]
        }"#;
        let parsed: WireResponse = serde_json::from_str(src).unwrap();
        let msg = &parsed.choices.unwrap()[0].message;
        assert_eq!(msg.content, "hello world");
        let args = msg.tool_calls.as_ref().unwrap()[0]
            .function
            .arguments
            .clone()
            .unwrap();
        assert!(args.is_object());
    }

    #[test]
    fn classifies_no_tool_support() {
        let e = classify_status(400, r#"{"error":"model does not support tools"}"#);
        assert!(matches!(e, ProviderError::NoToolSupport(_)));
        let e2 = classify_status(400, "bad request");
        assert!(matches!(e2, ProviderError::Http(_)));
    }

    #[test]
    fn tool_call_parse_bad_json_keeps_text() {
        let tc = ToolCall::parse("1".into(), "write".into(), "{oops".into());
        assert_eq!(tc.name, "write");
        assert!(tc.arguments.is_null());
        assert_eq!(tc.arguments_text, "{oops");
    }
}
