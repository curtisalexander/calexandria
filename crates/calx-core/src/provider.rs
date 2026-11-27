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

    /// The model's context window in tokens, when the server says. Servers
    /// report it in different places: an entry of `/models` (OpenRouter's
    /// `context_length`, vLLM's `max_model_len`, LM Studio's
    /// `max_context_length`, llama.cpp's `meta.n_ctx_train`), or Ollama's
    /// `/api/show`. OpenAI's API does not, so this is `None` there. It gives
    /// up after a few seconds, and on any error.
    pub async fn context_window(&self, model: &str) -> Option<u64> {
        let http = match self {
            Client::Responses(c) => &c.http,
            Client::Chat(c) => &c.http,
        };
        let lookup = async {
            if let Ok(list) = http.get_json(&format!("{}/models", http.base_url)).await
                && let Some(n) = window_in_list(&list, model)
            {
                return Some(n);
            }
            // Ollama serves its own API next to the OpenAI-style one at /v1.
            let root = http.base_url.strip_suffix("/v1")?;
            let show = http
                .post_json(
                    &format!("{root}/api/show"),
                    &serde_json::json!({ "model": model }),
                )
                .await
                .ok()?;
            ollama_window(&show)
        };
        tokio::time::timeout(std::time::Duration::from_secs(4), lookup)
            .await
            .ok()
            .flatten()
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

    /// GETs `url` as JSON, with the server's auth.
    async fn get_json(&self, url: &str) -> Result<Value> {
        self.json(self.client.get(url)).await
    }

    /// POSTs `body` to `url` for JSON, with the server's auth.
    async fn post_json(&self, url: &str, body: &Value) -> Result<Value> {
        self.json(self.client.post(url).json(body)).await
    }

    async fn json(&self, mut request: reqwest::RequestBuilder) -> Result<Value> {
        if let Some(token) = self.auth.token().await? {
            request = request.bearer_auth(token);
        }
        let response = request.header("accept", "application/json").send().await?;
        anyhow::ensure!(response.status().is_success(), "{}", response.status());
        Ok(response.json().await?)
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

/// Fields servers use for a model's context window, most specific first.
const WINDOW_FIELDS: [&str; 8] = [
    "context_window",
    "context_length",
    "max_model_len",
    "max_context_length",
    "n_ctx",
    "n_ctx_train",
    "max_input_tokens",
    "input_token_limit",
];

/// The context window of `model` in a `/models` reply, read from its entry
/// or one level inside it, such as llama.cpp's `meta` or OpenRouter's
/// `top_provider`.
fn window_in_list(list: &Value, model: &str) -> Option<u64> {
    let entries = list["data"].as_array().or(list["models"].as_array())?;
    let entry = entries.iter().find(|e| {
        ["id", "slug", "name", "model"]
            .iter()
            .any(|k| e[*k].as_str() == Some(model))
    })?;
    let read = |v: &Value| {
        WINDOW_FIELDS
            .iter()
            .find_map(|f| v[*f].as_u64())
            .filter(|n| *n > 0)
    };
    read(entry).or_else(|| entry.as_object()?.values().find_map(read))
}

/// The window Ollama runs a model with: its `num_ctx` parameter when it has
/// one, else the model's own `<architecture>.context_length`.
fn ollama_window(show: &Value) -> Option<u64> {
    let num_ctx = show["parameters"].as_str().and_then(|p| {
        p.lines().find_map(|l| {
            let mut words = l.split_whitespace();
            (words.next()? == "num_ctx").then(|| words.next()?.parse().ok())?
        })
    });
    num_ctx.or_else(|| {
        show["model_info"]
            .as_object()?
            .iter()
            .find(|(k, _)| k.ends_with(".context_length"))?
            .1
            .as_u64()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_the_window_each_kind_of_server_reports() {
        let cases = [
            // OpenRouter
            (
                json!({"data": [{"id": "a/b", "context_length": 1}, {"id": "m", "context_length": 131072, "top_provider": {"context_length": 64000}}]}),
                Some(131072),
            ),
            // vLLM
            (
                json!({"data": [{"id": "m", "max_model_len": 32768}]}),
                Some(32768),
            ),
            // LM Studio
            (
                json!({"data": [{"id": "m", "max_context_length": 8192}]}),
                Some(8192),
            ),
            // llama.cpp
            (
                json!({"data": [{"id": "m", "meta": {"n_ctx_train": 40960}}]}),
                Some(40960),
            ),
            // OpenAI: no window, and other models' windows do not count
            (
                json!({"data": [{"id": "m", "owned_by": "openai"}, {"id": "x", "context_window": 9}]}),
                None,
            ),
            (
                json!({"models": [{"slug": "m", "context_window": 272000}]}),
                Some(272000),
            ),
        ];
        for (list, want) in cases {
            assert_eq!(window_in_list(&list, "m"), want, "{list}");
        }

        let show = json!({"parameters": "stop \"<|im_end|>\"\nnum_ctx 16384", "model_info": {"qwen3.context_length": 40960}});
        assert_eq!(ollama_window(&show), Some(16384));
        let show = json!({"parameters": "stop x", "model_info": {"general.architecture": "qwen3", "qwen3.context_length": 40960}});
        assert_eq!(ollama_window(&show), Some(40960));
    }
}
