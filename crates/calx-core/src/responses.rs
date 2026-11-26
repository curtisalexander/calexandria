//! A streaming client for OpenAI's Responses API.

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::auth::ChatGptAuth;
use crate::message::{Event, Item, ToolCall, Usage};
use crate::sse::SseParser;
use crate::tools::ToolSpec;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub struct Request<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub items: &'a [Item],
    pub tools: &'a [ToolSpec],
    /// Reasoning effort, such as "low", "medium", or "high".
    pub effort: Option<&'a str>,
    /// Groups requests from one conversation so the server can reuse its prompt cache.
    pub session_id: &'a str,
}

pub struct Reply {
    pub items: Vec<Item>,
    pub usage: Usage,
}

pub struct ResponsesClient {
    http: reqwest::Client,
    base_url: String,
    auth: ChatGptAuth,
}

impl ResponsesClient {
    /// `CALX_OPENAI_BASE_URL` overrides the endpoint, such as for a proxy or a test server.
    pub fn new(auth: ChatGptAuth) -> Self {
        let base_url =
            std::env::var("CALX_OPENAI_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let user_agent = format!("calx/{}", env!("CARGO_PKG_VERSION"));
        let http = reqwest::Client::builder()
            .user_agent(user_agent)
            .build()
            .expect("the HTTP client configuration is valid");
        Self {
            http,
            base_url,
            auth,
        }
    }

    /// Sends the conversation and streams the reply, calling `on_event` for each piece.
    pub async fn stream(
        &self,
        request: &Request<'_>,
        mut on_event: impl FnMut(Event),
    ) -> Result<Reply> {
        let body = request_body(request);
        let mut retried = false;
        let response = loop {
            let token = self.auth.access_token().await?;
            let response = self
                .http
                .post(format!("{}/responses", self.base_url))
                .bearer_auth(token)
                .header("accept", "text/event-stream")
                .json(&body)
                .send()
                .await
                .context("could not reach OpenAI")?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !retried {
                retried = true;
                self.auth.refresh().await?;
                continue;
            }
            break response;
        };

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or(body);
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                bail!(
                    "{message}\nCheck your ChatGPT plan usage at https://chatgpt.com/settings/usage"
                );
            }
            bail!("OpenAI returned {status}: {message}");
        }

        let mut parser = SseParser::default();
        let mut reply = Reply {
            items: Vec::new(),
            usage: Usage::default(),
        };
        let mut completed = false;
        let mut bytes = response.bytes_stream();
        while let Some(chunk) = bytes.next().await {
            let chunk = chunk.context("the reply stream broke off")?;
            for data in parser.push(&chunk) {
                if data == "[DONE]" {
                    continue;
                }
                let event: Value = serde_json::from_str(&data)
                    .with_context(|| format!("could not parse stream event: {data}"))?;
                match event["type"].as_str().unwrap_or_default() {
                    "response.output_text.delta" => {
                        let delta = event["delta"].as_str().unwrap_or_default();
                        on_event(Event::TextDelta(delta.to_string()));
                    }
                    "response.reasoning_summary_text.delta" => {
                        let delta = event["delta"].as_str().unwrap_or_default();
                        on_event(Event::ReasoningDelta(delta.to_string()));
                    }
                    "response.output_item.done" => {
                        if let Some(item) = output_item(&event["item"]) {
                            reply.items.push(item);
                        }
                    }
                    "response.completed" => {
                        completed = true;
                        reply.usage = usage(&event["response"]["usage"]);
                    }
                    "response.incomplete" => {
                        let reason = event["response"]["incomplete_details"]["reason"]
                            .as_str()
                            .unwrap_or("unknown");
                        bail!("the reply stopped early ({reason})");
                    }
                    "response.failed" => {
                        let message = event["response"]["error"]["message"]
                            .as_str()
                            .unwrap_or("unknown error");
                        bail!("the reply failed: {message}");
                    }
                    "error" => {
                        let message = event["message"]
                            .as_str()
                            .or(event["error"]["message"].as_str())
                            .unwrap_or("unknown error");
                        bail!("the reply failed: {message}");
                    }
                    _ => {}
                }
            }
        }
        if !completed {
            bail!("the reply stream ended before the reply was complete");
        }
        Ok(reply)
    }
}

/// Converts a finished output item from the stream to a conversation item.
fn output_item(item: &Value) -> Option<Item> {
    let raw = Some(item.clone());
    match item["type"].as_str()? {
        "message" => Some(Item::Assistant {
            text: joined_text(&item["content"], "output_text"),
            raw,
        }),
        "reasoning" => Some(Item::Reasoning {
            summary: joined_text(&item["summary"], "summary_text"),
            raw,
        }),
        "function_call" => Some(Item::ToolCall {
            call: ToolCall {
                id: item["call_id"].as_str()?.to_string(),
                name: item["name"].as_str()?.to_string(),
                arguments: item["arguments"].as_str().unwrap_or("{}").to_string(),
            },
            raw,
        }),
        _ => None,
    }
}

fn joined_text(parts: &Value, kind: &str) -> String {
    let texts: Vec<&str> = parts
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["type"] == kind)
        .filter_map(|p| p["text"].as_str())
        .collect();
    texts.join("\n\n")
}

/// Converts a conversation item to Responses input. Items this API returned
/// keep their IDs, so the server can match calls to the reasoning behind them.
fn input_item(item: &Item) -> Option<Value> {
    let id = |raw: &Option<Value>| raw.as_ref().and_then(|r| r.get("id").cloned());
    Some(match item {
        Item::User { text } => json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": text }],
        }),
        Item::Assistant { text, raw } => {
            let mut message = json!({
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{ "type": "output_text", "text": text, "annotations": [] }],
            });
            if let Some(id) = id(raw) {
                message["id"] = id;
            }
            if let Some(phase) = raw.as_ref().and_then(|r| r.get("phase")) {
                message["phase"] = phase.clone();
            }
            message
        }
        // Reasoning only means something to the API that produced it, which
        // needs its encrypted content back unchanged.
        Item::Reasoning { raw, .. } => raw.clone()?,
        Item::ToolCall { call, raw } => {
            let mut value = json!({
                "type": "function_call",
                "call_id": call.id,
                "name": call.name,
                "arguments": call.arguments,
                "status": "completed",
            });
            if let Some(id) = id(raw) {
                value["id"] = id;
            }
            value
        }
        Item::ToolResult { call_id, output } => json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output,
        }),
    })
}

fn request_body(request: &Request<'_>) -> Value {
    // The system prompt goes first as a developer message rather than in
    // `instructions`, so the start of the prompt stays identical across turns
    // and the server can cache it.
    let mut input = vec![json!({
        "type": "message",
        "role": "developer",
        "content": [{ "type": "input_text", "text": request.system }],
    })];
    input.extend(request.items.iter().filter_map(input_item));
    let mut body = json!({
        "model": request.model,
        "input": input,
        "stream": true,
        "store": false,
        "prompt_cache_key": request.session_id,
    });
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                    "strict": false,
                })
            })
            .collect();
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = json!("auto");
        body["parallel_tool_calls"] = json!(true);
    }
    if let Some(effort) = request.effort {
        body["reasoning"] = json!({ "effort": effort, "summary": "auto" });
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    body
}

fn usage(value: &Value) -> Usage {
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    Usage {
        input_tokens: n(&value["input_tokens"]),
        cached_input_tokens: n(&value["input_tokens_details"]["cached_tokens"]),
        output_tokens: n(&value["output_tokens"]),
    }
}
