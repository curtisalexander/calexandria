//! The agent loop: ask the model, run the tools it calls, send back the
//! results, and repeat until it answers without calling a tool.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::compaction;
use crate::message::{Event, Exposure, Item, ToolCall};
use crate::provider::{Client, Request};
use crate::tools::{Tool, ToolSpec, Toolbox};

/// Most model requests one prompt may take, so a confused model cannot loop forever.
pub const MAX_STEPS: usize = 100;

/// The result a declined tool call gets, so the UI can tell it from one that ran.
pub const DECLINED: &str =
    "The user declined to run this. Ask them how to proceed instead of retrying it.";

/// The result a call gets when the user sent a message before it ran.
pub const SKIPPED: &str =
    "Skipped: the user sent a message before this ran. Read it before calling tools again.";

pub struct Agent {
    pub client: Client,
    pub toolbox: Toolbox,
    pub model: String,
    pub effort: Option<String>,
    pub system: String,
    pub session_id: String,
    /// The whole conversation so far, including compaction records. Requests
    /// send it with the compactions applied.
    pub items: Vec<Item>,
    /// How many tokens the context took at the latest request, as the server
    /// counted them; 0 until the first request.
    pub context_tokens: u64,
    /// Compact the conversation once it reaches this many tokens; `None` never does.
    pub compact_at: Option<u64>,
    /// After a compaction that failed, or left the context still over
    /// `compact_at`, the next one waits until the context reaches this; 0
    /// when it need not wait.
    pub compact_again_at: u64,
}

/// A tool call waiting for the user's yes or no.
#[derive(Debug, Clone)]
pub struct Approval {
    /// The call's place in the conversation, as in [`Event`].
    pub seq: usize,
    pub name: String,
    pub summary: String,
    /// What the call would change, such as a diff, when the tool can tell.
    pub preview: Option<String>,
    pub exposure: Exposure,
    /// Text the user may rewrite before saying yes, such as a memory's.
    pub editable: Option<String>,
}

/// The user's answer to an [`Approval`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
    /// Yes, with the user's text in place of [`Approval::editable`].
    Edited(String),
}

impl From<bool> for Answer {
    fn from(yes: bool) -> Self {
        if yes { Answer::Yes } else { Answer::No }
    }
}

impl Agent {
    /// Runs one prompt to completion and returns the final reply text.
    ///
    /// `approve` is awaited before each tool that needs approval runs. It is
    /// async so a full-screen UI can keep drawing while the user decides.
    ///
    /// `steer` hands over messages the user sent while the turn runs. They
    /// are read at each pause: before each tool call, and after a reply. Once
    /// there are any, the calls still waiting in that batch are skipped, and
    /// the messages are added as user items before the next request, so the
    /// model reads them before it goes on. A message that arrives during the
    /// final reply keeps the turn going.
    ///
    /// Dropping the returned future cancels the turn; the caller then drops
    /// the items it added, as for an error. A tool already running on its
    /// blocking thread still finishes in the background.
    pub async fn run(
        &mut self,
        prompt: &str,
        on_event: &mut dyn FnMut(Event),
        approve: &mut impl AsyncFnMut(Approval) -> Answer,
        steer: &mut dyn FnMut() -> Vec<String>,
    ) -> Result<String> {
        self.items.push(Item::User {
            text: prompt.to_string(),
        });
        for step in 1..=MAX_STEPS {
            // Every call has its output here, so compaction sees a whole conversation.
            let tokens = self
                .context_tokens
                .max(compaction::view_tokens(&self.items));
            if let Some(at) = self.compact_at
                && tokens >= at.max(self.compact_again_at)
            {
                self.compact(on_event).await;
            }
            let items = compaction::request_items(&self.items);
            let specs = self.specs();
            on_event(Event::RequestStarted {
                step,
                items: self.items.len(),
            });
            let started = Instant::now();
            let request = Request {
                model: &self.model,
                system: &self.system,
                items: &items,
                tools: &specs,
                effort: self.effort.as_deref(),
                session_id: &self.session_id,
            };
            let reply = self.client.stream(&request, &mut *on_event).await?;
            on_event(Event::RequestFinished {
                usage: reply.usage,
                duration_ms: elapsed_ms(started),
            });
            // Some local servers report no usage; the estimate stands in then.
            if reply.usage.input_tokens > 0 {
                self.context_tokens = reply.usage.input_tokens + reply.usage.output_tokens;
            }
            let first = self.items.len() + 1;
            let mut calls = Vec::new();
            let mut text = Vec::new();
            for (i, item) in reply.items.iter().enumerate() {
                match item {
                    Item::ToolCall { call, .. } => calls.push((first + i, call.clone())),
                    Item::Assistant { text: t, .. } => text.push(t.clone()),
                    _ => {}
                }
            }
            self.items.extend(reply.items);
            let mut steering = Vec::new();
            if calls.is_empty() {
                steering = steer();
                if steering.is_empty() {
                    return Ok(text.join("\n\n"));
                }
            }
            // Every call gets a result, even a refused, failed, or skipped
            // one, because the API rejects a conversation with an unanswered call.
            for (seq, call) in calls {
                if steering.is_empty() {
                    steering = steer();
                }
                let skip = !steering.is_empty();
                let output = self.call_tool(seq, &call, skip, on_event, approve).await;
                self.items.push(Item::ToolResult {
                    call_id: call.id,
                    output,
                });
            }
            steering.extend(steer());
            for text in steering {
                self.items.push(Item::User { text: text.clone() });
                on_event(Event::Steered {
                    seq: self.items.len(),
                    text,
                });
            }
        }
        bail!("stopped after {MAX_STEPS} model requests without a final answer")
    }

    /// The tools to offer: the toolbox, and `recall` once there is a compaction.
    /// It joins when the conversation's start changes anyway, so it costs
    /// no prompt cache that compaction did not already.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.toolbox.specs();
        if compaction::is_compacted(&self.items) {
            let recall = compaction::Recall {
                items: Arc::default(),
            };
            specs.push(recall.spec());
        }
        specs
    }

    /// A tool by name, as offered by [`Agent::specs`].
    pub fn tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if name == compaction::RECALL && compaction::is_compacted(&self.items) {
            return Some(Arc::new(self.recall()));
        }
        self.toolbox.get(name)
    }

    /// The legs of the trifecta the tool calls that ran have opened in this
    /// conversation; declined and skipped calls open none.
    pub fn opened(&self) -> Exposure {
        let ran: std::collections::HashSet<&str> = self
            .items
            .iter()
            .filter_map(|item| match item {
                Item::ToolResult { call_id, output } if output != DECLINED && output != SKIPPED => {
                    Some(call_id.as_str())
                }
                _ => None,
            })
            .collect();
        let mut exposure = Exposure::default();
        for item in &self.items {
            if let Item::ToolCall { call, .. } = item
                && ran.contains(call.id.as_str())
                && let Some(tool) = self.tool(&call.name)
            {
                exposure |= tool.exposure();
            }
        }
        exposure
    }

    fn recall(&self) -> compaction::Recall {
        compaction::Recall {
            items: Arc::new(self.items.clone()),
        }
    }

    async fn call_tool(
        &self,
        seq: usize,
        call: &ToolCall,
        skip: bool,
        on_event: &mut dyn FnMut(Event),
        approve: &mut impl AsyncFnMut(Approval) -> Answer,
    ) -> String {
        let name = call.name.clone();
        let mut args: Result<Value, _> = serde_json::from_str(&call.arguments);
        // A memory records whether the session had read untrusted content,
        // which only the harness knows; the model cannot set it.
        if call.name == crate::memory::REMEMBER
            && let Ok(Value::Object(map)) = &mut args
        {
            map.insert("untrusted".into(), self.opened().untrusted.into());
        }
        let tool = self.tool(&call.name);
        let summary = match (&tool, &args) {
            (Some(tool), Ok(args)) => tool.summary(args),
            _ => call.arguments.clone(),
        };
        on_event(Event::ToolStarted {
            seq,
            name: name.clone(),
            summary: summary.clone(),
            exposure: tool.as_ref().map(|t| t.exposure()).unwrap_or_default(),
        });

        // Timed from when the tool starts running, not while it waits for approval.
        let mut started = Instant::now();
        let (output, is_error) = match (tool, args) {
            (None, _) => (format!("Error: there is no tool named {name}"), true),
            (_, Err(e)) => (
                format!("Error: the arguments are not valid JSON: {e}"),
                true,
            ),
            _ if skip => (SKIPPED.to_string(), true),
            (Some(tool), Ok(mut args)) => {
                let editable = (name == crate::memory::REMEMBER)
                    .then(|| args["text"].as_str().unwrap_or_default().to_string());
                let answer = if tool.needs_approval() {
                    approve(Approval {
                        seq,
                        name: name.clone(),
                        summary,
                        preview: tool.preview(&args),
                        exposure: tool.exposure(),
                        editable,
                    })
                    .await
                } else {
                    Answer::Yes
                };
                // The model learns what the user changed, so it does not
                // go on believing its own wording was saved.
                let edited = match answer {
                    Answer::No => None,
                    Answer::Yes => Some(None),
                    Answer::Edited(text) => {
                        args["text"] = text.clone().into();
                        Some(Some(text))
                    }
                };
                if let Some(edited) = edited {
                    started = Instant::now();
                    match tokio::task::spawn_blocking(move || tool.run(args)).await {
                        Ok(Ok(output)) => match edited {
                            Some(text) => (
                                format!("{output} The user rewrote it before saving it as: {text}"),
                                false,
                            ),
                            None => (output, false),
                        },
                        Ok(Err(e)) => (format!("Error: {e:#}"), true),
                        Err(e) => (format!("Error: the tool crashed: {e}"), true),
                    }
                } else {
                    (DECLINED.to_string(), true)
                }
            }
        };
        on_event(Event::ToolFinished {
            seq,
            name,
            output: output.clone(),
            is_error,
            duration_ms: elapsed_ms(started),
        });
        output
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}
