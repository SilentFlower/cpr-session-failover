//! 插件 RPC 入口，组合严格绑定、延迟选号与持久切换记录

use std::{
    collections::BTreeMap,
    error::Error,
    future::Future,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cpr_session_failover::{
    audit::{Audit, Record, metadata},
    diagnostics, fault,
    identity::identify,
    policy::{Action, Config, Policy},
};
use form_urlencoded::parse as parse_query;
use gateway_plugin_sdk::{
    CallBudgets, ErrorCode, Manifest, PluginFault, Stage,
    call::{
        host::{LogLevel, LogRequest, LogResult},
        management::{
            CommandDescriptor, CommandInvocation, CommandParameter, CommandParameterType,
            CommandRegistration, CommandResult, CommandValue, ManagementRegistration,
            ManagementRequest, ManagementResponse, ManagementRoute,
        },
        middleware::{MiddlewareHeader, MiddlewareMount},
        policy::{AccountScheduleDecision, AccountScheduleRequest},
        services::usage::{GetRecord, GetUsageRecord, UsageOutcome},
    },
    client::{
        Empty, HostClient, MiddlewareResponse, PluginBuilder, PluginSession, RequestCall,
        SessionConfig, TypedCall, TypedReply, methods,
    },
};
use serde_json::{Value, json};

/// 每个插件会话共享的业务配置、单调时钟和审计入口
struct App {
    config: Config,
    started: Instant,
    policy: Arc<Mutex<Policy>>,
    audit: Audit,
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
            if runtime
                .get("max_waiting_per_account")
                .and_then(Value::as_u64)
                == Some(0)
            {
                identity.blocked_reason = Some("account_queue_disabled".into());
            } else if runtime
                .get("concurrency_wait_timeout_seconds")
                .and_then(Value::as_u64)
                .is_none_or(|seconds| seconds <= u64::from(self.config.wait_seconds))
            {
                identity.blocked_reason = Some("queue_budget_too_short".into());
            }
            if self.policy().register(
                &request_id,
                &call.request.head.client_key_id,
                identity,
                self.now(),
            ) {
                guard = Some(RequestGuard {
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
        let response_metadata = result
            .as_ref()
            .ok()
            .and_then(|response| response.metadata.clone());
        let response_error = result.as_ref().err().map(error_code);
        let ids = if attempt_index.is_some() || response_error.is_some() {
            self.audit.for_request(&request_id).await
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
                        report_error(&call.host, &call.request.request_id, &error).await;
                        AccountScheduleDecision::Delegate
                    }
                }
            }
        };
        Ok(TypedReply::new(decision))
    }

    async fn reconcile(&self, host: &HostClient) -> Result<(), PluginFault> {
        let journal = self.audit.list(host).await?;
        let mut records: Vec<&Record> = journal
            .records
            .iter()
            .filter(|r| {
                r.terminal_status == "pending"
                    || (r.evidence_status == "pending" && r.evidence_attempts < 3)
            })
            .collect();
        // 长请求不能一直占据维护批次，先处理最久未检查的记录
        records.sort_by_key(|r| r.last_checked_at_ms.unwrap_or(0));
        for record in records.into_iter().take(3) {
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
            match diagnostics::fetch(host, record).await {
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
        request_id: Option<&str>,
        limit: usize,
    ) -> Result<Value, PluginFault> {
        let journal = self.audit.list(host).await?;
        let records: Vec<Record> = journal
            .records
            .into_iter()
            .rev()
            .filter(|r| request_id.is_none_or(|id| r.request_id == id))
            .take(limit.min(self.config.history_limit))
            .collect();
        Ok(
            json!({"records": records, "configuration": self.config, "计时口径": "从首次选号开始；其他请求完成迁移后重新计算等待", "绑定证据": "bindingChanged 为 null 表示尚未核实，admitted 只确认实际取得账号名额"}),
        )
    }
}

// 父调用取消时同步归还协调位置，不能启动已经脱离父调用的宿主回调
/// 父请求结束时归还内存协调位置
struct RequestGuard {
    policy: Arc<Mutex<Policy>>,
    request_id: String,
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
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
        routes: vec![ManagementRoute {
            method: "GET".into(),
            path: "history".into(),
            request_content_types: Vec::new(),
            response_content_types: vec!["application/json".into()],
        }],
        resources: Vec::new(),
        pages: Vec::new(),
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
                    name: "limit".into(),
                    description: "返回最近记录数，最多 500".into(),
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
    let value = app.history(&call.host, request_id, limit).await?;
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
    let value = app
        .history(
            &call.host,
            pairs.get("requestId").map(String::as_str),
            limit,
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
        audit: Audit::new(config.history_limit),
        config,
        started: Instant::now(),
        policy: Arc::new(Mutex::new(Policy::default())),
    });
    let middleware_app = app.clone();
    let schedule_app = app.clone();
    let management_app = app.clone();
    let command_app = app.clone();
    let plugin = PluginBuilder::from_json(include_bytes!("../plugin.json"))?
        .middleware(move |call: RequestCall| {
            let app = middleware_app.clone();
            async move { app.request(call).await }
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
