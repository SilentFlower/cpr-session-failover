//! 单进程会话迁移协调，不保存或改写宿主账号绑定

use std::cmp::Ordering;
use std::collections::HashMap;

use gateway_plugin_sdk::call::policy::{AccountScheduleCandidate, AccountScheduleRequest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{audit::Record, identity::Identity};

const MAX_REQUESTS: usize = 2048;
const MAX_SESSIONS: usize = 4096;

/// 插件配置；排队总预算继续由宿主控制
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    pub wait_seconds: u32,
    /// 只接受旧配置中的字段，保留策略不再使用该值，保存设置时移除
    #[serde(default, skip_serializing, rename = "historyLimit")]
    pub legacy_history_limit: Option<usize>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            wait_seconds: 15,
            legacy_history_limit: None,
        }
    }
}

impl Config {
    /// 校验插件配置，不隐式扩大宿主排队或并发上限
    /// @return 参数非法时返回中文说明
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=110).contains(&self.wait_seconds) {
            return Err("迁移等待阈值须为 1～110 秒".into());
        }
        Ok(())
    }
}

/// 当前请求在一次选号期间的计时与迁移资格
#[derive(Clone)]
struct Pending {
    identity: Identity,
    key_id: String,
    first_scheduled: Option<u64>,
    scheduled_attempt: Option<u32>,
    blocked_logged: bool,
}

/// 会话内的短期迁移协调，不包含真实绑定账号
#[derive(Default)]
struct Session {
    reserved: Option<Reservation>,
    last_migration: u64,
    touched_at: u64,
}

/// 等待宿主确认的单次选号意图
#[derive(Clone)]
struct Reservation {
    request_id: String,
    attempt_index: u32,
    record_id: String,
    account_id: String,
}

/// 调度动作；记录持久化成功后才能把显式选号交给宿主
#[derive(Clone)]
pub enum Action {
    Delegate,
    Pick { account_id: String },
    RecordOnly(Record),
    Propose(Record),
    CandidateUnavailable { record_id: String },
}

/// 有界的请求计时与会话迁移意图，不是第二份绑定表
#[derive(Default)]
pub struct Policy {
    pending: HashMap<String, Pending>,
    sessions: HashMap<String, Session>,
}

impl Policy {
    /// 登记模型请求；超出本地协调容量时委托宿主原行为
    /// @param request_id 宿主模型请求 ID
    /// @param key_id 宿主认证后的 Key ID
    /// @param identity 显式会话摘要与续写限制
    /// @param now 单调时钟的毫秒值
    /// @return 是否成功登记
    pub fn register(
        &mut self,
        request_id: &str,
        key_id: &str,
        identity: Identity,
        now: u64,
    ) -> bool {
        if self.pending.len() >= MAX_REQUESTS || self.pending.contains_key(request_id) {
            return false;
        }
        if self.sessions.len() >= MAX_SESSIONS
            && !self.sessions.contains_key(&identity.session_hash)
        {
            let oldest = self
                .sessions
                .iter()
                .filter(|(key, s)| {
                    s.reserved.is_none()
                        && !self
                            .pending
                            .values()
                            .any(|p| &p.identity.session_hash == *key)
                })
                .min_by_key(|(_, s)| s.touched_at)
                .map(|(k, _)| k.clone());
            let Some(oldest) = oldest else {
                return false;
            };
            self.sessions.remove(&oldest);
        }
        self.sessions
            .entry(identity.session_hash.clone())
            .or_default()
            .touched_at = now;
        self.pending.insert(
            request_id.into(),
            Pending {
                identity,
                key_id: key_id.into(),
                first_scheduled: None,
                scheduled_attempt: None,
                blocked_logged: false,
            },
        );
        true
    }

    /// 计算本轮调度意图；没有新账号时继续由宿主等待并遵守原队列预算
    /// @param input 宿主当前候选快照
    /// @param config 插件配置
    /// @param now 单调时钟的毫秒值
    /// @param wall_ms 仅用于记录展示的 UTC 毫秒时间
    /// @return 委托、候选选择或待持久化记录
    pub fn schedule(
        &mut self,
        input: &AccountScheduleRequest,
        config: &Config,
        now: u64,
        wall_ms: u64,
    ) -> Action {
        if input.provider != "openai" {
            return Action::Delegate;
        }
        let Some(pending) = self.pending.get_mut(&input.request_id) else {
            return Action::Delegate;
        };
        if pending.scheduled_attempt != Some(input.attempt_index) {
            pending.first_scheduled = Some(now);
            pending.scheduled_attempt = Some(input.attempt_index);
        }
        let first = *pending.first_scheduled.get_or_insert(now);
        let session = self
            .sessions
            .get_mut(&pending.identity.session_hash)
            .expect("已登记请求保留会话协调状态");
        session.touched_at = now;
        if let Some(reserved) = &session.reserved {
            if reserved.request_id != input.request_id {
                return Action::Delegate;
            }
            if reserved.attempt_index == input.attempt_index
                && input
                    .candidates
                    .iter()
                    .any(|c| c.account_id == reserved.account_id && c.available)
            {
                return Action::Pick {
                    account_id: reserved.account_id.clone(),
                };
            }
            let id = reserved.record_id.clone();
            session.reserved = None;
            return Action::CandidateUnavailable { record_id: id };
        }
        // 其他请求已经完成迁移时，旧等待者从迁移时刻重新计时，避免按旧截止连续搬动绑定
        let waited = now.saturating_sub(first.max(session.last_migration));
        let invalid_queue = matches!(
            pending.identity.blocked_reason.as_deref(),
            Some("account_queue_disabled" | "queue_budget_too_short")
        );
        if waited < u64::from(config.wait_seconds) * 1000 && !invalid_queue {
            return Action::Delegate;
        }
        if let Some(reason) = &pending.identity.blocked_reason {
            if pending.blocked_logged {
                return Action::Delegate;
            }
            pending.blocked_logged = true;
            return Action::RecordOnly(Record::blocked(
                input,
                &pending.identity.session_hash,
                &pending.key_id,
                waited,
                wall_ms,
                reason,
            ));
        }
        let candidate = input
            .candidates
            .iter()
            .filter(|c| c.available)
            .min_by(|a, b| compare(a, b, &pending.identity.session_hash));
        let Some(candidate) = candidate else {
            return Action::Delegate;
        };
        let record = Record::proposed(
            input,
            &pending.identity.session_hash,
            &pending.key_id,
            waited,
            wall_ms,
            &candidate.account_id,
        );
        session.reserved = Some(Reservation {
            request_id: input.request_id.clone(),
            attempt_index: input.attempt_index,
            record_id: record.id.clone(),
            account_id: candidate.account_id.clone(),
        });
        Action::Propose(record)
    }

    /// 撤销未能持久化或已取消的迁移意图，不修改宿主绑定
    /// @param request_id 原模型请求 ID
    /// @param record_id 精确匹配的意图 ID，避免清除后续意图
    /// @return 无
    pub fn abandon(&mut self, request_id: &str, record_id: &str) {
        let Some(pending) = self.pending.get(request_id) else {
            return;
        };
        if let Some(session) = self.sessions.get_mut(&pending.identity.session_hash)
            && session
                .reserved
                .as_ref()
                .is_some_and(|r| r.request_id == request_id && r.record_id == record_id)
        {
            session.reserved = None;
        }
    }

    /// 用宿主 attempt 中间件提供的实际账号确认名额取得
    /// @param request_id 原模型请求 ID
    /// @param attempt_index 真实尝试序号
    /// @param account_id 宿主已经取得租约的账号
    /// @param now 单调时钟的毫秒值
    /// @return 本次对应的迁移记录及目标是否一致，缺少意图时返回 None
    pub fn admitted(
        &mut self,
        request_id: &str,
        attempt_index: u32,
        account_id: &str,
        now: u64,
    ) -> Option<(String, bool)> {
        let pending = self.pending.get_mut(request_id)?;
        // 每次实际取得账号都终止当前计时；上游错误后的新尝试必须重新开始选号等待
        pending.first_scheduled = None;
        pending.scheduled_attempt = None;
        let session = self.sessions.get_mut(&pending.identity.session_hash)?;
        let reserved = session.reserved.as_ref()?;
        if reserved.request_id != request_id || reserved.attempt_index != attempt_index {
            return None;
        }
        let reserved = session.reserved.take()?;
        let matched = reserved.account_id == account_id;
        if matched {
            session.last_migration = now;
        }
        Some((reserved.record_id, matched))
    }

    /// 释放结束或取消请求持有的内存与迁移协调位置
    /// @param request_id 原模型请求 ID
    /// @return 无
    pub fn finish(&mut self, request_id: &str) {
        if let Some(pending) = self.pending.remove(request_id)
            && let Some(session) = self.sessions.get_mut(&pending.identity.session_hash)
            && session
                .reserved
                .as_ref()
                .is_some_and(|r| r.request_id == request_id)
        {
            session.reserved = None;
        }
    }
}

fn compare(a: &AccountScheduleCandidate, b: &AccountScheduleCandidate, session: &str) -> Ordering {
    let load = |c: &AccountScheduleCandidate| {
        if c.maximum_concurrency == 0 {
            (0_u128, 1_u128)
        } else {
            (u128::from(c.in_flight), u128::from(c.maximum_concurrency))
        }
    };
    let (an, ad) = load(a);
    let (bn, bd) = load(b);
    (an * bd)
        .cmp(&(bn * ad))
        .then_with(|| b.weight.cmp(&a.weight))
        .then_with(|| {
            // 同负载使用会话稳定散列分散迁移目标，减少并发等待者抢同一个账号
            let rank = |id: &str| Sha256::digest(format!("{session}\0{id}").as_bytes());
            rank(&a.account_id).cmp(&rank(&b.account_id))
        })
}
