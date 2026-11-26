//! The agent loop: ask the model, run the tools it calls, send back the
//! results, and repeat until it answers without calling a tool.

use std::time::Instant;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::message::{Event, Exposure, Item, ToolCall};
use crate::responses::{Request, ResponsesClient};
use crate::tools::Toolbox;

/// Most model requests one prompt may take, so a confused model cannot loop forever.
pub const MAX_STEPS: usize = 100;

/// The result a declined tool call gets, so the UI can tell it from one that ran.
pub const DECLINED: &str =
    "The user declined to run this. Ask them how to proceed instead of retrying it.";

pub struct Agent {
    pub client: ResponsesClient,
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
    /// Dropping the returned future cancels the turn; the caller then drops
    /// the items it added, as for an error. A tool already running on its
    /// blocking thread still finishes in the background.
    pub async fn run(
        &mut self,
        prompt: &str,
        on_event: &mut dyn FnMut(Event),
        approve: &mut impl AsyncFnMut(Approval) -> bool,
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
            if calls.is_empty() {
                return Ok(text.join("\n\n"));
            }
            // Every call gets a result, even a refused or failed one, because
            // the API rejects a conversation with an unanswered call.
            for (seq, call) in calls {
                let output = self.call_tool(seq, &call, on_event, approve).await;
                self.items.push(Item::ToolResult {
                    call_id: call.id,
                    output,
                });
            }
        }
        bail!("stopped after {MAX_STEPS} model requests without a final answer")
    }

    async fn call_tool(
        &self,
        seq: usize,
        call: &ToolCall,
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
