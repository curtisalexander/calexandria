//! The agent loop: ask the model, run the tools it calls, send back the
//! results, and repeat until it answers without calling a tool.

use anyhow::{Result, bail};
use serde_json::Value;

use crate::message::{Event, Item, ToolCall};
use crate::responses::{Request, ResponsesClient};
use crate::tools::Toolbox;

/// Most model requests one prompt may take, so a confused model cannot loop forever.
const MAX_STEPS: usize = 100;

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

impl Agent {
    /// Runs one prompt to completion and returns the final reply text.
    ///
    /// `approve` is asked, with the tool name and its one-line summary, before
    /// each tool that needs approval runs.
    pub async fn run(
        &mut self,
        prompt: &str,
        on_event: &mut dyn FnMut(Event),
        approve: &mut dyn FnMut(&str, &str) -> bool,
    ) -> Result<String> {
        self.items.push(Item::User {
            text: prompt.to_string(),
        });
        let specs = self.toolbox.specs();
        for _ in 0..MAX_STEPS {
            let request = Request {
                model: &self.model,
                system: &self.system,
                items: &self.items,
                tools: &specs,
                effort: self.effort.as_deref(),
                session_id: &self.session_id,
            };
            let reply = self.client.stream(&request, &mut *on_event).await?;
            let calls: Vec<ToolCall> = reply
                .items
                .iter()
                .filter_map(|item| match item {
                    Item::ToolCall { call, .. } => Some(call.clone()),
                    _ => None,
                })
                .collect();
            let text: Vec<&str> = reply
                .items
                .iter()
                .filter_map(|item| match item {
                    Item::Assistant { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let text = text.join("\n\n");
            self.items.extend(reply.items);
            if calls.is_empty() {
                return Ok(text);
            }
            // Every call gets a result, even a refused or failed one, because
            // the API rejects a conversation with an unanswered call.
            for call in calls {
                let output = self.call_tool(&call, on_event, approve).await;
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
        call: &ToolCall,
        on_event: &mut dyn FnMut(Event),
        approve: &mut dyn FnMut(&str, &str) -> bool,
    ) -> String {
        let name = call.name.clone();
        let args: Result<Value, _> = serde_json::from_str(&call.arguments);
        let tool = self.toolbox.get(&call.name);
        let summary = match (&tool, &args) {
            (Some(tool), Ok(args)) => tool.summary(args),
            _ => call.arguments.clone(),
        };
        on_event(Event::ToolStarted {
            name: name.clone(),
            summary: summary.clone(),
        });

        let (output, is_error) = match (tool, args) {
            (None, _) => (format!("Error: there is no tool named {name}"), true),
            (_, Err(e)) => (
                format!("Error: the arguments are not valid JSON: {e}"),
                true,
            ),
            (Some(tool), Ok(_)) if tool.needs_approval() && !approve(&name, &summary) => (
                "The user declined to run this. Ask them how to proceed instead of retrying it."
                    .to_string(),
                true,
            ),
            (Some(tool), Ok(args)) => {
                match tokio::task::spawn_blocking(move || tool.run(args)).await {
                    Ok(Ok(output)) => (output, false),
                    Ok(Err(e)) => (format!("Error: {e:#}"), true),
                    Err(e) => (format!("Error: the tool crashed: {e}"), true),
                }
            }
        };
        on_event(Event::ToolFinished {
            name,
            output: output.clone(),
            is_error,
        });
        output
    }
}
