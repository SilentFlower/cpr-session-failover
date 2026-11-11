//! 插件 RPC 入口，组合严格绑定、延迟选号与持久切换记录

use std::{
    collections::BTreeMap,
    error::Error,
    future::Future,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cpr_session_failover::{
    audit::{Audit, HistoryQuery, Record, metadata},
    diagnostics, fault,
    identity::identify,
    policy::{Action, Config, Policy},
    settings::SettingsControl,
};
use form_urlencoded::parse as parse_query;
use gateway_plugin_sdk::{
    CallBudgets, Capability, ErrorCode, Manifest, PluginFault, SendState, Stage,
    call::{
        host::{LogLevel, LogRequest, LogResult},
        management::{
            CommandDescriptor, CommandInvocation, CommandParameter, CommandParameterType,
            CommandRegistration, CommandResult, CommandValue, ManagementPage,
            ManagementRegistration, ManagementRequest, ManagementResource, ManagementResponse,
            ManagementRoute,
        },
        middleware::{MiddlewareHeader, MiddlewareMount, MiddlewareTransport},
        policy::{AccountScheduleDecision, AccountScheduleRequest},
        services::usage::{GetRecord, GetUsageRecord, UsageOutcome},
    },
    client::{
        Empty, HostClient, MiddlewareCall, MiddlewareResponse, PluginBuilder, PluginSession,
        RequestCall, SessionConfig, TypedCall, TypedReply, WebSocketCall, WebSocketDirection,
        WebSocketKind, WebSocketMessage, WebSocketPayload, methods,
    },
};
use serde::Serialize;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// 每个插件会话共享的业务配置、单调时钟和审计入口
struct App {
    config: Config,
    started: Instant,
    policy: Arc<Mutex<Policy>>,
    audit: Audit,
    settings: SettingsControl,
}

/// 查询时生成 UTC 展示字段，不修改持久历史的 schema 或使用浏览器本地时区
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoryEntry {
    #[serde(flatten)]
    record: Record,
    decision_time_utc: Option<String>,
    admitted_time_utc: Option<String>,
    completed_time_utc: Option<String>,
}

impl App {
    fn now(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn policy(&self) -> MutexGuard<'_, Policy> {
        self.policy.lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn request(&self, mut call: RequestCall) -> Result<MiddlewareResponse, PluginFault> {
        let request_id = call.request.head.request_id.clone();
        let attempt_index = call.request.head.attempt_index;
        let websocket_response = call.request.head.mount == MiddlewareMount::Request
            && call.request.head.transport == MiddlewareTransport::WebSocket
            && call.request.head.endpoint == "/v1/responses"
            && call.request.head.operation == "generate";
        let mut guard = None;
        if call.request.head.mount == MiddlewareMount::Request
            && call.request.head.protocol == "openai"
            && matches!(
                call.request.head.endpoint.as_str(),
                "/v1/responses" | "/v1/chat/completions"
            )
            && let Some(mut identity) = identify(
                &call.request.head.client_key_id,
                &call.request.head.headers,
                &call.request.body,
            )
        {
            if !websocket_response {
                identity.replay_turn = None;
            }
            let runtime = call
                .request
                .head
                .settings
                .get_mut("runtime")
                .ok_or_else(|| fault("宿主没有提供请求运行设置"))?;
            let mode = runtime
                .get_mut("openai_account_affinity")
                .ok_or_else(|| fault("宿主缺少账号亲和设置"))?;
            // 只改本次模型调用，已有原生续写也按严格绑定复核，不能把会话拉回旧账号
            *mode = json!("strict");
            if identity.blocked_reason.is_none() {
                if runtime
                    .get("max_waiting_per_account")
                    .and_then(Value::as_u64)
                    == Some(0)
                {
                    identity.blocked_reason = Some("account_queue_disabled".into());
                } else if runtime
                    .get("account_queue_timeout_seconds")
                    .or_else(|| runtime.get("concurrency_wait_timeout_seconds"))
                    .and_then(Value::as_u64)
                    .is_none_or(|seconds| seconds <= u64::from(self.config.wait_seconds))
                {
                    identity.blocked_reason = Some("queue_budget_too_short".into());
                }
            }
            if self.policy().register(
                &request_id,
                &call.request.head.client_key_id,
                identity,
                self.now(),
            ) {
                self.audit.track(&request_id);
                guard = Some(RequestGuard {
                    audit: &self.audit,
                    policy: self.policy.clone(),
                    request_id: request_id.clone(),
                });
            }
        }
        if call.request.head.mount == MiddlewareMount::Attempt
            && let (Some(account), Some(attempt)) = (
                call.request.head.account_id.as_deref(),
                call.request.head.attempt_index,
            )
        {
            let admission = self
                .policy()
                .admitted(&request_id, attempt, account, self.now());
            if let Some((id, matched)) = admission {
                // attempt 入口在宿主取得账号名额及原子认领绑定之后，不能用调度返回值代替此事实
                if let Err(error) = self
                    .audit
                    .update(&call.host, &id, |r| {
                        r.actual_account_id = Some(account.into());
                        r.admitted_at_ms = Some(wall_ms());
                        r.stage = if matched {
                            "admitted"
                        } else {
                            "different_account"
                        }
                        .into();
                    })
                    .await
                {
                    report_error(&call.host, &request_id, &error).await;
                }
            }
        }
        let result = call.next.run(call.request).await;
        if websocket_response
            && match &result {
                Ok(response) => response.status == 429,
                Err(error) => {
                    error.send_state == SendState::NotSent
                        && error
                            .details
                            .as_ref()
                            .and_then(|detail| detail.get("kind"))
                            .and_then(Value::as_str)
                            == Some("concurrency_queue_timeout")
                }
            }
        {
            // 初始 429 也可能由上游生成，最终由错误帧和原请求 ID 复核
            self.policy().note_initial_429(&request_id, self.now());
        }
        let response_metadata = result
            .as_ref()
            .ok()
            .and_then(|response| response.metadata.clone());
        let response_error = result.as_ref().err().map(error_code);
        let ids = if attempt_index.is_some() || response_error.is_some() {
            self.audit.for_request(&request_id)
        } else {
            Vec::new()
        };
        if !ids.is_empty() {
            for id in ids {
                let update = self
                    .audit
                    .update(&call.host, &id, |r| {
                        if attempt_index.is_none() {
                            r.request_error_code.clone_from(&response_error);
                            return;
                        }
                        if Some(r.attempt_index) != attempt_index || r.admitted_at_ms.is_none() {
                            return;
                        }
                        if let Some(metadata) = &response_metadata
                            && r.actual_account_id.as_deref()
                                == Some(metadata.provider_account_id.as_str())
                            && let Some(selection) = &metadata.selection_observation
                        {
                            r.account_selection_wait_ms = Some(selection.account_selection_wait_ms);
                        }
                        if response_error.is_some() {
                            r.error_code.clone_from(&response_error);
                        }
                    })
                    .await;
                if let Err(error) = update {
                    report_error(&call.host, &request_id, &error).await;
                }
            }
        }
        drop(guard);
        result
    }

    async fn websocket(
        &self,
        call: WebSocketCall,
    ) -> Result<Option<WebSocketMessage>, PluginFault> {
        let outgoing = call.direction == WebSocketDirection::Outgoing;
        let Some(mut message) = call.next.run(call.message).await? else {
            return Ok(None);
        };
        if !outgoing
            || message.kind != WebSocketKind::Text
            || !self.policy().awaiting_replay_frame(self.now())
        {
            return Ok(Some(message));
        }
        let bytes = message.payload.collect().await?;
        let replacement = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|mut frame| {
                if frame.get("type")?.as_str()? != "error"
                    || frame.get("status")?.as_u64()? != 429
                    || frame.pointer("/error/code")?.as_str()? != "concurrency_queue_timeout"
                {
                    return None;
                }
                let request_id = frame.pointer("/headers/x-request-id")?.as_str()?;
                if !self.policy().offer_replay(request_id, self.now()) {
                    return None;
                }
                // 在交付边界投影客户端恢复信号，宿主诊断仍保留真实排队失败
                frame["status"] = json!(400);
                frame["error"] = json!({
                    "type":"invalid_request_error",
                    "code":"previous_response_not_found",
                    "message":"Replay the full request to continue on an available account.",
                    "param":"previous_response_id"
                });
                serde_json::to_vec(&frame).ok()
            });
        message.payload = WebSocketPayload::from_bytes(replacement.unwrap_or(bytes));
        Ok(Some(message))
    }

    async fn schedule(
        &self,
        call: TypedCall<AccountScheduleRequest>,
    ) -> Result<TypedReply<AccountScheduleDecision>, PluginFault> {
        let action = self
            .policy()
            .schedule(&call.request, &self.config, self.now(), wall_ms());
        let decision = match action {
            Action::Delegate => AccountScheduleDecision::Delegate,
            Action::Pick { account_id } => AccountScheduleDecision::Pick { account_id },
            Action::RecordOnly(record) => {
                if let Err(error) = scheduling_audit(self.audit.append(&call.host, record)).await {
                    self.audit.note_failure(&error);
                    report_error(&call.host, &call.request.request_id, &error).await;
                }
                AccountScheduleDecision::Delegate
            }
            Action::CandidateUnavailable { record_id } => {
                if let Err(error) =
                    scheduling_audit(self.audit.update(&call.host, &record_id, |r| {
                        r.stage = "candidate_unavailable".into();
                    }))
                    .await
                {
                    report_error(&call.host, &call.request.request_id, &error).await;
                }
                AccountScheduleDecision::Delegate
            }
            Action::Propose(record) => {
                let mut guard = IntentGuard {
                    policy: self.policy.clone(),
                    request_id: call.request.request_id.clone(),
                    record_id: record.id.clone(),
                    committed: false,
                };
                match scheduling_audit(self.audit.append(&call.host, record.clone())).await {
                    Ok(()) => {
                        guard.committed = true;
                        AccountScheduleDecision::Pick {
                            account_id: record
                                .proposed_account_id
                                .clone()
                                .expect("迁移意图包含候选账号"),
                        }
                    }
                    Err(error) => {
                        // 记录没能落地就不发起迁移，客户端仍沿用宿主原等待及超时行为
                        self.audit.note_failure(&error);
                        report_error(&call.host, &call.request.request_id, &error).await;
                        AccountScheduleDecision::Delegate
                    }
                }
            }
        };
        Ok(TypedReply::new(decision))
    }

    async fn reconcile(&self, host: &HostClient) -> Result<(), PluginFault> {
        for record in self.audit.pending(host).await? {
            self.audit
                .update(host, &record.id, |r| {
                    r.last_checked_at_ms = Some(wall_ms());
                })
                .await?;
            if record.terminal_status == "pending" {
                match host
                    .service::<GetRecord>(GetUsageRecord {
                        id: record.request_id.clone(),
                    })
                    .await
                {
                    Ok(usage) if usage.id == record.request_id => {
                        if usage.data.outcome == UsageOutcome::Running {
                            continue;
                        }
                        self.audit
                            .update(host, &record.id, |r| {
                                r.outcome = serde_json::to_value(usage.data.outcome)
                                    .ok()
                                    .and_then(|v| v.as_str().map(str::to_owned));
                                r.completed_at_ms = usage
                                    .data
                                    .completed_at_ms
                                    .and_then(|ms| u64::try_from(ms).ok());
                                r.terminal_status = "known".into();
                            })
                            .await?;
                    }
                    _ => {
                        // 选号阶段可能尚未创建请求行；超过最大排队窗口仍不可读时明确保留缺口
                        if wall_ms().saturating_sub(record.decision_at_ms) >= 180_000 {
                            self.audit
                                .update(host, &record.id, |r| {
                                    r.terminal_status = "unavailable".into();
                                })
                                .await?;
                        } else {
                            continue;
                        }
                    }
                }
            }
            if record.evidence_status != "pending" || record.evidence_attempts >= 3 {
                continue;
            }
            self.audit
                .update(host, &record.id, |r| {
                    r.evidence_attempts = r.evidence_attempts.saturating_add(1);
                })
                .await?;
            match diagnostics::fetch(host, &record).await {
                Ok(evidence) => {
                    self.audit
                        .update(host, &record.id, |r| {
                            if r.actual_account_id.is_none() && evidence.actual_account_id.is_some()
                            {
                                r.actual_account_id.clone_from(&evidence.actual_account_id);
                                r.stage = if r.actual_account_id == r.proposed_account_id {
                                    "admitted_by_trace"
                                } else {
                                    "different_account"
                                }
                                .into();
                            }
                            r.from_account_id.clone_from(&evidence.from_account_id);
                            r.binding_changed = evidence.binding_changed;
                            if evidence.account_selection_wait_ms.is_some() {
                                r.account_selection_wait_ms = evidence.account_selection_wait_ms;
                            }
                            r.account_queue_wait_ms = evidence.account_queue_wait_ms;
                            r.request_error_code
                                .clone_from(&evidence.request_error_code);
                            r.evidence_status = if evidence.binding_changed.is_some() {
                                "verified"
                            } else {
                                "unavailable"
                            }
                            .into();
                        })
                        .await?;
                }
                Err(_) => {
                    // 诊断可能晚于请求终态落库；有限次数补齐，缺失不解释为未发生切换
                    self.audit
                        .update(host, &record.id, |r| {
                            if r.evidence_attempts >= 3 {
                                r.evidence_status = "unavailable".into();
                            }
                        })
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn history(
        &self,
        host: &HostClient,
        query: HistoryQuery<'_>,
    ) -> Result<Value, PluginFault> {
        let as_of = query.as_of;
        let limit = query.limit;
        let history = self.audit.query(host, query).await?;
        let records: Vec<HistoryEntry> = history
            .records
            .into_iter()
            .map(|record| HistoryEntry {
                decision_time_utc: format_utc(Some(record.decision_at_ms)),
                admitted_time_utc: format_utc(record.admitted_at_ms),
                completed_time_utc: format_utc(record.completed_at_ms),
                record,
            })
            .collect();
        Ok(json!({
            "records":records, "configuration":self.config,
            "pagination":{"page":history.page,"pageSize":limit,"total":history.total},
            "asOf":as_of,
            "storage":{"incompleteWrites":history.incomplete,"automaticDeletion":false,"lastWriteError":self.audit.last_write_error()},
            "计时口径":"从首次选号开始，其他请求完成迁移后重新计算等待",
            "绑定证据":"bindingChanged 为 null 表示尚未核实，admitted 只确认实际取得账号名额"
        }))
    }
}

// 父调用取消时同步归还协调位置，不能启动已经脱离父调用的宿主回调
/// 父请求结束时归还内存协调位置
struct RequestGuard<'a> {
    audit: &'a Audit,
    policy: Arc<Mutex<Policy>>,
    request_id: String,
}
impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        self.audit.forget(&self.request_id);
        self.policy
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .finish(&self.request_id);
    }
}

/// 持久化取消或失败时撤销尚未返回宿主的迁移意图
struct IntentGuard {
    policy: Arc<Mutex<Policy>>,
    request_id: String,
    record_id: String,
    committed: bool,
}
impl Drop for IntentGuard {
    fn drop(&mut self) {
        if !self.committed {
            self.policy
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .abandon(&self.request_id, &self.record_id);
        }
    }
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

fn format_utc(timestamp_ms: Option<u64>) -> Option<String> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp_ms?) * 1_000_000)
        .ok()?
        .format(&Rfc3339)
        .ok()
}

fn error_code(error: &PluginFault) -> String {
    serde_json::to_value(error.code)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "plugin_error".into())
}

async fn scheduling_audit(
    operation: impl Future<Output = Result<(), PluginFault>> + Send,
) -> Result<(), PluginFault> {
    tokio::time::timeout(Duration::from_millis(800), operation)
        .await
        .unwrap_or_else(|_| {
            Err(PluginFault::new(
                ErrorCode::Timeout,
                "切换记录保存超时，保持原绑定等待",
            ))
        })
}

async fn report_error(host: &HostClient, request_id: &str, error: &PluginFault) {
    let _ = tokio::time::timeout(
        Duration::from_millis(100),
        metadata::<_, LogResult>(
            host,
            "host.log",
            LogRequest {
                event: "session_failover.audit_error".into(),
                level: LogLevel::Warn,
                fields: BTreeMap::from([
                    ("requestId".into(), json!(request_id)),
                    ("errorCode".into(), json!(error_code(error))),
                ]),
            },
        ),
    )
    .await;
}

fn management_registration() -> ManagementRegistration {
    ManagementRegistration {
        routes: [
            ("GET", "history"),
            ("GET", "settings"),
            ("POST", "settings"),
            ("GET", "resources"),
        ]
        .into_iter()
        .map(|(method, path)| ManagementRoute {
            method: method.into(),
            path: path.into(),
            request_content_types: if method == "POST" {
                vec!["application/json".into()]
            } else {
                Vec::new()
            },
            response_content_types: vec!["application/json".into()],
        })
        .collect(),
        resources: ["web/index.html", "web/app.js", "web/app.css"]
            .into_iter()
            .map(|path| ManagementResource {
                path: path.into(),
                public: false,
            })
            .collect(),
        pages: vec![ManagementPage {
            id: "history".into(),
            title: "会话切换记录".into(),
            description: Some("管理插件设置，查看选号等待与改绑结果".into()),
            entry: "web/index.html".into(),
            icon: None,
        }],
        callbacks: Vec::new(),
    }
}

fn command_registration() -> CommandRegistration {
    CommandRegistration {
        commands: vec![CommandDescriptor {
            name: "history".into(),
            description: "查看会话换号、改绑证据及最终请求结果".into(),
            parameters: vec![
                CommandParameter {
                    name: "request-id".into(),
                    description: "按模型请求 ID 筛选".into(),
                    value_type: CommandParameterType::String,
                    required: false,
                    sensitive: false,
                    default: None,
                },
                CommandParameter {
                    name: "page".into(),
                    description: "历史页码，从 1 开始".into(),
                    value_type: CommandParameterType::Int,
                    required: false,
                    sensitive: false,
                    default: Some(CommandValue::Int(1)),
                },
                CommandParameter {
                    name: "limit".into(),
                    description: "每页记录数，1～500，不影响历史保留".into(),
                    value_type: CommandParameterType::Int,
                    required: false,
                    sensitive: false,
                    default: Some(CommandValue::Int(50)),
                },
            ],
        }],
    }
}

async fn command(
    app: &App,
    call: TypedCall<CommandInvocation>,
) -> Result<TypedReply<CommandResult>, PluginFault> {
    if call.request.name != "history" {
        return Err(fault("未知插件命令"));
    }
    let request_id = match call.request.arguments.get("request-id") {
        Some(CommandValue::String(id)) => Some(id.as_str()),
        None => None,
        _ => return Err(fault("请求 ID 参数格式无效")),
    };
    let limit = match call.request.arguments.get("limit") {
        Some(CommandValue::Int(n)) if (1..=500).contains(n) => *n as usize,
        None => 50,
        _ => return Err(fault("返回记录数须为 1～500")),
    };
    let page = match call.request.arguments.get("page") {
        Some(CommandValue::Int(n)) if *n > 0 => {
            usize::try_from(*n).map_err(|_| fault("历史页码超限"))?
        }
        None => 1,
        _ => return Err(fault("历史页码须大于 0")),
    };
    let value = app
        .history(
            &call.host,
            HistoryQuery {
                request_id,
                result: "all",
                page,
                limit,
                as_of: wall_ms(),
            },
        )
        .await?;
    Ok(TypedReply::new(CommandResult {
        stdout: format!(
            "{}\n",
            serde_json::to_string_pretty(&value).map_err(|_| fault("记录编码失败"))?
        ),
        stderr: String::new(),
        exit_code: 0,
        accounts: Vec::new(),
    }))
}

async fn management(
    app: &App,
    call: TypedCall<ManagementRequest>,
) -> Result<TypedReply<ManagementResponse>, PluginFault> {
    if matches!(call.request.path.as_str(), "settings" | "resources") {
        return app
            .settings
            .handle(&call.host, &call.request, &call.payload)
            .await;
    }
    let pairs: BTreeMap<String, String> = parse_query(call.request.query.as_bytes())
        .into_owned()
        .collect();
    let limit = pairs
        .get("limit")
        .map(|s| s.parse::<usize>())
        .transpose()
        .map_err(|_| fault("记录数格式无效"))?
        .unwrap_or(50);
    if call.request.method != "GET" || call.request.path != "history" || !(1..=500).contains(&limit)
    {
        return Err(fault("历史查询参数无效"));
    }
    let page = pairs
        .get("page")
        .map(|s| s.parse::<usize>())
        .transpose()
        .map_err(|_| fault("历史页码无效"))?
        .unwrap_or(1);
    let result = pairs.get("result").map(String::as_str).unwrap_or("all");
    let as_of = pairs
        .get("asOf")
        .map(|s| s.parse::<u64>())
        .transpose()
        .map_err(|_| fault("历史时间锚点无效"))?
        .unwrap_or_else(wall_ms);
    if page == 0 || !matches!(result, "all" | "changed" | "unchanged" | "unknown") {
        return Err(fault("历史查询条件无效"));
    }
    let value = app
        .history(
            &call.host,
            HistoryQuery {
                request_id: pairs
                    .get("requestId")
                    .filter(|s| !s.is_empty())
                    .map(String::as_str),
                result,
                page,
                limit,
                as_of,
            },
        )
        .await?;
    Ok(TypedReply::new(ManagementResponse {
        status: 200,
        content_type: "application/json".into(),
        headers: vec![MiddlewareHeader {
            name: "cache-control".into(),
            value: b"no-store".to_vec(),
        }],
    })
    .with_payload(serde_json::to_vec(&value).map_err(|_| fault("记录编码失败"))?))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut session = PluginSession::accept(
        tokio::io::stdin(),
        tokio::io::stdout(),
        SessionConfig::default(),
    )
    .await?;
    let manifest = Manifest::from_author_slice(include_bytes!("../plugin.json"))?;
    if session.handshake().plugin_id != manifest.plugin_id()?
        || session.handshake().contributes != manifest.contributes
    {
        return Err("插件握手身份不匹配".into());
    }
    let config: Config = serde_json::from_value(session.handshake().configuration.clone())?;
    config.validate()?;
    session.set_call_budgets(CallBudgets::from([(Stage::Maintenance, 30_000)]))?;
    let app = Arc::new(App {
        settings: SettingsControl::new(
            session.handshake().instance_id.clone(),
            session.handshake().artifact_sha256.clone(),
            manifest
                .contributes
                .get(&Capability::Middleware)
                .ok_or("缺少中间件声明")?
                .id
                .clone(),
            manifest
                .contributes
                .get(&Capability::Scheduler)
                .ok_or("缺少调度声明")?
                .id
                .clone(),
        ),
        audit: Audit::new(),
        config,
        started: Instant::now(),
        policy: Arc::new(Mutex::new(Policy::default())),
    });
    let middleware_app = app.clone();
    let schedule_app = app.clone();
    let management_app = app.clone();
    let command_app = app.clone();
    let plugin = PluginBuilder::from_json(include_bytes!("../plugin.json"))?
        .middleware(move |call: MiddlewareCall| {
            let app = middleware_app.clone();
            async move {
                match call {
                    MiddlewareCall::Request(call) => app.request(*call).await.map(Into::into),
                    MiddlewareCall::WebSocket(call) => app.websocket(*call).await.map(Into::into),
                    other => other.forward().await,
                }
            }
        })?
        .on(methods::SCHEDULE_ACCOUNT, move |call| {
            let app = schedule_app.clone();
            async move { app.schedule(call).await }
        })?
        .management(management_registration(), move |call| {
            let app = management_app.clone();
            async move { management(&app, call).await }
        })?
        .command_line(command_registration(), move |call| {
            let app = command_app.clone();
            async move { command(&app, call).await }
        })?
        .on(methods::RECONCILE, move |call| {
            let app = app.clone();
            async move {
                app.reconcile(&call.host).await?;
                Ok(TypedReply::new(Empty {}))
            }
        })?
        .build()?;
    session.run(plugin).await?;
    Ok(())
}
