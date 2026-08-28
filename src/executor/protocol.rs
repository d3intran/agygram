use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
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

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum StreamEvent {
    Init { conversation_id: String },
    TextDelta(String),
    ThinkingDelta(String),
    ToolUse { tool: String, summary: String },
    Result {
        conversation_id: Option<String>,
        response: String,
        status: String,
        usage: Option<TokenUsage>,
        duration_seconds: Option<f64>,
    },
    Error(String),
}

#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub output: String,
    pub conversation_id: Option<String>,
    #[allow(dead_code)]
    pub status: String,
    pub usage: Option<TokenUsage>,
    pub duration_seconds: Option<f64>,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum RawAgyJsonEvent {
    Init {
        conversation_id: Option<String>,
        #[serde(default)]
        #[allow(dead_code)]
        init: Option<serde_json::Value>,
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

#[derive(Deserialize, Debug)]
pub(crate) struct StepUpdatePayload {
    pub conversation_id: Option<String>,
    #[allow(dead_code)]
    pub step_type: Option<String>,
    #[allow(dead_code)]
    pub state: Option<String>,
    pub text_delta: Option<String>,
    pub thinking_delta: Option<String>,
    pub tool_call: Option<serde_json::Value>,
    pub tool_name: Option<String>,
    pub tool_info: Option<serde_json::Value>,
    pub duration_seconds: Option<f64>,
    pub usage: Option<TokenUsage>,
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
