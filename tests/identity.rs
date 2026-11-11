//! 验证显式身份、Key 隔离、逻辑元数据优先级与续写标记

use cpr_session_failover::identity::identify;
use gateway_plugin_sdk::call::middleware::MiddlewareHeader;

#[test]
fn cache_key_and_authorization_cannot_create_session_identity() {
    let headers = [MiddlewareHeader {
        name: "authorization".into(),
        value: b"not-a-real-secret".to_vec(),
    }];
    assert!(
        identify(
            "key",
            &headers,
            br#"{"prompt_cache_key":"cache-only","input":"question"}"#
        )
        .is_none()
    );
}

#[test]
fn logical_session_metadata_overrides_cache_session_header() {
    let header = [MiddlewareHeader {
        name: "session-id".into(),
        value: b"cache-route".to_vec(),
    }];
    let body = br#"{"client_metadata":{"session_id":"logical-session"},"input":"question"}"#;
    let from_metadata = identify("key", &header, body).unwrap();
    let expected = identify("key", &[], br#"{"session_id":"logical-session"}"#).unwrap();
    assert_eq!(from_metadata.session_hash, expected.session_hash);
    assert_ne!(
        expected.session_hash,
        identify("other-key", &[], br#"{"session_id":"logical-session"}"#)
            .unwrap()
            .session_hash
    );
    assert!(!expected.session_hash.contains("logical-session"));
}

#[test]
fn frame_turn_metadata_overrides_connection_context() {
    let header = [MiddlewareHeader {
        name: "x-codex-turn-metadata".into(),
        value: br#"{"session_id":"frame-session"}"#.to_vec(),
    }];
    assert_eq!(
        identify("key", &header, br#"{"session_id":"connection-session"}"#).unwrap(),
        identify("key", &[], br#"{"session_id":"frame-session"}"#).unwrap()
    );
}

#[test]
fn native_reference_and_prewarm_are_conservatively_protected() {
    for (body, reason) in [
        (
            br#"{"session_id":"s","previous_response_id":"R1"}"#.as_slice(),
            "native_continuation",
        ),
        (
            br#"{"session_id":"s","input":[{"type":"item_reference","id":"item"}]}"#.as_slice(),
            "item_reference",
        ),
        (
            br#"{"session_id":"s","generate":false}"#.as_slice(),
            "prewarm",
        ),
    ] {
        assert_eq!(
            identify("key", &[], body)
                .unwrap()
                .blocked_reason
                .as_deref(),
            Some(reason)
        );
    }
}

#[test]
fn full_replay_requires_the_same_root_thread_turn_and_model() {
    let envelope = |thread: &str, turn: &str, model: &str, native: bool| {
        let mut value = serde_json::json!({
            "model":model,
            "client_metadata":{"session_id":"root","thread_id":thread,"turn_id":turn},
            "input":[{"type":"message","role":"user","content":"test"}]
        });
        if native {
            value["previous_response_id"] = serde_json::json!("resp_A");
        }
        serde_json::to_vec(&value).unwrap()
    };
    let turn = |key, thread, turn, model, native| {
        identify(key, &[], &envelope(thread, turn, model, native))
            .unwrap()
            .replay_turn
    };
    let original = turn("key", "root", "turn", "model", true);
    assert!(original.is_some());
    assert_eq!(original, turn("key", "root", "turn", "model", false));
    assert_ne!(
        original,
        turn("another_key", "root", "turn", "model", false)
    );
    assert_ne!(original, turn("key", "root", "other_turn", "model", false));
    assert_ne!(original, turn("key", "root", "turn", "other_model", false));
    assert!(turn("key", "child", "turn", "model", false).is_none());
}
