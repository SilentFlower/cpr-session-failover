//! 通过宿主原生管理接口维护当前插件配置，私有状态不另存一份设置

use std::collections::BTreeMap;

use form_urlencoded::{Serializer, parse};
use gateway_plugin_sdk::{
    PluginFault,
    call::{
        management::{ManagementRequest, ManagementResponse},
        middleware::{MiddlewareHeader, http::Version},
    },
    client::{HostClient, HttpBody, HttpFrame, HttpRequest, TypedReply},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, from_slice, from_value, json, to_vec};

use crate::{fault, policy::Config};

const MAX_CONTROL_BYTES: usize = 8 * 1024 * 1024;

/// 只定位握手确认的当前实例，不接受页面传入实例 ID 或制品摘要
pub struct SettingsControl {
    instance_id: String,
    artifact_sha256: String,
    middleware_id: String,
    scheduler_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Instance {
    id: String,
    name: String,
    artifact_sha256: String,
    enabled: bool,
    configuration: Config,
    bindings: Vec<Value>,
    revision: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RequiredConfiguration {
    wait_seconds: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SaveInput {
    expected_revision: u64,
    configuration: RequiredConfiguration,
    bindings: Vec<Value>,
}

#[derive(Deserialize, Serialize)]
struct ResourceOption {
    id: String,
    name: String,
    enabled: bool,
}

struct Failure {
    status: u16,
    code: &'static str,
    message: String,
}

impl Failure {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            code: "invalid_settings",
            message: message.into(),
        }
    }

    fn conflict() -> Self {
        Self {
            status: 409,
            code: "settings_changed",
            message: "插件设置已变化，请重新读取后再保存".into(),
        }
    }

    fn unavailable() -> Self {
        Self {
            status: 503,
            code: "settings_unavailable",
            message: "无法确认操作结果，请重新读取设置".into(),
        }
    }
}

impl SettingsControl {
    /// 创建当前实例的管理入口，只使用宿主握手及清单中的身份
    /// @param instance_id 宿主确认的实例 ID
    /// @param artifact_sha256 当前不可变制品摘要
    /// @param middleware_id 清单规范化后的中间件贡献项 ID
    /// @param scheduler_id 清单规范化后的调度贡献项 ID
    /// @return 绑定当前实例的设置管理器
    pub fn new(
        instance_id: String,
        artifact_sha256: String,
        middleware_id: String,
        scheduler_id: String,
    ) -> Self {
        Self {
            instance_id,
            artifact_sha256,
            middleware_id,
            scheduler_id,
        }
    }

    /// 处理设置及范围选项，写入沿用宿主的版本检查、校验和发布流程
    /// @param host 当前管理调用的受管宿主客户端
    /// @param request 已通过管理员认证的管理请求
    /// @param payload 页面提交的 JSON 正文
    /// @return 插件原始 JSON 响应，不把未知写入结果伪装成未保存
    pub async fn handle(
        &self,
        host: &HostClient,
        request: &ManagementRequest,
        payload: &[u8],
    ) -> Result<TypedReply<ManagementResponse>, PluginFault> {
        let result = match (request.method.as_str(), request.path.as_str()) {
            ("GET", "settings") if request.query.is_empty() => self.view(host).await,
            ("POST", "settings") if request.query.is_empty() => self.save(host, payload).await,
            ("GET", "resources") => resources(host, &request.query).await,
            _ => Err(Failure::invalid("设置接口参数无效")),
        };
        let (status, value) = match result {
            Ok(value) => (200, value),
            Err(error) => (
                error.status,
                json!({"error":{"code":error.code,"message":error.message}}),
            ),
        };
        Ok(TypedReply::new(ManagementResponse {
            status,
            content_type: "application/json".into(),
            headers: vec![MiddlewareHeader {
                name: "cache-control".into(),
                value: b"no-store".to_vec(),
            }],
        })
        .with_payload(to_vec(&value).map_err(|_| fault("设置响应编码失败"))?))
    }

    async fn instance(&self, host: &HostClient) -> Result<Instance, Failure> {
        let data = native(host, "GET", "/api/admin/plugins/instances", None).await?;
        let value = data
            .as_array()
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item.get("id").and_then(Value::as_str) == Some(&self.instance_id))
            })
            .ok_or_else(Failure::conflict)?;
        let instance: Instance = from_value(value.clone()).map_err(|_| Failure::unavailable())?;
        if instance.artifact_sha256 != self.artifact_sha256 || !instance.enabled {
            return Err(Failure::conflict());
        }
        Ok(instance)
    }

    async fn view(&self, host: &HostClient) -> Result<Value, Failure> {
        let instance = self.instance(host).await?;
        Ok(json!({
            "revision":instance.revision,
            "configuration":instance.configuration,
            "bindings":instance.bindings,
            "definitions":[
                {"contribution":self.middleware_id,"stage":"request","label":"请求中间件 · 请求开始","providerScope":false},
                {"contribution":self.middleware_id,"stage":"attempt","label":"请求中间件 · 每次尝试","providerScope":true},
                {"contribution":self.scheduler_id,"stage":"scheduling","label":"请求调度 · 账号调度","providerScope":true}
            ],
            "historyStorage":{"maximumBytes":16 * 1024 * 1024,"maximumStateRecords":10_000,"automaticDeletion":false}
        }))
    }

    async fn save(&self, host: &HostClient, payload: &[u8]) -> Result<Value, Failure> {
        let input: SaveInput =
            from_slice(payload).map_err(|_| Failure::invalid("设置格式无效，请提交完整参数"))?;
        let configuration = Config {
            wait_seconds: input.configuration.wait_seconds,
            ..Config::default()
        };
        configuration.validate().map_err(Failure::invalid)?;
        let instance = self.instance(host).await?;
        if instance.revision != input.expected_revision {
            return Err(Failure::conflict());
        }
        // 只更新当前实例的参数和规则，名称、启停状态及密钥继续由宿主管理
        // 不能直接改进程内快照，否则宿主设置弹窗、重启及升级会看到不同配置
        native(
            host,
            "POST",
            "/api/admin/plugins/instances/update",
            Some(json!({
                "id":instance.id,
                "instance":{
                    "expectedRevision":input.expected_revision,
                    "name":instance.name,
                    "artifactSha256":instance.artifact_sha256,
                    "enabled":instance.enabled,
                    "configuration":configuration,
                    "bindings":input.bindings
                }
            })),
        )
        .await?;
        Ok(json!({"saved":true}))
    }
}

async fn resources(host: &HostClient, query: &str) -> Result<Value, Failure> {
    let pairs: BTreeMap<String, String> = parse(query.as_bytes()).into_owned().collect();
    let kind = pairs.get("kind").map(String::as_str).unwrap_or("");
    let cursor = pairs.get("cursor").map(String::as_str).unwrap_or("");
    let page = if kind == "groups" {
        if cursor.is_empty() {
            1
        } else {
            cursor
                .parse::<u32>()
                .map_err(|_| Failure::invalid("分组页码无效"))?
        }
    } else {
        1
    };
    let path = match kind {
        "keys" => {
            let mut encoded = Serializer::new(String::new());
            encoded.append_pair("limit", "200");
            if !cursor.is_empty() {
                encoded.append_pair("cursor", cursor);
            }
            format!("/api/admin/client-keys?{}", encoded.finish())
        }
        "groups" if page > 0 => {
            format!("/api/admin/account-groups/options?pageSize=100&page={page}")
        }
        _ => return Err(Failure::invalid("范围选项类型无效")),
    };
    let data = native(host, "GET", &path, None).await?;
    let options = data
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(Failure::unavailable)?;
    let items: Vec<ResourceOption> = options
        .iter()
        .map(|item| from_value(item.clone()).map_err(|_| Failure::unavailable()))
        .collect::<Result<_, _>>()?;
    let next = if kind == "keys" {
        data.get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_owned)
    } else {
        let total = data
            .pointer("/page/totalPages")
            .and_then(Value::as_u64)
            .ok_or_else(Failure::unavailable)?;
        (u64::from(page) < total).then(|| (u64::from(page) + 1).to_string())
    };
    Ok(json!({"items":items,"nextCursor":next}))
}

async fn native(
    host: &HostClient,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> Result<Value, Failure> {
    let (headers, body) = match body {
        Some(value) => (
            vec![MiddlewareHeader {
                name: "content-type".into(),
                value: b"application/json".to_vec(),
            }],
            HttpBody::from_bytes(to_vec(&value).map_err(|_| Failure::unavailable())?),
        ),
        None => (Vec::new(), HttpBody::empty()),
    };
    // 受管内部分派使用插件宿主身份，不读取或转发浏览器 Cookie、Key 或管理员令牌
    let mut response = host
        .dispatch_http(HttpRequest {
            settings: Value::Null,
            method: method.into(),
            uri: uri.into(),
            version: Version::Http11,
            headers,
            timeout_ms: None,
            body,
        })
        .await
        .map_err(|_| Failure::unavailable())?;
    let status = response.status;
    let mut bytes = Vec::new();
    while let Some(frame) = response
        .body
        .read()
        .await
        .map_err(|_| Failure::unavailable())?
    {
        if let HttpFrame::Data(chunk) = frame {
            if bytes.len().saturating_add(chunk.len()) > MAX_CONTROL_BYTES {
                let _ = response.body.close().await;
                return Err(Failure::unavailable());
            }
            bytes.extend(chunk);
        }
    }
    let value: Value = from_slice(&bytes).map_err(|_| Failure::unavailable())?;
    match status {
        200..=299 if value.get("code").and_then(Value::as_u64) == Some(200) => {
            Ok(value.get("data").cloned().unwrap_or(Value::Null))
        }
        409 => Err(Failure::conflict()),
        400 => Err(Failure::invalid(
            value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("宿主拒绝此配置"),
        )),
        _ => Err(Failure::unavailable()),
    }
}
