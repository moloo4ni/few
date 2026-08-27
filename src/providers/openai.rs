use super::{
    Msg, ProbeOutcome, Provider, ProviderError, Reply, Role, StreamDelta, ToolCall, ToolDef, Usage,
};
use futures_util::StreamExt;
use reqwest::{Client, Response};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
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

    async fn post_stream(&self, path: &str, body: &Value) -> Result<Response, ProviderError> {
        let mut req = self.client.post(self.url(path)).json(body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ProviderError::Http(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(classify_status(status.as_u16(), &text));
        }
        Ok(resp)
    }

    fn chat_body(&self, messages: &[Msg], tools: &[ToolDef], tool_choice: Value) -> Value {
        let wire_msgs: Vec<WireMsg> = messages.iter().map(WireMsg::from_msg).collect();
        let mut body = json!({
            "model": self.current_model(),
            "messages": wire_msgs,
            "stream": true,
            "stream_options": {"include_usage": true},
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
        body
    }

    async fn stream_once<F>(
        &self,
        messages: &[Msg],
        tools: &[ToolDef],
        tool_choice: Value,
        mut on_delta: F,
    ) -> Result<Reply, ProviderError>
    where
        F: FnMut(StreamDelta) + Send,
    {
        let body = self.chat_body(messages, tools, tool_choice);
        let resp = self.post_stream("/chat/completions", &body).await?;
        let mut stream = resp.bytes_stream();

        let mut asm = StreamAssembler::default();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|e| ProviderError::Http(e.to_string()))?;
            asm.push_bytes(&bytes, &mut on_delta);
            if let Some(err) = asm.error.take() {
                return Err(ProviderError::Http(err));
            }
            if asm.done {
                break;
            }
        }
        Ok(asm.finish())
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

    pub async fn probe_tool_calling(&self) -> ProbeOutcome {
        let msgs = [Msg::user("Call the few_probe tool immediately.")];
        let tools = [ToolDef {
            name: "few_probe",
            description: "no-op probe used by Few at startup",
            parameters: json!({"type": "object", "properties": {}, "required": []}),
        }];

        let mut last_error: Option<ProviderError> = None;
        for choice in [json!("required"), json!("auto")] {
            match self.stream_once(&msgs, &tools, choice, |_| {}).await {
                Ok(reply) => {
                    return if reply.tool_calls.is_empty() {
                        ProbeOutcome::Unsupported(
                            "model responded with plain text instead of a native tool call"
                                .to_owned(),
                        )
                    } else {
                        ProbeOutcome::Supported
                    };
                }
                Err(e @ ProviderError::NoToolSupport(_)) => {
                    return ProbeOutcome::Unsupported(e.to_string());
                }
                Err(e) => last_error = Some(e),
            }
        }

        ProbeOutcome::Unavailable(
            last_error
                .map(|e| e.to_string())
                .unwrap_or_else(|| "no response".into()),
        )
    }
}

impl Provider for OpenAiProvider {
    fn model_name(&self) -> String {
        self.current_model()
    }

    async fn complete_streaming<F>(
        &self,
        messages: &[Msg],
        tools: &[ToolDef],
        on_delta: F,
    ) -> Result<Reply, ProviderError>
    where
        F: FnMut(StreamDelta) + Send,
    {
        self.stream_once(messages, tools, json!("auto"), on_delta)
            .await
    }
}

#[derive(Default)]
struct ToolCallAcc {
    id: Option<String>,
    name: String,
    args: String,
}

#[derive(Default)]
struct StreamAssembler {
    buf: String,
    done: bool,
    error: Option<String>,
    content: String,
    reasoning: String,
    calls: BTreeMap<u64, ToolCallAcc>,
    usage: Usage,
}

impl StreamAssembler {
    fn push_bytes(&mut self, bytes: &[u8], on_delta: &mut impl FnMut(StreamDelta)) {
        self.buf.push_str(&String::from_utf8_lossy(bytes));
        while let Some(pos) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=pos).collect();
            let line = line.trim();
            if self.handle_line(line, on_delta).is_err() {
                return;
            }
        }
    }

    fn handle_line(
        &mut self,
        line: &str,
        on_delta: &mut impl FnMut(StreamDelta),
    ) -> Result<(), ()> {
        let Some(data) = line.strip_prefix("data:") else {
            return Ok(());
        };
        let data = data.trim();
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        if data.is_empty() {
            return Ok(());
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return Ok(());
        };
        if let Some(errmsg) = extract_error(&v) {
            self.error = Some(errmsg);
            return Err(());
        }
        if let Some(u) = v.get("usage") {
            if let Ok(parsed) = serde_json::from_value::<Usage>(u.clone()) {
                self.usage = parsed;
            }
        }
        let Some(delta) = v
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|ch| ch.get("delta"))
        else {
            return Ok(());
        };

        if let Some(t) = delta.get("content").and_then(|t| t.as_str()) {
            if !t.is_empty() {
                self.content.push_str(t);
                on_delta(StreamDelta::Text(t.to_owned()));
            }
        }
        for key in ["reasoning_content", "reasoning"] {
            if let Some(t) = delta.get(key).and_then(|t| t.as_str()) {
                if !t.is_empty() {
                    self.reasoning.push_str(t);
                    on_delta(StreamDelta::Reasoning(t.to_owned()));
                }
            }
        }
        if let Some(tc_list) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tc_list {
                let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                let entry = self.calls.entry(idx).or_default();
                if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                    if entry.id.is_none() {
                        entry.id = Some(id.to_owned());
                    }
                }
                if let Some(f) = tc.get("function") {
                    if let Some(n) = f.get("name").and_then(|n| n.as_str()) {
                        entry.name.push_str(n);
                    }
                    if let Some(a) = f.get("arguments").and_then(|a| a.as_str()) {
                        entry.args.push_str(a);
                    }
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> Reply {
        let tool_calls = self
            .calls
            .into_iter()
            .map(|(idx, acc)| {
                let id = acc
                    .id
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("call_{idx}"));
                let args = if acc.args.is_empty() {
                    "{}".to_owned()
                } else {
                    acc.args
                };
                ToolCall::parse(id, acc.name, args)
            })
            .collect();
        Reply {
            content: self.content,
            reasoning: if self.reasoning.is_empty() {
                None
            } else {
                Some(self.reasoning)
            },
            tool_calls,
            usage: self.usage,
        }
    }
}

const UNSUPPORT_PHRASES: &[&str] = &[
    "not supported",
    "unsupported",
    "does not support",
    "doesn't support",
    "no support for",
];

fn classify_status(status: u16, body: &str) -> ProviderError {
    let lowered = body.to_lowercase();
    let mentions_tools = lowered.contains("tool") || lowered.contains("function");
    let denies_capability = UNSUPPORT_PHRASES.iter().any(|p| lowered.contains(p));
    if matches!(status, 400 | 404 | 422 | 501) && mentions_tools && denies_capability {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn feed(sse: &str) -> (StreamAssembler, Vec<String>) {
        let mut asm = StreamAssembler::default();
        let out = RefCell::new(Vec::new());
        asm.push_bytes(sse.as_bytes(), &mut |d| match d {
            StreamDelta::Text(t) => out.borrow_mut().push(format!("text:{t}")),
            StreamDelta::Reasoning(t) => out.borrow_mut().push(format!("think:{t}")),
        });
        (asm, out.into_inner())
    }

    #[test]
    fn sse_streams_text_and_reasoning_and_usage() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\",\"reasoning_content\":\"why\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3}}\n\n",
            "data: [DONE]\n\n"
        );
        let (asm, deltas) = feed(sse);
        assert_eq!(deltas, vec!["text:Hel", "text:lo", "think:why"]);
        let reply = asm.finish();
        assert_eq!(reply.content, "Hello");
        assert_eq!(reply.reasoning.as_deref(), Some("why"));
        assert_eq!(reply.usage.prompt_tokens, 11);
        assert_eq!(reply.usage.completion_tokens, 3);
    }

    #[test]
    fn sse_merges_fragmented_tool_calls_by_index() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"tc1\",\"function\":{\"name\":\"wr\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"ite\",\"arguments\":\"th\\\": \\\"x\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"name\":\"shell\"}}]}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (asm, _) = feed(sse);
        let reply = asm.finish();
        assert_eq!(reply.tool_calls.len(), 2);
        let first = &reply.tool_calls[0];
        assert_eq!(first.id, "tc1");
        assert_eq!(first.name, "write");
        assert_eq!(
            first.arguments.get("path").and_then(|p| p.as_str()),
            Some("x")
        );
        assert_eq!(reply.tool_calls[1].name, "shell");
        assert!(
            reply.tool_calls[1].arguments.is_object(),
            "missing-args tool call must default to {{}}"
        );
    }

    #[test]
    fn sse_error_chunk_surfaces() {
        let sse = "data: {\"error\":{\"message\":\"boom\"}}\n\n";
        let (asm, _) = feed(sse);
        assert_eq!(asm.error.as_deref(), Some("boom"));
    }

    #[test]
    fn sse_tolerates_garbage_lines() {
        let sse = ": keep-alive\n\nevent: ping\ndata: not json\ndata: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
        let (asm, deltas) = feed(sse);
        assert_eq!(deltas, vec!["text:ok"]);
        assert_eq!(asm.finish().content, "ok");
    }

    #[test]
    fn classifies_no_tool_support() {
        let e = classify_status(400, r#"{"error":"model does not support tools"}"#);
        assert!(matches!(e, ProviderError::NoToolSupport(_)));
        let e = classify_status(400, "function calling is unsupported by this model");
        assert!(matches!(e, ProviderError::NoToolSupport(_)));
        let e = classify_status(
            400,
            r#"{"error":{"message":"Upstream request failed: Model is unavailable."}}"#,
        );
        assert!(
            matches!(e, ProviderError::Http(_)),
            "transient gateway errors must not look like missing tool support"
        );
        let e = classify_status(400, "bad request");
        assert!(matches!(e, ProviderError::Http(_)));
    }

    #[test]
    fn tool_call_parse_bad_json_keeps_text() {
        let tc = ToolCall::parse("1".into(), "write".into(), "{oops".into());
        assert_eq!(tc.name, "write");
        assert!(tc.arguments.is_null());
        assert_eq!(tc.arguments_text, "{oops");
    }
}
