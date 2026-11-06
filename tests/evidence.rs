//! 验证切换证据不跨请求或尝试串用，未知事实不伪装成成功

use cpr_session_failover::{audit::Record, diagnostics::evidence};
use gateway_plugin_sdk::call::policy::AccountScheduleRequest;
use serde_json::json;

fn record() -> Record {
    let mut r = Record::proposed(
        &AccountScheduleRequest {
            request_id: "req_test".into(),
            attempt_index: 2,
            provider: "openai".into(),
            model: None,
            candidates: vec![],
        },
        "session-hash",
        "key_test",
        5000,
        10_000,
        "B",
    );
    r.actual_account_id = Some("B".into());
    r
}

#[test]
fn picks_matching_attempt_and_actual_account_not_an_earlier_failed_choice() {
    let document = json!({"schemaVersion":5,"requests":[{"requestId":"req_test","evidence":{"trace":{"events":[
        {"stage":"request.routed","data":{"requiredAccount":null,"accountPolicy":{"affinity":"strict"}}},
        {"stage":"account.selection","attemptIndex":1,"data":{"selectedAccountId":"B","preferredAccountId":"wrong-attempt"}},
        {"stage":"account.selection","attemptIndex":2,"data":{"selectedAccountId":"C","preferredAccountId":"wrong-choice"}},
        {"stage":"account.selection","attemptIndex":2,"data":{"selectedAccountId":"B","preferredAccountId":"A"}},
        {"stage":"account.queue.acquired","attemptIndex":2,"data":{"waitMs":5001}}
    ]}}}]});
    let e = evidence(&document, &record()).unwrap();
    assert_eq!(e.from_account_id.as_deref(), Some("A"));
    assert_eq!(e.binding_changed, Some(true));
    assert_eq!(e.account_queue_wait_ms, Some(5001));
}

#[test]
fn intent_without_actual_admission_cannot_be_reported_as_binding_change() {
    let mut record = record();
    record.actual_account_id = None;
    let document = json!({"schemaVersion":5,"requests":[{"requestId":"req_test","evidence":{"trace":{"events":[
        {"stage":"request.routed","data":{"requiredAccount":null,"accountPolicy":{"affinity":"strict"}}},{"stage":"account.selection","attemptIndex":2,"data":{"selectedAccountId":"B","preferredAccountId":"A"}}]}}}]});
    assert_eq!(evidence(&document, &record).unwrap().binding_changed, None);
}

#[test]
fn missing_evidence_is_unknown_and_other_request_is_rejected() {
    let document = json!({"schemaVersion":5,"requests":[{"requestId":"req_test","evidence":{"trace":{"events":[
        {"stage":"request.routed","data":{"requiredAccount":null,"accountPolicy":{"affinity":"strict"}}},]}}}]});
    let e = evidence(&document, &record()).unwrap();
    assert_eq!(e.binding_changed, None);
    assert_eq!(e.from_account_id, None);
    assert!(
        evidence(
            &json!({"schemaVersion":5,"requests":[{"requestId":"other"}]}),
            &record()
        )
        .is_err()
    );
}

#[test]
fn same_account_choice_is_not_counted_as_migration() {
    let document = json!({"schemaVersion":5,"requests":[{"requestId":"req_test","evidence":{"trace":{"events":[
        {"stage":"request.routed","data":{"requiredAccount":null,"accountPolicy":{"affinity":"strict"}}},{"stage":"account.selection","attemptIndex":2,"data":{"selectedAccountId":"B","preferredAccountId":"B"}}]}}}]});
    assert_eq!(
        evidence(&document, &record()).unwrap().binding_changed,
        Some(false)
    );
}

#[test]
fn actual_account_can_be_recovered_from_host_admission_trace() {
    let mut r = record();
    r.actual_account_id = None;
    let document = json!({"schemaVersion":5,"requests":[{"requestId":"req_test","evidence":{"trace":{"events":[
        {"stage":"request.routed","data":{"requiredAccount":null,"accountPolicy":{"affinity":"strict"}}},
        {"stage":"account.selection","attemptIndex":2,"data":{"selectedAccountId":"B","preferredAccountId":"A"}},
        {"stage":"account.selected","attemptIndex":2,"data":{"accountId":"B","selectionMs":5010}}
    ]}}}]});
    let e = evidence(&document, &r).unwrap();
    assert_eq!(e.actual_account_id.as_deref(), Some("B"));
    assert_eq!(e.binding_changed, Some(true));
    assert_eq!(e.account_selection_wait_ms, Some(5010));
}

#[test]
fn superseded_intent_is_not_counted_as_successful_migration() {
    let mut r = record();
    r.actual_account_id = None;
    let document = json!({"schemaVersion":5,"requests":[{"requestId":"req_test","evidence":{"trace":{"events":[
        {"stage":"request.routed","data":{"requiredAccount":null,"accountPolicy":{"affinity":"strict"}}},
        {"stage":"account.selection","attemptIndex":2,"data":{"selectedAccountId":"C","preferredAccountId":"A"}},
        {"stage":"account.selected","attemptIndex":2,"data":{"accountId":"C","selectionMs":5010}}
    ]}}}]});
    assert_eq!(
        evidence(&document, &r).unwrap().binding_changed,
        Some(false)
    );
}

#[test]
fn fixed_account_preference_is_not_misreported_as_old_session_binding() {
    let document = json!({"schemaVersion":5,"requests":[{"requestId":"req_test","evidence":{"trace":{"events":[
        {"stage":"request.routed","data":{"requiredAccount":"B","accountPolicy":{"affinity":"strict"}}},
        {"stage":"account.selection","attemptIndex":2,"data":{"selectedAccountId":"B","preferredAccountId":"B"}}
    ]}}}]});
    let e = evidence(&document, &record()).unwrap();
    assert_eq!(e.from_account_id, None);
    assert_eq!(e.binding_changed, None);
}

#[test]
fn missing_routing_policy_keeps_binding_change_unknown() {
    let document = json!({"schemaVersion":5,"requests":[{"requestId":"req_test","evidence":{"trace":{"events":[
        {"stage":"account.selection","attemptIndex":2,"data":{"selectedAccountId":"B","preferredAccountId":"A"}}
    ]}}}]});
    assert_eq!(
        evidence(&document, &record()).unwrap().binding_changed,
        None
    );
}
