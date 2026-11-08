use serde::{Deserialize, Deserializer, Serialize};

fn deserialize_string_or_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrU64 {
        U64(u64),
        String(String),
    }

    match StringOrU64::deserialize(deserializer)? {
        StringOrU64::U64(v) => Ok(v),
        StringOrU64::String(s) => s.trim().parse::<u64>().map_err(serde::de::Error::custom),
    }
}

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(deserialize_with = "deserialize_string_or_u64")]
    pub expires_in: u64,
}

#[derive(Debug, Deserialize)]
pub struct GatewayResponse {
    pub url: String,
    pub shards: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WsPayload {
    pub op: u8,
    #[serde(default)]
    pub d: Option<serde_json::Value>,
    #[serde(default)]
    pub s: Option<u64>,
    #[serde(default)]
    pub t: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct WsIdentifyData<'a> {
    pub token: &'a str,
    pub intents: u32,
    pub shard: [u32; 2],
}

#[derive(Debug, Serialize)]
pub struct WsResumeData<'a> {
    pub token: &'a str,
    pub session_id: &'a str,
    pub seq: u64,
}

#[derive(Debug, Deserialize)]
pub struct C2CMessageEvent {
    pub id: String,
    pub content: String,
    pub timestamp: String,
    pub author: AuthorUser,
}

#[derive(Debug, Deserialize)]
pub struct AuthorUser {
    pub user_openid: String,
}

#[derive(Debug, Deserialize)]
pub struct GroupMessageEvent {
    pub id: String,
    pub content: String,
    pub timestamp: String,
    pub group_openid: String,
    pub author: AuthorMember,
}

#[derive(Debug, Deserialize)]
pub struct AuthorMember {
    pub member_openid: String,
}

#[derive(Debug, Serialize)]
pub struct MarkdownPayload<'a> {
    pub content: &'a str,
}

#[derive(Debug, Serialize)]
pub struct SendMessageRequest<'a> {
    #[serde(skip_serializing_if = "str::is_empty")]
    pub content: &'a str,
    pub msg_type: u8,
    pub msg_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg_seq: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<MarkdownPayload<'a>>,
}

#[derive(Debug, Serialize)]
pub struct StreamMessageRequest<'a> {
    pub input_mode: &'static str,
    pub input_state: u8,
    pub content_type: &'static str,
    pub content_raw: &'a str,
    pub event_id: &'a str,
    pub msg_id: &'a str,
    pub msg_seq: u32,
    pub index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_msg_id: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
pub struct MessageResponse {
    pub id: Option<String>,
    pub timestamp: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_response_deserialization() {
        let json_str = r#"{"access_token":"token_123","expires_in":"4871"}"#;
        let parsed: TokenResponse = serde_json::from_str(json_str).expect("string expires_in must parse");
        assert_eq!(parsed.access_token, "token_123");
        assert_eq!(parsed.expires_in, 4871);

        let json_num = r#"{"access_token":"token_123","expires_in":7200}"#;
        let parsed_num: TokenResponse = serde_json::from_str(json_num).expect("number expires_in must parse");
        assert_eq!(parsed_num.expires_in, 7200);
    }
}
