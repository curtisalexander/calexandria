//! Compaction in place, after exe.dev's Shelley (https://blog.exe.dev/compaction).
//!
//! When the conversation grows past a threshold, calx shows the model an
//! index of it and asks for a plan: trim tool outputs it no longer needs in
//! full, and squish runs of items into shorter rewritten records. A linter
//! rejects plans that cut too little or too much or would break the
//! conversation, and the model tries again with the reasons.
//!
//! The plan is saved as an [`Item::Compaction`] in the append-only history,
//! and nothing is deleted. Each request sends [`view`] of the history: the
//! items with every compaction so far applied, and without the compaction
//! records themselves, so the model never sees the compaction work. Once a
//! conversation has a compaction, the model also gets the [`Recall`] tool,
//! which reads the originals back by their numbers.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::Agent;
use crate::message::{Event, Exposure, Item};
use crate::provider::Request;
use crate::tools::{Tool, ToolSpec};

/// The default size, in tokens, at which a turn compacts the conversation.
pub const DEFAULT_COMPACT_AT: u64 = 200_000;
/// How many plans the model may propose before calx gives up for this turn.
pub const MAX_ATTEMPTS: u32 = 3;
/// The latest visible items, which a plan must leave as they are.
pub const KEEP_LATEST: usize = 4;
/// A plan must leave at most this share of the conversation ...
const MOST_KEPT: f64 = 0.7;
/// ... and at least this share, so the model does not lose the plot.
const LEAST_KEPT: f64 = 0.15;

/// Replace a tool output with the part still needed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trim {
    pub item: usize,
    pub keep: String,
}

/// Replace items `from` through `to` with a shorter record of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Squish {
    pub from: usize,
    pub to: usize,
    pub text: String,
}

/// What the model proposes, as JSON.
#[derive(Debug, Default, Deserialize)]
struct Plan {
    #[serde(default)]
    trim: Vec<Trim>,
    #[serde(default)]
    squish: Vec<Squish>,
}

/// An item as the model sees it, covering items `first` through `last` of
/// the history: one item, or a squished run of them.
#[derive(Debug, Clone, PartialEq)]
pub struct Visible {
    pub first: usize,
    pub last: usize,
    pub item: Item,
}

/// The history as the model sees it: every compaction applied, in order, to
/// the items before it, and the compaction records left out.
pub fn view(items: &[Item]) -> Vec<Visible> {
    let mut out: Vec<Visible> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let Item::Compaction {
            trims, squishes, ..
        } = item
        else {
            out.push(Visible {
                first: i + 1,
                last: i + 1,
                item: item.clone(),
            });
            continue;
        };
        for trim in trims {
            if let Some(v) = out.iter_mut().find(|v| v.first == trim.item)
                && let Item::ToolResult { output, .. } = &mut v.item
            {
                *output = trimmed(trim.item, &trim.keep);
            }
        }
        for squish in squishes {
            let start = out.iter().position(|v| v.first >= squish.from);
            let end = out.iter().rposition(|v| v.last <= squish.to);
            if let (Some(start), Some(end)) = (start, end)
                && start <= end
            {
                out.splice(
                    start..=end,
                    [Visible {
                        first: squish.from,
                        last: squish.to,
                        item: Item::Assistant {
                            text: squished(squish),
                            raw: None,
                        },
                    }],
                );
            }
        }
    }
    out
}

/// The items to send the model.
pub fn request_items(items: &[Item]) -> Vec<Item> {
    view(items).into_iter().map(|v| v.item).collect()
}

fn trimmed(item: usize, keep: &str) -> String {
    format!("[Output §{item} trimmed; `recall` §{item} reads all of it.]\n{keep}")
}

fn squished(s: &Squish) -> String {
    format!(
        "[Condensed record of items §{}–§{}; `recall` reads the originals.]\n{}",
        s.from, s.to, s.text
    )
}

/// Estimated tokens, at 4 characters a token.
pub fn estimate(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

/// Estimated tokens of an item as sent.
fn item_tokens(item: &Item) -> u64 {
    match item {
        Item::User { text } | Item::Assistant { text, .. } => estimate(text),
        Item::Reasoning { summary, .. } => estimate(summary),
        Item::ToolCall { call, .. } => estimate(&call.name) + estimate(&call.arguments),
        Item::ToolResult { output, .. } => estimate(output),
        Item::Compaction { .. } => 0,
    }
}

/// Estimated tokens of the conversation as the model sees it.
pub fn view_tokens(items: &[Item]) -> u64 {
    view(items).iter().map(|v| item_tokens(&v.item)).sum()
}

/// One line per visible item, for the model to refer to them by number.
fn index(visible: &[Visible]) -> String {
    let mut out = String::new();
    for v in visible {
        let id = if v.first == v.last {
            format!("§{}", v.first)
        } else {
            format!("§{}–§{}", v.first, v.last)
        };
        let what = match &v.item {
            _ if v.first != v.last => "condensed record".to_string(),
            Item::User { text } => format!("user: {}", first_line(text)),
            Item::Assistant { text, .. } => format!("assistant: {}", first_line(text)),
            Item::Reasoning { .. } => "reasoning".to_string(),
            Item::ToolCall { call, .. } => {
                format!("call {} {}", call.name, first_line(&call.arguments))
            }
            Item::ToolResult { output, .. } => format!("output: {}", first_line(output)),
            Item::Compaction { .. } => continue,
        };
        out.push_str(&format!("{id} · ~{} tok · {what}\n", item_tokens(&v.item)));
    }
    out
}

fn first_line(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut s: String = line.trim().chars().take(70).collect();
    if line.trim().chars().count() > 70 || text.trim().lines().nth(1).is_some() {
        s.push('…');
    }
    s
}

fn nudge(visible: &[Visible], tokens: u64) -> String {
    let kept = visible
        .len()
        .checked_sub(KEEP_LATEST)
        .map(|i| visible[i].first)
        .unwrap_or(1);
    format!(
        "Pause the task: the conversation is about {tokens} tokens long. Compact it so you \
can keep working without losing the plot. Do not call any tools now.

Reply with only a JSON object:
{{\"trim\": [{{\"item\": N, \"keep\": \"...\"}}], \"squish\": [{{\"from\": A, \"to\": B, \"text\": \"...\"}}]}}

- trim replaces the tool output §N with `keep`: the lines you still need from it, such \
as paths, findings, errors, and numbers. Trim large outputs you have already acted on.
- squish replaces items §A through §B with `text`, a condensed record of them: what the \
user asked for, what you did and found, files changed, decisions, and what is still \
open. Write it so you could carry on from it alone.
- Keep each tool call in the same range as its output, and leave alone what you still \
need word for word, and the latest items, §{kept} onward.
- Aim to leave between a third and two thirds of the conversation.
- Nothing is lost: afterward the `recall` tool reads any original item by its number.

The conversation, one item per line, with estimated tokens:
{}",
        index(visible)
    )
}

/// Reads a plan from the model's reply, which may wrap it in a code fence.
fn parse(reply: &str) -> Result<Plan, String> {
    let (Some(start), Some(end)) = (reply.find('{'), reply.rfind('}')) else {
        return Err("the reply has no JSON object".into());
    };
    serde_json::from_str(&reply[start..=end]).map_err(|e| format!("the JSON is not a plan: {e}"))
}

/// Checks a plan against the visible conversation. Returns every problem,
/// so the model can fix them all at once.
fn lint(visible: &[Visible], plan: &Plan) -> Result<(), Vec<String>> {
    let mut problems = Vec::new();
    let protected = visible
        .len()
        .checked_sub(KEEP_LATEST)
        .map(|i| visible[i].first)
        .unwrap_or(1);
    if plan.trim.is_empty() && plan.squish.is_empty() {
        problems.push("the plan trims and squishes nothing".into());
    }
    let at = |seq: usize| visible.iter().position(|v| v.first == seq);

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for s in &plan.squish {
        let name = format!("squish §{}–§{}", s.from, s.to);
        let (Some(start), Some(end)) = (at(s.from), visible.iter().position(|v| v.last == s.to))
        else {
            problems.push(format!(
                "{name}: both ends must be items in the index, and a condensed record is squished whole or not at all"
            ));
            continue;
        };
        if start > end {
            problems.push(format!("{name}: `from` comes after `to`"));
            continue;
        }
        if s.to >= protected {
            problems.push(format!("{name}: leave §{protected} onward as they are"));
        }
        if ranges.iter().any(|&(a, b)| start <= b && a <= end) {
            problems.push(format!("{name}: overlaps another squish"));
        }
        ranges.push((start, end));
        let range = &visible[start..=end];
        let calls: Vec<&str> = range
            .iter()
            .filter_map(|v| match &v.item {
                Item::ToolCall { call, .. } if v.first == v.last => Some(call.id.as_str()),
                _ => None,
            })
            .collect();
        let results: Vec<&str> = range
            .iter()
            .filter_map(|v| match &v.item {
                Item::ToolResult { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        if calls.iter().any(|c| !results.contains(c)) || results.iter().any(|r| !calls.contains(r))
        {
            problems.push(format!(
                "{name}: splits a tool call from its output; move an end so each call and its output are both in the range or both outside it"
            ));
        }
        // OpenAI's reasoning items must stay with the item that follows them.
        let reasoning = |i: usize| matches!(visible[i].item, Item::Reasoning { .. });
        if reasoning(end) || (start > 0 && reasoning(start - 1)) {
            problems.push(format!(
                "{name}: splits reasoning from the item after it; move an end by one"
            ));
        }
        let replaced: u64 = range.iter().map(|v| item_tokens(&v.item)).sum();
        if s.text.trim().len() < 20 {
            problems.push(format!("{name}: the text is too short to carry on from"));
        } else if estimate(&s.text) * 2 > replaced {
            problems.push(format!(
                "{name}: the text (~{} tok) must be under half of what it replaces (~{replaced} tok)",
                estimate(&s.text)
            ));
        }
    }
    for t in &plan.trim {
        let name = format!("trim §{}", t.item);
        let Some(i) = at(t.item).filter(|&i| {
            visible[i].first == visible[i].last
                && matches!(visible[i].item, Item::ToolResult { .. })
        }) else {
            problems.push(format!("{name}: only a tool output can be trimmed"));
            continue;
        };
        if t.item >= protected {
            problems.push(format!("{name}: leave §{protected} onward as they are"));
        }
        if ranges.iter().any(|&(a, b)| a <= i && i <= b) {
            problems.push(format!("{name}: is inside a squish already"));
        }
        let original = item_tokens(&visible[i].item);
        if t.keep.trim().is_empty() {
            problems.push(format!("{name}: say in `keep` what the output was"));
        } else if estimate(&t.keep) * 2 > original {
            problems.push(format!(
                "{name}: `keep` (~{} tok) must be under half of the output (~{original} tok)",
                estimate(&t.keep)
            ));
        }
    }
    if problems.is_empty() {
        let before: u64 = visible.iter().map(|v| item_tokens(&v.item)).sum();
        let after = after_tokens(visible, plan);
        let share = after as f64 / before.max(1) as f64;
        if share > MOST_KEPT {
            problems.push(format!(
                "the plan leaves {:.0}% of the conversation (~{after} of ~{before} tok); cut it to two thirds or less",
                share * 100.0
            ));
        } else if share < LEAST_KEPT {
            problems.push(format!(
                "the plan leaves only {:.0}% of the conversation (~{after} of ~{before} tok); keep more detail, at least a sixth",
                share * 100.0
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Estimated tokens of the visible conversation once a plan is applied.
fn after_tokens(visible: &[Visible], plan: &Plan) -> u64 {
    let before: u64 = visible.iter().map(|v| item_tokens(&v.item)).sum();
    let squished_away: u64 = visible
        .iter()
        .filter(|v| {
            plan.squish
                .iter()
                .any(|s| s.from <= v.first && v.last <= s.to)
        })
        .map(|v| item_tokens(&v.item))
        .sum();
    let squishes: u64 = plan.squish.iter().map(|s| estimate(&squished(s))).sum();
    let trims: i64 = plan
        .trim
        .iter()
        .filter_map(|t| {
            let v = visible.iter().find(|v| v.first == t.item)?;
            Some(estimate(&trimmed(t.item, &t.keep)) as i64 - item_tokens(&v.item) as i64)
        })
        .sum();
    ((before - squished_away + squishes) as i64 + trims).max(0) as u64
}

impl Agent {
    /// Compacts the conversation: asks the model for a plan, lints it, and
    /// lets it try again with the problems, up to [`MAX_ATTEMPTS`] times. A
    /// plan that passes is added as an [`Item::Compaction`].
    ///
    /// It reports through `on_event`, and never fails: a conversation that
    /// cannot be compacted carries on as it is.
    pub async fn compact(&mut self, on_event: &mut dyn FnMut(Event)) -> bool {
        let tokens = self.context_tokens.max(view_tokens(&self.items));
        on_event(Event::CompactionStarted { tokens });
        match self.plan(tokens, on_event).await {
            Ok((plan, attempts)) => {
                let before_est = view_tokens(&self.items);
                self.items.push(Item::Compaction {
                    trims: plan.trim,
                    squishes: plan.squish,
                    before: tokens,
                    after: 0,
                    attempts,
                });
                let after_est = view_tokens(&self.items);
                // The model counts the system prompt and tools too, which
                // compaction leaves alone, so scale the real count.
                let after = tokens * after_est / before_est.max(1);
                self.context_tokens = after;
                let seq = self.items.len();
                let Some(Item::Compaction {
                    trims,
                    squishes,
                    after: saved,
                    ..
                }) = self.items.last_mut()
                else {
                    unreachable!("just pushed");
                };
                *saved = after;
                on_event(Event::Compacted {
                    seq,
                    before: tokens,
                    after,
                    trims: trims.len(),
                    squishes: squishes.len(),
                    attempts,
                });
                true
            }
            Err(e) => {
                on_event(Event::CompactionFailed {
                    reason: format!("{e:#}"),
                });
                false
            }
        }
    }

    async fn plan(&self, tokens: u64, on_event: &mut dyn FnMut(Event)) -> Result<(Plan, u32)> {
        let visible = view(&self.items);
        if visible.len() <= KEEP_LATEST + 1 {
            bail!("there is nothing to compact yet");
        }
        // The same items, system prompt, and tools as a normal request, so
        // the server's prompt cache still covers everything before the nudge.
        let mut scratch: Vec<Item> = visible.iter().map(|v| v.item.clone()).collect();
        scratch.push(Item::User {
            text: nudge(&visible, tokens),
        });
        let specs = self.specs();
        let mut last_problems = String::new();
        for attempt in 1..=MAX_ATTEMPTS {
            let request = Request {
                model: &self.model,
                system: &self.system,
                items: &scratch,
                tools: &specs,
                effort: self.effort.as_deref(),
                session_id: &self.session_id,
            };
            // The plan is not part of the conversation, so its text is not
            // shown; its usage is, since it counts against the plan or budget.
            let started = std::time::Instant::now();
            let reply = self
                .client
                .stream(&request, |_| {})
                .await
                .context("the compaction request failed")?;
            on_event(Event::RequestFinished {
                usage: reply.usage,
                duration_ms: started.elapsed().as_millis() as u64,
            });
            let text: Vec<&str> = reply
                .items
                .iter()
                .filter_map(|i| match i {
                    Item::Assistant { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let calls: Vec<String> = reply
                .items
                .iter()
                .filter_map(|i| match i {
                    Item::ToolCall { call, .. } => Some(call.id.clone()),
                    _ => None,
                })
                .collect();
            let problems = if !calls.is_empty() {
                vec!["call no tools while compacting; reply with the JSON plan only".to_string()]
            } else {
                match parse(&text.join("\n")) {
                    Ok(plan) => match lint(&visible, &plan) {
                        Ok(()) => return Ok((plan, attempt)),
                        Err(problems) => problems,
                    },
                    Err(problem) => vec![problem],
                }
            };
            last_problems = problems.join("; ");
            scratch.extend(reply.items);
            // Every call needs an answer, or the next request is rejected.
            for call_id in calls {
                scratch.push(Item::ToolResult {
                    call_id,
                    output: "Not run: compaction is in progress.".into(),
                });
            }
            let list: Vec<String> = problems.iter().map(|p| format!("- {p}")).collect();
            scratch.push(Item::User {
                text: format!(
                    "That plan was not applied:\n{}\nReply with a corrected JSON object.",
                    list.join("\n")
                ),
            });
        }
        bail!("no acceptable plan after {MAX_ATTEMPTS} tries: {last_problems}")
    }
}

/// Most of the conversation one `recall` returns, so reading the originals
/// back cannot undo the compaction in one call.
const MAX_RECALL_BYTES: usize = 40_000;

/// Reads items of the conversation as they were before any compaction.
pub struct Recall {
    /// The whole history, as it was when the call was made.
    pub items: Arc<Vec<Item>>,
}

#[derive(Deserialize)]
struct RecallArgs {
    from: usize,
    to: Option<usize>,
}

impl Tool for Recall {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: RECALL,
            description: "Read items of this conversation as they were before it was \
                compacted, by their § numbers, such as those in a condensed record or a \
                trimmed output. Use it when you need the exact text, such as a file's \
                contents or a command's full output, instead of guessing from the record."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "from": { "type": "integer", "description": "First item, such as 12 for §12." },
                    "to": { "type": "integer", "description": "Last item; default: only `from`." }
                },
                "required": ["from"]
            }),
        }
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn summary(&self, args: &Value) -> String {
        match (args["from"].as_u64(), args["to"].as_u64()) {
            (Some(a), Some(b)) if b != a => format!("§{a}–§{b}"),
            (Some(a), _) => format!("§{a}"),
            _ => "?".into(),
        }
    }

    fn exposure(&self) -> Exposure {
        // It replays what the other tools read: private, and maybe untrusted.
        Exposure {
            private: true,
            untrusted: true,
            egress: false,
        }
    }

    fn run(&self, args: Value) -> Result<String> {
        let a: RecallArgs = crate::tools::parse_args(args)?;
        let to = a.to.unwrap_or(a.from);
        let n = self.items.len();
        if a.from < 1 || a.from > to || to > n {
            bail!("items are §1 to §{n}, and `from` must not be after `to`");
        }
        let mut out = String::new();
        for seq in a.from..=to {
            let text = match &self.items[seq - 1] {
                Item::User { text } => format!("user:\n{text}"),
                Item::Assistant { text, .. } => format!("assistant:\n{text}"),
                Item::Reasoning { summary, .. } => format!("reasoning summary:\n{summary}"),
                Item::ToolCall { call, .. } => {
                    format!("call {} ({}):\n{}", call.name, call.id, call.arguments)
                }
                Item::ToolResult { call_id, output } => format!("output of {call_id}:\n{output}"),
                Item::Compaction { .. } => "a compaction record".into(),
            };
            let entry = format!("§{seq} {text}\n\n");
            if !out.is_empty() && out.len() + entry.len() > MAX_RECALL_BYTES {
                out.push_str(&format!(
                    "[Stopped before §{seq} to keep this short; recall from §{seq} for the rest.]"
                ));
                return Ok(out);
            }
            out.push_str(&entry);
        }
        Ok(out.trim_end().to_string())
    }
}

/// The name of the [`Recall`] tool.
pub const RECALL: &str = "recall";

/// Whether the history has a compaction, so the model may need [`Recall`].
pub fn is_compacted(items: &[Item]) -> bool {
    items.iter().any(|i| matches!(i, Item::Compaction { .. }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCall;

    fn user(text: &str) -> Item {
        Item::User { text: text.into() }
    }
    fn said(text: &str) -> Item {
        Item::Assistant {
            text: text.into(),
            raw: None,
        }
    }
    fn call(id: &str) -> Item {
        Item::ToolCall {
            call: ToolCall {
                id: id.into(),
                name: "read".into(),
                arguments: "{}".into(),
            },
            raw: None,
        }
    }
    fn output(id: &str, text: &str) -> Item {
        Item::ToolResult {
            call_id: id.into(),
            output: text.into(),
        }
    }
    fn reasoning() -> Item {
        Item::Reasoning {
            summary: String::new(),
            raw: None,
        }
    }
    fn squish(from: usize, to: usize, text: &str) -> Squish {
        Squish {
            from,
            to,
            text: text.into(),
        }
    }

    /// §1 user, §2 call, §3 output (~300 tok), §4 reply, §5 user, §6
    /// reasoning, §7 call, §8 output (~1000 tok), §9 long reply (~300 tok),
    /// then four short recent items.
    fn history() -> Vec<Item> {
        let big = "x".repeat(4000);
        let long = format!(
            "The tests read config from the wrong place. {}",
            "y".repeat(1150)
        );
        vec![
            user("fix the build"),
            call("a"),
            output("a", &"x".repeat(1200)),
            said("I read it; the error is in lib.rs."),
            user("now the tests"),
            reasoning(),
            call("b"),
            output("b", &big),
            said(&long),
            user("go on"),
            said("ok"),
            user("and?"),
            said("done"),
        ]
    }

    #[test]
    fn view_applies_compactions_in_order_and_keeps_numbering() {
        let mut items = history();
        items.push(Item::Compaction {
            trims: vec![Trim {
                item: 8,
                keep: "config path is read from $HOME".into(),
            }],
            squishes: vec![squish(1, 4, "Fixed the build error in lib.rs.")],
            before: 0,
            after: 0,
            attempts: 1,
        });
        let v = view(&items);
        let ids: Vec<(usize, usize)> = v.iter().map(|v| (v.first, v.last)).collect();
        assert_eq!(&ids[..3], &[(1, 4), (5, 5), (6, 6)]);
        assert!(matches!(&v[0].item, Item::Assistant { text, .. } if text.contains("lib.rs")));
        assert!(
            matches!(&v[4].item, Item::ToolResult { output, .. } if output.ends_with("from $HOME"))
        );
        // The record itself is never shown.
        assert_eq!(v.len(), 13 - 3);

        // A later compaction can squish an earlier record whole.
        items.push(user("more"));
        items.push(Item::Compaction {
            trims: vec![],
            squishes: vec![squish(1, 9, "Fixed the build and the test config.")],
            before: 0,
            after: 0,
            attempts: 1,
        });
        let v = view(&items);
        assert_eq!((v[0].first, v[0].last), (1, 9));
        assert_eq!(v[1].first, 10);
        // The originals are still in the history.
        assert_eq!(items[2], history()[2]);
    }

    #[test]
    fn recall_reads_originals_and_stops_at_an_item_boundary() {
        let mut items = history();
        items.push(Item::Compaction {
            trims: vec![Trim {
                item: 8,
                keep: "short".into(),
            }],
            squishes: vec![squish(1, 4, "Fixed the build error in lib.rs.")],
            before: 0,
            after: 0,
            attempts: 1,
        });
        let recall = Recall {
            items: Arc::new(items),
        };
        // The trimmed output comes back whole, numbered as in the record.
        let out = recall.run(json!({ "from": 8 })).unwrap();
        assert_eq!(out, format!("§8 output of b:\n{}", "x".repeat(4000)));
        let out = recall.run(json!({ "from": 3, "to": 4 })).unwrap();
        assert!(out.starts_with("§3 output of a:\nxxx"));
        assert!(out.ends_with("§4 assistant:\nI read it; the error is in lib.rs."));
        for bad in [
            json!({ "from": 0 }),
            json!({ "from": 5, "to": 4 }),
            json!({ "from": 15 }),
        ] {
            assert!(recall.run(bad).is_err());
        }

        // A range too big for one call ends between items and says where to go on.
        let big = Recall {
            items: Arc::new(vec![output("a", &"z".repeat(30_000)); 3]),
        };
        let out = big.run(json!({ "from": 1, "to": 3 })).unwrap();
        assert!(out.contains("§1 output") && !out.contains("§2 output"));
        assert!(out.ends_with("recall from §2 for the rest.]"));
    }

    #[test]
    fn lint_rejects_plans_that_break_the_conversation_or_miss_the_target() {
        let visible = view(&history());
        let summary = "The user asked to fix the build; the error was in lib.rs.";
        let cases: Vec<(Plan, &str)> = vec![
            (Plan::default(), "nothing"),
            // Ends between a call (§2) and its output (§3).
            (
                Plan {
                    squish: vec![squish(1, 2, summary)],
                    ..Plan::default()
                },
                "splits a tool call",
            ),
            // Starts right after reasoning (§6), which must stay with §7.
            (
                Plan {
                    squish: vec![squish(1, 4, summary), squish(7, 9, summary)],
                    ..Plan::default()
                },
                "splits reasoning",
            ),
            // §10 onward are the latest four items.
            (
                Plan {
                    squish: vec![squish(1, 10, summary)],
                    ..Plan::default()
                },
                "leave §10 onward",
            ),
            // Only the smaller output: leaves too much.
            (
                Plan {
                    trim: vec![Trim {
                        item: 3,
                        keep: "error at lib.rs:12".into(),
                    }],
                    ..Plan::default()
                },
                "cut it to two thirds",
            ),
            // Everything gone: leaves too little.
            (
                Plan {
                    squish: vec![squish(1, 9, "Fixed things, all done now.")],
                    ..Plan::default()
                },
                "keep more detail",
            ),
            (
                Plan {
                    trim: vec![Trim {
                        item: 4,
                        keep: "x".into(),
                    }],
                    ..Plan::default()
                },
                "only a tool output",
            ),
        ];
        for (plan, expected) in cases {
            let problems = lint(&visible, &plan).expect_err(expected).join("; ");
            assert!(
                problems.contains(expected),
                "{expected:?} not in {problems:?}"
            );
        }

        // Both big outputs trimmed, the first exchange squished: about right.
        let good = parse(
            "```json\n{\"trim\": [{\"item\": 8, \"keep\": \"config is read from $HOME; tests expect ./config.\"}],\
             \"squish\": [{\"from\": 1, \"to\": 4, \"text\": \"The user asked to fix the build. I read the log (§3): the error was a missing import in lib.rs, which I fixed.\"}]}\n```",
        )
        .unwrap();
        assert_eq!(lint(&visible, &good), Ok(()));
        let after = after_tokens(&visible, &good);
        let mut items = history();
        items.push(Item::Compaction {
            trims: good.trim,
            squishes: good.squish,
            before: 0,
            after: 0,
            attempts: 1,
        });
        assert_eq!(after, view_tokens(&items));
    }
}
