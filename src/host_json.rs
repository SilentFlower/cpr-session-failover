//! 通过公开宿主 HTTP 回调读取页面所需的原生管理数据

use gateway_plugin_sdk::{
    call::middleware::http::Version,
    client::{HostClient, HttpBody, HttpFrame, HttpRequest},
};
use serde_json::Value;

/// 读取有界 JSON 成功信封，缺失或失败由页面保留未知状态
/// @param host 当前管理调用的受管客户端
/// @param uri 已确认的宿主只读管理接口
/// @return 原生业务数据，失败时返回 None
pub(crate) async fn get(host: &HostClient, uri: String) -> Option<Value> {
    let mut response = host
        .dispatch_http(HttpRequest {
            settings: Value::Null,
            method: "GET".into(),
            uri,
            version: Version::Http11,
            headers: Vec::new(),
            timeout_ms: Some(1500),
            body: HttpBody::empty(),
        })
        .await
        .ok()?;
    if response.status != 200 {
        let _ = response.body.close().await;
        return None;
    }
    let mut bytes = Vec::new();
    loop {
        let frame = match response.body.read().await {
            Ok(frame) => frame,
            Err(_) => {
                let _ = response.body.close().await;
                return None;
            }
        };
        let Some(frame) = frame else { break };
        if let HttpFrame::Data(chunk) = frame {
            if bytes.len().saturating_add(chunk.len()) > 2 * 1024 * 1024 {
                let _ = response.body.close().await;
                return None;
            }
            bytes.extend(chunk);
        }
    }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    if value.get("code").and_then(Value::as_u64) != Some(200) {
        return None;
    }
    value.get("data").cloned()
}
