//! Build Codex thread items from Claude Code tool calls.

use codex_app_server_protocol::CommandAction;
use codex_app_server_protocol::CommandExecutionSource;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallStatus;
use codex_app_server_protocol::FileUpdateChange;
use codex_app_server_protocol::McpToolCallError;
use codex_app_server_protocol::McpToolCallResult;
use codex_app_server_protocol::McpToolCallStatus;
use codex_app_server_protocol::PatchApplyStatus;
use codex_app_server_protocol::PatchChangeKind;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadItemPayload;
use codex_app_server_protocol::UserInput;
use codex_utils_path_uri::LegacyAppPathString;
use serde_json::Value;
use serde_json::json;
use std::path::Path;

/// What a Claude Code tool call becomes on the Codex side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Command,
    File,
    Mcp,
    Other,
}

pub fn tool_kind(tool_name: &str) -> ToolKind {
    match tool_name {
        "Bash" => ToolKind::Command,
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => ToolKind::File,
        name if name.starts_with("mcp__") => ToolKind::Mcp,
        _ => ToolKind::Other,
    }
}

/// How a tool call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Failed,
    Declined,
}

fn item(payload: ThreadItemPayload, started_at_ms: i64) -> ThreadItem {
    ThreadItem {
        payload,
        started_at_ms: Some(started_at_ms),
        completed_at_ms: None,
    }
}

pub fn item_id(item: &ThreadItem) -> &str {
    item.id()
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Array(parts) => parts
            .iter()
            .map(|part| match part.get("text").and_then(Value::as_str) {
                Some(text) => text.to_string(),
                None => text_of(part),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn lines_of(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    text.strip_suffix('\n')
        .unwrap_or(text)
        .split('\n')
        .collect()
}

/// A one-hunk diff replacing `old` with `new`.
fn hunk(old: &str, new: &str) -> String {
    let old_lines = lines_of(old);
    let new_lines = lines_of(new);
    let mut diff = format!(
        "@@ -{},{} +{},{} @@\n",
        usize::from(!old_lines.is_empty()),
        old_lines.len(),
        usize::from(!new_lines.is_empty()),
        new_lines.len()
    );
    for line in old_lines {
        diff.push_str(&format!("-{line}\n"));
    }
    for line in new_lines {
        diff.push_str(&format!("+{line}\n"));
    }
    diff
}

/// Claude Code's `structuredPatch` as unified-diff hunks.
fn patch_to_diff(patch: &[Value]) -> String {
    let mut diff = String::new();
    for hunk in patch {
        let number = |key: &str| hunk.get(key).and_then(Value::as_i64).unwrap_or(0);
        diff.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            number("oldStart"),
            number("oldLines"),
            number("newStart"),
            number("newLines")
        ));
        for line in hunk
            .get("lines")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            diff.push_str(line.as_str().unwrap_or_default());
            diff.push('\n');
        }
    }
    diff
}

fn update_kind() -> PatchChangeKind {
    PatchChangeKind::Update { move_path: None }
}

/// The changes a file tool proposes, from its input alone (before it runs).
fn proposed_changes(tool_name: &str, input: &Value) -> Vec<FileUpdateChange> {
    let path = input
        .get("file_path")
        .or_else(|| input.get("notebook_path"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let (kind, diff) = match tool_name {
        "Write" => {
            let content = str_field(input, "content");
            if Path::new(&path).exists() {
                (update_kind(), hunk("", content))
            } else {
                (PatchChangeKind::Add, content.to_string())
            }
        }
        "MultiEdit" => {
            let diff = input
                .get("edits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|edit| hunk(str_field(edit, "old_string"), str_field(edit, "new_string")))
                .collect::<String>();
            (update_kind(), diff)
        }
        "NotebookEdit" => (update_kind(), hunk("", str_field(input, "new_source"))),
        _ => (
            update_kind(),
            hunk(
                str_field(input, "old_string"),
                str_field(input, "new_string"),
            ),
        ),
    };
    vec![FileUpdateChange { path, kind, diff }]
}

/// Item for a tool call that has started.
pub fn start_tool_item(
    id: &str,
    tool_name: &str,
    input: &Value,
    cwd: &str,
    now_ms: i64,
) -> ThreadItem {
    let payload = match tool_kind(tool_name) {
        ToolKind::Command => {
            let command = str_field(input, "command").to_string();
            ThreadItemPayload::CommandExecution {
                sandbox_type: None,
                model_context: None,
                id: id.to_string(),
                plugin_id: None,
                script_path: None,
                command: command.clone(),
                cwd: LegacyAppPathString::from_string(cwd),
                process_id: None,
                source: CommandExecutionSource::Agent,
                status: CommandExecutionStatus::InProgress,
                command_actions: vec![CommandAction::Unknown { command }],
                aggregated_output: None,
                exit_code: None,
                duration_ms: None,
            }
        }
        ToolKind::File => ThreadItemPayload::FileChange {
            id: id.to_string(),
            changes: proposed_changes(tool_name, input),
            status: PatchApplyStatus::InProgress,
        },
        ToolKind::Mcp => {
            let mut parts = tool_name.splitn(3, "__").skip(1);
            ThreadItemPayload::McpToolCall {
                id: id.to_string(),
                server: parts.next().unwrap_or_default().to_string(),
                tool: parts.next().unwrap_or_default().to_string(),
                status: McpToolCallStatus::InProgress,
                arguments: input.clone(),
                app_context: None,
                mcp_app_resource_uri: None,
                mcp_app_ui: None,
                plugin_id: None,
                read_only_hint: None,
                result: None,
                error: None,
                duration_ms: None,
            }
        }
        ToolKind::Other => ThreadItemPayload::DynamicToolCall {
            id: id.to_string(),
            namespace: None,
            tool: tool_name.to_string(),
            arguments: input.clone(),
            status: DynamicToolCallStatus::InProgress,
            content_items: None,
            success: None,
            duration_ms: None,
        },
    };
    item(payload, now_ms)
}

/// Finish a started tool item.
///
/// `result_text` is the tool result's content; `structured` is Claude Code's
/// `tool_use_result`, when it gave one.
pub fn finish_tool_item(
    mut item: ThreadItem,
    outcome: Outcome,
    result_text: &Value,
    structured: Option<&Value>,
    now_ms: i64,
) -> ThreadItem {
    let text = text_of(result_text);
    let ok = outcome == Outcome::Completed;
    let elapsed = item.started_at_ms.map(|started| (now_ms - started).max(0));
    item.completed_at_ms = Some(now_ms);
    match &mut item.payload {
        ThreadItemPayload::CommandExecution {
            status,
            aggregated_output,
            exit_code,
            duration_ms,
            ..
        } => {
            *status = match outcome {
                Outcome::Completed => CommandExecutionStatus::Completed,
                Outcome::Failed => CommandExecutionStatus::Failed,
                Outcome::Declined => CommandExecutionStatus::Declined,
            };
            if outcome != Outcome::Declined {
                let output = match structured.filter(|value| value.get("stdout").is_some()) {
                    Some(value) => [str_field(value, "stdout"), str_field(value, "stderr")]
                        .into_iter()
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    None => text.clone(),
                };
                *aggregated_output = Some(output);
                // Claude Code reports no exit code for a command that
                // succeeded and states "Exit code N" in a failure's text.
                *exit_code = if ok { Some(0) } else { stated_exit_code(&text) };
                *duration_ms = elapsed;
            }
        }
        ThreadItemPayload::FileChange {
            status, changes, ..
        } => {
            *status = match outcome {
                Outcome::Completed => PatchApplyStatus::Completed,
                Outcome::Failed => PatchApplyStatus::Failed,
                Outcome::Declined => PatchApplyStatus::Declined,
            };
            if let (true, Some(value)) = (ok, structured) {
                let path = value
                    .get("filePath")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .or_else(|| changes.first().map(|change| change.path.clone()))
                    .unwrap_or_default();
                let patch = value.get("structuredPatch").and_then(Value::as_array);
                if value.get("type").and_then(Value::as_str) == Some("create") {
                    *changes = vec![FileUpdateChange {
                        path,
                        kind: PatchChangeKind::Add,
                        diff: str_field(value, "content").to_string(),
                    }];
                } else if let Some(patch) = patch.filter(|patch| !patch.is_empty()) {
                    *changes = vec![FileUpdateChange {
                        path,
                        kind: update_kind(),
                        diff: patch_to_diff(patch),
                    }];
                }
            }
        }
        ThreadItemPayload::McpToolCall {
            status,
            result,
            error,
            duration_ms,
            ..
        } => {
            *duration_ms = elapsed;
            if ok {
                *status = McpToolCallStatus::Completed;
                *result = Some(Box::new(McpToolCallResult {
                    content: vec![json!({ "type": "text", "text": text })],
                    structured_content: None,
                    meta: None,
                }));
            } else {
                *status = McpToolCallStatus::Failed;
                *error = Some(McpToolCallError {
                    message: if text.is_empty() {
                        "tool call did not complete".to_string()
                    } else {
                        text
                    },
                });
            }
        }
        ThreadItemPayload::DynamicToolCall {
            status,
            content_items,
            success,
            duration_ms,
            ..
        } => {
            *duration_ms = elapsed;
            *status = if ok {
                DynamicToolCallStatus::Completed
            } else {
                DynamicToolCallStatus::Failed
            };
            *success = Some(ok);
            *content_items = Some(vec![DynamicToolCallOutputContentItem::InputText { text }]);
        }
        _ => {}
    }
    item
}

fn stated_exit_code(text: &str) -> Option<i32> {
    let rest = text.split("Exit code ").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

pub fn user_message_item(
    id: String,
    input: Vec<UserInput>,
    client_id: Option<String>,
    now_ms: i64,
) -> ThreadItem {
    ThreadItem {
        payload: ThreadItemPayload::UserMessage {
            id,
            client_id,
            content: input,
        },
        started_at_ms: Some(now_ms),
        completed_at_ms: Some(now_ms),
    }
}

pub fn agent_message_item(
    id: String,
    text: String,
    started_at_ms: i64,
    completed_at_ms: Option<i64>,
) -> ThreadItem {
    ThreadItem {
        payload: ThreadItemPayload::AgentMessage {
            moderation: None,
            id,
            text,
            phase: None,
            memory_citation: None,
            delivery: None,
            questions: None,
        },
        started_at_ms: Some(started_at_ms),
        completed_at_ms,
    }
}

/// A one-line description of a tool call, for approval prompts.
pub fn describe_tool(tool_name: &str, input: &Value) -> String {
    if tool_name == "Bash" {
        return str_field(input, "command").to_string();
    }
    let mut text = format!("{tool_name} {input}");
    if text.len() > 2000 {
        let mut end = 2000;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// The path a file tool call touches.
pub fn changed_path(item: &ThreadItem) -> Option<String> {
    match &item.payload {
        ThreadItemPayload::FileChange { changes, .. } => {
            changes.first().map(|change| change.path.clone())
        }
        _ => None,
    }
}
