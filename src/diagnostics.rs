//! 通过宿主只读诊断导出补齐实际前账号，不保存原始请求或诊断包

use std::io::{Cursor, Read};

use gateway_plugin_sdk::{
    PluginFault,
    call::middleware::http::Version,
    client::{HostClient, HttpBody, HttpFrame, HttpRequest},
};
use serde_json::Value;
use zip::ZipArchive;

use crate::{audit::Record, fault};

const MAX_ZIP_BYTES: usize = 8 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: u64 = 16 * 1024 * 1024;

/// 从已保存的宿主证据提取出的最小切换事实
#[derive(Debug, PartialEq, Eq)]
pub struct Evidence {
    pub actual_account_id: Option<String>,
    pub from_account_id: Option<String>,
    pub binding_changed: Option<bool>,
    pub account_selection_wait_ms: Option<u64>,
    pub account_queue_wait_ms: Option<u64>,
    pub request_error_code: Option<String>,
}

/// 下载当前切换请求的诊断，不访问外部上游或读取凭据
/// @param host 维护调用中的宿主客户端
/// @param record 需要补齐的切换记录
/// @return 经大小与目标校验的最小切换事实
pub async fn fetch(host: &HostClient, record: &Record) -> Result<Evidence, PluginFault> {
    if record.request_id.len() > 128
        || !record
            .request_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        return Err(fault("诊断请求 ID 无效"));
    }
    let mut response = host
        .dispatch_http(HttpRequest {
            settings: Value::Null,
            method: "GET".into(),
            uri: format!(
                "/api/admin/diagnostics/export?requestId={}",
                record.request_id
            ),
            version: Version::Http11,
            headers: Vec::new(),
            timeout_ms: Some(5_000),
            body: HttpBody::empty(),
        })
        .await?;
    if response.status != 200 {
        let _ = response.body.close().await;
        return Err(fault("请求诊断尚不可读取"));
    }
    let mut bytes = Vec::new();
    while let Some(frame) = response.body.read().await? {
        if let HttpFrame::Data(chunk) = frame {
            if bytes.len().saturating_add(chunk.len()) > MAX_ZIP_BYTES {
                let _ = response.body.close().await;
                return Err(fault("请求诊断超过读取预算"));
            }
            bytes.extend(chunk);
        }
    }
    parse_zip(&bytes, record)
}

/// 只读取 ZIP 中的 diagnostics.json，不解压文件到磁盘
/// @param bytes 受管 HTTP 返回的诊断归档
/// @param record 要匹配的请求及尝试
/// @return 已验证目标的切换事实，缺失事实保持 None
pub fn parse_zip(bytes: &[u8], record: &Record) -> Result<Evidence, PluginFault> {
    if bytes.len() > MAX_ZIP_BYTES {
        return Err(fault("请求诊断超过读取预算"));
    }
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(|_| fault("请求诊断归档无效"))?;
    let document = archive
        .by_name("diagnostics.json")
        .map_err(|_| fault("请求诊断缺少结构化证据"))?;
    if document.size() > MAX_DOCUMENT_BYTES {
        return Err(fault("结构化诊断超过读取预算"));
    }
    let mut buffer = Vec::new();
    document
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut buffer)
        .map_err(|_| fault("请求诊断读取失败"))?;
    if buffer.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(fault("结构化诊断超过读取预算"));
    }
    let document: Value =
        serde_json::from_slice(&buffer).map_err(|_| fault("结构化诊断格式无效"))?;
    evidence(&document, record)
}

/// 复核请求、尝试和实际选中账号后读取前账号；不从候选列表推测旧绑定
/// @param document 宿主 schemaVersion 5 的脱敏诊断
/// @param record 对应的切换记录
/// @return 关键事件被淘汰或未保存时返回未知事实
pub fn evidence(document: &Value, record: &Record) -> Result<Evidence, PluginFault> {
    if document.get("schemaVersion").and_then(Value::as_u64) != Some(5) {
        return Err(fault("请求诊断版本不兼容"));
    }
    let request = document
        .get("requests")
        .and_then(Value::as_array)
        .and_then(|rows| {
            rows.iter()
                .find(|r| r.get("requestId").and_then(Value::as_str) == Some(&record.request_id))
        })
        .ok_or_else(|| fault("请求诊断目标不匹配"))?;
    let events = request
        .pointer("/evidence/trace/events")
        .and_then(Value::as_array)
        .ok_or_else(|| fault("请求诊断尚无时间线"))?;
    let relevant: Vec<&Value> = events
        .iter()
        .filter(|e| {
            e.get("attemptIndex").and_then(Value::as_u64) == Some(u64::from(record.attempt_index))
        })
        .collect();
    let acquired = relevant
        .iter()
        .rev()
        .find(|e| e.get("stage").and_then(Value::as_str) == Some("account.selected"));
    let trace_actual = acquired
        .and_then(|e| e.pointer("/data/accountId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 128);
    if record
        .actual_account_id
        .as_deref()
        .zip(trace_actual)
        .is_some_and(|(known, traced)| known != traced)
    {
        return Err(fault("请求诊断中的实际账号与已确认账号不一致"));
    }
    let actual = record.actual_account_id.as_deref().or(trace_actual);
    let selected = relevant.iter().rev().find(|e| {
        e.get("stage").and_then(Value::as_str) == Some("account.selection")
            && actual.is_some()
            && e.pointer("/data/selectedAccountId").and_then(Value::as_str) == actual
    });
    // 固定账号也会成为 preferredAccountId，只有普通严格绑定才能把该字段解释为旧绑定
    let ordinary_strict = events
        .iter()
        .rev()
        .find(|e| e.get("stage").and_then(Value::as_str) == Some("request.routed"))
        .is_some_and(|e| {
            e.pointer("/data/requiredAccount")
                .is_some_and(Value::is_null)
                && e.pointer("/data/accountPolicy/affinity")
                    .and_then(Value::as_str)
                    == Some("strict")
        });
    let from = selected
        .filter(|_| ordinary_strict)
        .and_then(|e| e.pointer("/data/preferredAccountId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .map(str::to_owned);
    let binding_changed = if actual
        .zip(record.proposed_account_id.as_deref())
        .is_some_and(|(actual, proposed)| actual != proposed)
    {
        Some(false)
    } else {
        from.as_deref()
            .zip(actual)
            .map(|(before, after)| before != after)
    };
    let wait = relevant
        .iter()
        .rev()
        .find(|e| e.get("stage").and_then(Value::as_str) == Some("account.queue.acquired"))
        .and_then(|e| e.pointer("/data/waitMs"))
        .and_then(Value::as_u64);
    Ok(Evidence {
        actual_account_id: actual.map(str::to_owned),
        from_account_id: from,
        binding_changed,
        account_selection_wait_ms: acquired
            .and_then(|e| e.pointer("/data/selectionMs"))
            .and_then(Value::as_u64),
        account_queue_wait_ms: wait,
        request_error_code: events
            .iter()
            .rev()
            .find(|e| e.get("stage").and_then(Value::as_str) == Some("request.finished"))
            .and_then(|e| e.pointer("/data/errorKind"))
            .and_then(Value::as_str)
            .filter(|s| s.len() <= 128)
            .map(str::to_owned),
    })
}
