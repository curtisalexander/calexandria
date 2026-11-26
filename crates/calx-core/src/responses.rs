//! A streaming client for OpenAI's Responses API.

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::auth::ChatGptAuth;
use crate::message::{Event, Message, Role, Usage};
use crate::sse::SseParser;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub struct Request<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub messages: &'a [Message],
    /// Reasoning effort, such as "low", "medium", or "high".
    pub effort: Option<&'a str>,
    /// Groups requests from one conversation so the server can reuse its prompt cache.
    pub session_id: &'a str,
}

pub struct Reply {
    pub text: String,
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
            text: String::new(),
            usage: Usage::default(),
        };
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
                        reply.text.push_str(delta);
                        on_event(Event::TextDelta(delta.to_string()));
                    }
                    "response.reasoning_summary_text.delta" => {
                        let delta = event["delta"].as_str().unwrap_or_default();
                        on_event(Event::ReasoningDelta(delta.to_string()));
                    }
                    "response.completed" => reply.usage = usage(&event["response"]["usage"]),
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
        Ok(reply)
    }
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
    input.extend(request.messages.iter().map(|m| match m.role {
        Role::User => json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": m.text }],
        }),
        Role::Assistant => json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": m.text }],
        }),
    }));
    let mut body = json!({
        "model": request.model,
        "input": input,
        "stream": true,
        "store": false,
        "prompt_cache_key": request.session_id,
    });
    if let Some(effort) = request.effort {
        body["reasoning"] = json!({ "effort": effort, "summary": "auto" });
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
