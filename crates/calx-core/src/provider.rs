//! What the agent loop asks a model API for, and the APIs that answer:
//! OpenAI's Responses API ([`crate::responses`]) and the Chat Completions API
//! that most other servers speak ([`crate::chat`]), such as Ollama, MLX,
//! vLLM, OpenRouter, and company gateways.

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::auth::Auth;
use crate::chat::ChatClient;
use crate::message::{Event, Item, Usage};
use crate::responses::ResponsesClient;
use crate::tools::ToolSpec;

pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

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

/// Which wire format a server speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    Responses,
    Chat,
}

pub enum Client {
    Responses(ResponsesClient),
    Chat(ChatClient),
}

impl Client {
    pub fn new(api: Api, base_url: &str, auth: Auth) -> Self {
        let http = Http::new(base_url, auth);
        match api {
            Api::Responses => Client::Responses(ResponsesClient { http }),
            Api::Chat => Client::Chat(ChatClient { http }),
        }
    }

    /// Sends the conversation and streams the reply, calling `on_event` for each piece.
    pub async fn stream(
        &self,
        request: &Request<'_>,
        on_event: impl FnMut(Event),
    ) -> Result<Reply> {
        match self {
            Client::Responses(c) => c.stream(request, on_event).await,
            Client::Chat(c) => c.stream(request, on_event).await,
        }
    }

    /// For the user: "responses · https://api.openai.com/v1 · ChatGPT".
    pub fn describe(&self) -> String {
        let (api, http) = match self {
            Client::Responses(c) => ("responses", &c.http),
            Client::Chat(c) => ("chat", &c.http),
        };
        format!("{api} · {} · {}", http.base_url, http.auth.describe())
    }
}

/// An HTTP client for one server, with its auth.
pub(crate) struct Http {
    client: reqwest::Client,
    pub(crate) base_url: String,
    pub(crate) auth: Auth,
}

impl Http {
    fn new(base_url: &str, auth: Auth) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(format!("calx/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("the HTTP client configuration is valid");
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            auth,
        }
    }

    /// POSTs `body` to `path` for a stream of server-sent events. A 401 gets
    /// one retry with a refreshed token; any other failure is an error that
    /// carries the server's message.
    pub(crate) async fn post_stream(&self, path: &str, body: &Value) -> Result<reqwest::Response> {
        let url = format!("{}{path}", self.base_url);
        let mut retried = false;
        let response = loop {
            let mut request = self
                .client
                .post(&url)
                .header("accept", "text/event-stream")
                .json(body);
            if let Some(token) = self.auth.token().await? {
                request = request.bearer_auth(token);
            }
            let response = request
                .send()
                .await
                .with_context(|| format!("could not reach {}", self.base_url))?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED
                && !retried
                && self.auth.refresh().await?
            {
                retried = true;
                continue;
            }
            break response;
        };
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().await.unwrap_or_default();
        let message = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| {
                let error = &v["error"];
                error["message"]
                    .as_str()
                    .or(error.as_str())
                    .map(str::to_string)
            })
            .unwrap_or(body);
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS && matches!(self.auth, Auth::ChatGpt(_))
        {
            bail!("{message}\nCheck your ChatGPT plan usage at https://chatgpt.com/settings/usage");
        }
        bail!("{} returned {status}: {message}", self.base_url)
    }
}
