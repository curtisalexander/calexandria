//! A streaming client for the Chat Completions API (`/chat/completions`),
//! which most model servers other than OpenAI's Responses API speak: Ollama,
//! LM Studio, MLX (`mlx_lm.server`), vLLM, llama.cpp, OpenRouter, and
//! company gateways.
//!
//! Chat Completions keeps no reasoning between requests, so reasoning items
//! are shown while they stream and then left out of later requests.

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::message::{Event, Item, ToolCall, Usage};
use crate::provider::{Http, Reply, Request};
use crate::sse::SseParser;

pub struct ChatClient {
    pub(crate) http: Http,
}

impl ChatClient {
    /// Sends the conversation and streams the reply, calling `on_event` for each piece.
    pub async fn stream(
        &self,
        request: &Request<'_>,
        mut on_event: impl FnMut(Event),
    ) -> Result<Reply> {
        let body = request_body(request);
        let response = self.http.post_stream("/chat/completions", &body).await?;
        let mut parser = SseParser::default();
        let mut reply = Assembler::default();
        let mut bytes = response.bytes_stream();
        'stream: while let Some(chunk) = bytes.next().await {
            let chunk = chunk.context("the reply stream broke off")?;
            for data in parser.push(&chunk) {
                if data == "[DONE]" {
                    break 'stream;
                }
                let chunk: Value = serde_json::from_str(&data)
                    .with_context(|| format!("could not parse stream chunk: {data}"))?;
                reply.push(&chunk, &mut on_event)?;
            }
        }
        reply.finish()
    }
}

/// Converts the conversation to Chat Completions messages. A reply's text
/// and tool calls become one assistant message, as the API expects.
fn messages(request: &Request<'_>) -> Vec<Value> {
    let mut out = vec![json!({ "role": "system", "content": request.system })];
    for item in request.items {
        let last_assistant = out.last_mut().filter(|m| m["role"] == "assistant");
        match item {
            Item::User { text } => out.push(json!({ "role": "user", "content": text })),
            Item::Assistant { text, .. } => match last_assistant {
                Some(m) if m["tool_calls"].is_null() && m["content"].is_string() => {
                    let joined = format!("{}\n\n{text}", m["content"].as_str().unwrap_or(""));
                    m["content"] = json!(joined);
                }
                _ => out.push(json!({ "role": "assistant", "content": text })),
            },
            Item::Reasoning { .. } | Item::Compaction { .. } => {}
            Item::ToolCall { call, .. } => {
                let call = json!({
                    "id": call.id,
                    "type": "function",
                    "function": { "name": call.name, "arguments": call.arguments },
                });
                match last_assistant {
                    Some(m) => match m["tool_calls"].as_array_mut() {
                        Some(calls) => calls.push(call),
                        None => m["tool_calls"] = json!([call]),
                    },
                    None => out.push(json!({
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [call],
                    })),
                }
            }
            Item::ToolResult { call_id, output } => out.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": output,
            })),
        }
    }
    out
}

fn request_body(request: &Request<'_>) -> Value {
    let mut body = json!({
        "model": request.model,
        "messages": messages(request),
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    },
                })
            })
            .collect();
        body["tools"] = Value::Array(tools);
    }
    if let Some(effort) = request.effort {
        body["reasoning_effort"] = json!(effort);
    }
    body
}

const THINK_START: &str = "<think>";
const THINK_END: &str = "</think>";

/// The length of the longest end of `text` that is the start of `tag`.
fn partial_tag(text: &str, tag: &str) -> usize {
    (1..tag.len())
        .rev()
        .find(|&n| text.ends_with(&tag[..n]))
        .unwrap_or(0)
}

/// Builds a reply from streamed chunks, in which a tool call arrives in
/// pieces keyed by its index: the ID and name first, then the arguments.
#[derive(Default)]
struct Assembler {
    reasoning: String,
    text: String,
    /// Reply text not yet sorted into text and `<think>` reasoning, kept
    /// while it might end in part of a tag.
    unsorted: String,
    thinking: bool,
    calls: Vec<ToolCall>,
    usage: Usage,
    finished: bool,
}

impl Assembler {
    fn push(&mut self, chunk: &Value, on_event: &mut impl FnMut(Event)) -> Result<()> {
        if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
            let message = error["message"].as_str().unwrap_or("unknown error");
            bail!("the reply failed: {message}");
        }
        if chunk["usage"].is_object() {
            let u = &chunk["usage"];
            let n = |v: &Value| v.as_u64().unwrap_or(0);
            self.usage = Usage {
                input_tokens: n(&u["prompt_tokens"]),
                cached_input_tokens: n(&u["prompt_tokens_details"]["cached_tokens"]),
                output_tokens: n(&u["completion_tokens"]),
            };
        }
        let Some(choice) = chunk["choices"].get(0) else {
            return Ok(());
        };
        let delta = &choice["delta"];
        // Servers that show reasoning name it either way.
        if let Some(r) = delta["reasoning_content"]
            .as_str()
            .or(delta["reasoning"].as_str())
            .filter(|r| !r.is_empty())
        {
            self.reasoning.push_str(r);
            on_event(Event::ReasoningDelta(r.to_string()));
        }
        if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            self.unsorted.push_str(text);
            self.sort(false, on_event);
        }
        for part in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = match part["index"].as_u64() {
                Some(i) => i as usize,
                // Without an index, an ID starts a new call; else it continues the last.
                None if part["id"].is_string() || self.calls.is_empty() => self.calls.len(),
                None => self.calls.len() - 1,
            };
            while self.calls.len() <= index {
                self.calls.push(ToolCall {
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                });
            }
            let call = &mut self.calls[index];
            if let Some(id) = part["id"].as_str().filter(|id| !id.is_empty()) {
                call.id = id.to_string();
            }
            if let Some(name) = part["function"]["name"].as_str()
                && call.name.is_empty()
            {
                call.name = name.to_string();
            }
            if let Some(args) = part["function"]["arguments"].as_str() {
                call.arguments.push_str(args);
            }
        }
        match choice["finish_reason"].as_str() {
            Some(reason @ ("length" | "content_filter")) => {
                bail!("the reply stopped early ({reason})")
            }
            Some(_) => {
                self.finished = true;
                self.sort(true, on_event);
            }
            None => {}
        }
        Ok(())
    }

    /// Moves reply text into text or reasoning: local reasoning models such
    /// as Qwen and DeepSeek put their thinking in the reply between
    /// `<think>` and `</think>`. Until `end`, it keeps back what could be
    /// the start of a tag split across chunks.
    fn sort(&mut self, end: bool, on_event: &mut impl FnMut(Event)) {
        loop {
            let tag = if self.thinking {
                THINK_END
            } else {
                THINK_START
            };
            let (out, found) = match self.unsorted.find(tag) {
                Some(at) => {
                    let out: String = self.unsorted.drain(..at).collect();
                    self.unsorted.drain(..tag.len());
                    (out, true)
                }
                None => {
                    let keep = if end {
                        0
                    } else {
                        partial_tag(&self.unsorted, tag)
                    };
                    let at = self.unsorted.len() - keep;
                    (self.unsorted.drain(..at).collect(), false)
                }
            };
            if !out.is_empty() {
                if self.thinking {
                    self.reasoning.push_str(&out);
                    on_event(Event::ReasoningDelta(out));
                } else {
                    self.text.push_str(&out);
                    on_event(Event::TextDelta(out));
                }
            }
            if !found {
                return;
            }
            self.thinking = !self.thinking;
        }
    }

    fn finish(mut self) -> Result<Reply> {
        if !self.finished {
            bail!("the reply stream ended before the reply was complete");
        }
        self.sort(true, &mut |_| {});
        // Some chat templates open the think block themselves, so the reply
        // has only the closing tag: what comes before it was reasoning.
        if let Some(at) = self.text.find(THINK_END) {
            let thought: String = self.text.drain(..at + THINK_END.len()).collect();
            self.reasoning.push_str(thought.trim_end_matches(THINK_END));
        }
        self.text = self.text.trim_start().to_string();
        self.reasoning = self.reasoning.trim().to_string();
        let mut items = Vec::new();
        if !self.reasoning.is_empty() {
            items.push(Item::Reasoning {
                summary: self.reasoning,
                raw: None,
            });
        }
        if !self.text.is_empty() {
            items.push(Item::Assistant {
                text: self.text,
                raw: None,
            });
        }
        for (i, mut call) in self.calls.into_iter().enumerate() {
            // Some local servers leave out the ID; results still need one to point at.
            if call.id.is_empty() {
                call.id = format!("call_{i}_{}", crate::new_id());
            }
            if call.arguments.trim().is_empty() {
                call.arguments = "{}".into();
            }
            items.push(Item::ToolCall { call, raw: None });
        }
        Ok(Reply {
            items,
            usage: self.usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_with_text_and_calls_is_one_assistant_message() {
        let call = |id: &str| Item::ToolCall {
            call: ToolCall {
                id: id.into(),
                name: "read".into(),
                arguments: "{}".into(),
            },
            raw: None,
        };
        let items = vec![
            Item::User { text: "hi".into() },
            Item::Reasoning {
                summary: "think".into(),
                raw: None,
            },
            Item::Assistant {
                text: "Looking.".into(),
                raw: None,
            },
            call("a"),
            call("b"),
            Item::ToolResult {
                call_id: "a".into(),
                output: "A".into(),
            },
            Item::ToolResult {
                call_id: "b".into(),
                output: "B".into(),
            },
            Item::Assistant {
                text: "Done.".into(),
                raw: None,
            },
        ];
        let request = Request {
            model: "m",
            system: "sys",
            items: &items,
            tools: &[],
            effort: None,
            session_id: "s",
        };
        let roles: Vec<String> = messages(&request)
            .iter()
            .map(|m| {
                let calls = m["tool_calls"].as_array().map_or(0, Vec::len);
                format!("{}:{calls}", m["role"].as_str().unwrap())
            })
            .collect();
        assert_eq!(
            roles,
            [
                "system:0",
                "user:0",
                "assistant:2",
                "tool:0",
                "tool:0",
                "assistant:0"
            ]
        );
        let m = messages(&request);
        assert_eq!(m[2]["content"], "Looking.");
        assert_eq!(m[4]["tool_call_id"], "b");
    }

    #[test]
    fn assembles_tool_calls_that_arrive_in_pieces() {
        let chunks = [
            json!({"choices":[{"delta":{"reasoning_content":"Plan."}}]}),
            json!({"choices":[{"delta":{"content":"Reading "}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":""}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"c2","function":{"name":"grep","arguments":"{\"pat"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a\"}"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"tern\":\"x\"}"}}]}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":30,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":20}}}),
        ];
        let mut events = Vec::new();
        let mut a = Assembler::default();
        for c in &chunks {
            a.push(c, &mut |e| events.push(e)).unwrap();
        }
        let reply = a.finish().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(reply.usage.cached_input_tokens, 20);
        let calls: Vec<(String, String, String)> = reply
            .items
            .iter()
            .filter_map(|i| match i {
                Item::ToolCall { call, .. } => {
                    Some((call.id.clone(), call.name.clone(), call.arguments.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            calls,
            [
                ("c1".into(), "read".into(), r#"{"path":"a"}"#.into()),
                ("c2".into(), "grep".into(), r#"{"pattern":"x"}"#.into()),
            ]
        );
        assert!(matches!(&reply.items[1], Item::Assistant { text, .. } if text == "Reading "));
    }

    #[test]
    fn think_tags_become_reasoning_even_split_across_chunks() {
        let run = |chunks: &[&str]| {
            let mut a = Assembler::default();
            let mut shown = (String::new(), String::new());
            let mut on_event = |e: Event| match e {
                Event::ReasoningDelta(d) => shown.0.push_str(&d),
                Event::TextDelta(d) => shown.1.push_str(&d),
                _ => {}
            };
            for c in chunks {
                a.push(&json!({"choices":[{"delta":{"content":c}}]}), &mut on_event)
                    .unwrap();
            }
            a.push(
                &json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
                &mut on_event,
            )
            .unwrap();
            let reply = a.finish().unwrap();
            let get = |want: &str| {
                reply.items.iter().find_map(|i| match (i, want) {
                    (Item::Reasoning { summary, .. }, "r") => Some(summary.clone()),
                    (Item::Assistant { text, .. }, "t") => Some(text.clone()),
                    _ => None,
                })
            };
            (get("r"), get("t"), shown)
        };
        let (r, t, shown) = run(&["<thi", "nk>plan it", "</th", "ink>\n\nAnswer: 4 < 5"]);
        assert_eq!(r.as_deref(), Some("plan it"));
        assert_eq!(t.as_deref(), Some("Answer: 4 < 5"));
        assert_eq!(shown, ("plan it".into(), "\n\nAnswer: 4 < 5".into()));
        // A template that opened the block itself: only the closing tag comes.
        let (r, t, _) = run(&["hmm, two", "</think>", "Two."]);
        assert_eq!(
            (r.as_deref(), t.as_deref()),
            (Some("hmm, two"), Some("Two."))
        );
        // No tags, and a lone < at the end: all text.
        let (r, t, _) = run(&["a <b", " and <"]);
        assert_eq!((r, t.as_deref()), (None, Some("a <b and <")));
    }

    #[test]
    fn a_stream_without_a_finish_reason_is_an_error() {
        let mut a = Assembler::default();
        a.push(
            &json!({"choices":[{"delta":{"content":"half"}}]}),
            &mut |_| {},
        )
        .unwrap();
        assert!(a.finish().is_err());
    }
}
