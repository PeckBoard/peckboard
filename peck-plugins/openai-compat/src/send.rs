//! OpenAI Chat Completions turn (`POST {root}/v1/chat/completions`) via
//! `peckboard_http_request`.
//!
//! Non-streaming, one request per tool round. Multi-turn history lives in
//! the plugin store in OpenAI message shape (the server is stateless). When
//! `enable_tools` is on, each turn offers Peckboard MCP tools as OpenAI
//! function tools and runs calls through `peckboard_provider_invoke_mcp`.

use serde_json::{Value, json};

use crate::check;
use crate::event::{CrashKind, ProviderEvent, ToolImage};
use crate::host::{self, HostFn};
use crate::models::{DEFAULT_MODEL_ID, PROVIDER_ID};
use crate::settings;

const HISTORY_COLLECTION: &str = "history";
/// Per-session `tool_use_id` allocator. Deliberately NOT the `history`
/// collection (which core wipes on session clear): persisted events outlive
/// the trimmed transcript, so ids must never repeat within a session.
const COUNTER_COLLECTION: &str = "counters";
/// Stored transcript cap. Crossing it trims down to `HISTORY_TRIM_TARGET`
/// at once, so the prompt prefix then stays byte-identical (and the
/// server's prefix cache warm) for many turns.
const MAX_HISTORY_MESSAGES: usize = 160;
const HISTORY_TRIM_TARGET: usize = MAX_HISTORY_MESSAGES * 3 / 5;
/// Max continue nudges per turn for a reply that announces a step and stops.
const MAX_CONTINUE_NUDGES: usize = 2;
const CONTINUE_NUDGE: &str =
    "Continue: make that tool call now. If you're done, give your final answer instead.";
/// No cap on tool rounds per turn: a busy model may call as many tools as
/// the task needs. A model that repeats the exact same calls is told so
/// (the repeat not run) from the `REPEAT_WARN_AT`th round in a row, and the
/// turn ends at `MAX_IDENTICAL_TOOL_ROUNDS`.
const REPEAT_WARN_AT: usize = 3;
const MAX_IDENTICAL_TOOL_ROUNDS: usize = 5;
/// Reads of the same file window allowed with no file change in between; the
/// next are refused, and the turn ends after `MAX_READ_REFUSALS` refusals.
/// Seen: 265 read_file calls cycling through seven files in a "final review",
/// a cycle too long for `repeat_run`.
const MAX_UNCHANGED_READS: usize = 3;
const MAX_READ_REFUSALS: usize = 12;
const REREAD_REFUSED: &str = "Not run: you have read this several times this turn and no file has \
     changed since. Stop reading and act on what you know: make a specific fix, or finish now with \
     your report, including what you could not verify.";
/// Cap on the response body quoted in an HTTP-error crash reason.
const MAX_ERROR_CHARS: usize = 2_000;
/// Tool images shown to the model: only the newest one, within about 5 MB
/// of base64. vLLM's default --limit-mm-per-prompt is one image, and earlier
/// images of the turn are swapped back to their stubs before a new one goes.
const MAX_TOOL_IMAGES_PER_ROUND: usize = 1;
const MAX_TOOL_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Per-turn token rollup. The server reports `usage` once per completion,
/// so a turn that ran tools produces one set per round; they sum into the
/// single `Usage` event the turn emits.
#[derive(Default)]
struct TurnUsage {
    /// Σ `prompt_tokens` over the turn's rounds — every prompt token the
    /// server processed, replayed history included.
    input: i64,
    /// Σ `completion_tokens` over the turn's rounds.
    output: i64,
    /// `prompt_tokens` of the last round that reported one: the context
    /// window at end of turn. Summing it would double-count the replayed
    /// transcript.
    context: i64,
}

impl TurnUsage {
    fn add_round(&mut self, prompt: i64, completion: i64) {
        self.input += prompt;
        self.output += completion;
        if prompt > 0 {
            self.context = prompt;
        }
    }

    fn is_empty(&self) -> bool {
        self.input == 0 && self.output == 0
    }
}

/// One tool call from `choices[0].message.tool_calls`, normalised: `wire_id`
/// is what the `tool` result must echo as `tool_call_id`, `args` the parsed
/// arguments object, `arguments` the JSON string sent back in history.
struct ToolCall {
    wire_id: String,
    name: String,
    args: Value,
    arguments: String,
    /// Why an argument couldn't be coerced (`coerce_args`), added to the
    /// tool's error if the call fails.
    notes: Vec<String>,
    /// Argument names the tool's schema doesn't have; the model is told they
    /// were ignored.
    ignored: Vec<String>,
}

/// A tool that failed this turn and hasn't succeeded since: its name, how
/// many times it failed in a row, and the first line of the latest error.
#[derive(Debug, Clone, PartialEq)]
struct FailedCall {
    name: String,
    times: usize,
    error: String,
}

/// What a turn's tool calls leave behind for later rounds.
#[derive(Default)]
struct TurnRecord {
    /// Calls that failed and weren't redone successfully.
    failures: Vec<FailedCall>,
    /// Paths used successfully, for `retry_args` to correct a mistyped one.
    paths: Vec<String>,
    /// The user asked for research and no web page has been read yet: the
    /// first file write is refused once (`RESEARCH_GATE`).
    research_pending: bool,
    /// `read_file` calls per file window since the last file change.
    reads: Vec<(String, usize)>,
    /// Reads refused by `MAX_UNCHANGED_READS` this turn.
    read_refusals: usize,
}

impl TurnRecord {
    /// Count a read_file of this window; returns how many times it has now
    /// been asked for since the last file change.
    fn note_read(&mut self, args: &Value) -> usize {
        let field = |k: &str| args.get(k).map_or(String::new(), Value::to_string);
        let path = args.get("path").and_then(Value::as_str).unwrap_or("");
        let key = format!(
            "{}|{}|{}",
            path.trim_start_matches('/'),
            field("start_line"),
            field("line_count")
        );
        if let Some(entry) = self.reads.iter_mut().find(|(k, _)| *k == key) {
            entry.1 += 1;
            return entry.1;
        }
        self.reads.push((key, 1));
        1
    }
}

/// Tools that read a web page; only these satisfy the research gate. Not
/// `search_web`: four probes ran a search or two, opened no result, and
/// planned from the snippets.
const PAGE_READ_TOOLS: &[&str] = &["fetch_web", "parse_web", "web_get_part", "fetch_url"];

/// Seen twice: asked to "research and plan before you build", the model ran
/// one search and opened nothing, then (next probe) ran none at all and
/// copied an earlier attempt from disk, calling that research.
const RESEARCH_GATE: &str = "Not run: you were asked to research first, and you haven't read any \
     web page yet (search results are only snippets). Open 2–3 real reference designs or \
     examples with parse_web (from search results, or sites you know), write what you learned \
     into RESEARCH.md, then write files.";
/// Whether the user's message asks for research before the work. Phrases
/// count only as words of prose, not inside a path or identifier: "fix the
/// bug in research.py" or "update ResearchPanel.tsx" must not gate writes.
fn asks_for_research(prompt: &str) -> bool {
    let p = prompt.to_ascii_lowercase();
    let ident = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '/' | '.' | '-' | '\\');
    [
        "research",
        "researching",
        "look up",
        "inspiration",
        "look online",
        "search the web",
        "find examples",
    ]
    .iter()
    .any(|w| {
        p.match_indices(w).any(|(at, _)| {
            let before = p[..at].chars().next_back();
            let after = p[at + w.len()..].chars().next();
            // A sentence may end right after the word (`… research.`).
            let after_ok = match after {
                None => true,
                Some('.') => p[at + w.len() + 1..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace),
                Some(c) => !ident(c),
            };
            !before.is_some_and(ident) && after_ok
        })
    })
}

/// Record one tool call's outcome: a success clears that tool's failures
/// (a redone step), a failure adds to them.
fn record_outcome(failures: &mut Vec<FailedCall>, name: &str, ok: bool, error: &str) {
    if ok {
        failures.retain(|f| f.name != name);
        return;
    }
    let line: String = error
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("tool failed")
        .trim()
        .chars()
        .take(200)
        .collect();
    match failures.iter_mut().find(|f| f.name == name) {
        Some(f) => {
            f.times += 1;
            f.error = line;
        }
        None => failures.push(FailedCall {
            name: name.to_string(),
            times: 1,
            error: line,
        }),
    }
}

/// The user message asking the model to check its final answer against the
/// tool calls that failed this turn.
fn failed_call_check(failures: &[FailedCall]) -> String {
    let list = failures
        .iter()
        .map(|f| {
            let times = if f.times > 1 {
                format!(" ({} times)", f.times)
            } else {
                String::new()
            };
            format!("- {}{times}: {}", f.name, f.error)
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Before you finish: these tool calls failed this turn and were not redone \
         successfully afterwards:\n{list}\n\nIf your answer above says or implies that any of \
         them worked (for example \"verified\", \"re-checked\", \"stopped\", \"passed\"), \
         write a corrected final answer that says what failed and what is still unverified. \
         Otherwise reply with exactly: OK"
    )
}

/// Whether a reply is just "OK" (any case, stray punctuation or markup).
fn is_bare_ok(text: &str) -> bool {
    text.trim()
        .trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace())
        .eq_ignore_ascii_case("ok")
}

pub fn run(payload: &Value) -> Result<(), String> {
    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or("provider.send payload missing session_id")?;
    let cfg = payload.get("spawn_config").cloned().unwrap_or(json!({}));
    let message = payload.get("message").cloned().unwrap_or(json!({}));
    let prompt = message.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let system_prompt = payload
        .get("system_prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let conversation_id = payload
        .get("conversation_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    // `default` (the seed placeholder, prefixed or bare) means the Default
    // Model setting.
    let Some(model_raw) = cfg
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| {
            let bare = s
                .strip_prefix(PROVIDER_ID)
                .and_then(|r| r.strip_prefix(':'));
            !s.is_empty() && *s != DEFAULT_MODEL_ID && bare != Some(DEFAULT_MODEL_ID)
        })
        .map(str::to_string)
        .or_else(|| settings::str("default_model"))
    else {
        return crash(
            session_id,
            "openai-compat plugin: no model selected — pick an openai-compat:<model> or set \
             Default Model in the provider settings",
            CrashKind::Unknown,
        );
    };
    let (model, root) = match settings::resolve_model_ref(&model_raw) {
        Ok(resolved) => resolved,
        Err(e) => return crash(session_id, &e, CrashKind::Unknown),
    };
    // Usage rows carry the fully-qualified id (the session's prefixed
    // selection).
    let prefix = format!("{PROVIDER_ID}:");
    let model_label = if model_raw.starts_with(&prefix) {
        model_raw.clone()
    } else {
        format!("{prefix}{model_raw}")
    };

    emit(
        session_id,
        &ProviderEvent::Started {
            model: model_raw.clone(),
            conversation_id: conversation_id.clone(),
            metadata: json!({ "provider": PROVIDER_ID }),
        },
    )?;

    let headers = settings::http_headers();

    // Durable history is user-first; the CURRENT system prompt is
    // prepended to the request only, never persisted.
    let mut history = load_history(session_id);
    while history
        .first()
        .and_then(|m| m.get("role").and_then(|r| r.as_str()))
        == Some("system")
    {
        history.remove(0);
    }
    history.push(user_message(prompt, &message));
    // Persist the user turn now, so an interrupt or crash mid-turn doesn't
    // forget the user said anything. Any trim happens here, before the
    // request, so this turn's prompt prefix matches what's stored.
    save_history(session_id, &mut history);

    let mut messages = request_messages(system_prompt, &history, &[]);
    // Assistant + tool turns produced this turn, appended to `history` at
    // whichever exit the turn takes (never the system message).
    let mut new_messages: Vec<Value> = Vec::new();

    let mut tools = if settings::bool("enable_tools", true) {
        function_tools(session_id)
    } else {
        None
    };
    let mut send_images = settings::bool("send_tool_images", true);
    // `(index in messages, history stub)` for each tool-image message sent
    // this turn, so an image rejection can swap them back out.
    let mut tool_image_msgs: Vec<(usize, Value)> = Vec::new();
    let url = format!("{root}/v1/chat/completions");
    let mut usage = TurnUsage::default();
    let enable_thinking = settings::enable_thinking(&model);
    // Thinking Budget applies only to rounds that ask for reasoning.
    let thinking_budget = if enable_thinking == Some(true) {
        settings::thinking_budget()
    } else {
        None
    };
    let (mut sampling, bad_sampling) = settings::sampling();
    if !sampling.contains_key("temperature")
        && let Some(t) = settings::default_temperature(&model, enable_thinking)
    {
        sampling.insert("temperature".into(), json!(t));
    }
    for problem in bad_sampling {
        emit(
            session_id,
            &ProviderEvent::System {
                text: format!("Ignoring sampling setting: {problem}."),
                subtype: "warning".into(),
                detail: Value::Null,
            },
        )?;
    }

    let mut signatures: Vec<String> = Vec::new();
    let mut nudges = 0;
    let mut overflow_retried = false;
    // Tool calls that failed this turn and weren't redone successfully (the
    // once-per-turn check on the final answer they trigger), and the paths
    // the turn used successfully.
    let mut record = TurnRecord {
        research_pending: asks_for_research(prompt),
        ..TurnRecord::default()
    };
    let mut failure_checked = false;
    let mut awaiting_check_reply = false;
    let mut compacted = false;
    loop {
        if should_stop(session_id) {
            return interrupted_exit(session_id, &model_label, history, new_messages, &usage);
        }
        if let Some(budget) = settings::compact_turn_tokens()
            // This turn's size only. Measuring the whole conversation made a
            // session with long history compact every round from the start, so
            // the model remembered just its last few steps and re-read in circles.
            && estimate_tokens(&new_messages) > budget
            && compact_turn(&mut messages, &mut new_messages) > 0
            && !compacted
        {
            compacted = true;
            emit(
                session_id,
                &ProviderEvent::System {
                    text: "Long turn compacted: file contents already written, long tool results \
                           and reasoning from older rounds were shortened to save context."
                        .into(),
                    subtype: "info".into(),
                    detail: Value::Null,
                },
            )?;
        }
        let mut body = json!({
            "model": model,
            "messages": messages,
            "stream": false,
        });
        if let Some(t) = &tools {
            body["tools"] = t.clone();
        }
        if let Some(on) = enable_thinking {
            body["chat_template_kwargs"] = json!({ "enable_thinking": on });
        }
        if let Some(obj) = body.as_object_mut() {
            obj.extend(sampling.clone());
        }
        // A thinking round is capped at the Thinking Budget (or the user's
        // Max Tokens, if lower); see `force_answer` for what happens at it.
        // Every round gets the same budget. 0.1.128 halved it after the
        // turn's first round to curb drafting in reasoning, but the cap also
        // covers the answer, so every large file write was cut at ~4k tokens
        // and had to be generated again.
        let budget = thinking_budget.filter(|b| {
            body.get("max_tokens")
                .and_then(Value::as_u64)
                .is_none_or(|cap| cap > *b as u64)
        });
        if let Some(b) = budget {
            body["max_tokens"] = json!(b);
        }
        let request = body.clone();
        // Blocking: `peckboard_http_request` has no cancel or poll handle
        // (one request, bounded only by `timeout_secs`), so a stop landing
        // mid-request is seen only once the server answers, and the server
        // keeps generating until then. Cancelling it would need a streaming
        // request the host can drop on stop.
        let resp = match host::call_host(
            HostFn::HttpRequest,
            &json!({
                "url": url,
                "method": "POST",
                "headers": headers,
                "body": body.to_string(),
                "timeout_secs": settings::timeout_secs(),
            }),
        ) {
            Ok(resp) => resp,
            // Transport failure. The commonest cause on local hardware is the
            // request timeout cutting off a long generation, and reqwest's
            // message ("error sending request for url …") doesn't say so.
            Err(e) => {
                emit_usage(session_id, &model_label, &usage)?;
                return crash(
                    session_id,
                    &format!(
                        "openai-compat: request to {url} failed: {e}. If the server is up and was \
                         still generating, the reply took longer than the Request Timeout ({} s); \
                         raise it in the provider settings.",
                        settings::timeout_secs()
                    ),
                    CrashKind::Timeout,
                );
            }
        };
        let status = resp.get("status").and_then(|v| v.as_u64()).unwrap_or(0);
        let body = resp.get("body").and_then(|v| v.as_str()).unwrap_or("");
        if !(200..300).contains(&status) {
            // The prompt outgrew the model's context window: drop about half
            // of the older turns and retry once. Checked before the tools
            // retry — an overflow message can mention tools too.
            if status == 400
                && !overflow_retried
                && is_context_overflow(body)
                && trim_older_turns(&mut history)
            {
                overflow_retried = true;
                save_history(session_id, &mut history);
                messages = request_messages(system_prompt, &history, &new_messages);
                // Rebuilt from history, which holds only the image stubs.
                tool_image_msgs.clear();
                emit(
                    session_id,
                    &ProviderEvent::System {
                        text: "History trimmed to fit the model's context window; older \
                               messages were dropped."
                            .into(),
                        subtype: "warning".into(),
                        detail: Value::Null,
                    },
                )?;
                continue;
            }
            // A text-only model or server 400s image input: swap the images
            // sent this turn for their stubs, stop sending more, and retry
            // the round once.
            if status == 400
                && !tool_image_msgs.is_empty()
                && !is_context_overflow(body)
                && is_image_rejection(body)
            {
                // The rejected images were never seen: say so in both the
                // request and the stored history, so the model doesn't report
                // on them (seen: "both widths render well" after a rejection).
                let unseen = json!({
                    "role": "user",
                    "content": "[The screenshot(s) from this round could not be shown to you: the \
                                server rejected image input. Don't describe how the page looks; \
                                check it another way, e.g. browser_outline or browser_find, or \
                                by reading the code.]",
                });
                for (i, stub) in tool_image_msgs.drain(..) {
                    messages[i] = unseen.clone();
                    if let Some(slot) = new_messages.iter_mut().rev().find(|m| **m == stub) {
                        *slot = unseen.clone();
                    }
                }
                send_images = false;
                emit(
                    session_id,
                    &ProviderEvent::System {
                        text: format!(
                            "Server rejected image input for model '{model}'; continuing \
                             without tool images. Turn off Send Tool Images to skip this retry."
                        ),
                        subtype: "warning".into(),
                        detail: Value::Null,
                    },
                )?;
                continue;
            }
            // A server/model without tool calling (e.g. vLLM without
            // --enable-auto-tool-choice) 400s a request carrying `tools`:
            // drop them for the rest of the turn and retry this round once.
            if status == 400
                && tools.is_some()
                && !is_context_overflow(body)
                && body.to_ascii_lowercase().contains("tool")
            {
                tools = None;
                emit(
                    session_id,
                    &ProviderEvent::System {
                        text: format!(
                            "Server rejected tools for model '{model}'; continuing without \
                             tools. Turn off Enable Tools to skip this retry."
                        ),
                        subtype: "warning".into(),
                        detail: Value::Null,
                    },
                )?;
                continue;
            }
            emit_usage(session_id, &model_label, &usage)?;
            return crash(
                session_id,
                &format!("openai-compat HTTP {status}: {}", error_message(body)),
                CrashKind::Unknown,
            );
        }
        // An empty or unparseable 2xx body means the server closed the
        // connection before producing anything — the one genuine NoOutput.
        let Ok(mut parsed) = serde_json::from_str::<Value>(body) else {
            emit_usage(session_id, &model_label, &usage)?;
            return no_output(session_id);
        };
        // Counts ride the same body; accumulate before the error check so a
        // response that also carries an error still contributes them.
        let u = parsed.get("usage");
        let count = |k: &str| u.and_then(|u| u.get(k)).and_then(|v| v.as_i64());
        usage.add_round(
            count("prompt_tokens").unwrap_or(0),
            count("completion_tokens").unwrap_or(0),
        );
        if let Some(err) = parsed.get("error").filter(|e| !e.is_null()) {
            emit_usage(session_id, &model_label, &usage)?;
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
                .or_else(|| err.as_str().map(str::to_string))
                .unwrap_or_else(|| err.to_string());
            return crash(session_id, &msg, CrashKind::Unknown);
        }
        let Some(mut msg) = parsed
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .cloned()
        else {
            emit_usage(session_id, &model_label, &usage)?;
            return no_output(session_id);
        };
        // Thinking budget: the round was capped at Thinking Budget tokens and
        // stopped there — mid-reasoning, or mid-answer after a long
        // reasoning. Replay the reasoning (finished or not) closed off and
        // have the model continue that same message (vLLM's
        // continue_final_message) with no cap, so only reasoning is bounded:
        // it answers or calls a tool now instead of reasoning on until the
        // request times out (seen on UI tasks: 10k+ tokens of design notes).
        // Also when the cut reply carries tool calls: the cap covers the
        // arguments too, and the server still parses a call cut mid-argument
        // (seen: data.js written cut off mid-string, then two edit_file
        // calls whose `edits` JSON ended mid-way), so it is redone uncapped.
        // vLLM reports `finish_reason: "tool_calls"` for such a cut call
        // once it parses one (seen: write_file content cut at ~3,900 tokens
        // of a 4,096 cap, `path` missing, reported as tool_calls), so the
        // token count is the test as well as the finish reason.
        let used_whole_cap = budget.is_some_and(|b| {
            parsed
                .pointer("/usage/completion_tokens")
                .and_then(Value::as_u64)
                .is_some_and(|n| n >= b as u64)
        });
        let mut forced_think: Option<String> = None;
        if budget.is_some()
            && (finish_reason_of(&parsed) == Some("length") || used_whole_cap)
            && let Some(next) = force_answer(
                &url,
                &headers,
                &request,
                &round_reasoning(&msg),
                sampling.get("max_tokens"),
            )
            && let Some(next_msg) = next.pointer("/choices/0/message").cloned()
        {
            let think = round_reasoning(&msg);
            let u = next.get("usage");
            let count = |k: &str| u.and_then(|u| u.get(k)).and_then(|v| v.as_i64());
            usage.add_round(
                count("prompt_tokens").unwrap_or(0),
                count("completion_tokens").unwrap_or(0),
            );
            if !think.is_empty() {
                emit(
                    session_id,
                    &ProviderEvent::Thinking {
                        text: think.clone(),
                    },
                )?;
            }
            emit(
                session_id,
                &ProviderEvent::System {
                    text: format!(
                        "The round reached the Thinking Budget ({} tokens); the model was asked \
                         to act on what it had worked out.",
                        budget.unwrap_or_default()
                    ),
                    subtype: "info".into(),
                    detail: Value::Null,
                },
            )?;
            parsed = next;
            msg = next_msg;
            forced_think = (!think.is_empty()).then_some(think);
        }
        // A tool call the server's parser couldn't read arrives as raw
        // `<tool_call>` text in `content` (seen: `<parameter=write_file>`
        // where `<function=write_file>` belonged — a whole page written into
        // the chat instead of the file, ending the turn). Recover it.
        if tools.is_some()
            && msg
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_none_or(|a| a.is_empty())
            && let Some(content) = msg.get("content").and_then(Value::as_str)
            && let Some((rest, recovered)) = recover_inline_tool_calls(content, tools.as_ref())
        {
            msg["content"] = json!(rest);
            msg["tool_calls"] = Value::Array(recovered);
        }
        let field_think = ["reasoning_content", "reasoning"]
            .iter()
            .find_map(|k| msg.get(*k).and_then(|v| v.as_str()))
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
        if let Some(think) = &field_think {
            emit(
                session_id,
                &ProviderEvent::Thinking {
                    text: think.clone(),
                },
            )?;
        }
        // No server reasoning parser: the template's think block arrives
        // inline in `content`. Only the answer is shown and persisted.
        let (inline_think, text) =
            split_inline_think(msg.get("content").and_then(|v| v.as_str()).unwrap_or(""));
        if let Some(think) = &inline_think {
            emit(
                session_id,
                &ProviderEvent::Thinking {
                    text: think.clone(),
                },
            )?;
        }
        let round_thinking = forced_think.or(field_think).or(inline_think);
        // The reply to the failed-call check: a bare OK keeps the answer
        // already shown, so the check and the OK are dropped from the turn.
        let check_reply = std::mem::take(&mut awaiting_check_reply);
        let no_calls = msg
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_none_or(|a| a.is_empty());
        if check_reply && no_calls && is_bare_ok(&text) {
            new_messages.pop();
            break;
        }
        if check_reply && no_calls && !text.is_empty() {
            emit(
                session_id,
                &ProviderEvent::System {
                    text: "Some tool calls failed this turn; the model revised its final answer."
                        .into(),
                    subtype: "info".into(),
                    detail: Value::Null,
                },
            )?;
        }
        if !text.is_empty() {
            emit(session_id, &ProviderEvent::Text { text: text.clone() })?;
        }
        let finish_reason = parsed
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("finish_reason"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if finish_reason.as_deref() == Some("length") {
            emit(
                session_id,
                &ProviderEvent::System {
                    text: "The reply hit the max-token limit and was cut off. Raise Max Tokens \
                           (or the server's context length), or send a message to continue."
                        .into(),
                    subtype: "warning".into(),
                    detail: Value::Null,
                },
            )?;
        }
        let calls = parse_tool_calls(session_id, &msg, tools.as_ref());
        let mut assistant = json!({ "role": "assistant", "content": text });
        if !calls.is_empty() {
            assistant["tool_calls"] = Value::Array(
                calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.wire_id,
                            "type": "function",
                            "function": { "name": c.name, "arguments": c.arguments },
                        })
                    })
                    .collect(),
            );
        }
        // Within this turn, the model's own reasoning goes back to it on its
        // assistant messages: Qwen3-style templates render `reasoning_content`
        // into the think block of every assistant turn after the last user
        // query, and without it each tool round starts its reasoning from
        // scratch (and re-runs checks it already did). vLLM reads the
        // message's `reasoning` field into that template variable; llama.cpp's
        // server reads `reasoning_content`; each ignores the other. History
        // keeps only the answer, so older turns stay small and the prefix
        // stable.
        let mut sent = assistant.clone();
        if let Some(think) = round_thinking {
            sent["reasoning"] = json!(think);
            sent["reasoning_content"] = json!(think);
        }
        messages.push(sent);
        new_messages.push(assistant);
        // The stop may have landed while the (blocking) request was in
        // flight: persist what we have instead of dropping the exchange.
        if should_stop(session_id) {
            return interrupted_exit(session_id, &model_label, history, new_messages, &usage);
        }
        if calls.is_empty() {
            // The reply ended inside a tool call: have it re-sent (bounded,
            // like the continue nudge) instead of ending the turn on it.
            if tools.is_some() && nudges < MAX_CONTINUE_NUDGES && has_cut_tool_call(&text) {
                nudges += 1;
                emit(
                    session_id,
                    &ProviderEvent::System {
                        text: format!(
                            "The reply ended inside a tool call (finish reason: {}); it was not \
                             run and the model was asked to send it again.",
                            finish_reason.as_deref().unwrap_or("unknown")
                        ),
                        subtype: "warning".into(),
                        detail: Value::Null,
                    },
                )?;
                let nudge = json!({ "role": "user", "content": CUT_CALL_NUDGE });
                messages.push(nudge.clone());
                new_messages.push(nudge);
                continue;
            }
            // A tool call too garbled to read must not pass as the final
            // answer: the turn would end "complete" mid-task.
            if tools.is_some() && nudges < MAX_CONTINUE_NUDGES && has_garbled_tool_call(&text) {
                nudges += 1;
                emit(
                    session_id,
                    &ProviderEvent::System {
                        text: "The reply ended in a tool call too garbled to read; it was not \
                               run and the model was asked to send it again."
                            .into(),
                        subtype: "warning".into(),
                        detail: Value::Null,
                    },
                )?;
                let nudge = json!({ "role": "user", "content": GARBLED_CALL_NUDGE });
                messages.push(nudge.clone());
                new_messages.push(nudge);
                continue;
            }
            // "Let me check X." and then nothing: the agent announced a
            // step but stopped. Nudge it to act, a bounded number of times;
            // the nudge is persisted so the history stays consistent.
            if tools.is_some()
                && finish_reason.as_deref() == Some("stop")
                && nudges < MAX_CONTINUE_NUDGES
                && announces_unfinished_step(&text)
            {
                nudges += 1;
                let nudge = json!({ "role": "user", "content": CONTINUE_NUDGE });
                messages.push(nudge.clone());
                new_messages.push(nudge);
                continue;
            }
            // The answer is final, but tool calls failed this turn and were
            // never redone: local models then report them as done anyway
            // (seen: "re-verified" after five timed-out screenshots, "server
            // stopped" after a failed stop). Show the model the failures and
            // ask once for a corrected answer, or a bare OK to keep this one.
            if tools.is_some()
                && finish_reason.as_deref() == Some("stop")
                && !failure_checked
                && !record.failures.is_empty()
            {
                failure_checked = true;
                awaiting_check_reply = true;
                let check =
                    json!({ "role": "user", "content": failed_call_check(&record.failures) });
                messages.push(check.clone());
                new_messages.push(check);
                continue;
            }
            break;
        }
        // How many times the latest rounds repeat back to back, as one round
        // or a short cycle (`repeat_run`). Probe P alternated a read_file and a
        // no-op edit_file eleven times, which a one-round count never saw. A
        // re-read after each *different* edit is no cycle.
        signatures.push(tool_call_signature(&calls));
        let identical_rounds = repeat_run(&signatures);
        // Stuck-ish: the repeat isn't run, and the model is told why and to
        // change course. Ending the turn outright on the 3rd repeat killed
        // an hour-long build over one malformed read_file.
        if (REPEAT_WARN_AT..MAX_IDENTICAL_TOOL_ROUNDS).contains(&identical_rounds) {
            for c in &calls {
                let tool_msg = json!({
                    "role": "tool",
                    "tool_call_id": c.wire_id,
                    "content": format!(
                        "Not run: this is the same call as your previous {} replies, and its \
                         result won't change. Use what it returned, or do something different \
                         (other arguments, another tool, or your answer).",
                        identical_rounds - 1
                    ),
                });
                messages.push(tool_msg.clone());
                new_messages.push(tool_msg);
            }
            continue;
        }
        if identical_rounds >= MAX_IDENTICAL_TOOL_ROUNDS {
            // Stuck, not busy: end the turn normally without running the
            // repeat. Its calls are dropped from the stored assistant
            // message so the history has no dangling tool references.
            if let Some(obj) = new_messages.last_mut().and_then(Value::as_object_mut) {
                obj.remove("tool_calls");
            }
            emit(
                session_id,
                &ProviderEvent::System {
                    text: format!(
                        "Turn stopped: the model made the same tool call \
                         {MAX_IDENTICAL_TOOL_ROUNDS} times in a row. Send a message to continue."
                    ),
                    subtype: "warning".into(),
                    detail: Value::Null,
                },
            )?;
            break;
        }
        let mut round_images = Vec::new();
        if run_tool_calls(
            session_id,
            &calls,
            &mut messages,
            &mut new_messages,
            send_images,
            &mut round_images,
            &mut record,
        )? {
            return interrupted_exit(session_id, &model_label, history, new_messages, &usage);
        }
        // The nudge budget guards against stalling in a row, not across a
        // long build: a round of real work earns it back.
        nudges = 0;
        if record.read_refusals >= MAX_READ_REFUSALS {
            emit(
                session_id,
                &ProviderEvent::System {
                    text: "Turn stopped: the model kept re-reading files that had not changed. \
                           Send a message to continue."
                        .into(),
                    subtype: "warning".into(),
                    detail: Value::Null,
                },
            )?;
            break;
        }
        // The model sees this round's image now; history keeps a stub.
        // Earlier images of this turn go back to their stubs first: vLLM
        // allows one image per prompt unless started with a larger
        // --limit-mm-per-prompt ("At most 1 image(s) may be provided in one
        // prompt"), and the newest screenshot is the one that matters.
        if send_images {
            let shown = tool_image_message(&round_images);
            let picked = shown.as_ref().map(|s| s.2.clone()).unwrap_or_default();
            // Images left out by the cap: their "attached below" would be a
            // lie the model acts on.
            for (i, ri) in round_images.iter().enumerate() {
                if picked.contains(&i) {
                    continue;
                }
                let (from, to) = (
                    image_placeholder(&ri.img, true),
                    over_cap_placeholder(&ri.img),
                );
                for m in [&mut messages[ri.msg], &mut new_messages[ri.new_msg]] {
                    if let Some(c) = m["content"].as_str() {
                        m["content"] = json!(c.replacen(&from, &to, 1));
                    }
                }
            }
            if let Some((sent, stub, _)) = shown {
                for (i, old_stub) in tool_image_msgs.drain(..) {
                    messages[i] = old_stub;
                }
                tool_image_msgs.push((messages.len(), stub.clone()));
                messages.push(sent);
                new_messages.push(stub);
            }
        }
    }

    persist_turn(session_id, &mut history, new_messages);
    emit_usage(session_id, &model_label, &usage)?;
    emit(
        session_id,
        &ProviderEvent::Completed {
            conversation_id,
            result_meta: Value::Null,
        },
    )?;
    Ok(())
}

/// The user turn: plain string content, or OpenAI content parts
/// (`text` + `image_url` data URLs) when image attachments are present.
fn user_message(prompt: &str, message: &Value) -> Value {
    let images = image_attachments(message);
    if images.is_empty() {
        return json!({ "role": "user", "content": prompt });
    }
    let mut parts = vec![json!({ "type": "text", "text": prompt })];
    for (mime, data) in images {
        parts.push(json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{mime};base64,{data}") },
        }));
    }
    json!({ "role": "user", "content": parts })
}

/// `(thinking, answer)` from a `content` the chat template's think block
/// leaked into (no server reasoning parser): `reasoning</think>answer` —
/// the opening `<think>` usually sits in the prompt — or
/// `<think>reasoning</think>answer`. Splits on the LAST `</think>`; with
/// none, the content is all answer.
fn split_inline_think(content: &str) -> (Option<String>, String) {
    const CLOSE: &str = "</think>";
    let Some(idx) = content.rfind(CLOSE) else {
        return (None, content.to_string());
    };
    let before = content[..idx].trim_start();
    let think = before.strip_prefix("<think>").unwrap_or(before).trim();
    let answer = content[idx + CLOSE.len()..].trim_start().to_string();
    ((!think.is_empty()).then(|| think.to_string()), answer)
}

/// `finish_reason` of the first choice.
fn finish_reason_of(parsed: &Value) -> Option<&str> {
    parsed
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
}

/// Tool calls left as raw `<tool_call>` blocks in a reply's text because
/// the server's parser rejected them. Reads the qwen3_coder XML form —
/// the function from `<function=NAME>`, or from a first `<parameter=NAME>`
/// that names an offered tool (a real model slip) — or a JSON
/// `{"name", "arguments"}` body. Only looks after the last `</think>`.
/// Returns the text with the blocks removed plus OpenAI-shape tool calls,
/// or `None` when nothing usable was found.
fn recover_inline_tool_calls(content: &str, tools: Option<&Value>) -> Option<(String, Vec<Value>)> {
    const OPEN: &str = "<tool_call>";
    const CLOSE: &str = "</tool_call>";
    let answer_from = content
        .rfind("</think>")
        .map_or(0, |i| i + "</think>".len());
    let (head, answer) = content.split_at(answer_from);
    if !answer.contains(OPEN) {
        return None;
    }
    let tool_names: Vec<&str> = tools
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| t.pointer("/function/name").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let mut calls = Vec::new();
    let mut rest = head.to_string();
    let mut cursor = answer;
    while let Some(start) = cursor.find(OPEN) {
        rest.push_str(&cursor[..start]);
        let after = &cursor[start + OPEN.len()..];
        let (body, next) = match after.find(CLOSE) {
            Some(end) => (&after[..end], &after[end + CLOSE.len()..]),
            None => (after, ""),
        };
        // A call that never reached `</function>` was cut off (the reply ran
        // out of tokens): its arguments are partial and a later one (seen:
        // `path` after `content`) is missing. Running it wrote a stylesheet
        // cut mid-line, eleven times over. Leave it as text; `has_cut_tool_call`
        // makes the model re-send it.
        let complete = after.contains(CLOSE) || body.contains("</function>");
        if !complete {
            rest.push_str(OPEN);
            rest.push_str(body);
            cursor = next;
            continue;
        }
        if let Some(call) = parse_inline_call(body, &tool_names) {
            calls.push(call);
        } else {
            // Unreadable: keep it visible rather than silently dropping it.
            rest.push_str(OPEN);
            rest.push_str(body);
        }
        cursor = next;
    }
    rest.push_str(cursor);
    (!calls.is_empty()).then(|| (rest.trim_end().to_string(), calls))
}

/// Whether the reply's answer ends in a raw tool call that was cut off
/// before `</function>` (see `recover_inline_tool_calls`).
fn has_cut_tool_call(content: &str) -> bool {
    let answer = content.rfind("</think>").map_or(content, |i| &content[i..]);
    answer.rfind("<tool_call>").is_some_and(|at| {
        !answer[at..].contains("</function>") && !answer[at..].contains("</tool_call>")
    })
}

/// Whether the reply's answer ends in a complete raw tool call that
/// `recover_inline_tool_calls` couldn't read (seen: `<=read_file>` and
/// `=path>`, the `<function` and `<parameter` lost). Only at the very end,
/// so an answer that merely quotes the tags is still an answer.
fn has_garbled_tool_call(content: &str) -> bool {
    let answer = content.rfind("</think>").map_or(content, |i| &content[i..]);
    let end = answer.trim_end();
    answer.contains("<tool_call>")
        && (end.ends_with("</tool_call>") || end.ends_with("</function>"))
}

/// Sent when the reply ended in an unreadable tool call.
const GARBLED_CALL_NUDGE: &str = "Your last tool call was garbled (its tags were malformed), so it \
     was not run. Send that call again as a normal tool call.";

/// Sent when the reply ended inside a tool call.
const CUT_CALL_NUDGE: &str = "Your last tool call was cut off before it finished (the reply ran \
     out of room), so it was not run. Send it again. For a large file, write the first part with \
     write_file, then add the rest with write_file `append: true` in further calls.";

/// One `<tool_call>` body → an OpenAI tool call (`id` left empty; one is
/// generated downstream), or `None` if no offered tool can be identified.
fn parse_inline_call(body: &str, tool_names: &[&str]) -> Option<Value> {
    let trimmed = body.trim();
    if trimmed.starts_with('{') {
        let v: Value = serde_json::from_str(trimmed).ok()?;
        let name = v.get("name")?.as_str()?;
        if !tool_names.contains(&name) {
            return None;
        }
        let args = v.get("arguments").cloned().unwrap_or(json!({}));
        let args = match args {
            Value::String(s) => s,
            other => other.to_string(),
        };
        return Some(json!({
            "id": "", "type": "function",
            "function": { "name": name, "arguments": args },
        }));
    }
    let mut name = body
        .split_once("<function=")
        .and_then(|(_, r)| r.split_once('>'))
        .map(|(n, _)| n.trim().to_string());
    let mut args = serde_json::Map::new();
    for chunk in body.split("<parameter=").skip(1) {
        let Some((key, value)) = chunk.split_once('>') else {
            continue;
        };
        let key = key.trim();
        let value = value.rfind("</parameter>").map_or(value, |i| &value[..i]);
        let value = value.strip_prefix('\n').unwrap_or(value);
        let value = value.strip_suffix('\n').unwrap_or(value);
        // The slip: `<parameter=write_file>` standing in for the function.
        if name.is_none() && tool_names.contains(&key) {
            name = Some(key.to_string());
            continue;
        }
        args.insert(key.to_string(), Value::String(value.to_string()));
    }
    let name = name.filter(|n| tool_names.contains(&n.as_str()))?;
    Some(json!({
        "id": "", "type": "function",
        "function": { "name": name, "arguments": Value::Object(args).to_string() },
    }))
}

/// The reasoning of a reply, finished or not: the server's reasoning field,
/// else (no reasoning parser) the inline part before `</think>` — or all of
/// the content when `</think>` never came, since with thinking on it is
/// still reasoning. Empty when the reply had none.
fn round_reasoning(msg: &Value) -> String {
    if let Some(r) = ["reasoning_content", "reasoning"]
        .iter()
        .find_map(|k| msg.get(*k).and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
    {
        return r.trim().to_string();
    }
    let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
    match split_inline_think(content) {
        (Some(think), _) => think,
        (None, _) if content.contains("</think>") => String::new(),
        (None, _) => {
            let t = content.trim_start();
            t.strip_prefix("<think>").unwrap_or(t).trim().to_string()
        }
    }
}

/// Re-ask with the cut-off reasoning replayed as a closed think block on a
/// final assistant message the server continues (`continue_final_message`,
/// no generation prompt), so the model's next tokens are its answer or tool
/// call. Uses the user's Max Tokens (or none) instead of the budget. `None`
/// when the server refuses (non-vLLM servers may not support this); the
/// cut-off reply is then used as before, with its length warning.
fn force_answer(
    url: &str,
    headers: &serde_json::Map<String, Value>,
    request: &Value,
    think: &str,
    user_max_tokens: Option<&Value>,
) -> Option<Value> {
    let mut body = request.clone();
    let mut msgs = body.get("messages")?.as_array()?.clone();
    msgs.push(json!({
        "role": "assistant",
        "content": "",
        "reasoning": think,
        "reasoning_content": think,
    }));
    body["messages"] = Value::Array(msgs);
    body["continue_final_message"] = json!(true);
    body["add_generation_prompt"] = json!(false);
    match user_max_tokens {
        Some(cap) => body["max_tokens"] = cap.clone(),
        None => {
            body.as_object_mut()?.remove("max_tokens");
        }
    }
    let resp = host::call_host(
        HostFn::HttpRequest,
        &json!({
            "url": url,
            "method": "POST",
            "headers": headers,
            "body": body.to_string(),
            "timeout_secs": settings::timeout_secs(),
        }),
    )
    .ok()?;
    let status = resp.get("status").and_then(Value::as_u64).unwrap_or(0);
    if !(200..300).contains(&status) {
        return None;
    }
    let parsed: Value = serde_json::from_str(resp.get("body")?.as_str()?).ok()?;
    parsed.pointer("/choices/0/message")?;
    Some(parsed)
}

/// Whether a reply's last sentence announces a step it never took
/// ("Let me check the logs.", "Three questions. Let me work through
/// them.", "Now I'll run the tests:") — the agent stopping mid-task instead
/// of making the tool call.
fn announces_unfinished_step(answer: &str) -> bool {
    let Some(line) = answer.lines().map(str::trim).rfind(|l| !l.is_empty()) else {
        return false;
    };
    if line.ends_with(':') {
        return true;
    }
    // Last sentence of the last line: a one-line reply often leads with a
    // recap ("Three questions, all read-only.") before the announcement.
    let body = line.trim_end_matches(['.', '!', '…', ' ']);
    let sentence = body.rfind(['.', '!', '?']).map_or(body, |i| &body[i + 1..]);
    let lower = sentence.to_lowercase().replace('\u{2019}', "'");
    let mut lower = lower.trim_start_matches(|c: char| !c.is_alphanumeric());
    // Discourse openers in front of the announcement ("Now, let me …").
    for opener in [
        "now", "next", "first", "then", "ok", "okay", "alright", "good", "great",
    ] {
        if let Some(rest) = lower.strip_prefix(opener)
            && rest.starts_with([',', ' '])
        {
            lower = rest.trim_start_matches([',', ' ']);
            break;
        }
    }
    if lower.starts_with("let me know") {
        return false;
    }
    [
        "let me ",
        "i'll ",
        "i will ",
        "let's ",
        "i am going to ",
        "i'm going to ",
        // Seen ending a turn: "Now I'll align the HTML. Starting with the home
        // page." The announcement is the sentence before this one.
        "starting with ",
        "starting on ",
        "beginning with ",
        "continuing with ",
        "moving on to ",
    ]
    .iter()
    .any(|p| lower.starts_with(p))
}

/// Best human-readable message from an error body: OpenAI's
/// `{"error":{"message"}}`, a bare `{"error":"…"}` / `{"message"}` /
/// `{"detail"}`, else the raw (truncated) body.
fn error_message(body: &str) -> String {
    let from_json = serde_json::from_str::<Value>(body).ok().and_then(|v| {
        let e = v.get("error");
        e.and_then(|e| e.get("message"))
            .or(e)
            .or_else(|| v.get("message"))
            .or_else(|| v.get("detail"))
            .and_then(|m| m.as_str())
            .map(str::to_string)
    });
    let msg = from_json.unwrap_or_else(|| body.to_string());
    msg.chars().take(MAX_ERROR_CHARS).collect()
}

/// Normalise `message.tool_calls`. Arguments arrive as a JSON string
/// (sometimes an object on lax servers); unparseable or empty arguments
/// become `{}` rather than failing the turn, and argument types are coerced
/// to the offered tool schema ([`coerce_args`]). A call without an id gets a
/// generated one so the `tool` result can still reference it.
fn parse_tool_calls(session_id: &str, msg: &Value, tools: Option<&Value>) -> Vec<ToolCall> {
    let Some(arr) = msg.get("tool_calls").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for call in arr {
        let fn_obj = call.get("function").unwrap_or(call);
        let name = fn_obj
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let args = match fn_obj.get("arguments") {
            Some(Value::String(s)) if !s.trim().is_empty() => {
                serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({}))
            }
            Some(v @ Value::Object(_)) => v.clone(),
            _ => json!({}),
        };
        let mut args = if args.is_object() { args } else { json!({}) };
        let notes = match tool_schema(tools, &name) {
            Some(schema) => coerce_args(&mut args, schema),
            None => Vec::new(),
        };
        let wire_id = call
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!("call_{}", alloc_tool_ids(&format!("{session_id}:wire"), 1))
            });
        // Arguments the tool doesn't take are dropped by the host without a
        // word (seen: run_command with `path: "tmp-probe"` listed the project
        // root, and the model took it for tmp-probe).
        let ignored = tool_schema(tools, &name)
            .and_then(|s| s.get("properties"))
            .and_then(Value::as_object)
            .map(|props| {
                args.as_object()
                    .map(|o| {
                        o.keys()
                            .filter(|k| !props.contains_key(*k))
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        out.push(ToolCall {
            wire_id,
            name,
            arguments: args.to_string(),
            args,
            notes,
            ignored,
        });
    }
    out
}

/// What a round asked for — each call's name + canonical arguments, ignoring
/// the per-round wire ids — so two rounds compare equal exactly when the
/// model repeated itself.
fn tool_call_signature(calls: &[ToolCall]) -> String {
    calls
        .iter()
        .map(|c| format!("{}\u{0}{}", c.name, c.arguments))
        .collect::<Vec<_>>()
        .join("\u{1}")
}

/// How many times the tail of `history` repeats back to back, for any block
/// of up to `MAX_REPEAT_PERIOD` rounds: `… A A A` → 3, `A B C A B C A B C` → 3
/// (seen: find, cat and ls of one config, round and round).
fn repeat_run(history: &[String]) -> usize {
    (1..=MAX_REPEAT_PERIOD)
        .filter(|&period| history.len() >= period)
        .map(|period| {
            let block = &history[history.len() - period..];
            history
                .rchunks_exact(period)
                .take_while(|c| *c == block)
                .count()
        })
        .max()
        .unwrap_or(0)
}
/// Longest cycle of rounds `repeat_run` looks for.
const MAX_REPEAT_PERIOD: usize = 8;

/// The `parameters` schema the server was offered for tool `name`.
fn tool_schema<'a>(tools: Option<&'a Value>, name: &str) -> Option<&'a Value> {
    tools?.as_array()?.iter().find_map(|t| {
        let f = t.get("function")?;
        if f.get("name")?.as_str()? == name {
            f.get("parameters")
        } else {
            None
        }
    })
}

/// Local models (via vLLM's XML tool-call parsers) often get argument types
/// wrong: an array sent as a string — sometimes cut short, e.g.
/// `["-c", "print(1)` without the closing `"]` — a number as a string, `""`
/// for an optional number, or a bare scalar where a list of strings is
/// expected (`args: -1`). The host then rejects the call, and the model tends
/// to resend it unchanged until the identical-call guard ends the turn.
/// Coerce top-level arguments to the types the tool's schema declares; a
/// value that can't be coerced is left for the host to reject as before.
/// Leaked XML markup is salvaged first (`salvage_tool_markup`). Returns a
/// note for each array/object argument still left as text, to append to the
/// tool's error.
fn coerce_args(args: &mut Value, schema: &Value) -> Vec<String> {
    let (Some(obj), Some(props)) = (
        args.as_object_mut(),
        schema.get("properties").and_then(Value::as_object),
    ) else {
        return Vec::new();
    };
    salvage_tool_markup(obj, props);
    // A key with stray punctuation around it (seen: `"=command"`) is renamed
    // to the schema property it names, unless that one is already given.
    let stray: Vec<String> = obj
        .keys()
        .filter(|k| !props.contains_key(*k))
        .cloned()
        .collect();
    for key in stray {
        let clean = key.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_'));
        if clean != key && props.contains_key(clean) && !obj.contains_key(clean) {
            let v = obj.remove(&key).unwrap_or(Value::Null);
            obj.insert(clean.to_string(), v);
        }
    }
    let mut notes = Vec::new();
    let keys: Vec<String> = obj.keys().cloned().collect();
    for key in keys {
        let Some(prop) = props.get(&key) else {
            continue;
        };
        let Some(ty) = prop.get("type").and_then(Value::as_str) else {
            continue;
        };
        let Some(value) = obj.get(&key) else {
            continue;
        };
        let fixed = match (ty, value) {
            // Plain text where a list of strings belongs (`args: "-la"`, sent
            // three times in a row): split on whitespace like simple shell
            // words. Text that looks like JSON still goes to the parser.
            ("array", Value::String(s))
                if prop.pointer("/items/type").and_then(Value::as_str) == Some("string")
                    && !s.trim_start().starts_with('[')
                    && !s.trim().is_empty() =>
            {
                Some(json!(s.split_whitespace().collect::<Vec<_>>()))
            }
            ("array" | "object", Value::String(s)) => parse_json_lenient(s.trim(), ty),
            // A lone scalar where a list of strings belongs: one-item list.
            ("array", Value::Number(_) | Value::Bool(_))
                if prop.pointer("/items/type").and_then(Value::as_str) == Some("string") =>
            {
                Some(json!([value.to_string()]))
            }
            ("integer", Value::String(s)) => s.trim().parse::<i64>().ok().map(Value::from),
            ("number", Value::String(s)) => s
                .trim()
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number),
            ("boolean", Value::String(s)) => match s.trim() {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                _ => None,
            },
            _ => continue,
        };
        match fixed {
            Some(v) => {
                obj.insert(key, v);
            }
            // `""` for an optional non-string argument means "not given".
            None if value.as_str().is_some_and(|s| s.trim().is_empty()) => {
                obj.remove(&key);
            }
            // Still text: tell the model why, in the tool error. The host's
            // own message ("`edits` must be an array") doesn't say the JSON
            // was malformed, and the model resent the same call nine times.
            None if matches!(ty, "array" | "object") => {
                if let Some(s) = value.as_str() {
                    let why = serde_json::from_str::<Value>(s.trim())
                        .err()
                        .map_or_else(|| format!("not a JSON {ty}"), |e| e.to_string());
                    notes.push(format!(
                        "`{key}` arrived as text that isn't valid JSON ({why}). Send it as a \
                         real JSON {ty}, and escape every double quote inside a string value \
                         as \\\" (in HTML, single-quoted attributes avoid the problem)."
                    ));
                }
            }
            None => {}
        }
    }
    notes
}

/// Closing tags of the XML tool-call format (vLLM's qwen3_coder parser).
const TOOL_MARKUP_TAGS: &[&str] = &["</parameter>", "</function>", "</tool_call>"];

/// Undo XML tool-call markup leaking into string arguments: a malformed
/// qwen3_coder call can put the rest of the call inside one value
/// (`"15\n</parameter>\n<=path>…\n{\"path\": …}"`), so other arguments go
/// missing. Each such value is cut at its first closing tag; the leftover is
/// mined for the missing ones — a JSON object from its first `{` to its last
/// `}`, then `<parameter=NAME>VALUE</parameter>` blocks. Only keys the schema
/// declares and the call lacks (or sends as null) are filled.
fn salvage_tool_markup(
    obj: &mut serde_json::Map<String, Value>,
    props: &serde_json::Map<String, Value>,
) {
    let mut leftovers = Vec::new();
    for value in obj.values_mut() {
        let Value::String(s) = value else {
            continue;
        };
        // Markup leaked in front of a value (seen: `path:
        // "<parameter=search.html"`): drop the prefix; a still-wrong path is
        // then corrected by `retry_args`.
        if let Some(rest) = s.strip_prefix("<parameter=") {
            *s = rest.trim_start_matches('>').to_string();
        }
        let Some((cut, tag)) = TOOL_MARKUP_TAGS
            .iter()
            .filter_map(|t| s.find(t).map(|i| (i, *t)))
            .min()
        else {
            continue;
        };
        // Leaked markup is followed by nothing, a stray `>`, more tags or a
        // JSON object. Anything else is a legitimate string that merely
        // quotes the tag (e.g. write_file content for this very parser), so
        // leave it alone.
        let after = s[cut + tag.len()..].trim_start();
        if !(after.is_empty() || after.starts_with(['>', '<', '{'])) {
            continue;
        }
        leftovers.push(s[cut..].to_string());
        let kept = s[..cut].trim_end().len();
        s.truncate(kept);
    }
    let missing = |obj: &serde_json::Map<String, Value>, key: &str| {
        props.contains_key(key) && obj.get(key).is_none_or(Value::is_null)
    };
    for rest in leftovers {
        if let (Some(start), Some(end)) = (rest.find('{'), rest.rfind('}'))
            && start < end
            && let Ok(Value::Object(found)) = serde_json::from_str::<Value>(&rest[start..=end])
        {
            for (key, v) in found {
                if missing(obj, &key) {
                    obj.insert(key, v);
                }
            }
        }
        for block in rest.split("<parameter=").skip(1) {
            let Some((name, body)) = block.split_once('>') else {
                continue;
            };
            let name = name.trim();
            let value = body.split("</parameter>").next().unwrap_or("").trim();
            if !value.is_empty() && missing(obj, name) {
                obj.insert(name.to_string(), Value::String(value.to_string()));
            }
        }
    }
}

/// Parse `s` as a JSON array (`ty == "array"`) or object, also accepting it
/// with a missing closing bracket, or a missing closing quote + bracket.
fn parse_json_lenient(s: &str, ty: &str) -> Option<Value> {
    let (open, close) = if ty == "array" {
        ('[', ']')
    } else {
        ('{', '}')
    };
    if !s.starts_with(open) {
        return None;
    }
    let parse = |s: &str| {
        [String::new(), close.to_string(), format!("\"{close}")]
            .iter()
            .filter_map(|suffix| serde_json::from_str::<Value>(&format!("{s}{suffix}")).ok())
            .find(|v| {
                if ty == "array" {
                    v.is_array()
                } else {
                    v.is_object()
                }
            })
    };
    parse(s).or_else(|| parse(&escape_stray_quotes(s)))
}

/// Repair the JSON local models write inside a string argument: a double
/// quote left bare inside a string value (seen in edit_file `edits`:
/// `"text": "<input placeholder=\"Search…">"`, every retry), and raw
/// newlines or tabs. A quote closes a string only where JSON allows a string
/// to end — followed by `,` `:` `}` `]` or nothing; any other quote inside a
/// string is content and gets escaped.
fn escape_stray_quotes(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 16);
    let mut in_str = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if !in_str {
            in_str = c == '"';
            out.push(c);
        } else if c == '\\' {
            out.push(c);
            if let Some(&n) = chars.get(i + 1) {
                out.push(n);
                i += 1;
            }
        } else if c == '"' {
            let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
            if matches!(next, None | Some(',' | ':' | '}' | ']')) {
                in_str = false;
                out.push('"');
            } else {
                out.push_str("\\\"");
            }
        } else if c == '\n' {
            out.push_str("\\n");
        } else if c == '\t' {
            out.push_str("\\t");
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}
/// Emit the turn's single `Usage` event, or nothing when the server
/// reported no counts.
fn emit_usage(session_id: &str, model_label: &str, usage: &TurnUsage) -> Result<(), String> {
    if usage.is_empty() {
        return Ok(());
    }
    emit(
        session_id,
        &ProviderEvent::Usage {
            input_tokens: usage.input,
            output_tokens: usage.output,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            total_tokens: usage.context + usage.output,
            context_tokens: usage.context,
            model: Some(model_label.to_string()),
            turn_seq: None,
        },
    )
}

/// The user stopped the turn: emit accumulated usage, mark the partial
/// exchange with the interrupt marker, persist it (instead of forgetting
/// the turn happened), and report `Crashed{Interrupted}`.
fn interrupted_exit(
    session_id: &str,
    model_label: &str,
    mut history: Vec<Value>,
    mut new_messages: Vec<Value>,
    usage: &TurnUsage,
) -> Result<(), String> {
    emit_usage(session_id, model_label, usage)?;
    // A continue nudge the model never answered would leave two user turns
    // in a row once the next message arrives.
    if new_messages
        .last()
        .and_then(|m| m.get("role").and_then(|r| r.as_str()))
        == Some("user")
    {
        new_messages.pop();
    }
    if let Some(last) = new_messages.last_mut()
        && last.get("role").and_then(|r| r.as_str()) == Some("assistant")
        && let Some(text) = last
            .get("content")
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty())
    {
        last["content"] = json!(format!("{text}\n\n[Response interrupted by user]"));
    }
    // An assistant turn whose tool calls never got results is a dangling
    // reference servers reject on the next request: drop the unanswered
    // calls from the tail.
    drop_unanswered_tool_calls(&mut new_messages);
    history.extend(new_messages);
    save_history(session_id, &mut history);
    crash(session_id, "interrupted", CrashKind::Interrupted)
}

/// Strip `tool_calls` entries with no matching `tool` result from the last
/// assistant message (an interrupt between calls leaves some unanswered).
fn drop_unanswered_tool_calls(messages: &mut [Value]) {
    let answered: Vec<String> = messages
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .filter_map(|m| {
            m.get("tool_call_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect();
    let Some(last_assistant) = messages
        .iter_mut()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
    else {
        return;
    };
    let Some(calls) = last_assistant
        .get_mut("tool_calls")
        .and_then(|v| v.as_array_mut())
    else {
        return;
    };
    calls.retain(|c| {
        c.get("id")
            .and_then(|v| v.as_str())
            .is_some_and(|id| answered.iter().any(|a| a == id))
    });
    if calls.is_empty()
        && let Some(obj) = last_assistant.as_object_mut()
    {
        obj.remove("tool_calls");
    }
}

/// Append this turn's generated messages to the durable transcript.
fn persist_turn(session_id: &str, history: &mut Vec<Value>, new_messages: Vec<Value>) {
    history.extend(new_messages);
    save_history(session_id, history);
}

/// The structure-check note (`check::report`) for a successful
/// `write_file` / `edit_file` of a web file. edit_file's arguments are only
/// its ops (and write_file's only the tail in append mode), so those files
/// are read back.
fn written_file_note(session_id: &str, tool: &str, args: &Value) -> Option<String> {
    let path = args.get("path")?.as_str()?;
    if !check::is_checked(path) {
        return None;
    }
    let appended = args.get("append").and_then(Value::as_bool) == Some(true);
    let text = match tool {
        "write_file" if !appended => args.get("content")?.as_str()?.to_string(),
        "write_file" | "edit_file" => read_back(session_id, path)?,
        _ => return None,
    };
    check::report(path, &text)
}

/// The structure problems a web file has right now, appended to a
/// read_file of it (any window). Seen: the model read data.js after a note
/// about an unclosed `{` and declared it "complete and balanced"; the page
/// never loaded. `None` when the file is clean or not a web file.
fn read_file_note(session_id: &str, args: &Value) -> Option<String> {
    let path = args.get("path")?.as_str()?;
    if !check::is_checked(path) {
        return None;
    }
    let problems = check::problems(path, &read_back(session_id, path)?);
    if problems.is_empty() {
        return None;
    }
    let list: Vec<String> = problems.iter().map(|p| format!("- {p}")).collect();
    Some(format!(
        "{path} currently has structure problems (whole file checked):\n{}",
        list.join("\n")
    ))
}

/// Whether a search_web failure is the search provider throttling us.
fn search_rate_limited(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    e.contains("rate-limit")
        || e.contains("rate limit")
        || e.contains("http 202")
        || e.contains("429")
}

/// Seen: six search_web calls in a row, each rate-limited.
const SEARCH_LIMITED_HINT: &str = "Web search is rate-limited right now; retrying won't help for a \
     while. Read known pages directly instead with parse_web (e.g. a product's own site, its design \
     system or developer docs, well-known design write-ups), or carry on and note in your report \
     that search was unavailable.";

/// Close the parentheses the model dropped, in place: CSS declarations
/// (`check::repair_css_parens`) and JS callback calls
/// (`check::repair_js_parens`), in a write_file's content or an edit_file's
/// op texts (`.css` / `.js` only). Returns the note telling the model, so its
/// picture of the file matches.
fn repair_written_code(tool: &str, args: &mut Value) -> Option<String> {
    let path = args.get("path")?.as_str()?.to_string();
    let (mut css, mut js): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
    let (mut css_ops, mut js_ops) = (0, 0);
    match tool {
        "write_file" => {
            let content = args.get("content")?.as_str()?;
            let (fixed, at_css) = check::repair_css_parens(&path, content);
            let (fixed, at_js) = check::repair_js_parens(&path, &fixed);
            if !at_css.is_empty() || !at_js.is_empty() {
                args["content"] = json!(fixed);
                (css, js) = (at_css, at_js);
            }
        }
        // An edit's text has no surrounding context in HTML, so only .css
        // and .js.
        "edit_file" => {
            for op in args.get_mut("edits")?.as_array_mut()? {
                let Some(text) = op.get("text").and_then(Value::as_str) else {
                    continue;
                };
                let (fixed, at_css) = if path.to_ascii_lowercase().ends_with(".css") {
                    check::repair_css_parens(&path, text)
                } else {
                    (text.to_string(), Vec::new())
                };
                let (fixed, at_js) = check::repair_js_fragment(&path, &fixed);
                if !at_css.is_empty() || !at_js.is_empty() {
                    op["text"] = json!(fixed);
                    css_ops += at_css.len();
                    js_ops += at_js.len();
                }
            }
        }
        _ => return None,
    }
    let at = |lines: &[usize]| {
        if lines.is_empty() {
            String::new()
        } else {
            let shown: Vec<String> = lines.iter().take(12).map(usize::to_string).collect();
            format!(" (line {})", shown.join(", "))
        }
    };
    let mut notes = Vec::new();
    let n = css.len() + css_ops;
    if n > 0 {
        notes.push(format!(
            "Auto-repaired {n} CSS declaration(s) missing a closing `)` before `;`{}; the file has \
             the fix. You often drop the last `)` of nested calls: write `repeat(2, minmax(0, \
             1fr));` and `linear-gradient(transparent, rgba(0, 0, 0, .7));`.",
            at(&css)
        ));
    }
    let n = js.len() + js_ops;
    if n > 0 {
        notes.push(format!(
            "Auto-repaired {n} JS call(s) whose callback block ended in `}}` without the call's \
             closing `)`{}; the file has the fix. You often write `list.forEach(x => {{ … }}` \
             where `… }});` belongs: a single missing `)` stops the whole script, and the page \
             renders blank.",
            at(&js)
        ));
    }
    (!notes.is_empty()).then(|| notes.join("\n\n"))
}

/// A read_file asking for several windows at once (`start_line: [34, 270]`,
/// `line_count: [10, 12]` or one count for all) as one argument object per
/// window. Seen: the host read the array as no window, the model resent the
/// same call three times, and the turn ended mid-build.
fn read_windows(tool: &str, args: &Value) -> Option<Vec<Value>> {
    if tool != "read_file" {
        return None;
    }
    let starts = args.get("start_line")?.as_array()?;
    let counts = args.get("line_count");
    let windows = starts
        .iter()
        .enumerate()
        .map(|(i, start)| {
            let mut w = args.clone();
            w["start_line"] = start.clone();
            match counts {
                Some(Value::Array(c)) => match c.get(i) {
                    Some(n) => w["line_count"] = n.clone(),
                    None => {
                        w.as_object_mut().map(|o| o.remove("line_count"));
                    }
                },
                Some(n) => w["line_count"] = n.clone(),
                None => {}
            }
            w
        })
        .collect();
    Some(windows)
}

/// A standing hint appended to a successful result of `tool`. search_web:
/// asked to research before building a site, the model ran one search and
/// planned from the snippets alone, never opening a result.
fn tool_hint(tool: &str) -> Option<&'static str> {
    match tool {
        "search_web" => Some(
            "These are only titles and snippets, not research. Open the one or two most relevant \
             results with parse_web (pass `url`), then pull just the parts you need with \
             web_get_part (mode `search` or `lines`), and note what you learned in a file.",
        ),
        // Old results are compacted away in a long turn; notes in a file stay.
        "parse_web" | "web_get_part" => Some(
            "Write what's useful here (layout, components, patterns, the URL) into RESEARCH.md \
             now: this result will be dropped from your context later in a long task.",
        ),
        _ => None,
    }
}

/// The note for arguments the tool doesn't take (`ToolCall::ignored`).
fn ignored_note(tool: &str, ignored: &[String]) -> Option<String> {
    if ignored.is_empty() {
        return None;
    }
    let names = ignored
        .iter()
        .map(|k| format!("`{k}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut note = format!(
        "{names} isn't among {tool}'s documented arguments, so it was probably ignored; check \
         the result against what you meant."
    );
    if matches!(tool, "run_command" | "run_background") {
        note.push_str(
            " It always runs in the project folder; put a folder in `args` (e.g. `[\"-la\", \
             \"tmp-probe\"]`) to act on it.",
        );
    }
    Some(note)
}

/// Whether a browser tool failed because the browser itself can't run (a
/// host problem), not because of the page or the call.
fn browser_unavailable(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    [
        "sidecar",
        "no usable port",
        "browser server",
        "failed to launch",
    ]
    .iter()
    .any(|p| e.contains(p))
}

/// Seen twice: the browser sidecar couldn't start (a host problem), and the
/// model spent the rest of its turn on ps/ss/lsof instead of the task — once
/// right after trying to open youtube.com for research.
const BROWSER_DOWN_HINT: &str = "The browser is unavailable on this host; that's not something you can \
     or should fix. Don't debug it. For research use search_web and parse_web instead. For checking \
     pages, review the code and say in your report that the visual check was blocked.";

/// The error of a `provider_invoke_mcp` reply, `None` when it succeeded.
fn invoke_error(invoked: &Result<Value, String>) -> Option<String> {
    match invoked {
        Err(e) => Some(e.clone()),
        Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => None,
        Ok(v) => Some(
            v.get("error")
                .and_then(Value::as_str)
                .unwrap_or("tool failed")
                .to_string(),
        ),
    }
}

/// Drop exact repeats from an edit_file's `edits` (seen: the same line-275
/// update listed twice, which the host rejects as overlapping ranges).
fn dedupe_edit_ops(tool: &str, args: &mut Value) {
    if tool != "edit_file" {
        return;
    }
    if let Some(ops) = args.get_mut("edits").and_then(Value::as_array_mut) {
        let mut seen: Vec<Value> = Vec::new();
        ops.retain(|op| {
            let dup = seen.contains(op);
            if !dup {
                seen.push(op.clone());
            }
            !dup
        });
    }
}

/// Whether an edit_file changes nothing: no ops, or only inserts of empty
/// text (seen: `{op: insert, line: 91, text: ""}` sent eleven times, each
/// "succeeding", between re-reads of the same lines).
fn edit_is_noop(args: &Value) -> bool {
    let Some(ops) = args.get("edits").and_then(Value::as_array) else {
        return false;
    };
    ops.iter().all(|op| {
        op.get("op").and_then(Value::as_str) == Some("insert")
            && op
                .get("text")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
    })
}

const NOOP_EDIT: &str = "Not run: this edit changes nothing (it only inserts empty text). Decide \
     what the lines should become and send that as an `update` with the new text, or move on.";

/// Arguments fixed for one retry after `err`, with the note telling the
/// model what changed; `None` when there is no mechanical fix. All three
/// were seen looping until the repeat guard ended the turn:
/// - edit_file ops starting at column 1 with an `end_column` past the end of
///   the line: the model meant whole lines, so the columns are dropped.
/// - a mistyped path (`/tmp/probe/yt1/…` for `tmp-probe/yt1/…`): replaced
///   by the one path used earlier this turn within a small edit distance.
/// - edit_file with no `path` at all: the most recent path, guarded by the
///   edit's `original_hash`.
fn retry_args(tool: &str, args: &Value, err: &str, known: &[String]) -> Option<(Value, String)> {
    let lower = err.to_ascii_lowercase();
    if tool == "edit_file" && lower.contains("beyond the end of line") {
        let mut fixed = args.clone();
        let mut changed = false;
        for op in fixed.get_mut("edits")?.as_array_mut()? {
            if op.get("start_column").and_then(Value::as_u64) == Some(1)
                && let Some(o) = op.as_object_mut()
            {
                o.remove("start_column");
                changed |= o.remove("end_column").is_some();
            }
        }
        return changed.then(|| {
            (
                fixed,
                "Retried as whole-line edits: ops that started at column 1 had an end_column past \
                 the end of the line. To replace whole lines, omit start_column/end_column."
                    .to_string(),
            )
        });
    }
    // edit_file without its `path` (seen right after reading the file it
    // meant): the most recent path. Its `original_hash` must still match, so
    // a wrong guess is rejected instead of editing the wrong file.
    if tool == "edit_file"
        && args.get("path").is_none()
        && args.get("original_hash").is_some()
        && lower.contains("path")
        && lower.contains("required")
    {
        let last = known.last()?;
        let mut fixed = args.clone();
        fixed["path"] = json!(last);
        return Some((
            fixed,
            format!(
                "`path` was missing; used `{last}`, the file you used last. Always pass `path`."
            ),
        ));
    }
    let path_error = [
        "path must be",
        "not found",
        "no such file",
        "does not exist",
    ]
    .iter()
    .any(|p| lower.contains(p));
    let given = args.get("path")?.as_str()?;
    if !path_error || known.iter().any(|k| k == given) {
        return None;
    }
    let mut near: Vec<(usize, &String)> = known
        .iter()
        .map(|k| (edit_distance(given, k), k))
        .filter(|(d, _)| *d <= 3)
        .collect();
    near.sort();
    let best = near.first()?;
    if near.get(1).is_some_and(|n| n.0 == best.0) {
        return None;
    }
    let mut fixed = args.clone();
    fixed["path"] = json!(best.1);
    Some((
        fixed,
        format!(
            "Path corrected from `{given}` to `{}` (the path you used earlier); use that one.",
            best.1
        ),
    ))
}

/// Levenshtein distance in chars.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// Run each read_file window and join their numbered texts into one result,
/// in the `provider_invoke_mcp` reply shape. The first failure fails all.
fn read_each_window(session_id: &str, windows: &[Value]) -> Result<Value, String> {
    let mut parts = Vec::new();
    for args in windows {
        let v = host::call_host(
            HostFn::ProviderInvokeMcp,
            &json!({ "session_id": session_id, "name": "read_file", "arguments": args }),
        )?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            return Ok(v);
        }
        let result = v.get("result").cloned().unwrap_or(Value::Null);
        parts.push(
            file_result_text("read_file", &result).unwrap_or_else(|| tool_result_text(&result)),
        );
    }
    Ok(json!({ "ok": true, "result": parts.join("\n\n") }))
}

/// The text of `path` through `read_file` windows, up to 6,000 lines.
fn read_back(session_id: &str, path: &str) -> Option<String> {
    const WINDOW: u64 = 600;
    let mut text = String::new();
    for w in 0..10 {
        let v = host::call_host(
            HostFn::ProviderInvokeMcp,
            &json!({
                "session_id": session_id,
                "name": "read_file",
                "arguments": { "path": path, "start_line": 1 + w * WINDOW, "line_count": WINDOW },
            }),
        )
        .ok()?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            return None;
        }
        let result = v.get("result")?;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(result.get("content")?.as_str()?);
        if result.get("has_more").and_then(Value::as_bool) != Some(true) {
            break;
        }
    }
    Some(text)
}

/// Tool Set "Core coding tools": names offered on top of Extra Tools. Only
/// those the session actually exposes are sent (e.g. `graphify_*` exist only
/// while that plugin is active). Research needs the whole web set:
/// `search_web` alone only returns links, and `fetch_web` / `parse_web`
/// store pages and return a reference read in parts with `web_get_part`, so
/// a page never lands in the context whole. `stop_background` /
/// `list_background` let it stop the servers it starts (seen: "server
/// stopped" reported when it had no way to stop one).
const CORE_TOOLS: &[&str] = &[
    "ask_user",
    "background_status",
    "browser_act",
    "browser_find",
    "browser_open",
    "browser_outline",
    "browser_screenshot",
    "edit_file",
    "fetch_url",
    "fetch_web",
    "file_outline",
    "finish_card",
    "git",
    "graphify_explain",
    "graphify_path",
    "list_background",
    "list_files",
    "move_card_to_done",
    "parse_web",
    "read_file",
    "read_symbol",
    "run_background",
    "run_command",
    "run_tests",
    "search_files",
    "search_web",
    "stop_background",
    "update_card",
    "web_get_part",
    "write_file",
];

/// Peckboard MCP tools as OpenAI `{"type":"function","function":{…}}` defs,
/// narrowed by the Tool Set setting and sorted by name. ~200 definitions
/// cost ~45k prompt tokens and make a small model pick tools badly; the
/// sort keeps the array byte-identical across requests for the prefix cache.
fn function_tools(session_id: &str) -> Option<Value> {
    let mcp = host::call_host(
        HostFn::ProviderGetMcpConfig,
        &json!({ "session_id": session_id }),
    )
    .ok()?;
    let allowed = settings::str("tool_set")
        .filter(|s| s == "core")
        .map(|_| settings::str_list("extra_tools"));
    let offered = |name: &str| match &allowed {
        None => true,
        Some(extra) => CORE_TOOLS.contains(&name) || extra.iter().any(|e| e == name),
    };
    let defs = mcp.get("tool_defs")?.as_array()?;
    let mut tools: Vec<(String, Value)> = defs
        .iter()
        .filter_map(|d| {
            let name = d.get("name")?.as_str()?.to_string();
            if name.is_empty() || !offered(&name) {
                return None;
            }
            let def = json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": d.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                    "parameters": d.get("input_schema").cloned().unwrap_or(json!({"type": "object"})),
                }
            });
            Some((name, def))
        })
        .collect();
    tools.sort_by(|a, b| a.0.cmp(&b.0));
    if tools.is_empty() {
        None
    } else {
        Some(Value::Array(tools.into_iter().map(|(_, d)| d).collect()))
    }
}

/// Reserve `count` consecutive ordinals for `key`, bumping the durable
/// counter BEFORE any id is emitted so a crash mid-round can never hand the
/// same id out twice.
fn alloc_tool_ids(key: &str, count: u64) -> u64 {
    let cur = host::call_host(
        HostFn::StoreGet,
        &json!({ "collection": COUNTER_COLLECTION, "key": key }),
    )
    .ok()
    .and_then(|v| v.get("value").and_then(|x| x.as_u64()))
    .unwrap_or(0);
    let _ = host::call_host(
        HostFn::StorePut,
        &json!({
            "collection": COUNTER_COLLECTION,
            "key": key,
            "data": cur + count,
        }),
    );
    cur
}

/// Run one round's tool calls, feeding each result back as a `tool` turn.
/// Images the tools return go on `ToolEnd` for the UI and into `images`
/// (with the tool's name) for the caller to show the model; each call's
/// outcome goes into `record.failures`. Returns `Ok(true)` when the user
/// interrupted between calls — the caller persists the partial exchange.
fn run_tool_calls(
    session_id: &str,
    calls: &[ToolCall],
    messages: &mut Vec<Value>,
    new_messages: &mut Vec<Value>,
    send_images: bool,
    images: &mut Vec<RoundImage>,
    record: &mut TurnRecord,
) -> Result<bool, String> {
    let base = alloc_tool_ids(session_id, calls.len() as u64);
    for (i, call) in calls.iter().enumerate() {
        if should_stop(session_id) {
            return Ok(true);
        }
        let tool_id = format!("{PROVIDER_ID}-tool-{}", base + i as u64);
        emit(
            session_id,
            &ProviderEvent::ToolStart {
                tool_use_id: tool_id.clone(),
                name: call.name.clone(),
                input: call.args.clone(),
            },
        )?;
        // What runs may differ from what was asked: CSS parens the model
        // dropped are closed first, and multi-window reads run one by one.
        let mut args = call.args.clone();
        dedupe_edit_ops(&call.name, &mut args);
        let repaired = repair_written_code(&call.name, &mut args);
        let invoke = |args: &Value| match read_windows(&call.name, args) {
            Some(windows) => read_each_window(session_id, &windows),
            None => host::call_host(
                HostFn::ProviderInvokeMcp,
                &json!({
                    "session_id": session_id,
                    "name": call.name,
                    "arguments": args,
                }),
            ),
        };
        if PAGE_READ_TOOLS.contains(&call.name.as_str()) {
            record.research_pending = false;
        }
        let gated =
            record.research_pending && matches!(call.name.as_str(), "write_file" | "edit_file");
        if matches!(
            call.name.as_str(),
            "write_file" | "edit_file" | "file_delete" | "run_command" | "run_background"
        ) {
            record.reads.clear();
        }
        let mut invoked = if gated {
            record.research_pending = false;
            Ok(json!({ "ok": false, "error": RESEARCH_GATE }))
        } else if call.name == "read_file" && record.note_read(&args) > MAX_UNCHANGED_READS {
            record.read_refusals += 1;
            Ok(json!({ "ok": false, "error": REREAD_REFUSED }))
        } else if call.name == "edit_file" && edit_is_noop(&args) {
            Ok(json!({ "ok": false, "error": NOOP_EDIT }))
        } else if is_copied_placeholder(&args) {
            Ok(json!({ "ok": false, "error": COPIED_PLACEHOLDER }))
        } else {
            invoke(&args)
        };
        // One retry with a mechanical fix for errors a local model loops on.
        let mut retried = None;
        if let Some(err) = invoke_error(&invoked)
            && let Some((fixed, note)) = retry_args(&call.name, &args, &err, &record.paths)
        {
            let again = invoke(&fixed);
            if invoke_error(&again).is_none() {
                invoked = again;
                args = fixed;
                retried = Some(note);
            }
        }
        // Most recent last.
        if invoke_error(&invoked).is_none()
            && let Some(p) = args.get("path").and_then(Value::as_str)
        {
            record.paths.retain(|k| k != p);
            record.paths.push(p.to_string());
        }
        let (content, ok, call_images) = match invoked {
            Ok(v) if v.get("ok").and_then(|o| o.as_bool()) == Some(true) => {
                let result = v.get("result").cloned().unwrap_or(Value::Null);
                let (rest, found) = take_tool_images(&result);
                let text = if found.is_empty() {
                    file_result_text(&call.name, &result)
                        .unwrap_or_else(|| tool_result_text(&result))
                } else if has_text(&rest) {
                    tool_result_text(&rest)
                } else {
                    found
                        .iter()
                        .map(|img| image_placeholder(img, send_images))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                (text, true, found)
            }
            Ok(v) => (
                v.get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("tool failed")
                    .to_string(),
                false,
                Vec::new(),
            ),
            Err(e) => (e, false, Vec::new()),
        };
        let content = if ok || call.notes.is_empty() {
            content
        } else {
            format!("{content}\n{}", call.notes.join("\n"))
        };
        // A web file just written gets its structure checked while it's
        // fresh; the model then fixes it before judging a screenshot.
        let notes: Vec<String> = [
            retried,
            ignored_note(&call.name, &call.ignored),
            (!ok && call.name.starts_with("browser_") && browser_unavailable(&content))
                .then(|| BROWSER_DOWN_HINT.to_string()),
            (!ok && call.name == "search_web" && search_rate_limited(&content))
                .then(|| SEARCH_LIMITED_HINT.to_string()),
            ok.then_some(repaired).flatten(),
            ok.then(|| written_file_note(session_id, &call.name, &args))
                .flatten(),
            (ok && call.name == "read_file")
                .then(|| read_file_note(session_id, &args))
                .flatten(),
            ok.then(|| tool_hint(&call.name))
                .flatten()
                .map(str::to_string),
        ]
        .into_iter()
        .flatten()
        .collect();
        let content = if notes.is_empty() {
            content
        } else {
            format!("{content}\n\n{}", notes.join("\n\n"))
        };
        emit(
            session_id,
            &ProviderEvent::ToolEnd {
                tool_use_id: tool_id,
                output: if ok { Some(content.clone()) } else { None },
                error: if ok { None } else { Some(content.clone()) },
                images: call_images.clone(),
            },
        )?;
        record_outcome(&mut record.failures, &call.name, ok, &content);
        let (msg, new_msg) = (messages.len(), new_messages.len());
        images.extend(call_images.into_iter().map(|img| RoundImage {
            tool: call.name.clone(),
            img,
            msg,
            new_msg,
        }));
        // The UI keeps the full output; the model gets a capped copy so one
        // huge result can't blow the context window for every later round.
        let tool_msg = json!({
            "role": "tool",
            "tool_call_id": call.wire_id,
            "content": cap_tool_result(&content, settings::max_tool_result_chars()),
        });
        messages.push(tool_msg.clone());
        new_messages.push(tool_msg);
    }
    Ok(false)
}

/// Split the images out of a tool result. Core handlers embed one as
/// `_image_base64` (+ `_image_mime`, PNG by default) — the `/mcp` route
/// turns that into an MCP image block, but `invoke_mcp` hands back the raw
/// value — and plugin tools return MCP `{"type":"image","data","mimeType"}`
/// content parts. Returns the result without them, and the images.
fn take_tool_images(result: &Value) -> (Value, Vec<ToolImage>) {
    let mut images = Vec::new();
    let Some(obj) = result.as_object() else {
        return (result.clone(), images);
    };
    let mut rest = obj.clone();
    if let Some(data) = rest.remove("_image_base64") {
        let mime = rest
            .remove("_image_mime")
            .and_then(|m| m.as_str().map(str::to_string))
            .unwrap_or_else(|| "image/png".into());
        if let Some(data) = data.as_str().filter(|d| !d.is_empty()) {
            images.push(ToolImage {
                mime_type: mime,
                data_base64: data.to_string(),
            });
        }
    }
    if let Some(parts) = rest.get_mut("content").and_then(Value::as_array_mut) {
        parts.retain(|p| {
            if p.get("type").and_then(|t| t.as_str()) != Some("image") {
                return true;
            }
            if let Some(data) = p.get("data").and_then(|d| d.as_str()) {
                let mime = p.get("mimeType").and_then(|m| m.as_str());
                images.push(ToolImage {
                    mime_type: mime.unwrap_or("image/png").to_string(),
                    data_base64: data.to_string(),
                });
            }
            false
        });
    }
    (Value::Object(rest), images)
}

/// Whether a tool result with its images taken out still says anything: a
/// text content part, or any other field.
fn has_text(rest: &Value) -> bool {
    match rest {
        Value::Object(obj) => match obj.get("content").and_then(|c| c.as_array()) {
            Some(parts) => parts
                .iter()
                .any(|p| p.get("type").and_then(|t| t.as_str()) == Some("text")),
            None => !obj.is_empty(),
        },
        Value::Null => false,
        _ => true,
    }
}

/// The `tool` text standing in for an image-only result, e.g.
/// The `tool` text standing in for an image-only result, e.g.
/// `[image: image/png, 1280x800 screenshot attached below]`.
fn image_placeholder(img: &ToolImage, attached: bool) -> String {
    let mut desc = img.mime_type.clone();
    if let Some((w, h)) = png_dimensions(&img.data_base64) {
        desc.push_str(&format!(", {w}x{h}"));
    }
    if attached {
        desc.push_str(" screenshot attached below");
    } else {
        // Without this the model reports on screenshots it never saw.
        desc.push_str(" screenshot, not shown to you");
    }
    format!("[image: {desc}]")
}

/// The placeholder for an image left out by the per-request image cap.
/// Seen: a desktop and a phone screenshot taken in one reply, both labelled
/// "attached below" while only the newest was shown, then both retaken
/// together — and the desktop one missed again.
fn over_cap_placeholder(img: &ToolImage) -> String {
    let unseen = image_placeholder(img, false);
    format!(
        "{}: only one image reaches you per reply. To see it, take it again on its own \
         (one browser_screenshot per reply)]",
        unseen.trim_end_matches(']')
    )
}

/// Width and height from a base64 PNG's IHDR chunk (the first 24 bytes, so
/// only the first 32 base64 chars are decoded). `None` for anything else.
fn png_dimensions(data_base64: &str) -> Option<(u32, u32)> {
    let mut bytes = Vec::with_capacity(24);
    let (mut acc, mut bits) = (0u32, 0);
    for c in data_base64.bytes().take(32) {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        acc = ((acc << 6) | u32::from(v)) & 0xFFFF;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((acc >> bits) as u8);
        }
    }
    if bytes.len() < 24 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let be = |i: usize| u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    Some((be(16), be(20)))
}

/// A tool image from this round, with where its call's `tool` message sits
/// in the request messages and in the turn's new (persisted) messages.
struct RoundImage {
    tool: String,
    img: ToolImage,
    msg: usize,
    new_msg: usize,
}

/// The round's tool images as one `user` message for the model — the
/// `MAX_TOOL_IMAGES_PER_ROUND` most recent, within `MAX_TOOL_IMAGE_BYTES` of
/// base64 — plus the text-only stub that replaces it in history (old turns
/// stay small and the prompt prefix stable) and the indices of the images
/// picked. `None` when nothing fits.
fn tool_image_message(images: &[RoundImage]) -> Option<(Value, Value, Vec<usize>)> {
    let mut picked: Vec<usize> = Vec::new();
    let mut total = 0;
    for (i, entry) in images.iter().enumerate().rev() {
        if picked.len() == MAX_TOOL_IMAGES_PER_ROUND {
            break;
        }
        let size = entry.img.data_base64.len();
        if total + size > MAX_TOOL_IMAGE_BYTES {
            continue;
        }
        total += size;
        picked.push(i);
    }
    if picked.is_empty() {
        return None;
    }
    picked.reverse();
    let mut names: Vec<&str> = Vec::new();
    for &i in &picked {
        let name = images[i].tool.as_str();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let names = names.join(", ");
    // A bare "here is the screenshot" gets a lenient "looks good" from
    // local models even when the layout is visibly broken (seen: badges and
    // avatars unstyled after a CSS syntax slip, reported as fine). A generic
    // "list defects" still let a phone layout that overflowed its right edge
    // (Upload button and badges cut in half) pass as "all chips fit": walk
    // the model through concrete checks, edges first, before any verdict.
    let mut parts = vec![json!({
        "type": "text",
        "text": format!(
            "Screenshot(s) returned by {names}. If this shows UI you are building, inspect it \
             critically before deciding anything. Go through these checks and list every defect \
             you find:\n\
             1. Edges: look along the right, left and bottom edges. Is any text, button, badge \
             or card cut off? That means the page overflows its viewport.\n\
             2. Overlap and placement: elements on top of each other, misaligned or in the \
             wrong place.\n\
             3. Styling: unstyled, broken or invisible parts; text too low-contrast to read.\n\
             4. Completeness: items or layout the request asked for that are missing or wrong.\n\
             Fix each defect and take another screenshot. Don't call it done while any defect \
             is visible, and claim only what this screenshot shows."
        ),
    })];
    parts.extend(picked.iter().map(|&i| {
        let img = &images[i].img;
        json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{};base64,{}", img.mime_type, img.data_base64) },
        })
    }));
    let stub = json!({
        "role": "user",
        "content": format!("[{} screenshot(s) from {names} were shown here]", picked.len()),
    });
    Some((json!({ "role": "user", "content": parts }), stub, picked))
}

/// Whether a 400 body is the server refusing image input (a text-only
/// model, or a server without multimodal support).
fn is_image_rejection(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    ["image", "vision", "multimodal"]
        .iter()
        .any(|p| b.contains(p))
}

/// A tool result as the plain text the model reads: a string as-is, an
/// MCP-shaped `{"content":[{"type":"text","text":…}]}` as its text parts
/// joined, anything else (core tools return JSON objects) as pretty JSON.
/// `Value::to_string` would hand the model a JSON-quoted string with
/// escaped newlines.
fn tool_result_text(result: &Value) -> String {
    if let Value::String(s) = result {
        return s.clone();
    }
    if let Some(parts) = result.get("content").and_then(|c| c.as_array()) {
        let texts: Vec<&str> = parts
            .iter()
            .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect();
        if !texts.is_empty() {
            return texts.join("\n");
        }
    }
    serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string())
}

/// `read_file` / `read_symbol` results as numbered lines under a one-line
/// header, instead of pretty JSON. As JSON the whole file is one string of
/// `\n` and `\"` escapes with no line numbers, and edit_file needs line
/// numbers: the model counted escaped newlines in its reasoning (seen: 12–15k
/// chars of reasoning per round and the same 222 lines re-read four times
/// before a single edit). `None` for any other result shape.
fn file_result_text(tool: &str, result: &Value) -> Option<String> {
    let numbered = |content: &str, start: u64| -> String {
        content
            .lines()
            .enumerate()
            .map(|(i, l)| format!("{:>5}| {l}", start + i as u64))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let header = |obj: &serde_json::Map<String, Value>, skip: &[&str]| -> String {
        obj.iter()
            .filter(|(k, v)| !skip.contains(&k.as_str()) && !v.is_array() && !v.is_object())
            .map(|(k, v)| match v {
                Value::String(s) => format!("{k}: {s}"),
                other => format!("{k}: {other}"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    const NOTE: &str = "(line numbers and `| ` are not part of the file)";
    let obj = result.as_object()?;
    match tool {
        "read_file" => {
            let content = obj.get("content")?.as_str()?;
            let start = obj.get("start_line").and_then(Value::as_u64).unwrap_or(1);
            Some(format!(
                "{} {NOTE}\n{}",
                header(obj, &["content"]),
                numbered(content, start)
            ))
        }
        "read_symbol" => {
            let symbols = obj.get("symbols")?.as_array()?;
            let mut out = vec![format!("{} {NOTE}", header(obj, &[]))];
            for s in symbols {
                let s = s.as_object()?;
                let content = s.get("content")?.as_str()?;
                let start = s.get("start_line").and_then(Value::as_u64).unwrap_or(1);
                out.push(format!(
                    "\n{}\n{}",
                    header(s, &["content"]),
                    numbered(content, start)
                ));
            }
            Some(out.join("\n"))
        }
        _ => None,
    }
}

/// `text` cut to about `max` chars — the head and the tail, with a marker
/// in the middle saying how much was dropped and how to see it. Unchanged
/// when it fits or there's no cap.
fn cap_tool_result(text: &str, max: Option<usize>) -> String {
    let Some(max) = max else {
        return text.to_string();
    };
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    let head = max / 2;
    let tail = max - head;
    let omitted = total - head - tail;
    let byte_at = |n: usize| text.char_indices().nth(n).map_or(text.len(), |(i, _)| i);
    format!(
        "{}\n\n[… {} chars omitted: narrow the request (line ranges, filters, tail) to see more …]\n\n{}",
        &text[..byte_at(head)],
        thousands(omitted),
        &text[byte_at(total - tail)..],
    )
}

/// `183402` → `183,402`.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn load_history(session_id: &str) -> Vec<Value> {
    let v = host::call_host(
        HostFn::StoreGet,
        &json!({ "collection": HISTORY_COLLECTION, "key": session_id }),
    )
    .ok();
    v.and_then(|v| v.get("value").cloned())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

/// The request transcript: the CURRENT system prompt (never persisted),
/// the stored history, then this turn's generated messages.
fn request_messages(system_prompt: &str, history: &[Value], new_messages: &[Value]) -> Vec<Value> {
    let mut messages = Vec::with_capacity(history.len() + new_messages.len() + 1);
    if !system_prompt.trim().is_empty() {
        messages.push(json!({ "role": "system", "content": system_prompt }));
    }
    messages.extend_from_slice(history);
    messages.extend_from_slice(new_messages);
    messages
}

/// Whether an error body is the server saying the prompt exceeds the
/// model's context window (vLLM, OpenAI, llama.cpp, Anthropic-style
/// proxies all word it differently).
fn is_context_overflow(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    [
        "maximum context length",
        "context_length_exceeded",
        "prompt is too long",
        "reduce the length",
    ]
    .iter()
    .any(|p| b.contains(p))
}

/// Drop about half of the turns before the current user message (always
/// the last entry), realigned to a user-message boundary. `false` when
/// there was nothing older to drop.
fn trim_older_turns(history: &mut Vec<Value>) -> bool {
    let older = history.len().saturating_sub(1);
    if older == 0 {
        return false;
    }
    history.drain(0..older.div_ceil(2));
    while history.len() > 1
        && history
            .first()
            .and_then(|m| m.get("role").and_then(|r| r.as_str()))
            != Some("user")
    {
        history.remove(0);
    }
    true
}

/// Rough token count of stored messages (~3 chars per token, JSON included),
/// for the history budget. Errs high, which only trims a little early.
fn estimate_tokens(messages: &[Value]) -> usize {
    messages.iter().map(message_chars).sum::<usize>() / 3
}

/// A message's size for the estimates, with each attached image counted as
/// `IMAGE_CHARS` instead of its base64 (a screenshot's data URL alone would
/// read as ~100k tokens).
fn message_chars(m: &Value) -> usize {
    const IMAGE_CHARS: usize = 3_000;
    match m.get("content").and_then(Value::as_array) {
        Some(parts) => {
            let images = parts.iter().filter(|p| p["type"] == "image_url").count();
            let text: usize = parts
                .iter()
                .filter(|p| p["type"] != "image_url")
                .map(|p| p.to_string().len())
                .sum();
            text + images * IMAGE_CHARS + 32
        }
        None => m.to_string().len(),
    }
}

/// Tool rounds at the end of a turn that compaction leaves verbatim.
const KEEP_RECENT_ROUNDS: usize = 4;
/// Compaction keeps recent rounds verbatim up to about this size, however many
/// rounds that is (at least `KEEP_RECENT_ROUNDS`): the model's working memory.
const KEEP_RECENT_TOKENS: usize = 24_000;
/// Longest tool result kept in a compacted round (its start is kept).
const COMPACT_RESULT_CHARS: usize = 600;
const ELIDED_MARK: &str = "elided to save context";
/// Refusal for a call whose arguments carry a compaction note (seen:
/// `"edits": "[1 lines, 4218 chars elided to save context; …]"`).
const COPIED_PLACEHOLDER: &str = "Not run: these arguments contain a history note (\"elided to save \
     context\"), not real content. Earlier calls were shortened in your history; write the actual \
     content again (re-read the file first if you need it).";

/// Whether an argument is a compaction note and nothing else. Whole values
/// only: a real file that mentions the phrase (this one, say) must still be
/// writable.
fn is_copied_placeholder(args: &Value) -> bool {
    args.as_object().is_some_and(|o| {
        o.values().filter_map(Value::as_str).any(|s| {
            let s = s.trim();
            s.len() < 300 && s.starts_with('[') && s.ends_with(']') && s.contains(ELIDED_MARK)
        })
    })
}

/// Compact this turn's older rounds in place, in both the request
/// (`messages`, whose last `new_messages.len()` entries are this turn's) and
/// the turn's persisted messages. Every round before the last
/// `KEEP_RECENT_ROUNDS` becomes plain text: the assistant keeps only what it
/// said, and each result becomes a short `user` note saying what was run and
/// what came back (same message count, so stored indices stay valid). A long
/// build is one turn of 50+ rounds, and without this every file the model
/// ever wrote and every result it ever read is resent on every round until
/// the context fills (and a local model gets worse long before that).
///
/// Text, not shortened calls: when old calls kept their shape with a
/// placeholder where the content was, the model copied them — the
/// placeholder sent as `edits` or as a whole file's `content`, three probes
/// running. Only the last rounds keep real calls, and those are genuine.
/// Only called past the Compact Long Turns budget: rewriting old rounds
/// costs a prefix-cache miss from that point on. Returns how many messages
/// changed.
fn compact_turn(messages: &mut [Value], new_messages: &mut [Value]) -> usize {
    let start = messages.len().saturating_sub(new_messages.len());
    let assistants: Vec<usize> = (0..new_messages.len())
        .filter(|&k| new_messages[k]["role"] == "assistant")
        .collect();
    if assistants.len() <= KEEP_RECENT_ROUNDS {
        return 0;
    }
    let mut keep_from = new_messages.len();
    for (rounds, &at) in assistants.iter().rev().enumerate() {
        keep_from = at;
        if rounds + 1 >= KEEP_RECENT_ROUNDS
            && estimate_tokens(&new_messages[at..]) >= KEEP_RECENT_TOKENS
        {
            break;
        }
    }
    let mut names: Vec<(String, String)> = Vec::new();
    let mut changed = 0;
    for k in 0..keep_from {
        let mut hit = false;
        for m in [&mut messages[start + k], &mut new_messages[k]] {
            hit |= compact_message(m, &mut names);
        }
        changed += usize::from(hit);
    }
    changed
}

/// The few arguments worth keeping in a summary of an old call.
fn call_brief(name: &str, args: &serde_json::Map<String, Value>) -> String {
    let short = |v: &Value| {
        let s = match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let s: String = s.chars().take(120).collect();
        s.replace('\n', " ")
    };
    let mut parts: Vec<String> = Vec::new();
    for key in [
        "path",
        "command",
        "args",
        "url",
        "query",
        "start_line",
        "line_count",
        "action",
    ] {
        if let Some(v) = args.get(key) {
            parts.push(format!("{key}={}", short(v)));
        }
    }
    for (key, unit) in [("content", "content"), ("edits", "edit ops")] {
        match args.get(key) {
            Some(Value::String(s)) => parts.push(format!("{} lines of {unit}", s.lines().count())),
            Some(Value::Array(a)) => parts.push(format!("{} {unit}", a.len())),
            _ => {}
        }
    }
    format!("{name} ({})", parts.join(", "))
}

/// Compact one message (see `compact_turn`); `true` if it changed. `names`
/// maps the call ids summarised so far to their summaries, for the notes
/// that replace their results. Summaries live in those `user` notes, never
/// in the assistant's own words: written there, the model continued the
/// pattern and sent a summary as its reply instead of a tool call.
fn compact_message(m: &mut Value, names: &mut Vec<(String, String)>) -> bool {
    let mut changed = false;
    match m["role"].as_str() {
        Some("assistant") => {
            let Some(obj) = m.as_object_mut() else {
                return false;
            };
            changed |= obj.remove("reasoning").is_some();
            changed |= obj.remove("reasoning_content").is_some();
            let Some(Value::Array(calls)) = obj.remove("tool_calls") else {
                return changed;
            };
            for call in &calls {
                let f = &call["function"];
                let name = f["name"].as_str().unwrap_or("tool").to_string();
                let args = f["arguments"]
                    .as_str()
                    .and_then(|r| serde_json::from_str::<Value>(r).ok())
                    .and_then(|v| v.as_object().cloned())
                    .unwrap_or_default();
                // A plan is the turn's architecture, and there is no tool to
                // read it back: its text stays in full.
                let brief = if name == "propose_plan" {
                    let plan = args.get("markdown").and_then(Value::as_str).unwrap_or("");
                    format!("propose_plan with this plan:\n{plan}\n")
                } else {
                    call_brief(&name, &args)
                };
                if let Some(id) = call["id"].as_str() {
                    names.push((id.to_string(), brief));
                }
            }
            let text = obj
                .get("content")
                .and_then(Value::as_str)
                .map_or("", str::trim)
                .to_string();
            let said = if text.is_empty() {
                "Continuing."
            } else {
                &text
            };
            obj.insert("content".into(), json!(said));
            changed = true;
        }
        Some("tool") => {
            let id = m["tool_call_id"].as_str().unwrap_or("").to_string();
            let brief = names
                .iter()
                .find(|(k, _)| *k == id)
                .map_or("a tool", |(_, b)| b.as_str())
                .to_string();
            let text = m["content"].as_str().unwrap_or("");
            let total = text.chars().count();
            let head: String = text.chars().take(COMPACT_RESULT_CHARS / 2).collect();
            let more = if total > COMPACT_RESULT_CHARS / 2 {
                format!(
                    " … ({} more chars dropped; run the tool again if you need them)",
                    thousands(total - COMPACT_RESULT_CHARS / 2)
                )
            } else {
                String::new()
            };
            *m = json!({
                "role": "user",
                "content": format!(
                    "[Earlier step, shortened to save context. You ran {brief}. Result: {}{more}]",
                    head.replace('\n', " ")
                ),
            });
            changed = true;
        }
        _ => {}
    }
    changed
}

/// Store the transcript, trimming it in place first so the caller keeps
/// exactly what was stored. Over `MAX_HISTORY_MESSAGES` it drops down to
/// `HISTORY_TRIM_TARGET` in one go (hysteresis), not just the overflow:
/// trimming one message per turn would change the prompt prefix every turn
/// and defeat the server's prefix cache, re-prefilling the whole context.
/// Over the Max History Tokens budget it trims the same way.
fn save_history(session_id: &str, messages: &mut Vec<Value>) {
    if messages.len() > MAX_HISTORY_MESSAGES {
        let drop = messages.len() - HISTORY_TRIM_TARGET;
        messages.drain(0..drop);
        // Align to a user-message boundary: a leading `tool` result (or an
        // assistant turn whose `tool_calls` were trimmed off the front) is
        // a dangling reference servers reject.
        while matches!(
            messages
                .first()
                .and_then(|m| m.get("role").and_then(|r| r.as_str())),
            Some("assistant") | Some("tool")
        ) {
            messages.remove(0);
        }
    }
    // Token budget too: local models get markedly worse past ~60-100k
    // tokens of context (seen: empty-argument tool calls at 111k), and 160
    // messages of tool output can be far more than that. Same hysteresis as
    // above — trim to 60% of the budget at a user-message boundary — so the
    // prefix stays stable for many turns. The newest message always stays.
    if let Some(budget) = settings::max_history_tokens()
        && estimate_tokens(messages) > budget
    {
        let target = budget * 3 / 5;
        while messages.len() > 1 && estimate_tokens(messages) > target {
            messages.remove(0);
            while messages.len() > 1
                && messages
                    .first()
                    .and_then(|m| m.get("role").and_then(|r| r.as_str()))
                    != Some("user")
            {
                messages.remove(0);
            }
        }
    }
    let _ = host::call_host(
        HostFn::StorePut,
        &json!({
            "collection": HISTORY_COLLECTION,
            "key": session_id,
            "data": messages,
        }),
    );
}

/// `(mime, base64)` for every image attachment on the message.
fn image_attachments(message: &Value) -> Vec<(String, String)> {
    let Some(atts) = message.get("attachments").and_then(|a| a.as_array()) else {
        return Vec::new();
    };
    atts.iter()
        .filter_map(|a| {
            let mime = a.get("mime_type")?.as_str()?;
            if !mime.starts_with("image/") {
                return None;
            }
            let data = a.get("data_base64")?.as_str()?;
            Some((mime.to_string(), data.to_string()))
        })
        .collect()
}

fn no_output(session_id: &str) -> Result<(), String> {
    crash(
        session_id,
        "OpenAI-compatible server closed the stream before producing output",
        CrashKind::NoOutput,
    )
}

fn crash(session_id: &str, reason: &str, kind: CrashKind) -> Result<(), String> {
    emit(
        session_id,
        &ProviderEvent::Crashed {
            reason: reason.into(),
            error_kind: kind,
            exit_code: None,
            stderr: None,
        },
    )
}

fn emit(session_id: &str, event: &ProviderEvent) -> Result<(), String> {
    let value = serde_json::to_value(event).map_err(|e| e.to_string())?;
    host::call_host(
        HostFn::EmitProviderEvent,
        &json!({ "session_id": session_id, "event": value }),
    )
    .map(|_| ())
}

fn should_stop(session_id: &str) -> bool {
    host::call_host(
        HostFn::ProviderShouldStop,
        &json!({ "session_id": session_id }),
    )
    .ok()
    .and_then(|v| v.get("stop").and_then(|s| s.as_bool()))
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// In-memory host double answering the subset of host functions a turn
    /// touches. Chat responses are served in order; the last one repeats.
    /// `stop_from` = the 1-based `should_stop` call number from which the
    /// answer flips to `true` (0 = never).
    struct FakeHost {
        store: Arc<Mutex<HashMap<(String, String), Value>>>,
        events: Arc<Mutex<Vec<Value>>>,
        chat_bodies: Arc<Mutex<Vec<Value>>>,
        chat_urls: Arc<Mutex<Vec<String>>>,
        invoked: Arc<Mutex<Vec<Value>>>,
        chat_responses: Vec<Value>,
        stop_from: usize,
        stop_calls: Arc<Mutex<usize>>,
        tool_defs: Option<Value>,
        /// Plugin settings by key; unset keys answer `null`.
        settings: HashMap<String, Value>,
        /// What every `invoke_mcp` call returns as `result`; an `_error`
        /// key makes every call fail with that error instead.
        tool_result: Value,
    }

    impl FakeHost {
        fn new(chat_responses: Vec<Value>) -> Self {
            FakeHost {
                store: Arc::default(),
                events: Arc::default(),
                chat_bodies: Arc::default(),
                chat_urls: Arc::default(),
                invoked: Arc::default(),
                chat_responses,
                stop_from: 0,
                stop_calls: Arc::default(),
                tool_defs: None,
                settings: HashMap::new(),
                tool_result: json!({ "files": ["a.rs"] }),
            }
        }

        fn host_fn(&self) -> peck_plugin_native_host::HostFn {
            let store = self.store.clone();
            let events = self.events.clone();
            let chat_bodies = self.chat_bodies.clone();
            let chat_urls = self.chat_urls.clone();
            let invoked = self.invoked.clone();
            let chat_responses = self.chat_responses.clone();
            let stop_from = self.stop_from;
            let stop_calls = self.stop_calls.clone();
            let tool_defs = self.tool_defs.clone();
            let settings = self.settings.clone();
            let tool_result = self.tool_result.clone();
            Arc::new(move |name: &str, input: &str| {
                let input: Value = serde_json::from_str(input).unwrap_or(json!({}));
                let key = |v: &Value| {
                    (
                        v.get("collection")
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string(),
                        v.get("key")
                            .and_then(|k| k.as_str())
                            .unwrap_or("")
                            .to_string(),
                    )
                };
                match name {
                    "peckboard_get_plugin_setting" => {
                        let k = input.get("key").and_then(|k| k.as_str()).unwrap_or("");
                        json!({ "value": settings.get(k) }).to_string()
                    }
                    "peckboard_store_get" => {
                        let v = store.lock().unwrap().get(&key(&input)).cloned();
                        json!({ "value": v }).to_string()
                    }
                    "peckboard_store_put" => {
                        let data = input.get("data").cloned().unwrap_or(Value::Null);
                        store.lock().unwrap().insert(key(&input), data);
                        json!({ "ok": true }).to_string()
                    }
                    "peckboard_emit_provider_event" => {
                        events
                            .lock()
                            .unwrap()
                            .push(input.get("event").cloned().unwrap_or(Value::Null));
                        json!({ "ok": true }).to_string()
                    }
                    "peckboard_provider_should_stop" => {
                        let mut n = stop_calls.lock().unwrap();
                        *n += 1;
                        json!({ "stop": stop_from != 0 && *n >= stop_from }).to_string()
                    }
                    "peckboard_provider_get_mcp_config" => match &tool_defs {
                        Some(defs) => json!({ "tool_defs": defs }).to_string(),
                        None => json!({ "error": "no mcp in test" }).to_string(),
                    },
                    "peckboard_provider_invoke_mcp" => {
                        invoked.lock().unwrap().push(input.clone());
                        match tool_result.get("_error") {
                            Some(err) => json!({ "ok": false, "error": err }).to_string(),
                            None => json!({ "ok": true, "result": tool_result }).to_string(),
                        }
                    }
                    "peckboard_http_request" => {
                        let url = input.get("url").and_then(|u| u.as_str()).unwrap_or("");
                        chat_urls.lock().unwrap().push(url.to_string());
                        let body = input.get("body").and_then(|b| b.as_str()).unwrap_or("{}");
                        let mut bodies = chat_bodies.lock().unwrap();
                        bodies.push(serde_json::from_str(body).unwrap_or(json!({})));
                        let i = (bodies.len() - 1).min(chat_responses.len() - 1);
                        chat_responses[i].to_string()
                    }
                    other => json!({ "error": format!("unexpected host fn {other}") }).to_string(),
                }
            })
        }

        fn saved_history(&self) -> Vec<Value> {
            self.store
                .lock()
                .unwrap()
                .get(&(HISTORY_COLLECTION.to_string(), "s1".to_string()))
                .unwrap()
                .as_array()
                .unwrap()
                .clone()
        }
    }

    fn completion(message: Value, prompt: i64, completion: i64) -> Value {
        finished(message, prompt, completion, "stop")
    }

    fn finished(message: Value, prompt: i64, completion: i64, finish_reason: &str) -> Value {
        json!({
            "status": 200,
            "body": json!({
                "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason }],
                "usage": { "prompt_tokens": prompt, "completion_tokens": completion },
            })
            .to_string(),
        })
    }

    fn payload() -> Value {
        json!({
            "session_id": "s1",
            "spawn_config": { "model": "openai-compat:Qwen/Qwen3-32B" },
            "message": { "text": "hello" },
            "system_prompt": "NEW PROMPT",
        })
    }

    #[test]
    fn turn_usage_sums_rounds_and_keeps_last_context() {
        let mut usage = TurnUsage::default();
        assert!(usage.is_empty());
        usage.add_round(100, 20);
        usage.add_round(240, 15);
        usage.add_round(0, 0);
        usage.add_round(500, 30);
        assert_eq!(usage.input, 840);
        assert_eq!(usage.output, 65);
        assert_eq!(usage.context, 500);
    }

    /// The system prompt is prepended FRESH each turn and never persisted:
    /// a legacy stored system message is stripped, the current prompt leads
    /// the outgoing request, and the saved transcript stays user-first.
    #[test]
    fn system_prompt_is_fresh_per_turn_and_never_persisted() {
        let host = FakeHost::new(vec![completion(
            json!({ "role": "assistant", "content": "answer", "reasoning_content": "hmm" }),
            50,
            7,
        )]);
        host.store.lock().unwrap().insert(
            (HISTORY_COLLECTION.to_string(), "s1".to_string()),
            json!([
                { "role": "system", "content": "OLD PROMPT" },
                { "role": "user", "content": "q1" },
                { "role": "assistant", "content": "a1" },
            ]),
        );
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let urls = host.chat_urls.lock().unwrap();
        assert_eq!(urls[0], "http://localhost:8000/v1/chat/completions");
        let bodies = host.chat_bodies.lock().unwrap();
        assert_eq!(bodies[0]["model"], "Qwen/Qwen3-32B");
        assert_eq!(bodies[0]["stream"], false);
        let msgs = bodies[0]["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "NEW PROMPT");
        assert_eq!(msgs[1]["content"], "q1");
        assert!(!bodies[0].to_string().contains("OLD PROMPT"));

        let saved = host.saved_history();
        assert_eq!(saved.first().unwrap()["role"], "user");
        assert!(!saved.iter().any(|m| m["role"] == "system"));
        assert_eq!(saved.last().unwrap()["content"], "answer");

        let events = host.events.lock().unwrap();
        let thinking = events.iter().find(|e| e["kind"] == "thinking").unwrap();
        assert_eq!(thinking["text"], "hmm");
        let usage = events.iter().find(|e| e["kind"] == "usage").unwrap();
        assert_eq!(usage["model"], "openai-compat:Qwen/Qwen3-32B");
        assert_eq!(usage["context_tokens"], 50);
        assert_eq!(usage["total_tokens"], 57);
        assert!(events.iter().any(|e| e["kind"] == "completed"));
    }

    /// An empty/unparseable 2xx body is a NoOutput crash, not a silent
    /// completion with an empty message.
    #[test]
    fn empty_response_crashes_with_no_output() {
        let host = FakeHost::new(vec![json!({ "status": 200, "body": "" })]);
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let events = host.events.lock().unwrap();
        let crashed = events.iter().find(|e| e["kind"] == "crashed").unwrap();
        assert_eq!(crashed["error_kind"], "no_output");
        assert!(!events.iter().any(|e| e["kind"] == "completed"));
    }
    /// The `default` seed placeholder with no Default Model configured is a
    /// clear crash, never a request for a model literally named `default`.
    #[test]
    fn default_placeholder_without_setting_crashes_clearly() {
        let host = FakeHost::new(vec![json!({ "status": 200, "body": "" })]);
        let mut p = payload();
        p["spawn_config"]["model"] = json!("openai-compat:default");
        peck_plugin_native_host::with_host(host.host_fn(), || run(&p)).unwrap();
        assert!(host.chat_bodies.lock().unwrap().is_empty());
        let events = host.events.lock().unwrap();
        let crashed = events.iter().find(|e| e["kind"] == "crashed").unwrap();
        assert!(
            crashed["reason"]
                .as_str()
                .unwrap()
                .contains("no model selected")
        );
    }

    /// A non-2xx answer surfaces status + the server's error message.
    #[test]
    fn http_error_reports_status_and_message() {
        let host = FakeHost::new(vec![json!({
            "status": 404,
            "body": json!({ "error": { "message": "model 'x' not found" } }).to_string(),
        })]);
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let events = host.events.lock().unwrap();
        let crashed = events.iter().find(|e| e["kind"] == "crashed").unwrap();
        assert_eq!(
            crashed["reason"],
            "openai-compat HTTP 404: model 'x' not found"
        );
    }

    /// A stop landing while the request was in flight persists the partial
    /// exchange with the interrupt marker and emits the accumulated usage.
    #[test]
    fn interrupt_persists_marked_partial_and_usage() {
        let mut host = FakeHost::new(vec![completion(
            json!({ "role": "assistant", "content": "half an answer" }),
            11,
            4,
        )]);
        // First should_stop (loop top) passes; second (post-response) stops.
        host.stop_from = 2;
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let events = host.events.lock().unwrap();
        let crashed = events.iter().find(|e| e["kind"] == "crashed").unwrap();
        assert_eq!(crashed["error_kind"], "interrupted");
        let usage = events.iter().find(|e| e["kind"] == "usage").unwrap();
        assert_eq!(usage["total_tokens"], 15);

        let saved = host.saved_history();
        let last = saved.last().unwrap();
        assert_eq!(last["role"], "assistant");
        assert_eq!(
            last["content"],
            "half an answer\n\n[Response interrupted by user]"
        );
    }

    /// tool_use_ids come from a durable per-session counter, so they never
    /// repeat across rounds or turns.
    #[test]
    fn tool_ids_are_unique_across_rounds_and_turns() {
        let host = FakeHost::new(vec![json!({ "status": 200, "body": "" })]);
        let (a, b) = peck_plugin_native_host::with_host(host.host_fn(), || {
            (alloc_tool_ids("s1", 3), alloc_tool_ids("s1", 2))
        });
        assert_eq!(a, 0);
        assert_eq!(b, 3);
    }

    /// One tool round in OpenAI shape: tools offered as functions, the
    /// string `arguments` parsed for the MCP call, and the follow-up request
    /// carries the assistant `tool_calls` plus a `tool` result echoing the
    /// server's call id.
    #[test]
    fn tool_call_round_translates_to_openai_messages() {
        let mut host = FakeHost::new(vec![
            completion(
                json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": { "name": "list_files", "arguments": "{\"path\":\"src\"}" },
                    }],
                }),
                30,
                5,
            ),
            completion(json!({ "role": "assistant", "content": "done" }), 60, 3),
        ]);
        host.tool_defs = Some(json!([{
            "name": "list_files",
            "description": "List files",
            "input_schema": { "type": "object", "properties": { "path": { "type": "string" } } },
        }]));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let bodies = host.chat_bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0]["tools"][0]["type"], "function");
        assert_eq!(bodies[0]["tools"][0]["function"]["name"], "list_files");

        let invoked = host.invoked.lock().unwrap();
        assert_eq!(invoked[0]["name"], "list_files");
        assert_eq!(invoked[0]["arguments"], json!({ "path": "src" }));

        let msgs = bodies[1]["messages"].as_array().unwrap();
        let assistant = &msgs[msgs.len() - 2];
        assert_eq!(assistant["role"], "assistant");
        let call = &assistant["tool_calls"][0];
        assert_eq!(call["id"], "call_abc");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "list_files");
        let args: Value =
            serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args, json!({ "path": "src" }));
        let tool = &msgs[msgs.len() - 1];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "call_abc");
        assert_eq!(tool["content"], "{\n  \"files\": [\n    \"a.rs\"\n  ]\n}");

        let events = host.events.lock().unwrap();
        let start = events.iter().find(|e| e["kind"] == "tool_start").unwrap();
        assert_eq!(start["tool_use_id"], "openai-compat-tool-0");
        assert_eq!(start["input"], json!({ "path": "src" }));
        let usage = events.iter().find(|e| e["kind"] == "usage").unwrap();
        assert_eq!(usage["input_tokens"], 90);
        assert_eq!(usage["context_tokens"], 60);
        assert!(events.iter().any(|e| e["kind"] == "completed"));
    }

    /// A 400 that blames tools drops them and retries the round once.
    #[test]
    fn tools_rejected_retries_without_tools() {
        let mut host = FakeHost::new(vec![
            json!({
                "status": 400,
                "body": json!({ "error": { "message": "\"auto\" tool choice requires --enable-auto-tool-choice" } }).to_string(),
            }),
            completion(json!({ "role": "assistant", "content": "ok" }), 5, 1),
        ]);
        host.tool_defs = Some(json!([{ "name": "t", "input_schema": { "type": "object" } }]));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let bodies = host.chat_bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        assert!(bodies[0].get("tools").is_some());
        assert!(bodies[1].get("tools").is_none());
        let events = host.events.lock().unwrap();
        assert!(events.iter().any(|e| e["kind"] == "completed"));
    }

    /// A model repeating the exact same call is told the repeat wasn't run
    /// from the 3rd round on, and stopped at `MAX_IDENTICAL_TOOL_ROUNDS`:
    /// the turn completes (not crashes) with a warning, and the stored
    /// history ends on an assistant message with no dangling `tool_calls`.
    #[test]
    fn repeated_identical_tool_calls_end_the_turn() {
        let same_call = |id: &str| {
            completion(
                json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": id,
                        "type": "function",
                        "function": { "name": "t", "arguments": "{\"a\":1}" },
                    }],
                }),
                10,
                1,
            )
        };
        let mut host = FakeHost::new((1..=5).map(|i| same_call(&format!("c{i}"))).collect());
        host.tool_defs = Some(json!([{ "name": "t", "input_schema": { "type": "object" } }]));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        // Rounds 1-2 run; 3-4 are answered "Not run" without running; the
        // 5th ends the turn.
        assert_eq!(host.chat_bodies.lock().unwrap().len(), 5);
        assert_eq!(host.invoked.lock().unwrap().len(), 2);
        let skipped = host.saved_history();
        let not_run: Vec<_> = skipped
            .iter()
            .filter(|m| {
                m["role"] == "tool" && m["content"].as_str().unwrap().starts_with("Not run")
            })
            .collect();
        assert_eq!(not_run.len(), 2);
        let events = host.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| e["kind"] == "system" && e["subtype"] == "warning")
        );
        assert!(events.iter().any(|e| e["kind"] == "completed"));
        assert!(!events.iter().any(|e| e["kind"] == "crashed"));
        drop(events);
        let saved = host.saved_history();
        let last = saved.last().unwrap();
        assert_eq!(last["role"], "assistant");
        assert!(last.get("tool_calls").is_none());
    }

    #[test]
    fn image_attachments_become_data_url_parts() {
        let m = user_message(
            "look",
            &json!({ "attachments": [
                { "mime_type": "image/png", "data_base64": "AAAA" },
                { "mime_type": "text/plain", "data_base64": "BBBB" },
            ] }),
        );
        let parts = m["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], json!({ "type": "text", "text": "look" }));
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    fn answer(content: &str) -> Value {
        completion(json!({ "role": "assistant", "content": content }), 10, 2)
    }

    /// Thinking: Auto sends `enable_thinking: true` for Qwen3 ids only, On
    /// and Off force it either way.
    #[test]
    fn thinking_kwarg_follows_mode_and_model() {
        for (mode, model, want) in [
            (None, "openai-compat:Qwen/Qwen3-32B", Some(true)),
            (Some("auto"), "openai-compat:llama-3.1-8b", None),
            (Some("on"), "openai-compat:llama-3.1-8b", Some(true)),
            (Some("off"), "openai-compat:Qwen/Qwen3-32B", Some(false)),
        ] {
            let mut host = FakeHost::new(vec![answer("ok")]);
            if let Some(m) = mode {
                host.settings.insert("thinking".into(), json!(m));
            }
            let mut p = payload();
            p["spawn_config"]["model"] = json!(model);
            peck_plugin_native_host::with_host(host.host_fn(), || run(&p)).unwrap();
            let body = &host.chat_bodies.lock().unwrap()[0];
            let got = body
                .get("chat_template_kwargs")
                .map(|k| k["enable_thinking"].as_bool().unwrap());
            assert_eq!(got, want, "mode {mode:?}, model {model}");
            // An empty Temperature gets Qwen's recommended value, for Qwen3 only.
            let temp = model
                .contains("Qwen3")
                .then(|| json!(if want == Some(false) { 0.7 } else { 0.6 }));
            assert_eq!(
                body.get("temperature").cloned(),
                temp,
                "mode {mode:?}, model {model}"
            );
            // Thinking rounds carry the Thinking Budget as max_tokens.
            let budget = (want == Some(true)).then(|| json!(8192));
            assert_eq!(body.get("max_tokens").cloned(), budget, "mode {mode:?}");
        }
    }

    /// Inline `reasoning</think>answer` content: the reasoning becomes a
    /// thinking event and only the answer is shown and persisted.
    #[test]
    fn inline_think_is_split_and_only_the_answer_persisted() {
        let host = FakeHost::new(vec![answer("weigh options\n</think>\n\nThe answer.")]);
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let events = host.events.lock().unwrap();
        let thinking = events.iter().find(|e| e["kind"] == "thinking").unwrap();
        assert_eq!(thinking["text"], "weigh options");
        let text = events.iter().find(|e| e["kind"] == "text").unwrap();
        assert_eq!(text["text"], "The answer.");
        drop(events);
        assert_eq!(
            host.saved_history().last().unwrap()["content"],
            "The answer."
        );

        assert_eq!(
            split_inline_think("<think>a</think>b"),
            (Some("a".into()), "b".into())
        );
        assert_eq!(
            split_inline_think("x</think>y</think>z"),
            (Some("x</think>y".into()), "z".into())
        );
        assert_eq!(split_inline_think("plain"), (None, "plain".into()));
        assert_eq!(split_inline_think("<think></think>hi"), (None, "hi".into()));
    }

    /// A tool round's reasoning is sent back on its assistant message for the
    /// rest of the turn (Qwen3 templates render `reasoning_content`), but the
    /// saved history keeps only the answer.
    #[test]
    fn in_turn_reasoning_is_replayed_but_not_persisted() {
        let mut host = FakeHost::new(vec![
            completion(
                json!({
                    "role": "assistant",
                    "content": "file is broken at line 40\n</think>\n\nChecking it.",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "list_files", "arguments": "{}" },
                    }],
                }),
                30,
                5,
            ),
            completion(json!({ "role": "assistant", "content": "done" }), 60, 3),
        ]);
        host.tool_defs = Some(json!([{
            "name": "list_files",
            "input_schema": { "type": "object", "properties": {} },
        }]));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let bodies = host.chat_bodies.lock().unwrap();
        // Every round gets the full Thinking Budget: the cap covers the
        // answer too, so a halved one cut file writes off (0.1.128).
        assert_eq!(bodies[0]["max_tokens"], 8192);
        assert_eq!(bodies[1]["max_tokens"], 8192);
        let msgs = bodies[1]["messages"].as_array().unwrap();
        let replayed = &msgs[msgs.len() - 2];
        assert_eq!(replayed["content"], "Checking it.");
        assert_eq!(replayed["reasoning"], "file is broken at line 40");
        assert_eq!(replayed["reasoning_content"], "file is broken at line 40");
        drop(bodies);
        let history = host.saved_history();
        assert!(
            history
                .iter()
                .all(|m| m.get("reasoning").is_none() && m.get("reasoning_content").is_none())
        );
    }

    /// Sampling settings ride the request only when set; an out-of-range
    /// float is dropped with a warning instead of 400ing every turn.
    #[test]
    fn sampling_settings_pass_through_when_set() {
        let mut host = FakeHost::new(vec![answer("ok")]);
        for (k, v) in [
            ("temperature", json!("5")),
            ("top_p", json!("0.95")),
            ("top_k", json!(20)),
            ("min_p", json!("0")),
            ("presence_penalty", json!(" 1.5 ")),
            ("max_tokens", json!(4096)),
        ] {
            host.settings.insert(k.into(), v);
        }
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let body = &host.chat_bodies.lock().unwrap()[0];
        // The unusable temperature falls back to the Qwen3 default.
        assert_eq!(body["temperature"], 0.6);
        assert_eq!(body["top_p"], 0.95);
        assert_eq!(body["top_k"], 20);
        assert_eq!(body["min_p"], 0.0);
        assert_eq!(body["presence_penalty"], 1.5);
        assert_eq!(body["max_tokens"], 4096);
        let events = host.events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| e["kind"] == "system"
                    && e["text"].as_str().unwrap().contains("temperature"))
        );
    }

    /// Crossing the cap trims well below it once, so the next turn's prompt
    /// starts with exactly the previous turn's messages (prefix-cache hit).
    #[test]
    fn history_trim_has_hysteresis_and_keeps_the_prefix_stable() {
        let host = FakeHost::new(vec![answer("ok")]);
        let seeded: Vec<Value> = (0..MAX_HISTORY_MESSAGES)
            .map(|i| {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                json!({ "role": role, "content": format!("m{i}") })
            })
            .collect();
        host.store.lock().unwrap().insert(
            (HISTORY_COLLECTION.to_string(), "s1".to_string()),
            Value::Array(seeded),
        );
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let saved = host.saved_history();
        assert!(saved.len() <= HISTORY_TRIM_TARGET, "len {}", saved.len());
        assert_eq!(saved[0]["role"], "user");

        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let bodies = host.chat_bodies.lock().unwrap();
        let first = bodies[0]["messages"].as_array().unwrap();
        let second = bodies[1]["messages"].as_array().unwrap();
        assert_eq!(&second[..first.len()], &first[..]);
    }

    /// Past Max History Tokens the history drops to ~60% of the budget at a
    /// user boundary, keeping the newest message.
    #[test]
    fn history_token_budget_trims_old_turns() {
        let mut host = FakeHost::new(vec![answer("ok")]);
        host.settings
            .insert("max_history_tokens".into(), json!(1000));
        // 20 turns of ~600 chars (~200 est. tokens) each: ~4000 tokens.
        let seeded: Vec<Value> = (0..20)
            .map(|i| {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                json!({ "role": role, "content": format!("m{i} {}", "x".repeat(560)) })
            })
            .collect();
        host.store.lock().unwrap().insert(
            (HISTORY_COLLECTION.to_string(), "s1".to_string()),
            Value::Array(seeded),
        );
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let saved = host.saved_history();
        assert_eq!(saved[0]["role"], "user");
        // Trimmed to ≤ 600 est. tokens before the turn, plus this turn's reply.
        assert!(
            estimate_tokens(&saved) <= 700,
            "{}",
            estimate_tokens(&saved)
        );
        assert_eq!(saved.last().unwrap()["content"], "ok");
    }

    #[test]
    fn tool_results_are_plain_text() {
        assert_eq!(tool_result_text(&json!("line 1\nline 2")), "line 1\nline 2");
        assert_eq!(
            tool_result_text(&json!({ "content": [
                { "type": "text", "text": "a\nb" },
                { "type": "image", "data": "AAAA" },
                { "type": "text", "text": "c" },
            ] })),
            "a\nb\nc"
        );
        assert_eq!(tool_result_text(&json!({ "n": 1 })), "{\n  \"n\": 1\n}");
    }

    /// A reply announcing a step it never took gets a persisted continue
    /// nudge, at most `MAX_CONTINUE_NUDGES` times per turn.
    #[test]
    fn announce_and_stop_is_nudged_at_most_twice() {
        let mut host = FakeHost::new(vec![answer("Found it.\nLet me check the video detail.")]);
        host.tool_defs = Some(json!([{ "name": "t", "input_schema": { "type": "object" } }]));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        assert_eq!(
            host.chat_bodies.lock().unwrap().len(),
            1 + MAX_CONTINUE_NUDGES
        );
        let saved = host.saved_history();
        let nudges = saved
            .iter()
            .filter(|m| m["content"] == CONTINUE_NUDGE)
            .count();
        assert_eq!(nudges, MAX_CONTINUE_NUDGES);
        assert_eq!(saved.last().unwrap()["role"], "assistant");
        assert!(
            host.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e["kind"] == "completed")
        );

        assert!(announces_unfinished_step(
            "Now I'll align the HTML to the CSS class names. Starting with the home page."
        ));
        assert!(announces_unfinished_step("Now I’ll run the tests."));
        assert!(announces_unfinished_step("Here is the plan:\n\n"));
        assert!(!announces_unfinished_step("Let me know if you need more."));
        assert!(!announces_unfinished_step("All done."));
    }

    /// A final answer after a tool call that failed and was never redone gets
    /// one check: a bare OK keeps the answer (check and OK dropped), anything
    /// else is the revised answer.
    #[test]
    fn failed_tool_calls_trigger_one_check_of_the_final_answer() {
        let call = completion(
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": { "name": "t", "arguments": "{}" },
                }],
            }),
            10,
            1,
        );
        let run_with = |reply: &str| {
            let mut host = FakeHost::new(vec![
                call.clone(),
                answer("Done. Verified at both widths."),
                answer(reply),
            ]);
            host.tool_defs = Some(json!([{ "name": "t", "input_schema": { "type": "object" } }]));
            host.tool_result =
                json!({ "_error": "page.screenshot: Timeout 30000ms exceeded.\nCall log:" });
            peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
            host
        };

        let host = run_with("**OK**");
        let bodies = host.chat_bodies.lock().unwrap().clone();
        assert_eq!(bodies.len(), 3);
        let check = bodies[2]["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(check["role"], "user");
        let check = check["content"].as_str().unwrap();
        assert!(check.contains("- t: page.screenshot: Timeout 30000ms exceeded."));
        assert!(!check.contains("Call log"));
        let saved = host.saved_history();
        assert_eq!(
            saved.last().unwrap()["content"],
            "Done. Verified at both widths."
        );
        assert!(!saved.iter().any(|m| m["content"].as_str() == Some(check)));
        assert!(
            !host
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e["text"] == "**OK**")
        );

        let host = run_with("Correction: the screenshot timed out, so nothing was verified.");
        assert_eq!(host.chat_bodies.lock().unwrap().len(), 3);
        let saved = host.saved_history();
        assert_eq!(
            saved.last().unwrap()["content"],
            "Correction: the screenshot timed out, so nothing was verified."
        );
        assert!(host.events.lock().unwrap().iter().any(|e| {
            e["kind"] == "system"
                && e["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("revised its final answer"))
        }));

        // A failure redone successfully needs no check.
        let mut failures = Vec::new();
        record_outcome(&mut failures, "t", false, "boom");
        record_outcome(&mut failures, "t", false, "boom again");
        assert_eq!(failures[0].times, 2);
        record_outcome(&mut failures, "t", true, "");
        assert!(failures.is_empty());
        assert!(is_bare_ok(" ok. ") && !is_bare_ok("OK, but the build failed."));
    }

    /// No nudge without tools: a chat-only model can't act on it.
    #[test]
    fn announce_without_tools_is_not_nudged() {
        let host = FakeHost::new(vec![answer("Let me think about that.")]);
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        assert_eq!(host.chat_bodies.lock().unwrap().len(), 1);
    }

    /// Real replies from a local agent that ended its turn without acting,
    /// next to finished answers that must not be nudged.
    #[test]
    fn announcement_detection_reads_the_last_sentence() {
        for stalled in [
            "Three questions, all read-only. Let me work through them.",
            "Let me answer all three.",
            "The video is still processing. Now let me check the worker logs.",
            "Next, I'll run the build:",
            "Okay, I'm going to restart the service.",
        ] {
            assert!(announces_unfinished_step(stalled), "{stalled}");
        }
        for done in [
            "The build passes: `Finished dev profile in 4.83s`.",
            "Fixed. Let me know if you want autoplay muted.",
            "Both thumbnails return 403, so the bucket still denies anonymous reads.",
        ] {
            assert!(!announces_unfinished_step(done), "{done}");
        }
    }

    /// Argument shapes a local model actually sent (run_command / git), each
    /// coerced to the schema's types instead of failing validation.
    #[test]
    fn tool_args_are_coerced_to_the_schema() {
        let schema = json!({"type": "object", "properties": {
            "command": {"type": "string"},
            "args": {"type": "array", "items": {"type": "string"}},
            "timeout_secs": {"type": "integer"},
            "verbose": {"type": "boolean"},
        }});
        let coerce = |mut args: Value| {
            coerce_args(&mut args, &schema);
            args
        };
        // A stringified array that is complete, and one cut short.
        assert_eq!(
            coerce(json!({"command": "python3", "args": "[\"-c\", \"print(1)\"]"})),
            json!({"command": "python3", "args": ["-c", "print(1)"]})
        );
        assert_eq!(
            coerce(json!({"args": "[\"-c\", \"print('x', fib(30)"}))["args"],
            json!(["-c", "print('x', fib(30)"])
        );
        // `args: -1` for `git log -1`, a numeric string, `""` for a number.
        assert_eq!(coerce(json!({"args": -1}))["args"], json!(["-1"]));
        assert_eq!(
            coerce(json!({"timeout_secs": "30"}))["timeout_secs"],
            json!(30)
        );
        assert_eq!(
            coerce(json!({"timeout_secs": "", "verbose": "true"})),
            json!({"verbose": true})
        );
        // Strings stay strings; plain text for a string list is split into
        // words (`args: "-la"`, seen three times in a row).
        assert_eq!(
            coerce(json!({"command": "[not json"}))["command"],
            json!("[not json")
        );
        assert_eq!(coerce(json!({"args": "-la"}))["args"], json!(["-la"]));
        assert_eq!(
            coerce(json!({"args": "-la tmp-probe"}))["args"],
            json!(["-la", "tmp-probe"])
        );
        // Arguments a tool doesn't take are named back to the model.
        let note = ignored_note("run_command", &["path".into()]).unwrap();
        assert!(note.contains("`path`") && note.contains("project folder"));
        assert!(ignored_note("read_file", &[]).is_none());
        assert!(browser_unavailable(
            "no usable port for the browser sidecar in 3111..3121"
        ));
        assert!(!browser_unavailable("page_id not found"));
        // Tool-call markup leaking into a string argument.
        assert_eq!(
            coerce(json!({"command": "frontend/page.tsx\n</parameter>\n>"}))["command"],
            json!("frontend/page.tsx")
        );
        // … and in front of one (Probe S: `path: "<parameter=search.html"`).
        assert_eq!(
            coerce(json!({"command": "<parameter=search.html"}))["command"],
            json!("search.html")
        );

        // XML and JSON mixed into one parameter (a real read_file call): the
        // value is cut and coerced, the JSON tail fills the missing keys.
        let read_file = json!({"type": "object", "properties": {
            "path": {"type": "string"},
            "start_line": {"type": "integer"},
            "line_count": {"type": "integer"},
        }});
        let mut args = json!({"line_count": "15\n</parameter>\n<=path>\nfrontend/src/app/watch/[videoId]/page.tsx\n</parameter>\n{\n  \"path\": \"frontend/src/app/watch/[videoId]/page.tsx\",\n  \"start_line\": 42,\n  \"line_count\": 15\n}"});
        coerce_args(&mut args, &read_file);
        assert_eq!(
            args,
            json!({
                "path": "frontend/src/app/watch/[videoId]/page.tsx",
                "start_line": 42,
                "line_count": 15,
            })
        );
        // `<parameter=NAME>` blocks in the leftover fill missing keys too.
        let mut args =
            json!({"path": "a.rs\n</parameter>\n<parameter=start_line>\n7\n</parameter>"});
        coerce_args(&mut args, &read_file);
        assert_eq!(args, json!({"path": "a.rs", "start_line": 7}));
        // Content that merely quotes the tags (source code for this very
        // parser, say) is not leaked markup and stays intact.
        let code = "const TAGS: &[&str] = &[\"</parameter>\", \"</function>\"];";
        assert_eq!(coerce(json!({"command": code}))["command"], json!(code));
        // A key with stray punctuation is renamed; a real key isn't clobbered.
        assert_eq!(
            coerce(json!({"=command": "ps", "args": ["aux"]})),
            json!({"command": "ps", "args": ["aux"]})
        );
        assert_eq!(
            coerce(json!({"=command": "x", "command": "ps"}))["command"],
            json!("ps")
        );
    }

    #[test]
    fn stringified_json_with_bare_quotes_is_repaired() {
        let schema = json!({"type": "object", "properties": {
            "path": {"type": "string"},
            "edits": {"type": "array", "items": {"type": "object"}},
        }});
        // Real edit_file calls from a probe: quotes inside the HTML left
        // bare, at the end and in the middle, a raw newline too.
        let want =
            "<input type=\"text\" aria-label=\"Search videos\" placeholder=\"Search videos…\">";
        for edits in [
            r#"[{"op": "update", "start_line": 295, "end_line": 295, "text": "<input type=\"text\" aria-label=\"Search videos\" placeholder=\"Search videos…">"}]"#,
            r#"[{"op": "update", "start_line": 295, "end_line": 295, "text": "<input type=\"text" aria-label=\"Search videos\" placeholder=\"Search videos…">"}]"#,
            "[\n  {\n    \"op\": \"update\",\n    \"text\": \"<input type=\"text\" aria-label=\"Search videos\" placeholder=\"Search videos…\">\"\n  }\n]",
        ] {
            let mut args = json!({ "path": "a.html", "edits": edits });
            let notes = coerce_args(&mut args, &schema);
            assert!(notes.is_empty(), "{edits}: {notes:?}");
            assert_eq!(args["edits"][0]["text"], want, "{edits}");
            assert_eq!(args["edits"][0]["op"], "update");
        }
        // Beyond repair: left as text, with a note on why for the model.
        let mut args = json!({ "edits": "[{\"op\": update}]" });
        let notes = coerce_args(&mut args, &schema);
        assert!(args["edits"].is_string());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].starts_with("`edits` arrived as text that isn't valid JSON"));
    }

    #[test]
    fn length_finish_warns_about_the_token_limit() {
        let host = FakeHost::new(vec![finished(
            json!({ "role": "assistant", "content": "partial" }),
            10,
            2,
            "length",
        )]);
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let events = host.events.lock().unwrap();
        assert!(events.iter().any(|e| e["kind"] == "system"
            && e["subtype"] == "warning"
            && e["text"].as_str().unwrap().contains("max-token limit")));
        assert!(events.iter().any(|e| e["kind"] == "completed"));
    }

    /// A thinking round capped at the Thinking Budget that stops mid-
    /// reasoning is re-asked with the reasoning closed off on a continued
    /// final assistant message and no cap; its answer is the round's reply.
    #[test]
    fn thinking_budget_forces_an_answer() {
        let mut host = FakeHost::new(vec![
            finished(
                json!({ "role": "assistant", "content": "plan the grid, the cards, the" }),
                10,
                8192,
                "length",
            ),
            completion(
                json!({ "role": "assistant", "content": "Wrote the page." }),
                20,
                5,
            ),
        ]);
        host.settings.insert("thinking".into(), json!("on"));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let bodies = host.chat_bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0]["max_tokens"], 8192);
        assert_eq!(bodies[1]["continue_final_message"], true);
        assert_eq!(bodies[1]["add_generation_prompt"], false);
        assert!(bodies[1].get("max_tokens").is_none());
        let msgs = bodies[1]["messages"].as_array().unwrap();
        let prefill = msgs.last().unwrap();
        assert_eq!(prefill["role"], "assistant");
        assert_eq!(prefill["reasoning"], "plan the grid, the cards, the");
        drop(bodies);

        assert_eq!(
            host.saved_history().last().unwrap()["content"],
            "Wrote the page."
        );
        let events = host.events.lock().unwrap();
        assert!(
            events.iter().any(|e| e["kind"] == "system"
                && e["text"].as_str().unwrap().contains("Thinking Budget"))
        );
        assert!(
            !events.iter().any(|e| e["kind"] == "system"
                && e["text"].as_str().unwrap().contains("max-token limit"))
        );
    }

    /// A capped round cut off inside a tool call's arguments is redone
    /// uncapped too; the cut call never runs.
    #[test]
    fn thinking_budget_redoes_a_cut_off_tool_call() {
        let call = |content: &str| {
            json!({
                "role": "assistant",
                "content": "",
                "reasoning": "write data.js",
                "tool_calls": [{ "id": "w", "type": "function", "function": {
                    "name": "write_file",
                    "arguments": json!({ "path": "d.js", "content": content }).to_string(),
                } }],
            })
        };
        let mut host = FakeHost::new(vec![
            // As vLLM reports it: the whole 8192-token cap used, yet
            // `tool_calls` because it parsed the cut call.
            finished(call("const V = [{ id: 1, t: \"cut"), 10, 8192, "tool_calls"),
            completion(call("const V = [{ id: 1 }];\n"), 20, 9),
            answer("done"),
        ]);
        host.tool_defs =
            Some(json!([{ "name": "write_file", "input_schema": { "type": "object" } }]));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        assert_eq!(
            host.chat_bodies.lock().unwrap()[1]["continue_final_message"],
            true
        );
        let invoked = host.invoked.lock().unwrap();
        assert_eq!(invoked.len(), 1);
        assert_eq!(
            invoked[0]["arguments"]["content"],
            "const V = [{ id: 1 }];\n"
        );
    }

    /// A tool call the server failed to parse (here the model wrote
    /// `<parameter=write_file>` where `<function=write_file>` belonged)
    /// arrives as text; it is recovered and run, and the raw markup is not
    /// shown as the reply.
    #[test]
    fn unparsed_inline_tool_call_is_recovered() {
        let mut host = FakeHost::new(vec![
            completion(
                json!({ "role": "assistant", "content": "Writing the page.\n\n<tool_call>\n<parameter=write_file>\n<parameter=path>\ntmp/ui/index.html\n</parameter>\n<parameter=content>\n<!doctype html>\n<p>hi</p>\n</parameter>\n</function>\n</tool_call>" }),
                30,
                5,
            ),
            completion(json!({ "role": "assistant", "content": "done" }), 60, 3),
        ]);
        host.tool_defs = Some(json!([{
            "name": "write_file",
            "input_schema": { "type": "object", "properties": {
                "path": { "type": "string" }, "content": { "type": "string" } } },
        }]));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let invoked = host.invoked.lock().unwrap();
        assert_eq!(invoked.len(), 1);
        assert_eq!(invoked[0]["name"], "write_file");
        assert_eq!(
            invoked[0]["arguments"],
            json!({ "path": "tmp/ui/index.html", "content": "<!doctype html>\n<p>hi</p>" })
        );
        drop(invoked);
        let events = host.events.lock().unwrap();
        let text = events.iter().find(|e| e["kind"] == "text").unwrap();
        assert_eq!(text["text"], "Writing the page.");

        // The JSON (hermes-style) body works too; unknown tools are left alone.
        let tools = json!([{ "type": "function", "function": { "name": "ls" } }]);
        let (rest, calls) = recover_inline_tool_calls(
            "<tool_call>{\"name\": \"ls\", \"arguments\": {\"path\": \".\"}}</tool_call>",
            Some(&tools),
        )
        .unwrap();
        assert_eq!(rest, "");
        assert_eq!(calls[0]["function"]["name"], "ls");
        assert!(
            recover_inline_tool_calls(
                "<tool_call><function=rm_rf></function></tool_call>",
                Some(&tools)
            )
            .is_none()
        );
    }

    /// An oversized tool result reaches the model (and history) as head +
    /// tail around an omission marker; the UI's ToolEnd keeps all of it.
    #[test]
    fn long_tool_results_are_capped_for_the_model_only() {
        let full = format!("HEAD{}TAIL", "x".repeat(50_000));
        let mut host = FakeHost::new(vec![
            completion(
                json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": { "name": "t", "arguments": "{}" },
                    }],
                }),
                10,
                1,
            ),
            answer("done"),
        ]);
        host.tool_defs = Some(json!([{ "name": "t", "input_schema": { "type": "object" } }]));
        host.tool_result = json!(full);
        host.settings
            .insert("max_tool_result_chars".into(), json!(1000));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let bodies = host.chat_bodies.lock().unwrap();
        let msgs = bodies[1]["messages"].as_array().unwrap();
        let sent = msgs.last().unwrap()["content"].as_str().unwrap();
        assert!(sent.starts_with("HEAD") && sent.ends_with("TAIL"));
        assert!(sent.contains("[… 49,008 chars omitted: narrow the request"));
        assert!(sent.chars().count() < 1200);
        let events = host.events.lock().unwrap();
        let end = events.iter().find(|e| e["kind"] == "tool_end").unwrap();
        assert_eq!(end["output"].as_str().unwrap().len(), full.len());
        drop(events);
        let saved = host.saved_history();
        let tool = saved.iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(tool["content"], sent);

        assert_eq!(cap_tool_result("ééééé", None), "ééééé");
        assert!(cap_tool_result(&"é".repeat(100), Some(10)).starts_with("ééééé\n"));
        assert_eq!(thousands(183_402), "183,402");
        assert_eq!(thousands(999), "999");
    }

    fn overflow() -> Value {
        json!({
            "status": 400,
            "body": json!({ "error": { "message": "This model's maximum context length is \
                262144 tokens. However, your prompt contains at least 262145 input tokens." } })
            .to_string(),
        })
    }

    fn seed_turns(host: &FakeHost, n: usize) {
        let seeded: Vec<Value> = (0..n)
            .map(|i| {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                json!({ "role": role, "content": format!("m{i}") })
            })
            .collect();
        host.store.lock().unwrap().insert(
            (HISTORY_COLLECTION.to_string(), "s1".to_string()),
            Value::Array(seeded),
        );
    }

    /// A context-length 400 trims about half the older turns, warns,
    /// persists the trimmed history and retries the round.
    #[test]
    fn context_overflow_trims_history_and_retries_once() {
        let host = FakeHost::new(vec![overflow(), answer("fits now")]);
        seed_turns(&host, 10);
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let bodies = host.chat_bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        let before = bodies[0]["messages"].as_array().unwrap().len();
        let after = bodies[1]["messages"].as_array().unwrap();
        assert!(
            after.len() <= before / 2 + 1,
            "{} -> {}",
            before,
            after.len()
        );
        assert_eq!(after[1]["role"], "user");
        assert_eq!(after.last().unwrap()["content"], "hello");
        let events = host.events.lock().unwrap();
        assert!(
            events.iter().any(|e| e["kind"] == "system"
                && e["text"].as_str().unwrap().starts_with("History trimmed"))
        );
        assert!(events.iter().any(|e| e["kind"] == "completed"));
        drop(events);
        let saved = host.saved_history();
        assert_eq!(saved[0]["role"], "user");
        assert_eq!(saved.last().unwrap()["content"], "fits now");
        assert!(saved.len() < 10);
    }

    /// Still overflowing after the one trim: crash as before.
    #[test]
    fn second_context_overflow_crashes() {
        let host = FakeHost::new(vec![overflow()]);
        seed_turns(&host, 10);
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        assert_eq!(host.chat_bodies.lock().unwrap().len(), 2);
        let events = host.events.lock().unwrap();
        let crashed = events.iter().find(|e| e["kind"] == "crashed").unwrap();
        assert!(
            crashed["reason"]
                .as_str()
                .unwrap()
                .starts_with("openai-compat HTTP 400")
        );
    }

    fn offered_tool_names(host: &FakeHost) -> Vec<String> {
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let bodies = host.chat_bodies.lock().unwrap();
        bodies[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_string())
            .collect()
    }

    /// Tools go out sorted by name in both modes; Core keeps only the
    /// allowlisted names the session has, plus Extra Tools.
    #[test]
    fn tool_set_filters_to_core_plus_extra_and_sorts() {
        let defs = json!([
            { "name": "write_file", "input_schema": { "type": "object" } },
            { "name": "create_card", "input_schema": { "type": "object" } },
            { "name": "list_views", "input_schema": { "type": "object" } },
            { "name": "git", "input_schema": { "type": "object" } },
            { "name": "read_file", "input_schema": { "type": "object" } },
            { "name": "web_get_part", "input_schema": { "type": "object" } },
            { "name": "stop_background", "input_schema": { "type": "object" } },
        ]);
        let mut all = FakeHost::new(vec![answer("ok")]);
        all.tool_defs = Some(defs.clone());
        assert_eq!(
            offered_tool_names(&all),
            [
                "create_card",
                "git",
                "list_views",
                "read_file",
                "stop_background",
                "web_get_part",
                "write_file"
            ]
        );

        let mut core = FakeHost::new(vec![answer("ok")]);
        core.tool_defs = Some(defs);
        core.settings.insert("tool_set".into(), json!("core"));
        core.settings
            .insert("extra_tools".into(), json!(["create_card", "not_a_tool"]));
        assert_eq!(
            offered_tool_names(&core),
            [
                "create_card",
                "git",
                "read_file",
                "stop_background",
                "web_get_part",
                "write_file"
            ]
        );
    }

    /// Base64 of a PNG signature + IHDR for a 1280x800 image: enough for
    /// `png_dimensions`, and stands in for a screenshot's bytes.
    const PNG_1280X800: &str = "iVBORw0KGgoAAAANSUhEUgAABQAAAAMg";

    /// A host whose model calls `browser_screenshot` once, then answers.
    fn screenshot_host(tool_result: Value) -> FakeHost {
        let mut host = FakeHost::new(vec![
            completion(
                json!({
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "call_shot",
                        "type": "function",
                        "function": { "name": "browser_screenshot", "arguments": "{}" },
                    }],
                }),
                10,
                2,
            ),
            answer("looks good"),
        ]);
        host.tool_defs = Some(json!([{
            "name": "browser_screenshot",
            "input_schema": { "type": "object" },
        }]));
        host.tool_result = tool_result;
        host
    }

    fn image_urls(message: &Value) -> Vec<String> {
        message["content"]
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter(|p| p["type"] == "image_url")
                    .map(|p| p["image_url"]["url"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A core screenshot (`_image_base64`) reaches the UI as a ToolEnd image,
    /// the model as one image user message after the `tool` turn, and the
    /// saved history only as a text stub.
    #[test]
    fn tool_images_reach_the_ui_and_the_model_but_not_history() {
        let host = screenshot_host(json!({ "_image_base64": PNG_1280X800 }));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let placeholder = "[image: image/png, 1280x800 screenshot attached below]";
        let events = host.events.lock().unwrap();
        let end = events.iter().find(|e| e["kind"] == "tool_end").unwrap();
        assert_eq!(end["images"][0]["mime_type"], "image/png");
        assert_eq!(end["images"][0]["data_base64"], PNG_1280X800);
        assert_eq!(end["output"], placeholder);

        let bodies = host.chat_bodies.lock().unwrap();
        let msgs = bodies[1]["messages"].as_array().unwrap();
        let tool = &msgs[msgs.len() - 2];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["content"], placeholder);
        let shot = &msgs[msgs.len() - 1];
        assert_eq!(shot["role"], "user");
        let ask = shot["content"][0]["text"].as_str().unwrap();
        assert!(ask.starts_with("Screenshot(s) returned by browser_screenshot."));
        assert!(ask.contains("list every defect") && ask.contains("1. Edges"));
        assert_eq!(
            image_urls(shot),
            [format!("data:image/png;base64,{PNG_1280X800}")]
        );

        let saved = host.saved_history();
        assert!(
            !Value::Array(saved.clone())
                .to_string()
                .contains(PNG_1280X800)
        );
        assert!(saved.iter().any(|m| m["role"] == "user"
            && m["content"] == "[1 screenshot(s) from browser_screenshot were shown here]"));
        assert_eq!(saved.last().unwrap()["content"], "looks good");
    }

    /// MCP image parts are extracted too, the text part stays the tool
    /// result, and only the newest image goes to the model.
    #[test]
    fn mcp_image_parts_are_capped_per_round() {
        let part = |data: &str| json!({ "type": "image", "data": data, "mimeType": "image/jpeg" });
        let host = screenshot_host(json!({ "content": [
            part("AAAA"),
            part("BBBB"),
            { "type": "text", "text": "captured 3 frames" },
            part("CCCC"),
        ] }));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let events = host.events.lock().unwrap();
        let end = events.iter().find(|e| e["kind"] == "tool_end").unwrap();
        assert_eq!(end["images"].as_array().unwrap().len(), 3);
        assert_eq!(end["output"], "captured 3 frames");

        let bodies = host.chat_bodies.lock().unwrap();
        let msgs = bodies[1]["messages"].as_array().unwrap();
        // One image per request (vLLM's default limit): the newest.
        assert_eq!(
            image_urls(&msgs[msgs.len() - 1]),
            ["data:image/jpeg;base64,CCCC"]
        );
    }

    /// Two screenshots in one reply: only the newest is attached, and the
    /// other's `tool` text says so and how to see it, instead of "attached
    /// below" (seen: desktop + phone retaken together, desktop missed again).
    #[test]
    fn screenshot_over_the_image_cap_says_to_retake_it_alone() {
        let call = |id: &str| {
            json!({
                "id": id,
                "type": "function",
                "function": { "name": "browser_screenshot", "arguments": "{}" },
            })
        };
        let mut host = FakeHost::new(vec![
            completion(
                json!({
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [call("desk"), call("phone")],
                }),
                10,
                2,
            ),
            answer("done"),
        ]);
        host.tool_defs = Some(json!([{
            "name": "browser_screenshot",
            "input_schema": { "type": "object" },
        }]));
        host.tool_result = json!({ "_image_base64": PNG_1280X800 });
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let bodies = host.chat_bodies.lock().unwrap();
        let msgs = bodies[1]["messages"].as_array().unwrap();
        let n = msgs.len();
        assert_eq!(image_urls(&msgs[n - 1]).len(), 1);
        let (desk, phone) = (&msgs[n - 3], &msgs[n - 2]);
        assert_eq!(desk["tool_call_id"], "desk");
        let unseen = desk["content"].as_str().unwrap();
        assert!(unseen.contains("not shown to you") && unseen.contains("on its own"));
        assert_eq!(
            phone["content"],
            "[image: image/png, 1280x800 screenshot attached below]"
        );
        drop(bodies);
        // History says the same, so a later turn doesn't think it saw it.
        let saved = host.saved_history();
        assert!(saved.iter().any(|m| m["tool_call_id"] == "desk"
            && m["content"].as_str().unwrap().contains("not shown to you")));
    }

    /// Send Tool Images off: the UI still gets the image, the model only the
    /// text placeholder.
    #[test]
    fn send_tool_images_off_keeps_the_model_text_only() {
        let mut host = screenshot_host(json!({ "_image_base64": PNG_1280X800 }));
        host.settings
            .insert("send_tool_images".into(), json!(false));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let events = host.events.lock().unwrap();
        let end = events.iter().find(|e| e["kind"] == "tool_end").unwrap();
        assert_eq!(end["images"].as_array().unwrap().len(), 1);
        let bodies = host.chat_bodies.lock().unwrap();
        assert!(!bodies[1].to_string().contains("image_url"));
        let msgs = bodies[1]["messages"].as_array().unwrap();
        let last = &msgs[msgs.len() - 1];
        assert_eq!(last["role"], "tool");
        assert_eq!(
            last["content"],
            "[image: image/png, 1280x800 screenshot, not shown to you]"
        );
    }

    /// A server that 400s image input gets the round retried once with the
    /// images dropped, and a warning pointing at Send Tool Images.
    #[test]
    fn image_rejected_retries_without_images() {
        let mut host = screenshot_host(json!({ "_image_base64": PNG_1280X800 }));
        let reply = host.chat_responses[1].clone();
        host.chat_responses = vec![
            host.chat_responses[0].clone(),
            json!({
                "status": 400,
                "body": json!({ "error": { "message": "This model does not support image input" } }).to_string(),
            }),
            reply,
        ];
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let bodies = host.chat_bodies.lock().unwrap();
        assert_eq!(bodies.len(), 3);
        assert!(bodies[1].to_string().contains("image_url"));
        assert!(!bodies[2].to_string().contains("image_url"));
        assert!(bodies[2].get("tools").is_some());
        // The rejected image is marked as never seen, in the request and in
        // the stored history.
        assert!(bodies[2].to_string().contains("could not be shown to you"));
        drop(bodies);
        assert!(host.saved_history().iter().any(|m| {
            m["content"]
                .as_str()
                .is_some_and(|c| c.contains("could not be shown to you"))
        }));
        let events = host.events.lock().unwrap();
        assert!(
            events.iter().any(|e| e["kind"] == "system"
                && e["text"].as_str().unwrap().contains("Send Tool Images"))
        );
        assert!(events.iter().any(|e| e["kind"] == "completed"));
    }

    /// Compaction turns all but the last 4 rounds into text: no tool calls
    /// left to imitate, results as short `user` notes, the same message
    /// count, in the request and in what gets persisted alike; history before
    /// the turn is untouched; a second pass changes nothing; a plan keeps
    /// its full text.
    #[test]
    fn compact_turn_turns_old_rounds_into_text() {
        // Small rounds stay verbatim: memory goes by size, not a fixed count.
        let mut small: Vec<Value> = (0..8)
            .flat_map(|i| {
                vec![
                    json!({ "role": "assistant", "content": "", "tool_calls": [{
                        "id": format!("s{i}"), "type": "function",
                        "function": { "name": "read_file", "arguments": "{}" },
                    }] }),
                    json!({ "role": "tool", "tool_call_id": format!("s{i}"), "content": "x" }),
                ]
            })
            .collect();
        let mut request = small.clone();
        assert_eq!(compact_turn(&mut request, &mut small), 0);

        let page = "<div>card</div>\n".repeat(1000);
        let round = |i: usize| {
            let args = json!({ "path": format!("p{i}.html"), "content": page }).to_string();
            vec![
                json!({
                    "role": "assistant",
                    "content": "",
                    "reasoning": "think",
                    "tool_calls": [{
                        "id": format!("c{i}"),
                        "type": "function",
                        "function": { "name": "write_file", "arguments": args },
                    }],
                }),
                json!({ "role": "tool", "tool_call_id": format!("c{i}"), "content": page }),
            ]
        };
        let mut new_messages: Vec<Value> = (0..6).flat_map(round).collect();
        let older = json!({ "role": "tool", "tool_call_id": "old", "content": page });
        let mut messages = vec![json!({ "role": "user", "content": "go" }), older.clone()];
        messages.extend(new_messages.clone());
        let (before, count) = (estimate_tokens(&messages), messages.len());

        // Rounds 0 and 1 compacted (2 messages each), rounds 2-5 kept.
        assert_eq!(compact_turn(&mut messages, &mut new_messages), 4);
        assert_eq!(messages.len(), count);
        assert!(estimate_tokens(&messages) + 1_500 < before);
        assert_eq!(messages[1], older);
        for (k, m) in new_messages.iter().enumerate() {
            assert_eq!(&messages[2 + k], m);
            let compacted = k < 4;
            let text = m["content"].as_str().unwrap();
            if k % 2 == 0 {
                assert_eq!(m.get("tool_calls").is_none(), compacted, "message {k}");
                assert_eq!(m.get("reasoning").is_none(), compacted, "message {k}");
                if compacted {
                    assert_eq!(text, "Continuing.", "message {k}");
                }
            } else {
                assert_eq!(
                    m["role"],
                    if compacted { "user" } else { "tool" },
                    "message {k}"
                );
                if compacted {
                    assert!(
                        text.contains(&format!(
                            "You ran write_file (path=p{}.html, 1000 lines of content)",
                            k / 2
                        )),
                        "{text}"
                    );
                    assert!(m.get("tool_call_id").is_none());
                }
            }
        }
        assert_eq!(compact_turn(&mut messages, &mut new_messages), 0);

        // A plan keeps its markdown: the model has no way to read it back.
        let plan = json!({ "markdown": "# Plan\n- index.html", "title": "Site" }).to_string();
        let mut planned = json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{ "id": "p", "type": "function",
                "function": { "name": "propose_plan", "arguments": plan } }],
        });
        let mut names = Vec::new();
        assert!(compact_message(&mut planned, &mut names));
        let mut result = json!({ "role": "tool", "tool_call_id": "p", "content": "saved" });
        assert!(compact_message(&mut result, &mut names));
        assert!(
            result["content"]
                .as_str()
                .unwrap()
                .contains("# Plan\n- index.html")
        );
    }

    /// read_file reaches the model as numbered lines under a header that
    /// keeps the hash and paging fields, not as one escaped JSON string;
    /// other tools keep the generic rendering.
    #[test]
    fn file_reads_are_numbered_lines() {
        let result = json!({
            "content": "<div class=\"a\">\n  x\n</div>",
            "hash": "abc123",
            "has_more": true,
            "path": "ui/index.html",
            "start_line": 41,
            "total_lines": 90,
        });
        let text = file_result_text("read_file", &result).unwrap();
        let mut lines = text.lines();
        let head = lines.next().unwrap();
        assert!(head.contains("hash: abc123") && head.contains("has_more: true"));
        assert!(head.contains("total_lines: 90") && !head.contains("content"));
        assert_eq!(
            lines.collect::<Vec<_>>(),
            ["   41| <div class=\"a\">", "   42|   x", "   43| </div>"]
        );

        let sym = json!({ "count": 1, "hash": "h", "symbols": [
            { "name": "f", "start_line": 7, "content": "fn f() {}" },
        ] });
        let text = file_result_text("read_symbol", &sym).unwrap();
        assert!(text.contains("name: f") && text.ends_with("    7| fn f() {}"));
        assert!(file_result_text("write_file", &result).is_none());
    }

    /// read_file with array windows becomes one read per window, paired
    /// with its own count or sharing a scalar one; a normal read is left as
    /// one call.
    #[test]
    fn multi_window_reads_split_per_window() {
        let args = json!({ "path": "a.css", "start_line": [34, 270], "line_count": [10, 12] });
        let w = read_windows("read_file", &args).unwrap();
        assert_eq!(
            w,
            [
                json!({ "path": "a.css", "start_line": 34, "line_count": 10 }),
                json!({ "path": "a.css", "start_line": 270, "line_count": 12 }),
            ]
        );
        let shared = json!({ "path": "a", "start_line": [1, 50], "line_count": 20 });
        assert_eq!(
            read_windows("read_file", &shared).unwrap()[1]["line_count"],
            20
        );
        assert!(read_windows("read_file", &json!({ "path": "a", "start_line": 3 })).is_none());
        assert!(read_windows("list_files", &args).is_none());
    }

    /// A search_web result carries the read-the-results hint into the
    /// model's tool message; other tools don't.
    #[test]
    fn search_results_carry_a_read_hint() {
        let mut host = FakeHost::new(vec![
            completion(
                json!({
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{ "id": "s", "type": "function",
                        "function": { "name": "search_web", "arguments": "{\"query\":\"yt\"}" } }],
                }),
                10,
                2,
            ),
            answer("ok"),
        ]);
        host.tool_defs =
            Some(json!([{ "name": "search_web", "input_schema": { "type": "object" } }]));
        host.tool_result = json!("1. YouTube - youtube.com");
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let bodies = host.chat_bodies.lock().unwrap();
        let msgs = bodies[1]["messages"].as_array().unwrap();
        let tool = msgs.iter().find(|m| m["role"] == "tool").unwrap();
        let text = tool["content"].as_str().unwrap();
        assert!(text.starts_with("1. YouTube") && text.contains("parse_web"));
        assert!(tool_hint("read_file").is_none());
    }

    /// Probe J's two loops get one mechanical retry: column-1 edits that
    /// overshoot the line become whole-line edits, and a mistyped path is
    /// replaced by the one close path used earlier. Ambiguous or unrelated
    /// errors get no retry.
    #[test]
    fn retry_args_fixes_overshooting_columns_and_mistyped_paths() {
        let edit = json!({ "path": "a.css", "edits": [
            { "op": "update", "start_line": 275, "end_line": 275, "start_column": 1,
              "end_column": 54, "text": "x" },
            { "op": "update", "start_line": 9, "end_line": 9, "start_column": 5,
              "end_column": 7, "text": "y" },
        ] });
        let err = "edit 1: column 54 is beyond the end of line 275 (52 characters)";
        let (fixed, note) = retry_args("edit_file", &edit, err, &[]).unwrap();
        assert!(fixed["edits"][0].get("start_column").is_none());
        assert!(fixed["edits"][0].get("end_column").is_none());
        assert_eq!(fixed["edits"][1]["start_column"], 5);
        assert!(note.contains("omit start_column"));

        let known = vec![
            "tmp-probe/yt1/css/pages.css".to_string(),
            "tmp-probe/yt1/index.html".into(),
        ];
        let bad = json!({ "path": "/tmp/probe/yt1/css/pages.css", "start_line": 40 });
        let err = "path must be relative and within the project folder";
        let (fixed, note) = retry_args("read_file", &bad, err, &known).unwrap();
        assert_eq!(fixed["path"], "tmp-probe/yt1/css/pages.css");
        assert!(note.contains("corrected"));
        let far = json!({ "path": "/etc/passwd" });
        assert!(retry_args("read_file", &far, err, &known).is_none());
        assert!(retry_args("read_file", &bad, "permission denied", &known).is_none());

        // No path at all on a hash-guarded edit: the most recent path.
        let pathless = json!({ "original_hash": "h", "edits": [] });
        let err = "`path` (project-relative) is required";
        let (fixed, _) = retry_args("edit_file", &pathless, err, &known).unwrap();
        assert_eq!(fixed["path"], "tmp-probe/yt1/index.html");

        let mut dup = json!({ "path": "a", "edits": [
            { "op": "delete", "start_line": 1, "end_line": 1 },
            { "op": "delete", "start_line": 1, "end_line": 1 },
            { "op": "delete", "start_line": 3, "end_line": 3 },
        ] });
        dedupe_edit_ops("edit_file", &mut dup);
        assert_eq!(dup["edits"].as_array().unwrap().len(), 2);
    }

    /// Asked to research, a file write before any web *page* was read is
    /// refused once, even after a search; the retry runs. Not asked: no gate.
    #[test]
    fn research_request_gates_the_first_write_until_the_web_is_used() {
        let tool = |id: &str, name: &str| {
            completion(
                json!({ "role": "assistant", "content": "", "tool_calls": [{
                    "id": id, "type": "function",
                    "function": { "name": name, "arguments": "{\"path\":\"a.txt\",\"query\":\"q\"}" },
                }] }),
                10,
                2,
            )
        };
        let defs = json!([
            { "name": "write_file", "input_schema": { "type": "object" } },
            { "name": "search_web", "input_schema": { "type": "object" } },
        ]);
        let mut host = FakeHost::new(vec![
            // A search alone isn't research: the write after it is still
            // refused once; the retry runs (the gate is one-shot).
            tool("s", "search_web"),
            tool("w1", "write_file"),
            tool("w2", "write_file"),
            answer("done"),
        ]);
        host.tool_defs = Some(defs.clone());
        let mut p = payload();
        p["message"]["text"] = json!("Research and plan before you build a site.");
        peck_plugin_native_host::with_host(host.host_fn(), || run(&p)).unwrap();
        let names: Vec<String> = host
            .invoked
            .lock()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["search_web", "write_file"]);
        assert!(
            host.saved_history()
                .iter()
                .any(|m| m["tool_call_id"] == "w1"
                    && m["content"].as_str().unwrap().contains("research first"))
        );

        let mut plain = FakeHost::new(vec![tool("w", "write_file"), answer("done")]);
        plain.tool_defs = Some(defs);
        peck_plugin_native_host::with_host(plain.host_fn(), || run(&payload())).unwrap();
        assert_eq!(plain.invoked.lock().unwrap().len(), 1);

        // Prose asks gate; paths and identifiers don't.
        for yes in [
            "Research and plan before you build.",
            "Do some research first, then build it",
            "look up how others do it",
            "Find inspiration on dribbble",
            "Do the research.",
        ] {
            assert!(asks_for_research(yes), "{yes}");
        }
        for no in [
            "fix the bug in research.py",
            "update ResearchPanel.tsx",
            "rename src/research/ to src/studies/",
            "add a research_id column",
            "the lookup table is wrong",
        ] {
            assert!(!asks_for_research(no), "{no}");
        }
    }

    /// A raw tool call cut off before `</function>` (Probe M: write_file
    /// content cut mid-line, `path` never sent) is not run; the model is
    /// asked to re-send it. A complete raw call still runs.
    #[test]
    fn cut_off_raw_tool_call_is_not_run_but_resent() {
        let cut =
            "<tool_call>\n<function=write_file>\n<parameter=content>\nbody { color: red; }\nimg {";
        let mut host = FakeHost::new(vec![
            answer(cut),
            answer(
                "<tool_call>\n<function=write_file>\n<parameter=path>\na.css\n</parameter>\n<parameter=content>\nimg { }\n</parameter>\n</function>\n</tool_call>",
            ),
            answer("done"),
        ]);
        host.tool_defs =
            Some(json!([{ "name": "write_file", "input_schema": { "type": "object" } }]));
        host.settings.insert("thinking".into(), json!("off"));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let invoked = host.invoked.lock().unwrap();
        assert_eq!(invoked.len(), 1);
        assert_eq!(invoked[0]["arguments"]["path"], "a.css");
        let bodies = host.chat_bodies.lock().unwrap();
        let second = bodies[1]["messages"].as_array().unwrap();
        assert_eq!(second.last().unwrap()["content"], CUT_CALL_NUDGE);
        assert!(has_cut_tool_call(cut) && !has_cut_tool_call("plain answer"));
    }

    /// Probe T's last reply was a tool call with its tags mangled, and the
    /// turn ended "complete" on it. It is re-sent instead; an answer that
    /// only quotes the tags still ends the turn.
    #[test]
    fn garbled_raw_tool_call_is_resent_not_taken_as_the_answer() {
        let garbled = "Let me read the rest.\n\n<tool_call>\n<=read_file>\n<parameter=line_count>\n187\n</parameter>\n=path>\nsite/channel.html\n</parameter>\n</function>\n</tool_call>";
        let mut host = FakeHost::new(vec![
            answer(garbled),
            answer(
                "<tool_call>\n<function=read_file>\n<parameter=path>\nsite/notes.txt\n</parameter>\n</function>\n</tool_call>",
            ),
            answer("done"),
        ]);
        host.tool_defs =
            Some(json!([{ "name": "read_file", "input_schema": { "type": "object" } }]));
        host.settings.insert("thinking".into(), json!("off"));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();

        let invoked = host.invoked.lock().unwrap();
        assert_eq!(invoked.len(), 1);
        assert_eq!(invoked[0]["arguments"]["path"], "site/notes.txt");
        let bodies = host.chat_bodies.lock().unwrap();
        let second = bodies[1]["messages"].as_array().unwrap();
        assert_eq!(second.last().unwrap()["content"], GARBLED_CALL_NUDGE);
        assert!(!has_garbled_tool_call(
            "The parser reads `<tool_call>` blocks and that is all."
        ));
    }

    /// Reading a web file that is broken shows its problems with the read
    /// (Probe O called a broken data.js "complete and balanced"); a
    /// rate-limited search gets the don't-retry hint.
    #[test]
    fn reads_of_broken_web_files_carry_their_problems() {
        let mut host = FakeHost::new(vec![
            completion(
                json!({ "role": "assistant", "content": "", "tool_calls": [{
                    "id": "r", "type": "function",
                    "function": { "name": "read_file", "arguments": "{\"path\":\"yt/js/data.js\"}" },
                }] }),
                10,
                2,
            ),
            answer("done"),
        ]);
        host.tool_defs =
            Some(json!([{ "name": "read_file", "input_schema": { "type": "object" } }]));
        host.tool_result =
            json!({ "content": "const A = [\n  { id: 1 },\n", "path": "yt/js/data.js" });
        host.settings.insert("thinking".into(), json!("off"));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let saved = host.saved_history();
        let read = saved.iter().find(|m| m["tool_call_id"] == "r").unwrap();
        let text = read["content"].as_str().unwrap();
        assert!(text.contains("currently has structure problems"), "{text}");
        assert!(text.contains("never closed"), "{text}");

        assert!(search_rate_limited(
            "web search returned HTTP 202 (DuckDuckGo may be rate-limiting; try again)"
        ));
        assert!(!search_rate_limited("dns resolution failed"));
    }

    /// A review loop too long for `repeat_run`: three reads per file window,
    /// then refusals, and the turn stops after `MAX_READ_REFUSALS`. A file
    /// change resets the count.
    #[test]
    fn unchanged_rereads_are_refused_then_end_the_turn() {
        let read = |p: &str| {
            answer(&format!(
                "<tool_call>\n<function=read_file>\n<parameter=path>\n{p}\n</parameter>\n</function>\n</tool_call>"
            ))
        };
        let mut replies = Vec::new();
        for _ in 0..20 {
            for i in 0..=MAX_REPEAT_PERIOD {
                replies.push(read(&format!("f{i}.txt")));
            }
        }
        let mut host = FakeHost::new(replies);
        host.tool_defs =
            Some(json!([{ "name": "read_file", "input_schema": { "type": "object" } }]));
        host.settings.insert("thinking".into(), json!("off"));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        let files = MAX_REPEAT_PERIOD + 1;
        assert_eq!(host.invoked.lock().unwrap().len(), files * 3);
        // Three reads of each file, then the refusals end the turn: no further
        // requests.
        assert_eq!(
            host.chat_bodies.lock().unwrap().len(),
            files * 3 + MAX_READ_REFUSALS
        );

        let mut record = TurnRecord::default();
        let args = json!({ "path": "a.txt" });
        assert_eq!(record.note_read(&args), 1);
        assert_eq!(record.note_read(&json!({ "path": "/a.txt" })), 2);
        assert_eq!(
            record.note_read(&json!({ "path": "a.txt", "start_line": 5 })),
            1
        );
    }

    /// Probe P's read / no-op-edit alternation counts as a repeat; an edit
    /// loop that changes something each time does not; empty inserts are
    /// no-op edits.
    #[test]
    fn alternating_cycles_count_and_noop_edits_are_caught() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(repeat_run(&s(&["A", "A", "A"])), 3);
        assert_eq!(repeat_run(&s(&["x", "A", "B", "A", "B", "A", "B"])), 3);
        assert_eq!(repeat_run(&s(&["A", "B1", "A", "B2", "A", "B3"])), 1);
        assert_eq!(repeat_run(&s(&["A", "B"])), 1);
        assert_eq!(
            repeat_run(&s(&["x", "A", "B", "C", "A", "B", "C", "A", "B", "C"])),
            3
        );

        let noop = json!({ "path": "a.css", "edits": [
            { "op": "insert", "line": 91, "text": "", "column": 1, "edits": [] },
        ] });
        assert!(edit_is_noop(&noop));
        let real = json!({ "path": "a.css", "edits": [
            { "op": "insert", "line": 91, "text": "\n" },
        ] });
        assert!(!edit_is_noop(&real));
        assert!(!edit_is_noop(&json!({ "path": "a" })));
    }

    /// Probe Q copied a compaction placeholder as its `edits`: such a call
    /// is refused, not run, and compacted values read as notes.
    #[test]
    fn copied_compaction_placeholders_are_refused() {
        let args = json!({ "path": "a.css", "original_hash": "h",
            "edits": format!("[1 lines, 4218 chars {ELIDED_MARK}; this call already ran]") })
        .to_string();
        let mut host = FakeHost::new(vec![
            completion(
                json!({ "role": "assistant", "content": "", "tool_calls": [{
                    "id": "e", "type": "function",
                    "function": { "name": "edit_file", "arguments": args },
                }] }),
                10,
                2,
            ),
            answer("done"),
        ]);
        host.tool_defs =
            Some(json!([{ "name": "edit_file", "input_schema": { "type": "object" } }]));
        host.settings.insert("thinking".into(), json!("off"));
        peck_plugin_native_host::with_host(host.host_fn(), || run(&payload())).unwrap();
        assert!(host.invoked.lock().unwrap().is_empty());
        let saved = host.saved_history();
        let reply = saved.iter().find(|m| m["tool_call_id"] == "e").unwrap();
        assert!(reply["content"].as_str().unwrap().contains("history note"));

        // Real content that only mentions the phrase is not a placeholder.
        assert!(!is_copied_placeholder(&json!({
            "path": "notes.md",
            "content": format!("Old results were {ELIDED_MARK} in earlier versions.\n"),
        })));
    }
}
