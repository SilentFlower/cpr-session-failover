//! 持久切换历史与宿主状态 CAS，不按条数淘汰记录

use std::collections::{BTreeMap, HashMap};
use std::future::poll_fn;
use std::sync::{Mutex as StdMutex, MutexGuard};
use std::task::Poll;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_account_id: Option<String>,
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
            original_account_id: None,
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

/// 已部署版本的历史，保持只读以便直接升级
#[derive(Clone, Default, Deserialize)]
struct Journal {
    records: Vec<Record>,
}

/// 索引分页只限制一次读写的工作量，不限制历史保留条数
const INDEX_PAGE_ENTRIES: usize = 128;
const DISCOVERY_BATCH: usize = 64;

#[derive(Default, Deserialize, Serialize)]
struct Head {
    page: u64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexEntry {
    id: String,
    request_id: String,
    at: u64,
    committed: bool,
}

#[derive(Default, Deserialize, Serialize)]
struct IndexPage {
    entries: Vec<IndexEntry>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredRecord {
    page: u64,
    record: Record,
}

struct Catalog {
    entries: BTreeMap<String, IndexEntry>,
    legacy: HashMap<String, Record>,
    incomplete: Vec<(u64, IndexEntry)>,
}

#[derive(Default)]
struct Cache {
    requests: HashMap<String, Vec<String>>,
    versions: HashMap<String, u64>,
    pending: HashMap<String, Record>,
    repair_cursor: usize,
    last_write_error: Option<String>,
}

/// 历史查询条件，页大小只影响响应，不触发删除
pub struct HistoryQuery<'a> {
    pub request_id: Option<&'a str>,
    pub result: &'a str,
    pub page: usize,
    pub limit: usize,
    pub as_of: u64,
}

/// 同一时间锚点下的历史页，缺失的未提交记录单独报告
pub struct HistoryPage {
    pub records: Vec<Record>,
    pub page: usize,
    pub total: usize,
    pub incomplete: usize,
}

/// 宿主私有状态中的永久历史，达到宿主配额时拒绝新增而不删除旧记录
#[derive(Default)]
pub struct Audit {
    append_lock: Mutex<()>,
    cache: StdMutex<Cache>,
}

impl Audit {
    /// 创建存储入口，不在启动阶段调用宿主
    /// @return 等待首个父调用的存储实例
    pub fn new() -> Self {
        Self::default()
    }

    fn cache(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 开始跟踪已注册的活动请求，普通请求结束无需扫描历史
    /// @param request_id 已由策略接受的请求 ID
    /// @return 无
    pub fn track(&self, request_id: &str) {
        self.cache().requests.entry(request_id.into()).or_default();
    }

    /// 随父请求取消或结束释放活动索引
    /// @param request_id 结束的请求 ID
    /// @return 无
    pub fn forget(&self, request_id: &str) {
        self.cache().requests.remove(request_id);
    }

    /// 查询当前活动请求的记录，不读取全部持久历史
    /// @param request_id 请求 ID
    /// @return 当前进程创建的记录 ID
    pub fn for_request(&self, request_id: &str) -> Vec<String> {
        self.cache()
            .requests
            .get(request_id)
            .cloned()
            .unwrap_or_default()
    }

    /// 保留本进程最近一次写入错误的安全提示，不另写状态或暴露宿主原文
    /// @param error 宿主或持久化预算错误
    /// @return 无
    pub fn note_failure(&self, error: &PluginFault) {
        self.cache().last_write_error = Some(
            if error.code == ErrorCode::Capacity {
                "宿主历史存储配额已用尽，本次未发起迁移，已有记录保留"
            } else {
                "切换记录未能完整保存，本次未发起迁移，已有记录保留"
            }
            .into(),
        );
    }

    /// 读取本进程的最近写入错误，后续成功追加会清除提示
    /// @return 安全错误文案或空值
    pub fn last_write_error(&self) -> Option<String> {
        self.cache().last_write_error.clone()
    }

    fn remember(&self, record: &Record, version: u64) {
        let mut cache = self.cache();
        if cache.versions.get(&record.id).is_some_and(|v| *v > version) {
            return;
        }
        cache.versions.insert(record.id.clone(), version);
        if record.terminal_status == "pending"
            || (record.evidence_status == "pending" && record.evidence_attempts < 3)
        {
            cache.pending.insert(record.id.clone(), record.clone());
        } else {
            cache.pending.remove(&record.id);
        }
    }

    /// 先预约可发现的索引，再保存记录及提交标记，全部成功后才允许选号
    /// @param host 当前父调用的宿主客户端
    /// @param record 新记录，重复 ID 幂等
    /// @return 配额、冲突或取消失败时不提交迁移
    pub async fn append(&self, host: &HostClient, record: Record) -> Result<(), PluginFault> {
        let _lock = self.append_lock.lock().await;
        let key = format!("record-{}", record.id);
        let (stored, version) = get::<StoredRecord>(host, "history", &key).await?;
        let (stored, version) = match (stored, version) {
            (Some(stored), Some(version)) => (stored, version),
            _ => {
                let page = reserve(host, &record).await?;
                let stored = StoredRecord {
                    page,
                    record: record.clone(),
                };
                match put(host, &key, &stored, None).await {
                    Ok(version) => (stored, version),
                    Err(error) if error.code == ErrorCode::Conflict => {
                        let (stored, version) = get::<StoredRecord>(host, "history", &key).await?;
                        (
                            stored.ok_or_else(|| fault("切换记录并发写入后不可读"))?,
                            version.ok_or_else(|| fault("切换记录缺少版本"))?,
                        )
                    }
                    Err(error) => return Err(error),
                }
            }
        };
        commit(host, stored.page, &stored.record.id).await?;
        self.cache().last_write_error = None;
        self.remember(&stored.record, version);
        if let Some(ids) = self.cache().requests.get_mut(&stored.record.request_id)
            && !ids.contains(&stored.record.id)
        {
            ids.push(stored.record.id.clone());
        }
        emit(host, "session_failover.decision", &stored.record).await;
        Ok(())
    }

    /// 按记录精确版本更新，兼容旧日志且不覆盖并发更新
    /// @param host 当前父调用的宿主客户端
    /// @param id 精确记录 ID
    /// @param update 可重复执行的纯变换
    /// @return 精确 CAS 成功或返回明确错误
    pub async fn update(
        &self,
        host: &HostClient,
        id: &str,
        update: impl Fn(&mut Record),
    ) -> Result<(), PluginFault> {
        let key = format!("record-{id}");
        for _ in 0..3 {
            let (stored, version) = get::<StoredRecord>(host, "history", &key).await?;
            let Some(mut stored) = stored else {
                let (legacy, _) = get::<Journal>(host, "audit", "journal").await?;
                let record = legacy
                    .unwrap_or_default()
                    .records
                    .into_iter()
                    .find(|r| r.id == id)
                    .ok_or_else(|| fault("待更新的切换记录不存在"))?;
                // 旧日志保持原样，首次更新复制单条后继续使用相同的 CAS 路径
                self.append(host, record).await?;
                continue;
            };
            update(&mut stored.record);
            match put(host, &key, &stored, version).await {
                Ok(version) => {
                    self.remember(&stored.record, version);
                    emit(host, "session_failover.progress", &stored.record).await;
                    return Ok(());
                }
                Err(error) if error.code == ErrorCode::Conflict => continue,
                Err(error) => return Err(error),
            }
        }
        Err(fault("切换记录存在并发修改，未提交本次变更"))
    }

    /// 按时间锚点及条件读取一页，所有页均可访问，不自动淘汰历史
    /// @param host 当前管理调用的宿主客户端
    /// @param query 筛选及分页条件
    /// @return 历史页、匹配总数及未完成写入数
    pub async fn query(
        &self,
        host: &HostClient,
        query: HistoryQuery<'_>,
    ) -> Result<HistoryPage, PluginFault> {
        let catalog = catalog(host).await?;
        let incomplete = catalog.incomplete.len();
        let mut entries: Vec<_> = catalog
            .entries
            .values()
            .filter(|e| e.at <= query.as_of && query.request_id.is_none_or(|id| e.request_id == id))
            .cloned()
            .collect();
        entries.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| b.id.cmp(&a.id)));
        let mut total = entries.len();
        let mut page = query.page.min(total.div_ceil(query.limit).max(1));
        let records = if query.result == "all" {
            let selection = entries
                .into_iter()
                .skip((page - 1) * query.limit)
                .take(query.limit)
                .collect();
            read_records(host, &catalog, selection)
                .await?
                .into_iter()
                .map(|(record, _)| record)
                .collect()
        } else {
            let records = read_records(host, &catalog, entries).await?;
            let filtered: Vec<_> = records
                .into_iter()
                .map(|(record, _)| record)
                .filter(|r| match query.result {
                    "changed" => r.binding_changed == Some(true),
                    "unchanged" => r.binding_changed == Some(false),
                    "unknown" => r.binding_changed.is_none(),
                    _ => false,
                })
                .collect();
            total = filtered.len();
            page = query.page.min(total.div_ceil(query.limit).max(1));
            filtered
                .into_iter()
                .skip((page - 1) * query.limit)
                .take(query.limit)
                .collect()
        };
        Ok(HistoryPage {
            records,
            page,
            total,
            incomplete,
        })
    }

    /// 分批发现重启前未终结的记录，周期维护不重读全部历史正文
    /// @param host 当前维护调用的客户端
    /// @return 最久未检查的至多三条记录
    pub async fn pending(&self, host: &HostClient) -> Result<Vec<Record>, PluginFault> {
        let mut catalog = catalog(host).await?;
        let repair_start = {
            let mut cache = self.cache();
            let start = cache.repair_cursor;
            cache.repair_cursor = start.saturating_add(3) % catalog.incomplete.len().max(1);
            start
        };
        for (page, entry) in catalog
            .incomplete
            .iter()
            .cycle()
            .skip(repair_start)
            .take(3.min(catalog.incomplete.len()))
        {
            let (stored, _) =
                get::<StoredRecord>(host, "history", &format!("record-{}", entry.id)).await?;
            if let Some(stored) = stored {
                // 记录已保存但确认帧丢失时补齐索引，不重放选号，也不删除缺失预约
                commit(host, *page, &stored.record.id).await?;
                catalog.entries.insert(entry.id.clone(), entry.clone());
            }
        }
        let unseen: Vec<_> = {
            let cache = self.cache();
            catalog
                .entries
                .values()
                .filter(|e| !cache.versions.contains_key(&e.id))
                .take(DISCOVERY_BATCH)
                .cloned()
                .collect()
        };
        for (record, version) in read_records(host, &catalog, unseen).await? {
            // 精确版本阻止迟到的发现快照覆盖并发取得的终态
            self.remember(&record, version);
        }
        let mut pending: Vec<_> = self.cache().pending.values().cloned().collect();
        pending.sort_by_key(|r| r.last_checked_at_ms.unwrap_or(0));
        let mut result = Vec::new();
        for record in pending.into_iter().take(3) {
            if let Some((current, version)) = read_record(host, &catalog, &record.id).await? {
                self.remember(&current, version);
                if current.terminal_status == "pending"
                    || (current.evidence_status == "pending" && current.evidence_attempts < 3)
                {
                    result.push(current);
                }
            }
        }
        Ok(result)
    }
}

async fn reserve(host: &HostClient, record: &Record) -> Result<u64, PluginFault> {
    for _ in 0..6 {
        let (head, head_version) = get::<Head>(host, "history", "head").await?;
        let head = head.unwrap_or_default();
        let key = format!("index-{}", head.page);
        let (page, version) = get::<IndexPage>(host, "history", &key).await?;
        let mut page = page.unwrap_or_default();
        if page.entries.iter().any(|e| e.id == record.id) {
            return Ok(head.page);
        }
        if page.entries.len() >= INDEX_PAGE_ENTRIES {
            // 先公布下一页位置，取消不会留下索引不可达的孤立记录
            let next = Head {
                page: head.page + 1,
            };
            match put(host, "head", &next, head_version).await {
                Ok(_) => continue,
                Err(error) if error.code == ErrorCode::Conflict => continue,
                Err(error) => return Err(error),
            }
        }
        page.entries.push(IndexEntry {
            id: record.id.clone(),
            request_id: record.request_id.clone(),
            at: record.decision_at_ms,
            committed: false,
        });
        match put(host, &key, &page, version).await {
            Ok(_) => return Ok(head.page),
            Err(error) if error.code == ErrorCode::Conflict => continue,
            Err(error) => return Err(error),
        }
    }
    Err(fault("历史索引存在并发修改，未提交本次记录"))
}

async fn commit(host: &HostClient, page: u64, id: &str) -> Result<(), PluginFault> {
    let key = format!("index-{page}");
    for _ in 0..3 {
        let (index, version) = get::<IndexPage>(host, "history", &key).await?;
        let mut index = index.ok_or_else(|| fault("切换记录缺少预约索引"))?;
        let entry = index
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| fault("切换记录索引不存在"))?;
        if entry.committed {
            return Ok(());
        }
        entry.committed = true;
        match put(host, &key, &index, version).await {
            Ok(_) => return Ok(()),
            Err(error) if error.code == ErrorCode::Conflict => continue,
            Err(error) => return Err(error),
        }
    }
    Err(fault("历史索引提交发生并发冲突"))
}

async fn catalog(host: &HostClient) -> Result<Catalog, PluginFault> {
    let (legacy, _) = get::<Journal>(host, "audit", "journal").await?;
    let legacy: HashMap<_, _> = legacy
        .unwrap_or_default()
        .records
        .into_iter()
        .map(|r| (r.id.clone(), r))
        .collect();
    let mut catalog = Catalog {
        entries: BTreeMap::new(),
        legacy,
        incomplete: Vec::new(),
    };
    for record in catalog.legacy.values() {
        catalog.entries.insert(
            record.id.clone(),
            IndexEntry {
                id: record.id.clone(),
                request_id: record.request_id.clone(),
                at: record.decision_at_ms,
                committed: true,
            },
        );
    }
    let (head, _) = get::<Head>(host, "history", "head").await?;
    let head = head.unwrap_or_default();
    // 索引页数受宿主状态对象配额约束，损坏的头不能触发无界扫描
    if head.page >= 10_000 {
        return Err(fault("历史索引页号超出宿主配额"));
    }
    for number in 0..=head.page {
        let (page, _) = get::<IndexPage>(host, "history", &format!("index-{number}")).await?;
        let Some(page) = page else {
            if number < head.page {
                return Err(fault("历史索引页缺失"));
            }
            continue;
        };
        for entry in page.entries {
            if entry.committed {
                catalog.legacy.remove(&entry.id);
                catalog.entries.insert(entry.id.clone(), entry);
            } else {
                catalog.incomplete.push((number, entry));
            }
        }
    }
    Ok(catalog)
}

async fn read_record(
    host: &HostClient,
    catalog: &Catalog,
    id: &str,
) -> Result<Option<(Record, u64)>, PluginFault> {
    if let Some(record) = catalog.legacy.get(id) {
        // 维护与查询期间另一进程可能已经复制了旧记录，优先读取新状态
        let (stored, version) =
            get::<StoredRecord>(host, "history", &format!("record-{id}")).await?;
        return Ok(Some(match stored {
            Some(stored) => (stored.record, version.unwrap_or(0)),
            None => (record.clone(), 0),
        }));
    }
    let (stored, version) = get::<StoredRecord>(host, "history", &format!("record-{id}")).await?;
    Ok(stored.map(|stored| (stored.record, version.unwrap_or(0))))
}

async fn read_records(
    host: &HostClient,
    catalog: &Catalog,
    entries: Vec<IndexEntry>,
) -> Result<Vec<(Record, u64)>, PluginFault> {
    let mut result = Vec::with_capacity(entries.len());
    // 使用当前任务内的有界并行，不产生脱离父调用的后台读写
    for batch in entries.chunks(8) {
        let mut futures: Vec<_> = batch
            .iter()
            .map(|e| Box::pin(read_record(host, catalog, &e.id)))
            .collect();
        let mut values: Vec<Option<(Record, u64)>> = vec![None; batch.len()];
        poll_fn(|cx| {
            for (index, future) in futures.iter_mut().enumerate() {
                if values[index].is_some() {
                    continue;
                }
                match future.as_mut().poll(cx) {
                    Poll::Ready(Ok(Some(record))) => values[index] = Some(record),
                    Poll::Ready(Ok(None)) => {
                        return Poll::Ready(Err(fault("已提交的切换记录缺失")));
                    }
                    Poll::Ready(Err(error)) => {
                        return Poll::Ready(Err(error));
                    }
                    Poll::Pending => {}
                }
            }
            if values.iter().all(Option::is_some) {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        })
        .await?;
        result.extend(values.into_iter().flatten());
    }
    Ok(result)
}

async fn get<T: DeserializeOwned>(
    host: &HostClient,
    namespace: &str,
    key: &str,
) -> Result<(Option<T>, Option<u64>), PluginFault> {
    let result: StateGetResult = metadata(
        host,
        "host.state.get",
        StateGetRequest {
            namespace: namespace.into(),
            key: key.into(),
        },
    )
    .await?;
    match result.record {
        None => Ok((None, None)),
        Some(record) if record.schema_version == 1 => Ok((
            Some(serde_json::from_value(record.value).map_err(|_| fault("切换历史格式无效"))?),
            Some(record.version),
        )),
        Some(_) => Err(fault("切换历史版本不兼容")),
    }
}

async fn put<T: Serialize>(
    host: &HostClient,
    key: &str,
    value: &T,
    expected_version: Option<u64>,
) -> Result<u64, PluginFault> {
    let result: StatePutResult = metadata(
        host,
        "host.state.put",
        StatePutRequest {
            namespace: "history".into(),
            key: key.into(),
            value: serde_json::to_value(value).map_err(|_| fault("切换记录编码失败"))?,
            expected_version,
        },
    )
    .await?;
    Ok(result.version)
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
