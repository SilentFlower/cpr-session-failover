//! 从已认证模型请求读取显式会话身份，不从缓存键或认证头推测绑定

use std::fmt::Formatter;

use gateway_plugin_sdk::call::middleware::MiddlewareHeader;
use serde::{
    Deserialize, Deserializer,
    de::{SeqAccess, Visitor},
};
use serde_json::{Value, value::RawValue};
use sha2::{Digest, Sha256};

/// 插件协调使用的脱敏会话身份，不是宿主的绑定键
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub session_hash: String,
    pub blocked_reason: Option<String>,
    pub replay_turn: Option<[u8; 32]>,
}

/// 只反序列化身份字段，忽略业务内容和其他 metadata
#[derive(Default, Deserialize)]
struct Metadata {
    session_id: Option<String>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    #[serde(rename = "x-openai-subagent", alias = "subagent_kind")]
    subagent: Option<String>,
    #[serde(
        rename = "x-codex-turn-metadata",
        alias = "turn_metadata",
        alias = "turnMetadata"
    )]
    turn_metadata: Option<String>,
}

/// 原始 input 保持借用，不复制完整聊天消息
#[derive(Deserialize)]
struct Envelope<'a> {
    model: Option<String>,
    session_id: Option<String>,
    client_metadata: Option<Metadata>,
    context: Option<Metadata>,
    #[serde(alias = "turnMetadata", alias = "x-codex-turn-metadata")]
    turn_metadata: Option<String>,
    previous_response_id: Option<String>,
    generate: Option<bool>,
    #[serde(borrow)]
    input: Option<&'a RawValue>,
}

/// 仅识别引用输入，内容字段由 serde 跳过
#[derive(Deserialize)]
struct InputKind {
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Default)]
struct InputSummary {
    has_items: bool,
    has_reference: bool,
}

impl<'de> Deserialize<'de> for InputSummary {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct InputVisitor;

        impl<'de> Visitor<'de> for InputVisitor {
            type Value = InputSummary;

            fn expecting(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("模型输入数组")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut input: A) -> Result<Self::Value, A::Error> {
                let mut summary = InputSummary::default();
                while let Some(item) = input.next_element::<InputKind>()? {
                    summary.has_items = true;
                    summary.has_reference |= item.kind.as_deref() == Some("item_reference");
                }
                Ok(summary)
            }
        }

        deserializer.deserialize_seq(InputVisitor)
    }
}

fn text(value: Option<&str>) -> Option<&str> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 1024)
}

fn header<'a>(headers: &'a [MiddlewareHeader], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .and_then(|h| std::str::from_utf8(&h.value).ok())
        .and_then(|s| text(Some(s)))
}

fn turn_session(metadata: Option<&str>) -> Option<String> {
    let value: Value = serde_json::from_str(metadata?).ok()?;
    text(value.get("session_id").and_then(Value::as_str)).map(str::to_owned)
}

/// 读取与 CPR 当前会话解释一致的显式身份，只保留其带 Key 作用域的摘要
/// @param key_id 已认证请求头中的稳定 Key ID，不能传入认证明文
/// @param headers 模型请求当前有效头部
/// @param body Responses 请求正文，只解析身份及续写标记
/// @return 缺少可靠身份时返回 None，原生续写仍返回身份但标记禁止插件迁移
pub fn identify(key_id: &str, headers: &[MiddlewareHeader], body: &[u8]) -> Option<Identity> {
    identify_request(key_id, headers, body, true)
}

/// 读取显式会话身份，并按入口需要计算原生续写重发标识
/// @param key_id 已认证的稳定 Key ID
/// @param headers 模型请求当前有效头部
/// @param body 模型请求正文
/// @param allow_replay 是否允许为当前入口识别同轮重发
/// @return 会话身份及适用的限制，缺少可靠身份时返回 None
pub fn identify_request(
    key_id: &str,
    headers: &[MiddlewareHeader],
    body: &[u8],
    allow_replay: bool,
) -> Option<Identity> {
    if key_id.is_empty() || key_id.len() > 128 {
        return None;
    }
    let body: Envelope<'_> = serde_json::from_slice(body).ok()?;
    let client = body.client_metadata.as_ref();
    let context = body.context.as_ref();
    // 当前正文的逻辑身份优先，握手元数据可能属于连接建立时的旧轮次
    let session = text(client.and_then(|metadata| metadata.session_id.as_deref()))
        .map(str::to_owned)
        .or_else(|| turn_session(body.turn_metadata.as_deref()))
        .or_else(|| turn_session(client.and_then(|metadata| metadata.turn_metadata.as_deref())))
        .or_else(|| turn_session(header(headers, "x-codex-turn-metadata")))
        .or_else(|| turn_session(context.and_then(|metadata| metadata.turn_metadata.as_deref())))
        .or_else(|| text(body.session_id.as_deref()).map(str::to_owned))
        .or_else(|| header(headers, "session-id").map(str::to_owned))
        .or_else(|| text(context.and_then(|m| m.session_id.as_deref())).map(str::to_owned))?;
    let mut hash = Sha256::new();
    hash.update(b"cpr-session-failover-v1\0");
    hash.update(key_id.as_bytes());
    hash.update(b"\0");
    hash.update(session.as_bytes());
    let native = text(body.previous_response_id.as_deref()).is_some();
    let prewarm = body.generate == Some(false);
    let can_replay = allow_replay && !prewarm;
    // 已确认原生续写时，仅重发资格还取决于输入中的引用条目
    let input = if !native || can_replay {
        body.input
            .filter(|value| value.get().trim_start().starts_with('['))
            .and_then(|value| serde_json::from_str::<InputSummary>(value.get()).ok())
            .unwrap_or_default()
    } else {
        InputSummary::default()
    };
    let blocked_reason = if native {
        Some("native_continuation".into())
    } else if input.has_reference {
        Some("item_reference".into())
    } else if prewarm {
        Some("prewarm".into())
    } else {
        None
    };
    let session_hash = format!("{:x}", hash.finalize());
    // 只接受同一轮可识别的根线程；引用输入和预热不参与恢复
    let replay_turn = (can_replay
        && !input.has_reference
        && (native || (body.previous_response_id.is_none() && input.has_items)))
        .then(|| replay_turn(headers, &body, &session, &session_hash))
        .flatten();
    Some(Identity {
        session_hash,
        blocked_reason,
        replay_turn,
    })
}

fn replay_turn(
    headers: &[MiddlewareHeader],
    body: &Envelope<'_>,
    session: &str,
    session_hash: &str,
) -> Option<[u8; 32]> {
    let client = body.client_metadata.as_ref();
    // 重发机会只关联本轮元数据，不能从旧握手拼接线程或轮次
    let metadata = body
        .turn_metadata
        .as_deref()
        .or_else(|| client.and_then(|metadata| metadata.turn_metadata.as_deref()))
        .or_else(|| header(headers, "x-codex-turn-metadata"))
        .or_else(|| {
            body.context
                .as_ref()
                .and_then(|metadata| metadata.turn_metadata.as_deref())
        })
        .and_then(|metadata| serde_json::from_str::<Metadata>(metadata).ok());
    if header(headers, "x-openai-subagent").is_some()
        || client
            .into_iter()
            .chain(metadata.as_ref())
            .any(|metadata| text(metadata.subagent.as_deref()).is_some())
    {
        return None;
    }
    let thread = client
        .and_then(|metadata| text(metadata.thread_id.as_deref()))
        .or_else(|| {
            metadata
                .as_ref()
                .and_then(|metadata| text(metadata.thread_id.as_deref()))
        })
        .or_else(|| header(headers, "thread-id"))?;
    if thread != session {
        return None;
    }
    let turn = client
        .and_then(|metadata| text(metadata.turn_id.as_deref()))
        .or_else(|| {
            metadata
                .as_ref()
                .and_then(|metadata| text(metadata.turn_id.as_deref()))
        })
        .or_else(|| header(headers, "x-client-turn-id"))?;
    let model = text(body.model.as_deref())?;
    let mut hash = Sha256::new();
    hash.update(b"cpr-session-failover-replay-v1\0");
    for part in [session_hash, thread, turn, model] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    Some(hash.finalize().into())
}
