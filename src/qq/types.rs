use serde::{Deserialize, Deserializer, Serialize, de};
use std::borrow::Cow;
use std::fmt;

fn deserialize_string_or_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    struct StringOrU64Visitor;

    impl de::Visitor<'_> for StringOrU64Visitor {
        type Value = u64;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a non-negative integer or numeric string")
        }

        fn visit_u64<E: de::Error>(self, value: u64) -> Result<u64, E> {
            Ok(value)
        }

        fn visit_i64<E: de::Error>(self, value: i64) -> Result<u64, E> {
            u64::try_from(value)
                .map_err(|_| E::custom("negative integer is invalid for unsigned field"))
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<u64, E> {
            value.trim().parse::<u64>().map_err(E::custom)
        }
    }

    deserializer.deserialize_any(StringOrU64Visitor)
}

/// Request payload for `POST /app/getAppAccessToken`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TokenRequest<'a> {
    pub app_id: &'a str,
    pub client_secret: &'a str,
}

impl fmt::Debug for TokenRequest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenRequest")
            .field("app_id", &self.app_id)
            .field("client_secret", &"[REDACTED]")
            .finish()
    }
}

#[derive(Deserialize)]
pub(crate) struct TokenResponse {
    pub access_token: String,
    #[serde(deserialize_with = "deserialize_string_or_u64")]
    pub expires_in: u64,
}

impl fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenResponse")
            .field("access_token", &"[REDACTED]")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct GatewayResponse {
    pub url: String,
}

/// QQ WebSocket Gateway opcodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum WsOpcode {
    Dispatch = 0,
    Heartbeat = 1,
    Identify = 2,
    Resume = 6,
    Reconnect = 7,
    InvalidSession = 9,
    Hello = 10,
    HeartbeatAck = 11,
}

impl WsOpcode {
    #[inline]
    #[must_use]
    pub(crate) const fn from_u8(op: u8) -> Option<Self> {
        match op {
            0 => Some(Self::Dispatch),
            1 => Some(Self::Heartbeat),
            2 => Some(Self::Identify),
            6 => Some(Self::Resume),
            7 => Some(Self::Reconnect),
            9 => Some(Self::InvalidSession),
            10 => Some(Self::Hello),
            11 => Some(Self::HeartbeatAck),
            _ => None,
        }
    }

    #[inline]
    #[must_use]
    pub(crate) const fn as_u8(self) -> u8 {
        self as u8
    }
}

fn deserialize_opt_borrowed_cow<'de, D>(deserializer: D) -> Result<Option<Cow<'de, str>>, D::Error>
where
    D: Deserializer<'de>,
{
    struct OptCowVisitor;

    impl<'de> de::Visitor<'de> for OptCowVisitor {
        type Value = Option<Cow<'de, str>>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an optional string")
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D2: Deserializer<'de>>(
            self,
            deserializer: D2,
        ) -> Result<Self::Value, D2::Error> {
            deserializer.deserialize_str(self)
        }

        fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
            Ok(Some(Cow::Borrowed(value)))
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
            Ok(Some(Cow::Owned(value.to_owned())))
        }

        fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
            Ok(Some(Cow::Owned(value)))
        }
    }

    deserializer.deserialize_option(OptCowVisitor)
}

/// Lightweight WebSocket frame header parsed without building a `serde_json::Value` DOM.
#[derive(Debug, Deserialize)]
pub(crate) struct WsEnvelopeHeader<'a> {
    pub op: u8,
    #[serde(default)]
    pub s: Option<u64>,
    #[serde(default, borrow, deserialize_with = "deserialize_opt_borrowed_cow")]
    pub t: Option<Cow<'a, str>>,
}

/// Extracts `d` directly into a concrete type `T`, skipping `op`/`s`/`t` without DOM overhead.
#[derive(Debug, Deserialize)]
pub(crate) struct WsDataEnvelope<T> {
    pub d: T,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WsHelloData {
    #[serde(default)]
    pub heartbeat_interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WsReadyData {
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WsOptionalBoolEnvelope {
    #[serde(default)]
    pub d: Option<bool>,
}

#[derive(Serialize)]
pub(crate) struct WsOutgoingEnvelope<T> {
    pub op: u8,
    pub d: T,
}

#[derive(Serialize)]
pub(crate) struct WsIdentifyData<'a> {
    pub token: &'a str,
    pub intents: u32,
    pub shard: [u32; 2],
}

#[derive(Serialize)]
pub(crate) struct WsResumeData<'a> {
    pub token: &'a str,
    pub session_id: &'a str,
    pub seq: u64,
}

/// Scene context carried on QQ C2C and Group events (`message_scene`), including `ext`
/// entries like `"msg_idx=..."` and `"ref_msg_idx=..."`.
#[derive(Default, Deserialize, Clone)]
pub(crate) struct MessageScene {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub ext: Vec<String>,
}

impl MessageScene {
    #[must_use]
    pub(crate) fn ext_value(&self, key: &str) -> Option<&str> {
        for item in &self.ext {
            if let Some((k, v)) = item.split_once('=')
                && k.trim().eq_ignore_ascii_case(key)
            {
                let trimmed = v.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
        }
        None
    }

    #[inline]
    #[must_use]
    pub(crate) fn msg_idx(&self) -> Option<&str> {
        self.ext_value("msg_idx")
    }

    #[inline]
    #[must_use]
    pub(crate) fn ref_msg_idx(&self) -> Option<&str> {
        self.ext_value("ref_msg_idx")
    }
}

impl fmt::Debug for MessageScene {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let safe_ext: Vec<Cow<'_, str>> = self
            .ext
            .iter()
            .map(|entry| match entry.split_once('=') {
                Some((k, _)) if k.trim().to_ascii_lowercase().contains("token") => {
                    Cow::Owned(format!("{}=[REDACTED]", k.trim()))
                }
                _ => Cow::Borrowed(entry.as_str()),
            })
            .collect();
        f.debug_struct("MessageScene")
            .field("source", &self.source)
            .field("ext", &safe_ext)
            .finish()
    }
}

/// Optional explicit message reference (`message_reference`) on incoming/outgoing QQ messages.
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub(crate) struct MessageReference {
    #[serde(default)]
    pub message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_get_message_error: Option<bool>,
}

/// Author block inside a nested `msg_elements` item (`message_type = 103` reply/composite messages).
#[derive(Debug, Default, Deserialize, Clone)]
pub(crate) struct MsgElementAuthor {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub member_openid: Option<String>,
    #[serde(default)]
    pub user_openid: Option<String>,
}

impl MsgElementAuthor {
    #[must_use]
    pub(crate) fn best_openid(&self) -> Option<&str> {
        self.member_openid
            .as_deref()
            .or(self.user_openid.as_deref())
            .or(self.id.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

/// Granular message element inside `msg_elements` (e.g. `message_type = 103` quoted/reply messages).
#[derive(Debug, Default, Deserialize, Clone)]
pub(crate) struct MsgElement {
    #[serde(default)]
    pub msg_idx: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub author: Option<MsgElementAuthor>,
    #[serde(default)]
    pub attachments: Vec<MessageAttachment>,
    #[serde(default)]
    pub msg_elements: Vec<MsgElement>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct C2CMessageEvent {
    pub id: String,
    #[serde(default)]
    pub content: String,
    pub author: AuthorUser,
    #[serde(default)]
    pub attachments: Vec<MessageAttachment>,
    #[serde(default)]
    pub message_type: Option<u32>,
    #[serde(default)]
    pub message_scene: Option<MessageScene>,
    #[serde(default)]
    pub message_reference: Option<MessageReference>,
    #[serde(default)]
    pub msg_elements: Vec<MsgElement>,
}

/// Image/file payload attached to a QQ message. The URL is a short-lived signed link served by
/// QQ's CDN, so it must be downloaded immediately rather than stored in the session.
#[derive(Debug, Deserialize, Clone)]
pub(crate) struct MessageAttachment {
    #[serde(default)]
    pub url: String,
    #[serde(default, rename = "content_type")]
    pub content_type: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    /// SILK voice is the only format QQ records, but agy cannot read it. The platform also
    /// serves a WAV transcode, which is what we hand to the model instead.
    #[serde(default, rename = "voice_wav_url")]
    pub voice_wav_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AuthorUser {
    pub user_openid: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GroupMessageEvent {
    pub id: String,
    #[serde(default)]
    pub content: String,
    pub group_openid: String,
    pub author: AuthorMember,
    #[serde(default)]
    pub attachments: Vec<MessageAttachment>,
    #[serde(default)]
    pub message_type: Option<u32>,
    #[serde(default)]
    pub message_scene: Option<MessageScene>,
    #[serde(default)]
    pub message_reference: Option<MessageReference>,
    #[serde(default)]
    pub msg_elements: Vec<MsgElement>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AuthorMember {
    pub member_openid: String,
}

/// QQ Open Platform message delivery types (`msg_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum QqMessageType {
    Text = 0,
    Markdown = 2,
    Media = 7,
}

impl QqMessageType {
    #[inline]
    #[must_use]
    pub(crate) const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// QQ Open Platform rich-media upload file types (`file_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum QqMediaFileType {
    Image = 1,
    Video = 2,
    Voice = 3,
    File = 4,
}

impl QqMediaFileType {
    #[inline]
    #[must_use]
    pub(crate) const fn as_u8(self) -> u8 {
        self as u8
    }

    #[inline]
    #[must_use]
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Voice => "voice",
            Self::File => "file",
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct MarkdownPayload<'a> {
    pub content: &'a str,
}

#[derive(Debug, Serialize)]
pub(crate) struct MediaPayload<'a> {
    pub file_info: &'a str,
}

#[derive(Debug, Serialize)]
pub(crate) struct SendMessageRequest<'a> {
    #[serde(skip_serializing_if = "str::is_empty")]
    pub content: &'a str,
    pub msg_type: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg_seq: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<MarkdownPayload<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<MediaPayload<'a>>,
}

#[derive(Serialize)]
pub(crate) struct UploadMediaRequest<'a> {
    pub file_type: u8,
    pub srv_send_msg: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_data: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<&'a str>,
}

impl fmt::Debug for UploadMediaRequest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadMediaRequest")
            .field("file_type", &self.file_type)
            .field("srv_send_msg", &self.srv_send_msg)
            .field("file_name", &self.file_name)
            .field(
                "file_data",
                &self
                    .file_data
                    .map(|data| format!("[BASE64 {} chars]", data.len())),
            )
            .field("url", &self.url)
            .finish()
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct UploadMediaResponse {
    #[serde(default)]
    pub file_uuid: Option<String>,
    #[serde(default)]
    pub file_info: Option<String>,
    #[serde(default)]
    pub ttl: Option<u64>,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub(crate) struct MessageExtInfo {
    #[serde(default)]
    pub ref_idx: Option<String>,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub(crate) struct MessageResponse {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub ext_info: Option<MessageExtInfo>,
}

impl MessageResponse {
    #[must_use]
    pub(crate) fn ref_idx(&self) -> Option<&str> {
        self.ext_info
            .as_ref()
            .and_then(|ext| ext.ref_idx.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

/// Structured error payload returned by QQ Bot Open Platform v2 endpoints.
#[derive(Debug, Deserialize)]
pub(crate) struct QqApiErrorResponse {
    #[serde(default)]
    pub code: Option<i64>,
    #[serde(default)]
    pub err_code: Option<i64>,
    #[serde(default)]
    pub message: Option<String>,
}

impl QqApiErrorResponse {
    #[inline]
    #[must_use]
    pub(crate) const fn error_code(&self) -> Option<i64> {
        match self.code {
            Some(c) => Some(c),
            None => self.err_code,
        }
    }

    #[inline]
    #[must_use]
    pub(crate) const fn is_passive_reply_limit(&self) -> bool {
        matches!(self.error_code(), Some(40_034_128))
    }
}

#[inline]
#[must_use]
pub(crate) fn parse_qq_api_error(body: &str) -> Option<QqApiErrorResponse> {
    if body.trim().is_empty() {
        return None;
    }
    serde_json::from_str::<QqApiErrorResponse>(body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_group_event_parses_image_attachment() {
        let json = r#"{
            "id": "ROBOT1.0_abc",
            "content": " 讲解一下这道题",
            "timestamp": "2026-11-08T12:00:00+08:00",
            "group_openid": "B55E344E",
            "author": {"member_openid": "87233760"},
            "attachments": [{
                "content_type": "image/jpeg",
                "filename": "A1B2C3.jpg",
                "size": 345678,
                "url": "https://gchat.qpic.cn/x.jpg",
                "width": 2000,
                "height": 1500
            }]
        }"#;
        let event: GroupMessageEvent = serde_json::from_str(json).expect("group event with image");
        assert_eq!(event.attachments.len(), 1);
        let att = &event.attachments[0];
        assert_eq!(att.content_type.as_deref(), Some("image/jpeg"));
        assert_eq!(att.filename.as_deref(), Some("A1B2C3.jpg"));
        assert_eq!(att.size, Some(345678));
        assert_eq!(att.url, "https://gchat.qpic.cn/x.jpg");
        assert!(att.voice_wav_url.is_none());
    }

    #[test]
    fn test_c2c_event_without_attachments_defaults_to_empty() {
        let json = r#"{
            "id": "ROBOT1.0_abc",
            "content": "hi",
            "timestamp": "2026-11-08T12:00:00+08:00",
            "author": {"user_openid": "87233760"}
        }"#;
        let event: C2CMessageEvent = serde_json::from_str(json).expect("plain c2c event");
        assert!(event.attachments.is_empty());
    }

    #[test]
    fn test_voice_attachment_reads_wav_url() {
        let json = r#"{
            "id": "ROBOT1.0_abc",
            "content": "",
            "timestamp": "2026-11-08T12:00:00+08:00",
            "author": {"user_openid": "87233760"},
            "attachments": [{
                "content_type": "voice",
                "url": "https://gchat.qpic.cn/x.silk",
                "voice_wav_url": "https://gchat.qpic.cn/x.wav",
                "size": 1024
            }]
        }"#;
        let event: C2CMessageEvent = serde_json::from_str(json).expect("voice event");
        assert_eq!(
            event.attachments[0].voice_wav_url.as_deref(),
            Some("https://gchat.qpic.cn/x.wav")
        );
    }

    #[test]
    fn test_token_response_deserialization_and_redacted_debug() {
        let json_str = r#"{"access_token":"token_123","expires_in":" 4871 "}"#;
        let parsed: TokenResponse =
            serde_json::from_str(json_str).expect("string expires_in must parse");
        assert_eq!(parsed.access_token, "token_123");
        assert_eq!(parsed.expires_in, 4871);
        assert!(!format!("{parsed:?}").contains("token_123"));

        let json_num = r#"{"access_token":"token_123","expires_in":7200}"#;
        let parsed_num: TokenResponse =
            serde_json::from_str(json_num).expect("number expires_in must parse");
        assert_eq!(parsed_num.expires_in, 7200);
    }

    #[test]
    fn test_ws_envelope_header_and_data_avoid_dom() {
        let raw = r#"{"d":{"id":"m1","content":"hello","author":{"user_openid":"u1"}},"op":0,"s":42,"t":"C2C_MESSAGE_CREATE"}"#;
        let hdr: WsEnvelopeHeader<'_> = serde_json::from_str(raw).unwrap();
        assert_eq!(WsOpcode::from_u8(hdr.op), Some(WsOpcode::Dispatch));
        assert_eq!(hdr.s, Some(42));
        assert_eq!(hdr.t.as_deref(), Some("C2C_MESSAGE_CREATE"));
        assert!(matches!(hdr.t, Some(Cow::Borrowed(_))));

        let data: WsDataEnvelope<C2CMessageEvent> = serde_json::from_str(raw).unwrap();
        assert_eq!(data.d.id, "m1");
        assert_eq!(data.d.author.user_openid, "u1");
    }

    #[test]
    fn test_ws_outgoing_and_upload_serialization() {
        let hb = WsOutgoingEnvelope {
            op: WsOpcode::Heartbeat.as_u8(),
            d: None::<u64>,
        };
        assert_eq!(serde_json::to_string(&hb).unwrap(), r#"{"op":1,"d":null}"#);

        let hb_seq = WsOutgoingEnvelope {
            op: WsOpcode::Heartbeat.as_u8(),
            d: Some(9u64),
        };
        assert_eq!(serde_json::to_string(&hb_seq).unwrap(), r#"{"op":1,"d":9}"#);

        let upload = UploadMediaRequest {
            file_type: QqMediaFileType::Voice.as_u8(),
            srv_send_msg: false,
            file_name: Some("voice.mp3"),
            file_data: Some("Zm9v"),
            url: None,
        };
        let serialized = serde_json::to_string(&upload).unwrap();
        assert!(serialized.contains(r#""file_type":3"#));
        assert!(serialized.contains(r#""srv_send_msg":false"#));
        assert!(serialized.contains(r#""file_name":"voice.mp3""#));
        assert!(serialized.contains(r#""file_data":"Zm9v""#));
        assert!(!serialized.contains(r#""url""#));
        assert!(!format!("{upload:?}").contains("Zm9v"));

        let url_upload = UploadMediaRequest {
            file_type: QqMediaFileType::File.as_u8(),
            srv_send_msg: false,
            file_name: None,
            file_data: None,
            url: Some("https://example.com/report.pdf"),
        };
        let url_serialized = serde_json::to_string(&url_upload).unwrap();
        assert!(url_serialized.contains(r#""file_type":4"#));
        assert!(url_serialized.contains(r#""url":"https://example.com/report.pdf""#));
        assert!(!url_serialized.contains("file_data"));
    }

    #[test]
    fn test_media_file_types_and_api_error_parsing() {
        assert_eq!(QqMediaFileType::Image.as_u8(), 1);
        assert_eq!(QqMediaFileType::Video.as_u8(), 2);
        assert_eq!(QqMediaFileType::Voice.as_u8(), 3);
        assert_eq!(QqMediaFileType::File.as_u8(), 4);
        assert_eq!(QqMediaFileType::Video.label(), "video");
        assert_eq!(QqMediaFileType::File.label(), "file");

        let passive_err = parse_qq_api_error(r#"{"message":"passive limit","code":40034128}"#)
            .expect("passive limit JSON must parse");
        assert!(passive_err.is_passive_reply_limit());
        assert_eq!(passive_err.message.as_deref(), Some("passive limit"));

        let format_err = parse_qq_api_error(r#"{"err_code":850019,"message":"bad file"}"#)
            .expect("err_code JSON must parse");
        assert_eq!(format_err.error_code(), Some(850_019));
        assert!(!format_err.is_passive_reply_limit());

        let proactive = SendMessageRequest {
            content: "hello",
            msg_type: QqMessageType::Text.as_u8(),
            msg_id: None,
            msg_seq: None,
            markdown: None,
            media: None,
        };
        let json = serde_json::to_string(&proactive).unwrap();
        assert!(!json.contains("msg_id"));
        assert!(!json.contains("msg_seq"));
    }

    #[test]
    fn test_quote_scene_elements_and_sent_ref_idx_parsing() {
        let group_quote_json = r#"{
            "id": "ROBOT1.0_q1",
            "group_openid": "G1",
            "author": {"member_openid": "M_BOB"},
            "message_type": 103,
            "message_scene": {
                "source": "default",
                "ext": ["msg_idx=REFIDX_cur==", "ref_msg_idx=REFIDX_orig==", "auth_token=tok_secret"]
            },
            "msg_elements": [
                {
                    "msg_idx": "REFIDX_orig==",
                    "content": "原来的问题",
                    "author": {"member_openid": "M_ALICE"}
                },
                {
                    "msg_idx": "REFIDX_cur==",
                    "content": "补充追问"
                }
            ]
        }"#;
        let parsed: GroupMessageEvent = serde_json::from_str(group_quote_json)
            .expect("103 quote event with omitted top content");
        assert_eq!(parsed.content, "");
        assert_eq!(parsed.message_type, Some(103));
        let scene = parsed.message_scene.as_ref().unwrap();
        assert_eq!(scene.msg_idx(), Some("REFIDX_cur=="));
        assert_eq!(scene.ref_msg_idx(), Some("REFIDX_orig=="));
        assert!(!format!("{scene:?}").contains("tok_secret"));
        assert_eq!(parsed.msg_elements.len(), 2);

        let sent_resp_json = r#"{
            "id": "ROBOT1.0_bot_out",
            "timestamp": "2026-11-18T07:20:00+08:00",
            "ext_info": {"ref_idx": "REFIDX_bot_img=="}
        }"#;
        let sent: MessageResponse = serde_json::from_str(sent_resp_json).unwrap();
        assert_eq!(sent.id.as_deref(), Some("ROBOT1.0_bot_out"));
        assert_eq!(sent.ref_idx(), Some("REFIDX_bot_img=="));
    }
}
