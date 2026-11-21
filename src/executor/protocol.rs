use serde::{Deserialize, Serialize};

/// Token consumption metrics reported by the agy CLI.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub thinking_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

/// Final result payload carried inside [`StreamEvent::Result`].
#[derive(Debug, Clone, PartialEq)]
pub struct StreamResultData {
    pub conversation_id: Option<String>,
    pub response: String,
    #[allow(dead_code)]
    pub status: String,
    pub usage: Option<TokenUsage>,
    pub duration_seconds: Option<f64>,
    pub finished_reason: Option<String>,
    pub structured_output: Option<serde_json::Value>,
}

/// Incremental execution events emitted while streaming an agy prompt.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum StreamEvent {
    Init { conversation_id: String },
    TextDelta(String),
    ThinkingDelta(String),
    ToolUse { tool: String, summary: String },
    TurnComplete { text: String },
    Result(Box<StreamResultData>),
    Error(String),
    Checkpoint,
    ToolError { tool: String, message: String },
    SubagentUpdate { summary: String },
}

/// Final outcome returned when `AgyExecutor::run_prompt_streaming` completes or detaches.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionResult {
    pub output: String,
    pub conversation_id: Option<String>,
    #[allow(dead_code)]
    pub status: String,
    pub usage: Option<TokenUsage>,
    pub duration_seconds: Option<f64>,
    pub detached: bool,
    pub finished_reason: Option<String>,
    pub structured_output: Option<serde_json::Value>,
}

fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct InitPayload {
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub tools: Vec<String>,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum RawAgyJsonEvent {
    Init {
        conversation_id: Option<String>,
        #[serde(default)]
        init: Option<InitPayload>,
    },
    StepUpdate {
        step_update: StepUpdatePayload,
    },
    Result {
        result: ResultPayload,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StepType {
    AgentResponse,
    Tool,
    UserInput,
    PlannerThought,
    Checkpoint,
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum StepState {
    Active,
    Done,
    Error,
    Canceled,
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolErrorPayload {
    #[serde(rename = "type", default)]
    pub type_name: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

impl ToolErrorPayload {
    #[must_use]
    pub(crate) fn format_message(&self) -> String {
        let kind = self.type_name.as_deref().map(str::trim).unwrap_or("");
        let msg = self.message.as_deref().map(str::trim).unwrap_or("");
        match (kind.is_empty(), msg.is_empty()) {
            (false, false) => format!("{kind}: {msg}"),
            (true, false) => msg.to_string(),
            (false, true) => kind.to_string(),
            (true, true) => "tool execution failed".to_string(),
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
pub(crate) struct ToolInfoPayload {
    pub name: Option<String>,
    pub parameters: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<ToolErrorPayload>,
    #[serde(default)]
    #[allow(dead_code)]
    pub output: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SubagentEntry {
    #[serde(default)]
    pub type_name: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub log_uri: Option<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    #[allow(dead_code)]
    pub workspace_uris: Vec<String>,
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SubagentInfoPayload {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub subagents: Vec<SubagentEntry>,
}

impl SubagentInfoPayload {
    #[must_use]
    pub(crate) fn summary(&self) -> Option<String> {
        if self.subagents.is_empty() {
            return None;
        }
        let labels: Vec<String> = self
            .subagents
            .iter()
            .map(|entry| {
                let name = entry
                    .role
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        entry
                            .type_name
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                    })
                    .unwrap_or("subagent");
                if let Some(cid) = entry
                    .conversation_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let short = cid.get(..8).unwrap_or(cid);
                    format!("{name} ({short})")
                } else {
                    name.to_string()
                }
            })
            .collect();
        if labels.len() == 1 {
            Some(labels.into_iter().next().unwrap_or_default())
        } else {
            Some(format!("{} subagents: {}", labels.len(), labels.join(", ")))
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub(crate) struct ToolCallPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Value>,
    #[serde(
        rename = "CommandLine",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub command_line: Option<String>,
    #[serde(
        rename = "TargetFile",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub target_file: Option<String>,
    #[serde(
        rename = "AbsolutePath",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub absolute_path: Option<String>,
    #[serde(rename = "Query", default, skip_serializing_if = "Option::is_none")]
    pub query_upper: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(rename = "Pattern", default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(rename = "Url", default, skip_serializing_if = "Option::is_none")]
    pub url_upper: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(rename = "Prompt", default, skip_serializing_if = "Option::is_none")]
    pub prompt_upper: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "Role", default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(rename = "Subagents", default, skip_serializing_if = "Option::is_none")]
    pub subagents: Option<serde_json::Value>,
}

impl ToolCallPayload {
    /// Formats a JSON parameter summary for downstream bot formatters.
    pub(crate) fn summary(&self) -> String {
        if let Some(ref params) = self.parameters {
            params.to_string()
        } else if let Some(ref args) = self.arguments {
            args.to_string()
        } else {
            serde_json::to_string(self).unwrap_or_default()
        }
    }
}

#[derive(Deserialize, Debug)]
pub(crate) struct StepUpdatePayload {
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub step_index: Option<u64>,
    #[serde(default)]
    pub step_type: Option<StepType>,
    #[serde(default)]
    pub state: Option<StepState>,
    pub text_delta: Option<String>,
    pub thinking_delta: Option<String>,
    pub tool_call: Option<Box<ToolCallPayload>>,
    pub tool_name: Option<String>,
    pub tool_info: Option<Box<ToolInfoPayload>>,
    #[serde(default)]
    pub subagent_info: Option<Box<SubagentInfoPayload>>,
    pub duration_seconds: Option<f64>,
    pub usage: Option<TokenUsage>,
}

impl StepUpdatePayload {
    #[inline]
    pub(crate) fn is_agent_turn_done(&self) -> bool {
        self.step_type == Some(StepType::AgentResponse) && self.state == Some(StepState::Done)
    }

    #[inline]
    pub(crate) fn is_tool_step(&self) -> bool {
        self.step_type == Some(StepType::Tool)
    }

    #[inline]
    pub(crate) fn is_checkpoint(&self) -> bool {
        self.step_type == Some(StepType::Checkpoint)
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn is_terminal_state(state: Option<StepState>) -> bool {
        matches!(
            state,
            Some(StepState::Done | StepState::Error | StepState::Canceled)
        )
    }

    /// Returns `true` only when a tool step should be tracked as actively running (`ACTIVE` or
    /// omitted state). Terminal and unknown states (`DONE`/`ERROR`/`CANCELED`/`Other`) release
    /// active tracking instead of leaking pending tool counts.
    #[inline]
    pub(crate) fn is_active_tool_state(state: Option<StepState>) -> bool {
        matches!(state, Some(StepState::Active) | None)
    }

    /// Peeks the tool name without consuming it, for error reporting and background-activity checks.
    #[must_use]
    pub(crate) fn peek_tool_name(&self) -> Option<&str> {
        self.tool_name
            .as_deref()
            .or_else(|| {
                self.tool_info
                    .as_deref()
                    .and_then(|info| info.name.as_deref())
            })
            .or_else(|| {
                self.tool_call
                    .as_deref()
                    .and_then(|call| call.name.as_deref().or(call.tool.as_deref()))
            })
    }

    /// Extracts `(tool_name, formatted_error_message)` if `tool_info.error` is present.
    pub(crate) fn take_tool_error(
        &mut self,
        fallback_tool: Option<&str>,
    ) -> Option<(String, String)> {
        let err = self.tool_info.as_mut().and_then(|info| info.error.take())?;
        let tool = self
            .peek_tool_name()
            .or(fallback_tool)
            .unwrap_or("tool")
            .to_string();
        Some((tool, err.format_message()))
    }

    /// Extracts `(tool_name, summary)` from whichever tool field the CLI populated, moving strings
    /// instead of cloning where possible.
    pub(crate) fn take_tool_use(&mut self) -> Option<(String, String)> {
        let tool_name = self
            .tool_name
            .take()
            .or_else(|| self.tool_info.as_mut().and_then(|info| info.name.take()))
            .or_else(|| {
                self.tool_call
                    .as_mut()
                    .and_then(|call| call.name.take().or_else(|| call.tool.take()))
            })?;

        let summary = if let Some(ref info) = self.tool_info {
            info.parameters
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default()
        } else if let Some(ref call) = self.tool_call {
            call.summary()
        } else {
            String::new()
        };

        Some((tool_name, summary))
    }
}

#[derive(Deserialize, Debug)]
pub(crate) struct ResultPayload {
    pub conversation_id: Option<String>,
    pub status: Option<String>,
    pub response: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub finished_reason: Option<String>,
    #[serde(default)]
    pub structured_output: Option<serde_json::Value>,
    pub duration_seconds: Option<f64>,
    #[allow(dead_code)]
    pub num_turns: Option<u64>,
    pub usage: Option<TokenUsage>,
}

impl ResultPayload {
    #[inline]
    pub(crate) fn is_waiting(&self) -> bool {
        self.status
            .as_deref()
            .is_some_and(|st| st.eq_ignore_ascii_case("WAITING"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_event_parses_init_payload_and_ignores_unknown_fields() {
        let line = r#"{"event":"init","conversation_id":"conv-123","init":{"cwd":"/repo","model":"gemini-4-neon-high","agent":"coder","permission_mode":"always-proceed","tools":["read_file","run_command"],"nested":[1,2,{"a":"b"}]}}"#;
        let parsed: RawAgyJsonEvent = serde_json::from_str(line).expect("init event parses");
        match parsed {
            RawAgyJsonEvent::Init {
                conversation_id,
                init,
            } => {
                assert_eq!(conversation_id.as_deref(), Some("conv-123"));
                let init = init.expect("init payload parsed");
                assert_eq!(init.cwd.as_deref(), Some("/repo"));
                assert_eq!(init.model.as_deref(), Some("gemini-4-neon-high"));
                assert_eq!(init.agent.as_deref(), Some("coder"));
                assert_eq!(init.permission_mode.as_deref(), Some("always-proceed"));
                assert_eq!(init.tools, vec!["read_file", "run_command"]);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn step_update_maps_known_and_unknown_enums_and_nulls() {
        let done_line = r#"{"event":"step_update","step_update":{"step_type":"agent_response","state":"DONE","text_delta":"hi"}}"#;
        let RawAgyJsonEvent::StepUpdate { step_update } =
            serde_json::from_str(done_line).expect("done step parses")
        else {
            panic!("expected step_update");
        };
        assert!(step_update.is_agent_turn_done());
        assert_eq!(step_update.text_delta.as_deref(), Some("hi"));

        let checkpoint_line = r#"{"event":"step_update","step_update":{"step_index":4,"step_type":"checkpoint","state":"DONE","duration_seconds":0.53}}"#;
        let RawAgyJsonEvent::StepUpdate { step_update } =
            serde_json::from_str(checkpoint_line).expect("checkpoint step parses")
        else {
            panic!("expected step_update");
        };
        assert_eq!(step_update.step_type, Some(StepType::Checkpoint));
        assert!(step_update.is_checkpoint());

        let other_line = r#"{"event":"step_update","step_update":{"step_index":1,"step_type":"planner_thought","state":"RUNNING"}}"#;
        let RawAgyJsonEvent::StepUpdate { step_update } =
            serde_json::from_str(other_line).expect("other step parses")
        else {
            panic!("expected step_update");
        };
        assert_eq!(step_update.step_index, Some(1));
        assert_eq!(step_update.step_type, Some(StepType::PlannerThought));
        assert_eq!(step_update.state, Some(StepState::Other));
        assert!(!step_update.is_agent_turn_done());
        assert!(!StepUpdatePayload::is_active_tool_state(step_update.state));

        let tool_active_line = r#"{"event":"step_update","step_update":{"step_index":2,"step_type":"tool","state":"ACTIVE","tool_name":"run_command"}}"#;
        let RawAgyJsonEvent::StepUpdate { step_update } =
            serde_json::from_str(tool_active_line).expect("tool active step parses")
        else {
            panic!("expected step_update");
        };
        assert!(step_update.is_tool_step());
        assert_eq!(step_update.state, Some(StepState::Active));
        assert!(!StepUpdatePayload::is_terminal_state(step_update.state));
        assert!(StepUpdatePayload::is_active_tool_state(step_update.state));

        let err_res_line = r#"{"event":"result","result":{"status":"ERROR","response":"","error":"invalid model","finished_reason":"error"}}"#;
        let RawAgyJsonEvent::Result { result } =
            serde_json::from_str(err_res_line).expect("error result parses")
        else {
            panic!("expected result");
        };
        assert_eq!(result.status.as_deref(), Some("ERROR"));
        assert_eq!(result.error.as_deref(), Some("invalid model"));
        assert_eq!(result.finished_reason.as_deref(), Some("error"));
        assert!(!result.is_waiting());

        let null_line = r#"{"event":"step_update","step_update":{"step_type":null,"state":null}}"#;
        let RawAgyJsonEvent::StepUpdate { step_update } =
            serde_json::from_str(null_line).expect("null step fields parse")
        else {
            panic!("expected step_update");
        };
        assert_eq!(step_update.step_type, None);
        assert_eq!(step_update.state, None);
        assert!(!step_update.is_agent_turn_done());
    }

    #[test]
    fn step_update_parses_tool_error_output_and_subagent_info() {
        let err_tool_line = r#"{"event":"step_update","step_update":{"step_index":5,"step_type":"tool","state":"ERROR","tool_name":"read_file","tool_info":{"name":"read_file","parameters":{"Path":"/etc/shadow"},"output":{"ok":false},"error":{"type":"permission_denied","message":"blocked by deny rule"}}}}"#;
        let RawAgyJsonEvent::StepUpdate { mut step_update } =
            serde_json::from_str(err_tool_line).expect("tool error parses")
        else {
            panic!("expected step_update");
        };
        assert!(
            step_update
                .tool_info
                .as_ref()
                .and_then(|i| i.output.as_ref())
                .is_some()
        );
        let (tool, msg) = step_update
            .take_tool_error(None)
            .expect("tool error extracted");
        assert_eq!(tool, "read_file");
        assert_eq!(msg, "permission_denied: blocked by deny rule");

        let subagent_line = r#"{"event":"step_update","step_update":{"step_index":6,"step_type":"tool","state":"ACTIVE","subagent_info":{"subagents":[{"type_name":"researcher","role":"code-reviewer","conversation_id":"12345678-abcd-efgh","log_uri":"file:///tmp/log","workspace_uris":["file:///repo"]}]}}}"#;
        let RawAgyJsonEvent::StepUpdate { step_update } =
            serde_json::from_str(subagent_line).expect("subagent_info parses")
        else {
            panic!("expected step_update");
        };
        let sub = step_update.subagent_info.expect("subagent_info present");
        assert_eq!(sub.subagents.len(), 1);
        assert_eq!(sub.subagents[0].type_name.as_deref(), Some("researcher"));
        assert_eq!(sub.subagents[0].role.as_deref(), Some("code-reviewer"));
        assert_eq!(sub.subagents[0].workspace_uris, vec!["file:///repo"]);
        assert_eq!(sub.summary().as_deref(), Some("code-reviewer (12345678)"));
    }

    #[test]
    fn step_update_extracts_tool_info_and_tool_call() {
        let info_line = r#"{"event":"step_update","step_update":{"tool_info":{"name":"view_file","description":"ignored","parameters":{"AbsolutePath":"/tmp/a.rs"}}}}"#;
        let RawAgyJsonEvent::StepUpdate { mut step_update } =
            serde_json::from_str(info_line).expect("tool_info parses")
        else {
            panic!("expected step_update");
        };
        let (tool, summary) = step_update.take_tool_use().expect("tool extracted");
        assert_eq!(tool, "view_file");
        assert!(summary.contains("/tmp/a.rs"));

        let call_line = r#"{"event":"step_update","step_update":{"tool_call":{"tool":"run_command","CommandLine":"cargo test","extra_blob":"ignored"}}}"#;
        let RawAgyJsonEvent::StepUpdate { mut step_update } =
            serde_json::from_str(call_line).expect("tool_call parses")
        else {
            panic!("expected step_update");
        };
        let (tool, summary) = step_update.take_tool_use().expect("tool extracted");
        assert_eq!(tool, "run_command");
        assert!(summary.contains("cargo test"));
        assert!(!summary.contains("extra_blob"));
    }

    #[test]
    fn result_parses_structured_output_waiting_and_unknown_events() {
        let res_line = r#"{"event":"result","result":{"conversation_id":"c1","status":"SUCCESS","response":"ok","finished_reason":"stop","structured_output":{"title":"Card","items":[1,2]},"duration_seconds":1.25,"num_turns":2,"usage":{"input_tokens":10,"output_tokens":20,"thinking_tokens":5,"cache_read_tokens":3,"total_tokens":35}}}"#;
        let RawAgyJsonEvent::Result { result } =
            serde_json::from_str(res_line).expect("result parses")
        else {
            panic!("expected result");
        };
        assert_eq!(result.conversation_id.as_deref(), Some("c1"));
        assert_eq!(result.status.as_deref(), Some("SUCCESS"));
        assert_eq!(result.response.as_deref(), Some("ok"));
        assert_eq!(result.finished_reason.as_deref(), Some("stop"));
        assert_eq!(
            result
                .structured_output
                .as_ref()
                .and_then(|v| v.get("title"))
                .and_then(serde_json::Value::as_str),
            Some("Card")
        );
        assert_eq!(result.duration_seconds, Some(1.25));
        assert_eq!(result.usage.as_ref().map(|u| u.total_tokens), Some(35));

        let waiting_line = r#"{"event":"result","result":{"status":"WAITING","response":"Proceed?","finished_reason":"waiting_for_input"}}"#;
        let RawAgyJsonEvent::Result { result } =
            serde_json::from_str(waiting_line).expect("waiting result parses")
        else {
            panic!("expected result");
        };
        assert!(result.is_waiting());
        assert_eq!(result.finished_reason.as_deref(), Some("waiting_for_input"));

        let unknown_line = r#"{"event":"heartbeat","ts":12345}"#;
        let parsed: RawAgyJsonEvent = serde_json::from_str(unknown_line).expect("unknown parses");
        assert!(matches!(parsed, RawAgyJsonEvent::Unknown));
    }

    #[test]
    fn stream_event_stays_compact_after_boxing_result() {
        assert!(
            size_of::<StreamEvent>() <= 64,
            "StreamEvent unexpectedly large: {} bytes",
            size_of::<StreamEvent>()
        );
    }
}
