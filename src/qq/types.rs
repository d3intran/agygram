use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
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
pub struct SendMessageRequest<'a> {
    pub content: &'a str,
    pub msg_type: u8,
    pub msg_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg_seq: Option<u32>,
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
