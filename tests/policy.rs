//! 验证等待阈值、同会话迁移串行化、取消与原生续写保护

use cpr_session_failover::{
    identity::Identity,
    policy::{Action, Config, Policy},
};
use gateway_plugin_sdk::call::policy::{AccountScheduleCandidate, AccountScheduleRequest};

fn input(id: &str) -> AccountScheduleRequest {
    AccountScheduleRequest {
        request_id: id.into(),
        attempt_index: 1,
        provider: "openai".into(),
        model: Some("test-model".into()),
        candidates: vec![candidate("A", false, 3, 3), candidate("B", true, 0, 3)],
    }
}

fn candidate(id: &str, available: bool, used: u32, max: u32) -> AccountScheduleCandidate {
    AccountScheduleCandidate {
        account_id: id.into(),
        available,
        weight: 1,
        in_flight: used,
        maximum_concurrency: max,
        last_started_at_ms: None,
        quota_reset_at_ms: None,
        quota_remaining_rank: None,
        failure_rate_basis_points: None,
        first_output_latency_ms: None,
    }
}

fn register(policy: &mut Policy, id: &str, session: &str, blocked: Option<&str>) {
    assert!(policy.register(
        id,
        "key_test",
        Identity {
            session_hash: session.into(),
            blocked_reason: blocked.map(str::to_owned)
        },
        0
    ));
}

#[test]
fn waits_for_threshold_then_chooses_available_account() {
    let mut policy = Policy::default();
    register(&mut policy, "r1", "s1", None);
    let input = input("r1");
    let config = Config::default();
    assert!(matches!(
        policy.schedule(&input, &config, 100, 100),
        Action::Delegate
    ));
    assert!(matches!(
        policy.schedule(&input, &config, 5099, 5099),
        Action::Delegate
    ));
    let Action::Propose(record) = policy.schedule(&input, &config, 5100, 5100) else {
        panic!("达到阈值应提出迁移");
    };
    assert_eq!(record.proposed_account_id.as_deref(), Some("B"));
    assert_eq!(record.scheduler_wait_ms, 5000);
    assert_eq!(record.binding_changed, None);
}

#[test]
fn concurrent_waiters_follow_one_migration_and_restart_wait_clock() {
    let mut policy = Policy::default();
    register(&mut policy, "r1", "s1", None);
    register(&mut policy, "r2", "s1", None);
    let config = Config::default();
    policy.schedule(&input("r1"), &config, 0, 0);
    policy.schedule(&input("r2"), &config, 0, 0);
    let Action::Propose(first) = policy.schedule(&input("r1"), &config, 5000, 5000) else {
        panic!("首个等待者应取得迁移协调位置");
    };
    assert!(matches!(
        policy.schedule(&input("r2"), &config, 6000, 6000),
        Action::Delegate
    ));
    assert_eq!(policy.admitted("r1", 1, "B", 6100), Some((first.id, true)));
    assert!(matches!(
        policy.schedule(&input("r2"), &config, 10000, 10000),
        Action::Delegate
    ));
    assert!(matches!(
        policy.schedule(&input("r2"), &config, 11100, 11100),
        Action::Propose(_)
    ));
}

#[test]
fn cancellation_and_failed_persistence_release_reservation() {
    let mut policy = Policy::default();
    for id in ["r1", "r2"] {
        register(&mut policy, id, "s1", None);
        policy.schedule(&input(id), &Config::default(), 0, 0);
    }
    let Action::Propose(record) = policy.schedule(&input("r1"), &Config::default(), 5000, 5000)
    else {
        panic!("应提出迁移");
    };
    policy.abandon("r1", "different-id");
    assert!(matches!(
        policy.schedule(&input("r2"), &Config::default(), 5001, 5001),
        Action::Delegate
    ));
    policy.abandon("r1", &record.id);
    assert!(matches!(
        policy.schedule(&input("r2"), &Config::default(), 5002, 5002),
        Action::Propose(_)
    ));
    policy.finish("r2");
    assert!(matches!(
        policy.schedule(&input("r1"), &Config::default(), 5003, 5003),
        Action::Propose(_)
    ));
}

#[test]
fn contention_on_new_account_can_return_to_selection() {
    let mut policy = Policy::default();
    register(&mut policy, "r1", "s1", None);
    let mut input = input("r1");
    policy.schedule(&input, &Config::default(), 0, 0);
    assert!(matches!(
        policy.schedule(&input, &Config::default(), 5000, 5000),
        Action::Propose(_)
    ));
    input.candidates[1].available = false;
    input.candidates.push(candidate("C", true, 0, 3));
    assert!(matches!(
        policy.schedule(&input, &Config::default(), 5100, 5100),
        Action::CandidateUnavailable { .. }
    ));
    let Action::Propose(record) = policy.schedule(&input, &Config::default(), 5200, 5200) else {
        panic!("候选失效后应允许重新评估");
    };
    assert_eq!(record.proposed_account_id.as_deref(), Some("C"));
}

#[test]
fn native_continuation_only_records_block_and_never_picks_another_account() {
    let mut policy = Policy::default();
    register(&mut policy, "r1", "s1", Some("native_continuation"));
    policy.schedule(&input("r1"), &Config::default(), 0, 0);
    let Action::RecordOnly(record) = policy.schedule(&input("r1"), &Config::default(), 5000, 5000)
    else {
        panic!("原生续写等待达到阈值应留下保护记录");
    };
    assert_eq!(record.reason, "native_continuation");
    assert!(record.proposed_account_id.is_none());
    assert!(matches!(
        policy.schedule(&input("r1"), &Config::default(), 10000, 10000),
        Action::Delegate
    ));
}

#[test]
fn unavailable_pool_and_missing_middleware_do_not_force_selection() {
    let mut policy = Policy::default();
    register(&mut policy, "r1", "s1", None);
    let mut input = input("r1");
    input
        .candidates
        .iter_mut()
        .for_each(|c| c.available = false);
    policy.schedule(&input, &Config::default(), 0, 0);
    assert!(matches!(
        policy.schedule(&input, &Config::default(), 9000, 9000),
        Action::Delegate
    ));
    input.request_id = "unregistered".into();
    input.candidates[1].available = true;
    assert!(matches!(
        policy.schedule(&input, &Config::default(), 10000, 10000),
        Action::Delegate
    ));
}

#[test]
fn selection_respects_normalized_load_and_key_scoped_session_isolation() {
    let mut policy = Policy::default();
    for (id, session) in [("r1", "key-one-session"), ("r2", "key-two-session")] {
        register(&mut policy, id, session, None);
        policy.schedule(&input(id), &Config::default(), 0, 0);
    }
    let mut input = input("r1");
    input.candidates[1] = candidate("B", true, 2, 3);
    input.candidates.push(candidate("C", true, 2, 10));
    let Action::Propose(record) = policy.schedule(&input, &Config::default(), 5000, 5000) else {
        panic!("应提出迁移");
    };
    assert_eq!(record.proposed_account_id.as_deref(), Some("C"));
    input.request_id = "r2".into();
    assert!(matches!(
        policy.schedule(&input, &Config::default(), 5000, 5000),
        Action::Propose(_)
    ));
}

#[test]
fn upstream_time_is_not_counted_as_later_account_selection_wait() {
    let mut policy = Policy::default();
    register(&mut policy, "r1", "s1", None);
    let mut input = input("r1");
    policy.schedule(&input, &Config::default(), 0, 0);
    assert_eq!(policy.admitted("r1", 1, "A", 5), None);
    // 模拟账号 A 已经执行上游 60 秒；新的选号尝试必须再等待完整阈值
    input.attempt_index = 2;
    assert!(matches!(
        policy.schedule(&input, &Config::default(), 60_005, 60_005),
        Action::Delegate
    ));
    assert!(matches!(
        policy.schedule(&input, &Config::default(), 65_004, 65_004),
        Action::Delegate
    ));
    assert!(matches!(
        policy.schedule(&input, &Config::default(), 65_005, 65_005),
        Action::Propose(_)
    ));
}
