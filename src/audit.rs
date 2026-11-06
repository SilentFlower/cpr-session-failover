//! 有界切换日志与宿主私有状态 CAS，区分选号意图、取得账号和最终结果

use std::collections::VecDeque;
use std::time::Duration;

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::{
        host::{
            LogLevel, LogRequest, LogResult, StateGetRequest, StateGetResult, StatePutRequest,
            StatePutResult,
        },
        policy::AccountScheduleRequest,
    },
    client::HostClient,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::fault;

const MAX_JOURNAL_BYTES: usize = 240 * 1024;

/// 一次切换或受限续写的排查记录；未知事实保持空值
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub id: String,
    pub request_id: String,
    pub attempt_index: u32,
    pub session_hash: String,
    pub client_key_id: String,
    pub model: Option<String>,
    pub decision_at_ms: u64,
    pub scheduler_wait_ms: u64,
    pub stage: String,
    pub reason: String,
    pub proposed_account_id: Option<String>,
    pub actual_account_id: Option<String>,
    pub from_account_id: Option<String>,
    pub admitted_at_ms: Option<u64>,
    pub completed_at_ms: Option<u64>,
    pub outcome: Option<String>,
    pub error_code: Option<String>,
    pub request_error_code: Option<String>,
    pub account_selection_wait_ms: Option<u64>,
    pub account_queue_wait_ms: Option<u64>,
    pub binding_changed: Option<bool>,
    pub evidence_status: String,
    pub evidence_attempts: u8,
    pub terminal_status: String,
    pub last_checked_at_ms: Option<u64>,
}

impl Record {
    /// 构造待选择账号的意图，不能据此断言绑定已经变化
    /// @param input 宿主调度请求
    /// @param session_hash 带 Key 作用域的会话摘要
    /// @param key_id 已认证的 Key ID
    /// @param waited 首次选号后持续等待的毫秒数
    /// @param wall_ms 记录时间，UTC 毫秒
    /// @param account_id 计划申请的账号
    /// @return 尚未取得租约的切换记录
    pub fn proposed(
        input: &AccountScheduleRequest,
        session_hash: &str,
        key_id: &str,
        waited: u64,
        wall_ms: u64,
        account_id: &str,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            request_id: input.request_id.clone(),
            attempt_index: input.attempt_index,
            session_hash: session_hash.into(),
            client_key_id: key_id.into(),
            model: input.model.as_ref().map(|v| v.chars().take(128).collect()),
            decision_at_ms: wall_ms,
            scheduler_wait_ms: waited,
            stage: "proposed".into(),
            reason: "wait_threshold".into(),
            proposed_account_id: Some(account_id.into()),
            actual_account_id: None,
            from_account_id: None,
            admitted_at_ms: None,
            completed_at_ms: None,
            outcome: None,
            error_code: None,
            request_error_code: None,
            account_selection_wait_ms: None,
            account_queue_wait_ms: None,
            binding_changed: None,
            evidence_status: "pending".into(),
            evidence_attempts: 0,
            terminal_status: "pending".into(),
            last_checked_at_ms: None,
        }
    }

    /// 构造不能迁移的续写记录，不申请额外账号或伪造切换
    /// @param input 宿主调度请求
    /// @param session_hash 会话摘要
    /// @param key_id 已认证的 Key ID
    /// @param waited 已等待毫秒数
    /// @param wall_ms 记录时间，UTC 毫秒
    /// @param reason 阻止迁移的固定原因码
    /// @return 不改变宿主行为的排查记录
    pub fn blocked(
        input: &AccountScheduleRequest,
        session_hash: &str,
        key_id: &str,
        waited: u64,
        wall_ms: u64,
        reason: &str,
    ) -> Self {
        let mut record = Self::proposed(input, session_hash, key_id, waited, wall_ms, "");
        record.stage = "blocked".into();
        record.reason = reason.into();
        record.proposed_account_id = None;
        record.binding_changed = Some(false);
        record.evidence_status = "not_required".into();
        record
    }
}

/// 持久化的有限历史，字节上限独立于用户选择的条数
#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Journal {
    pub records: VecDeque<Record>,
}

impl Journal {
    /// 插入幂等记录并淘汰超出保留预算的旧记录
    /// @param record 待写入记录，重复 ID 不追加
    /// @param limit 用户设置的最大记录数
    /// @return 编码或单记录超限时返回错误
    pub fn append(&mut self, record: Record, limit: usize) -> Result<(), PluginFault> {
        if !self.records.iter().any(|r| r.id == record.id) {
            self.records.push_back(record);
        }
        self.trim(limit)
    }

    /// 应用记录变更后的条数和字节约束
    /// @param limit 最大保留记录数
    /// @return 单条记录无法放入状态预算时返回错误
    pub fn trim(&mut self, limit: usize) -> Result<(), PluginFault> {
        while self.records.len() > limit {
            self.records.pop_front();
        }
        while serde_json::to_vec(self)
            .map_err(|_| fault("切换记录编码失败"))?
            .len()
            > MAX_JOURNAL_BYTES
        {
            if self.records.len() <= 1 {
                return Err(fault("单条切换记录超过存储上限"));
            }
            self.records.pop_front();
        }
        Ok(())
    }
}

/// 宿主私有状态中的切换历史，本地锁只串行化日志写入
pub struct Audit {
    cache: Mutex<Journal>,
    limit: usize,
}

impl Audit {
    /// 创建日志存储，不在启动时调用尚未绑定父调用的宿主接口
    /// @param limit 最大保留条数
    /// @return 日志存储
    pub fn new(limit: usize) -> Self {
        Self {
            cache: Mutex::new(Journal::default()),
            limit,
        }
    }

    /// 在显式选号前持久化记录，存储失败时调用方必须撤销迁移意图
    /// @param host 当前父调用的宿主客户端
    /// @param record 切换或阻止记录
    /// @return CAS 成功后返回
    pub async fn append(&self, host: &HostClient, record: Record) -> Result<(), PluginFault> {
        self.mutate(host, |journal| journal.append(record.clone(), self.limit))
            .await?;
        emit(host, "session_failover.decision", &record).await;
        Ok(())
    }

    /// 更新现有切换记录，不把已淘汰记录重新插回历史
    /// @param host 当前父调用的宿主客户端
    /// @param id 精确的记录 ID
    /// @param update 可重复执行的纯状态变换
    /// @return 更新成功或记录已淘汰时返回
    pub async fn update(
        &self,
        host: &HostClient,
        id: &str,
        update: impl Fn(&mut Record),
    ) -> Result<(), PluginFault> {
        self.mutate(host, |journal| {
            if let Some(record) = journal.records.iter_mut().find(|r| r.id == id) {
                update(record);
            }
            journal.trim(self.limit)
        })
        .await?;
        let record = self
            .cache
            .lock()
            .await
            .records
            .iter()
            .find(|r| r.id == id)
            .cloned();
        if let Some(record) = record {
            emit(host, "session_failover.progress", &record).await;
        }
        Ok(())
    }

    /// 读取历史；管理查询从宿主刷新，不假定重启前的缓存仍然存在
    /// @param host 当前父调用的宿主客户端
    /// @return 当前保留的历史快照
    pub async fn list(&self, host: &HostClient) -> Result<Journal, PluginFault> {
        let mut cache = self.cache.lock().await;
        let (journal, _) = load(host).await?;
        *cache = journal.clone();
        Ok(journal)
    }

    /// 仅检查本代进程创建过的记录，普通请求完成不额外查询宿主数据库
    /// @param request_id 模型请求 ID
    /// @return 本地历史中匹配的记录 ID
    pub async fn for_request(&self, request_id: &str) -> Vec<String> {
        self.cache
            .lock()
            .await
            .records
            .iter()
            .filter(|r| r.request_id == request_id)
            .map(|r| r.id.clone())
            .collect()
    }

    async fn mutate(
        &self,
        host: &HostClient,
        change: impl Fn(&mut Journal) -> Result<(), PluginFault>,
    ) -> Result<(), PluginFault> {
        let mut cache = self.cache.lock().await;
        for _ in 0..3 {
            // 每次读取精确版本，覆盖配置切换时旧、新进程短暂并行的写入
            let (mut journal, version) = load(host).await?;
            change(&mut journal)?;
            let result: Result<StatePutResult, PluginFault> = metadata(
                host,
                "host.state.put",
                StatePutRequest {
                    namespace: "audit".into(),
                    key: "journal".into(),
                    value: serde_json::to_value(&journal).map_err(|_| fault("切换记录编码失败"))?,
                    expected_version: version,
                },
            )
            .await;
            match result {
                Ok(_) => {
                    *cache = journal;
                    return Ok(());
                }
                Err(error) if error.code == ErrorCode::Conflict => continue,
                Err(error) => return Err(error),
            }
        }
        Err(fault("切换记录存在并发修改，未提交本次变更"))
    }
}

async fn load(host: &HostClient) -> Result<(Journal, Option<u64>), PluginFault> {
    let result: StateGetResult = metadata(
        host,
        "host.state.get",
        StateGetRequest {
            namespace: "audit".into(),
            key: "journal".into(),
        },
    )
    .await?;
    match result.record {
        None => Ok((Journal::default(), None)),
        Some(record) if record.schema_version == 1 => Ok((
            serde_json::from_value(record.value).map_err(|_| fault("切换记录格式无效"))?,
            Some(record.version),
        )),
        Some(_) => Err(fault("切换记录版本不兼容")),
    }
}

/// 调用 SDK 已定义的元数据回调，拒绝意外正文
/// @param host 当前父调用的客户端
/// @param method SDK 中的固定方法名
/// @param input 该方法的已定义输入
/// @return 类型化回调结果
pub async fn metadata<I: Serialize, O: DeserializeOwned>(
    host: &HostClient,
    method: &str,
    input: I,
) -> Result<O, PluginFault> {
    let reply = host
        .call(
            method,
            serde_json::to_value(input).map_err(|_| fault("宿主回调编码失败"))?,
            Vec::new(),
        )
        .await
        .map_err(|e| e.into_plugin_fault())?;
    if !reply.payload.is_empty() {
        return Err(fault("宿主回调返回意外正文"));
    }
    serde_json::from_value(reply.result).map_err(|_| fault("宿主回调格式无效"))
}

/// 向宿主发送有界诊断事件；持久记录不依赖普通日志是否被采样丢弃
/// @param host 当前父调用的客户端
/// @param event 代码中固定的事件名
/// @param record 脱敏后的记录
/// @return 无
pub async fn emit(host: &HostClient, event: &str, record: &Record) {
    let Ok(Value::Object(fields)) = serde_json::to_value(record) else {
        return;
    };
    let _ = tokio::time::timeout(
        Duration::from_millis(100),
        metadata::<_, LogResult>(
            host,
            "host.log",
            LogRequest {
                event: event.into(),
                level: LogLevel::Info,
                fields: fields.into_iter().collect(),
            },
        ),
    )
    .await;
}
