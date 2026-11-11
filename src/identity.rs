//! 从已认证模型请求读取显式会话身份，不从缓存键或认证头推测绑定

use gateway_plugin_sdk::call::middleware::MiddlewareHeader;
use serde::Deserialize;
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
    #[serde(rename = "x-codex-turn-metadata", alias = "turn_metadata")]
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
    if key_id.is_empty() || key_id.len() > 128 {
        return None;
    }
    let body: Envelope<'_> = serde_json::from_slice(body).ok()?;
    let client = body.client_metadata.as_ref();
    let context = body.context.as_ref();
    let session = turn_session(header(headers, "x-codex-turn-metadata"))
        .or_else(|| text(client.and_then(|m| m.session_id.as_deref())).map(str::to_owned))
        .or_else(|| turn_session(body.turn_metadata.as_deref()))
        .or_else(|| turn_session(client.and_then(|m| m.turn_metadata.as_deref())))
        .or_else(|| turn_session(context.and_then(|m| m.turn_metadata.as_deref())))
        .or_else(|| text(body.session_id.as_deref()).map(str::to_owned))
        .or_else(|| header(headers, "session-id").map(str::to_owned))
        .or_else(|| text(context.and_then(|m| m.session_id.as_deref())).map(str::to_owned))?;
    let mut hash = Sha256::new();
    hash.update(b"cpr-session-failover-v1\0");
    hash.update(key_id.as_bytes());
    hash.update(b"\0");
    hash.update(session.as_bytes());
    let items = body
        .input
        .filter(|v| v.get().trim_start().starts_with('['))
        .and_then(|v| serde_json::from_str::<Vec<InputKind>>(v.get()).ok());
    let references = items.as_ref().is_some_and(|items| {
        items
            .iter()
            .any(|i| i.kind.as_deref() == Some("item_reference"))
    });
    let native = text(body.previous_response_id.as_deref()).is_some();
    let blocked_reason = if native {
        Some("native_continuation".into())
    } else if references {
        Some("item_reference".into())
    } else if body.generate == Some(false) {
        Some("prewarm".into())
    } else {
        None
    };
    let session_hash = format!("{:x}", hash.finalize());
    // 只接受同一轮可识别的根线程；引用输入和预热不参与恢复
    let replay_turn = (!references
        && body.generate != Some(false)
        && (native
            || (body.previous_response_id.is_none()
                && items.is_some_and(|items| !items.is_empty()))))
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
    let metadata = [
        header(headers, "x-codex-turn-metadata"),
        body.turn_metadata.as_deref(),
        client.and_then(|m| m.turn_metadata.as_deref()),
        body.context
            .as_ref()
            .and_then(|m| m.turn_metadata.as_deref()),
    ]
    .map(|value| value.and_then(|value| serde_json::from_str::<Metadata>(value).ok()));
    if header(headers, "x-openai-subagent").is_some()
        || client
            .into_iter()
            .chain(metadata.iter().flatten())
            .any(|m| text(m.subagent.as_deref()).is_some())
    {
        return None;
    }
    let thread = metadata[0]
        .as_ref()
        .and_then(|m| text(m.thread_id.as_deref()))
        .or_else(|| header(headers, "thread-id"))
        .or_else(|| client.and_then(|m| text(m.thread_id.as_deref())))
        .or_else(|| {
            metadata[1..]
                .iter()
                .flatten()
                .find_map(|m| text(m.thread_id.as_deref()))
        })?;
    if thread != session {
        return None;
    }
    let turn = metadata[0]
        .as_ref()
        .and_then(|m| text(m.turn_id.as_deref()))
        .or_else(|| header(headers, "x-client-turn-id"))
        .or_else(|| client.and_then(|m| text(m.turn_id.as_deref())))
        .or_else(|| {
            metadata[1..]
                .iter()
                .flatten()
                .find_map(|m| text(m.turn_id.as_deref()))
        })?;
    let model = text(body.model.as_deref())?;
    let mut hash = Sha256::new();
    hash.update(b"cpr-session-failover-replay-v1\0");
    for part in [session_hash, thread, turn, model] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    Some(hash.finalize().into())
}
