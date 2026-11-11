//! 按精确请求与 Key 读取触发时的分组快照，查询失败保留历史记录

use crate::host_json;
use form_urlencoded::Serializer;
use gateway_plugin_sdk::client::HostClient;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
use tokio::time::timeout;

/// 宿主请求发生时冻结的路由范围，缺失表示尚未取得历史事实
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Routing {
    pub routing_scope: Option<String>,
    #[serde(default)]
    pub routing_group_refs: Vec<String>,
    #[serde(default)]
    pub routing_group_names_snapshot: Vec<String>,
}

/// 仅消费历史详情中的请求归属和路由快照
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Detail {
    request_id: String,
    client_api_key_id: Option<String>,
    #[serde(flatten)]
    routing: Routing,
}

/// 仅补充当前页的触发分组，保留请求快照中的名称和全部分组
/// @param host 当前管理调用的受管客户端
/// @param requests 当前页请求 ID 及其原始 Key ID
/// @return 已核实归属的分组快照，失败项由调用方显示未知
pub(crate) async fn load(
    host: &HostClient,
    requests: &[(String, String)],
) -> BTreeMap<String, Routing> {
    let unique: BTreeSet<_> = requests.iter().cloned().collect();
    let requests: Vec<_> = unique.into_iter().collect();
    let mut result = BTreeMap::new();
    // 限制并发和总时间，宿主记录清理或慢查询不能阻止读取插件自己的历史
    let lookup = async {
        for chunk in requests.chunks(4) {
            // 同一父调用中并发轮询，保留 SDK 的挂起、期限与取消上下文
            let (a, b, c, d) = tokio::join!(
                lookup_one(host, chunk.first()),
                lookup_one(host, chunk.get(1)),
                lookup_one(host, chunk.get(2)),
                lookup_one(host, chunk.get(3)),
            );
            for (request, routing) in [a, b, c, d].into_iter().flatten() {
                result.insert(request, routing);
            }
        }
    };
    let _ = timeout(Duration::from_secs(2), lookup).await;
    result
}

async fn lookup_one(
    host: &HostClient,
    request: Option<&(String, String)>,
) -> Option<(String, Routing)> {
    let (request, key) = request?;
    let query = Serializer::new(String::new())
        .append_pair("id", request)
        .finish();
    let uri = format!("/api/admin/usage/records/detail?{query}");
    let value = host_json::get(host, uri).await?;
    let detail: Detail = serde_json::from_value(value).ok()?;
    if detail.request_id != *request
        || detail.client_api_key_id.as_deref() != Some(key.as_str())
        || !matches!(
            detail.routing.routing_scope.as_deref(),
            Some("all" | "groups")
        )
    {
        return None;
    }
    Some((request.clone(), detail.routing))
}
