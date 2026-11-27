//! The agent loop: ask the model, run the tools it calls, send back the
//! results, and repeat until it answers without calling a tool.

use std::time::Instant;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::message::{Event, Exposure, Item, ToolCall};
use crate::provider::{Client, Request};
use crate::tools::Toolbox;

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
    /// The whole conversation so far.
    pub items: Vec<Item>,
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
        approve: &mut impl AsyncFnMut(Approval) -> bool,
        steer: &mut dyn FnMut() -> Vec<String>,
    ) -> Result<String> {
        self.items.push(Item::User {
            text: prompt.to_string(),
        });
        let specs = self.toolbox.specs();
        for step in 1..=MAX_STEPS {
            on_event(Event::RequestStarted {
                step,
                items: self.items.len(),
            });
            let started = Instant::now();
            let request = Request {
                model: &self.model,
                system: &self.system,
                items: &self.items,
                tools: &specs,
                effort: self.effort.as_deref(),
                session_id: &self.session_id,
            };
            let reply = self.client.stream(&request, &mut *on_event).await?;
            on_event(Event::RequestFinished {
                usage: reply.usage,
                duration_ms: elapsed_ms(started),
            });
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

    async fn call_tool(
        &self,
        seq: usize,
        call: &ToolCall,
        skip: bool,
        on_event: &mut dyn FnMut(Event),
        approve: &mut impl AsyncFnMut(Approval) -> bool,
    ) -> String {
        let name = call.name.clone();
        let args: Result<Value, _> = serde_json::from_str(&call.arguments);
        let tool = self.toolbox.get(&call.name);
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
            (Some(tool), Ok(args)) => {
                let approved = !tool.needs_approval()
                    || approve(Approval {
                        seq,
                        name: name.clone(),
                        summary,
                        preview: tool.preview(&args),
                        exposure: tool.exposure(),
                    })
                    .await;
                if !approved {
                    (DECLINED.to_string(), true)
                } else {
                    started = Instant::now();
                    match tokio::task::spawn_blocking(move || tool.run(args)).await {
                        Ok(Ok(output)) => (output, false),
                        Ok(Err(e)) => (format!("Error: {e:#}"), true),
                        Err(e) => (format!("Error: the tool crashed: {e}"), true),
                    }
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
