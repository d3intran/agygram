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
}

#[derive(Deserialize, Debug)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum RawAgyJsonEvent {
    Init {
        conversation_id: Option<String>,
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
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum StepState {
    Done,
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Debug, Clone)]
pub(crate) struct ToolInfoPayload {
    pub name: Option<String>,
    pub parameters: Option<serde_json::Value>,
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
    pub step_type: Option<StepType>,
    #[serde(default)]
    pub state: Option<StepState>,
    pub text_delta: Option<String>,
    pub thinking_delta: Option<String>,
    pub tool_call: Option<Box<ToolCallPayload>>,
    pub tool_name: Option<String>,
    pub tool_info: Option<Box<ToolInfoPayload>>,
    pub duration_seconds: Option<f64>,
    pub usage: Option<TokenUsage>,
}

impl StepUpdatePayload {
    #[inline]
    pub(crate) fn is_agent_turn_done(&self) -> bool {
        self.step_type == Some(StepType::AgentResponse) && self.state == Some(StepState::Done)
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
    pub duration_seconds: Option<f64>,
    #[allow(dead_code)]
    pub num_turns: Option<u64>,
    pub usage: Option<TokenUsage>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_event_skips_nested_init_payload() {
        let line = r#"{"event":"init","conversation_id":"conv-123","init":{"model":"gemini","nested":[1,2,{"a":"b"}]}}"#;
        let parsed: RawAgyJsonEvent = serde_json::from_str(line).expect("init event parses");
        match parsed {
            RawAgyJsonEvent::Init { conversation_id } => {
                assert_eq!(conversation_id.as_deref(), Some("conv-123"));
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

        let other_line = r#"{"event":"step_update","step_update":{"step_type":"planner_thought","state":"RUNNING"}}"#;
        let RawAgyJsonEvent::StepUpdate { step_update } =
            serde_json::from_str(other_line).expect("other step parses")
        else {
            panic!("expected step_update");
        };
        assert_eq!(step_update.step_type, Some(StepType::Other));
        assert_eq!(step_update.state, Some(StepState::Other));
        assert!(!step_update.is_agent_turn_done());

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
    fn result_and_unknown_events_deserialize() {
        let res_line = r#"{"event":"result","result":{"conversation_id":"c1","status":"SUCCESS","response":"ok","duration_seconds":1.25,"num_turns":2,"usage":{"input_tokens":10,"output_tokens":20,"thinking_tokens":5,"cache_read_tokens":3,"total_tokens":35}}}"#;
        let RawAgyJsonEvent::Result { result } =
            serde_json::from_str(res_line).expect("result parses")
        else {
            panic!("expected result");
        };
        assert_eq!(result.conversation_id.as_deref(), Some("c1"));
        assert_eq!(result.status.as_deref(), Some("SUCCESS"));
        assert_eq!(result.response.as_deref(), Some("ok"));
        assert_eq!(result.duration_seconds, Some(1.25));
        assert_eq!(result.usage.as_ref().map(|u| u.total_tokens), Some(35));

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
