//! 启动真实插件进程，验证 RPC、持久化先于选号、取消回收及重启后的历史读取

use std::{
    collections::{HashMap, HashSet, VecDeque},
    env::var_os,
    ffi::OsString,
    io::{Cursor, Write},
    process::Stdio,
    time::Duration,
};

use gateway_plugin_sdk::{
    CallContext, ErrorCode, Frame, Handshake, Manifest, Message, PluginFault, Stage,
    call::middleware::MiddlewareTransport,
    client::{read_frame, write_frame},
};
use serde_json::{Value, json};
use tokio::{
    process::{Child, ChildStdin, ChildStdout, Command},
    time::{sleep, timeout},
};
use zip::{ZipWriter, write::SimpleFileOptions};

/// 使用真实 SDK 帧合同的受控宿主，所有账号与请求均为测试数据
struct Host {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    results: HashMap<u64, (Value, Vec<u8>)>,
    next: HashMap<u64, (u64, Value)>,
    cancelled: HashSet<u64>,
    state: HashMap<(String, String), (Value, u64)>,
    conflict_puts: usize,
    version: u64,
    fail_put: bool,
    fail_record: bool,
    fail_commit: bool,
    hold_put: bool,
    held_put: Vec<u64>,
    chunks: VecDeque<Vec<u8>>,
    call_ids: HashMap<u64, u64>,
    next_call_id: u64,
    host_queue_failover: bool,
}

impl Host {
    async fn start(state: HashMap<(String, String), (Value, u64)>, version: u64) -> Self {
        let executable = var_os("CPR_PLUGIN_TEST_BINARY")
            .unwrap_or_else(|| OsString::from(env!("CARGO_BIN_EXE_cpr-session-failover")));
        let mut child = Command::new(executable)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut host = Self {
            input: child.stdin.take().unwrap(),
            output: child.stdout.take().unwrap(),
            child,
            results: HashMap::new(),
            next: HashMap::new(),
            cancelled: HashSet::new(),
            state,
            conflict_puts: 0,
            version,
            fail_put: false,
            fail_record: false,
            fail_commit: false,
            hold_put: false,
            held_put: vec![],
            chunks: VecDeque::new(),
            call_ids: HashMap::new(),
            next_call_id: 1,
            host_queue_failover: true,
        };
        let manifest = Manifest::from_author_slice(include_bytes!("../plugin.json")).unwrap();
        host.send(
            Message::Hello {
                handshake: Handshake {
                    protocol_version: 7,
                    artifact_sha256: "a".repeat(64),
                    plugin_id: manifest.plugin_id().unwrap(),
                    instance_id: "test_instance".into(),
                    generation: 1,
                    incarnation: "test_incarnation".into(),
                    configuration: json!({"waitSeconds":1,"historyLimit":20}),
                    contributes: manifest.contributes,
                },
            },
            vec![],
        )
        .await;
        assert!(matches!(
            host.read().await.message,
            Message::Ready {
                protocol_version: 7,
                ..
            }
        ));
        host
    }

    async fn send(&mut self, message: Message, payload: Vec<u8>) {
        write_frame(&mut self.input, &Frame { message, payload })
            .await
            .unwrap();
    }

    async fn read(&mut self) -> Frame {
        timeout(Duration::from_secs(8), read_frame(&mut self.output))
            .await
            .expect("插件在预算内响应")
            .expect("插件帧完整")
    }

    async fn call(
        &mut self,
        id: u64,
        stage: Stage,
        method: &str,
        request_id: Option<&str>,
        params: Value,
        payload: Vec<u8>,
    ) {
        let wire_id = self.next_call_id;
        self.next_call_id += 2;
        self.call_ids.insert(id, wire_id);
        let account = params
            .get("account_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.send(
            Message::Call {
                id: wire_id,
                method: method.into(),
                context: CallContext {
                    request: None,
                    call_id: wire_id,
                    instance_id: "test_instance".into(),
                    generation: 1,
                    incarnation: "test_incarnation".into(),
                    stage,
                    timeout_ms: match stage {
                        Stage::Registration => 5_000,
                        Stage::Scheduling | Stage::Observation => 2_000,
                        _ => 30_000,
                    },
                    resource_stream: false,
                    resource_scope_id: format!("scope_{id}"),
                    request_id: request_id.map(str::to_owned),
                    attempt_id: account
                        .as_ref()
                        .map(|_| format!("{}:1", request_id.unwrap())),
                    account_id: account,
                    credential_revision: None,
                },
                params,
            },
            payload,
        )
        .await;
        if matches!(stage, Stage::Request | Stage::Attempt) {
            self.send(
                Message::Credit {
                    id: wire_id,
                    bytes: 1024 * 1024,
                    frames: 16,
                },
                vec![],
            )
            .await;
        }
    }

    async fn respond(&mut self, id: u64, result: Value, payload: Vec<u8>) {
        self.send(Message::Result { id, result }, payload).await;
    }

    fn logical_id(&self, wire_id: u64) -> u64 {
        *self
            .call_ids
            .iter()
            .find(|(_, wire)| **wire == wire_id)
            .expect("宿主已登记调用 ID")
            .0
    }

    async fn pump(&mut self) {
        let frame = self.read().await;
        match frame.message {
            Message::Callback {
                id,
                parent_id,
                method,
                params,
            } => match method.as_str() {
                "host.middleware.next" => {
                    self.next.insert(self.logical_id(parent_id), (id, params));
                }
                "host.state.get" => {
                    let key = (
                        params["namespace"].as_str().unwrap().to_owned(),
                        params["key"].as_str().unwrap().to_owned(),
                    );
                    let record = self.state.get(&key).map(|(value, version)| json!({"value":value,"version":version,"schema_version":1}));
                    self.respond(id, json!({"record":record}), vec![]).await;
                }
                "host.state.put" => {
                    if self.hold_put {
                        self.held_put.push(id);
                        return;
                    }
                    if self.fail_put
                        || (self.fail_record
                            && params["key"].as_str().unwrap().starts_with("record-"))
                        || (self.fail_commit
                            && params["value"]["entries"]
                                .as_array()
                                .is_some_and(|entries| {
                                    entries.iter().any(|entry| entry["committed"] == true)
                                }))
                    {
                        self.send(
                            Message::Error {
                                id,
                                error: PluginFault::new(
                                    ErrorCode::Capacity,
                                    "测试宿主状态配额已满",
                                ),
                            },
                            vec![],
                        )
                        .await;
                        return;
                    }
                    let key = (
                        params["namespace"].as_str().unwrap().to_owned(),
                        params["key"].as_str().unwrap().to_owned(),
                    );
                    let expected = self.state.get(&key).map(|(_, version)| *version);
                    if self.conflict_puts > 0 || params["expected_version"] != json!(expected) {
                        self.conflict_puts = self.conflict_puts.saturating_sub(1);
                        self.send(
                            Message::Error {
                                id,
                                error: PluginFault::new(ErrorCode::Conflict, "测试并发冲突"),
                            },
                            vec![],
                        )
                        .await;
                        return;
                    }
                    let value = params["value"].clone();
                    assert!(serde_json::to_vec(&value).unwrap().len() <= 262_144);
                    self.version += 1;
                    self.state.insert(key, (value, self.version));
                    self.respond(id, json!({"version":self.version}), vec![])
                        .await;
                }
                "host.log" => {
                    let text = serde_json::to_string(&params).unwrap();
                    assert!(!text.contains("session-private-test"));
                    assert!(!text.contains("authorization"));
                    self.respond(id, json!({"recorded":true}), vec![]).await;
                }
                "host.services.call" => {
                    assert_eq!(params["operation"], "usage.records.get");
                    assert_eq!(params["input"]["id"], "req_one");
                    self.respond(id, json!({"Ok":{"id":"req_one","data":{
                            "client_key_ref":"key_test","provider":"openai","account_ref":"B","requested_model":"test-model","model":"test-model","protocol":"openai","operation":"generate","endpoint":"/v1/responses","client_transport":"http_sse","service_tier":null,"started_at_ms":1000,"completed_at_ms":57600000,"outcome":"succeeded","tokens":{},"cost":{"kind":"unavailable"}
                        },"billing_snapshot":null,"provider_observation":null}}), vec![]).await;
                }
                "host.http.dispatch" => {
                    assert_eq!(params["method"], "GET");
                    assert_eq!(
                        params["uri"],
                        "/api/admin/diagnostics/export?requestId=req_one"
                    );
                    self.chunks.push_back(diagnostic_zip());
                    self.respond(id, json!({"status":200,"version":"HTTP/1.1","headers":[],"body":{"kind":"handle","handle":"diagnostic_body"},"response":null,"session":false}), vec![]).await;
                }
                "host.http.body_read" => {
                    let chunk = self.chunks.pop_front();
                    self.respond(
                        id,
                        json!({"eof":chunk.is_none(),"trailers":null}),
                        chunk.unwrap_or_default(),
                    )
                    .await;
                }
                "host.http.body_close" => {
                    self.respond(id, json!({}), vec![]).await;
                }
                _ => panic!("未预期的宿主回调：{method}"),
            },
            Message::Resume { id, epoch } => {
                self.send(Message::Resumed { id, epoch }, vec![]).await
            }
            Message::Result { id, result } => {
                self.results
                    .insert(self.logical_id(id), (result, frame.payload));
            }
            Message::Cancelled { id } => {
                self.cancelled.insert(self.logical_id(id));
            }
            Message::Error { id, error } => panic!("插件调用 {id} 失败：{}", error.message),
            Message::End { error: None, .. } => {}
            Message::Suspend { .. } | Message::CallbackConsumed { .. } | Message::Credit { .. } => {
            }
            other => panic!("未预期的插件消息：{other:?}"),
        }
    }

    async fn result(&mut self, id: u64) -> (Value, Vec<u8>) {
        while !self.results.contains_key(&id) {
            self.pump().await;
        }
        self.results.remove(&id).unwrap()
    }

    async fn wait_next(&mut self, id: u64) {
        while !self.next.contains_key(&id) {
            self.pump().await;
        }
    }

    async fn model(&mut self, id: u64, request: &str, attempt: bool) {
        self.model_input(
            id,
            request,
            attempt,
            br#"{"model":"test-model","session_id":"session-private-test","input":"test"}"#
                .to_vec(),
        )
        .await;
    }

    async fn model_input(&mut self, id: u64, request: &str, attempt: bool, body: Vec<u8>) {
        self.model_input_with_transport(id, request, attempt, MiddlewareTransport::HttpSse, body)
            .await;
    }

    async fn model_input_with_transport(
        &mut self,
        id: u64,
        request: &str,
        attempt: bool,
        transport: MiddlewareTransport,
        body: Vec<u8>,
    ) {
        let settings = if attempt {
            Value::Null
        } else {
            json!({"runtime":{"openai_account_affinity":"preferred","max_waiting_per_account":10,"account_queue_timeout_seconds":20,"switch_account_on_queue_failure":self.host_queue_failover},"fast_mode":"default","client_limits":{},"timeout_ms":null})
        };
        let params = json!({"settings_sources":{},"settings":settings,"request_id":request,"client_key_id":"key_test","account_group_ids":[],"mount":if attempt {"attempt"} else {"request"},"attempt_index":if attempt {Some(1)} else {None},"operation":"generate","protocol":"openai","endpoint":"/v1/responses","transport":transport,"provider":if attempt {Some("openai")} else {None},"model":"test-model","account_id":if attempt {Some("B")} else {None},"headers":[]});
        self.call(
            id,
            if attempt {
                Stage::Attempt
            } else {
                Stage::Request
            },
            "middleware.handle",
            Some(request),
            params,
            body,
        )
        .await;
        self.wait_next(id).await;
        if !attempt {
            let params = &self.next[&id].1;
            assert_eq!(
                params["settings"]["runtime"]["openai_account_affinity"],
                "strict"
            );
            assert_eq!(
                params["settings"]["runtime"]["switch_account_on_queue_failure"],
                self.host_queue_failover
            );
            assert_eq!(params["body"], "preserve");
            assert_eq!(params["header_mutations"], json!([]));
        }
    }

    async fn schedule(&mut self, id: u64, request: &str) -> Value {
        self.send_schedule(id, request).await;
        self.result(id).await.0
    }

    async fn send_schedule(&mut self, id: u64, request: &str) {
        self.send_candidates(id, request, json!([
            {"account_id":"A","available":false,"weight":1,"in_flight":3,"maximum_concurrency":3},
            {"account_id":"B","available":true,"weight":1,"in_flight":0,"maximum_concurrency":3}
        ])).await;
    }

    async fn send_candidates(&mut self, id: u64, request: &str, candidates: Value) {
        self.call(id, Stage::Scheduling, "policy.schedule_account", Some(request), json!({"request_id":request,"attempt_index":1,"provider":"openai","model":"test-model","candidates":candidates}), vec![]).await;
    }

    async fn websocket_output(&mut self, id: u64, frame: &Value) -> Value {
        let message = json!({"kind":{"kind":"text"},"payload":{"kind":"bytes"}});
        let bytes = serde_json::to_vec(frame).unwrap();
        self.call(
            id,
            Stage::WebSocket,
            "middleware.handle",
            None,
            json!({
                "connection_id":"ws_test", "direction":"outgoing", "headers":[], "message":message
            }),
            bytes.clone(),
        )
        .await;
        self.wait_next(id).await;
        let (next, forwarded) = self.next.remove(&id).unwrap();
        assert_eq!(forwarded, message);
        self.respond(next, forwarded, bytes).await;
        let (_, payload) = self.result(id).await;
        serde_json::from_slice(&payload).unwrap()
    }

    async fn finish_next(&mut self, parent: u64) {
        let (id, _) = self.next.remove(&parent).unwrap();
        self.respond(id, json!({"response":format!("response_{parent}"),"protocol":"openai","status":200,"headers":[],"body":{"handle":"unread_sse_body","framing":"sse_event"},"metadata":{"provider":"openai","upstream_model":"test-model","provider_account_id":"B","upstream_request_id":"upstream_test","transport":"http_sse","selection_observation":{"account_selection_wait_ms":1100,"capacity":null}}}), vec![]).await;
        let (result, _) = self.result(parent).await;
        assert_eq!(result["body"]["kind"], "pass_through");
        assert_eq!(result["body"]["body"]["handle"], "unread_sse_body");
    }

    async fn cancel(&mut self, id: u64) {
        self.send(
            Message::Cancel {
                id: self.call_ids[&id],
            },
            vec![],
        )
        .await;
        while !self.cancelled.contains(&id) {
            self.pump().await;
        }
    }

    fn record(&self, request_id: &str) -> &Value {
        &self
            .state
            .iter()
            .find(|((namespace, key), (value, _))| {
                namespace == "history"
                    && key.starts_with("record-")
                    && value["record"]["requestId"] == request_id
            })
            .unwrap()
            .1
            .0["record"]
    }

    async fn history(&mut self, id: u64, query: &str) -> Value {
        self.call(
            id,
            Stage::Management,
            "management.handle",
            None,
            json!({"method":"GET","path":"history","query":query,"content_type":null,"headers":[]}),
            vec![],
        )
        .await;
        let (response, payload) = self.result(id).await;
        assert_eq!(response["status"], 200);
        serde_json::from_slice(&payload).unwrap()
    }

    async fn stop(&mut self) {
        self.send(Message::Shutdown, vec![]).await;
        assert!(
            timeout(Duration::from_secs(3), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}

fn diagnostic_zip() -> Vec<u8> {
    let document = json!({"schemaVersion":5,"requests":[{"requestId":"req_one","evidence":{"trace":{"events":[
        {"stage":"request.routed","data":{"requiredAccount":null,"accountPolicy":{"affinity":"strict"}}},
        {"stage":"account.selection","attemptIndex":1,"data":{"selectedAccountId":"B","preferredAccountId":"A"}},
        {"stage":"account.selected","attemptIndex":1,"data":{"accountId":"B","selectionMs":1110}},
        {"stage":"account.queue.acquired","attemptIndex":1,"data":{"waitMs":1050}}
    ]}}}]});
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("diagnostics.json", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(&serde_json::to_vec(&document).unwrap())
        .unwrap();
    zip.finish().unwrap().into_inner()
}

#[tokio::test]
async fn migration_records_actual_admission_and_survives_restart() {
    let mut host = Host::start(HashMap::new(), 0).await;
    host.call(
        1,
        Stage::Registration,
        "management.register",
        None,
        json!({}),
        vec![],
    )
    .await;
    let (_, registration) = host.result(1).await;
    let registration: Value = serde_json::from_slice(&registration).unwrap();
    let entry = registration["pages"][0]["entry"].as_str().unwrap();
    let manifest = Manifest::from_author_slice(include_bytes!("../plugin.json")).unwrap();
    assert!(manifest.resources.contains_key(entry));
    assert!(
        registration["resources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|resource| resource["path"] == entry)
    );
    host.model(10, "req_one", false).await;
    host.model(11, "req_two", false).await;
    assert_eq!(host.schedule(20, "req_one").await["decision"], "delegate");
    assert_eq!(host.schedule(21, "req_two").await["decision"], "delegate");
    sleep(Duration::from_millis(1050)).await;
    assert_eq!(
        host.schedule(22, "req_one").await,
        json!({"decision":"pick","account_id":"B"})
    );
    assert_eq!(host.record("req_one")["stage"], "proposed");
    assert_eq!(host.schedule(23, "req_two").await["decision"], "delegate");
    host.model(30, "req_one", true).await;
    assert_eq!(host.record("req_one")["stage"], "admitted");
    host.finish_next(30).await;
    host.finish_next(10).await;
    assert_eq!(host.schedule(24, "req_two").await["decision"], "delegate");
    host.cancel(11).await;
    host.call(
        50,
        Stage::Maintenance,
        "plugin.reconcile",
        None,
        json!({}),
        vec![],
    )
    .await;
    host.result(50).await;
    let record = &host.record("req_one");
    assert_eq!(record["fromAccountId"], "A");
    assert_eq!(record["actualAccountId"], "B");
    assert_eq!(record["bindingChanged"], true);
    assert_eq!(record["outcome"], "succeeded");
    assert_eq!(record["accountSelectionWaitMs"], 1110);
    assert_eq!(record["accountQueueWaitMs"], 1050);
    let state = host.state.clone();
    let version = host.version;
    host.stop().await;
    let mut restarted = Host::start(state, version).await;
    restarted
        .call(
            60,
            Stage::CommandLine,
            "command_line.execute",
            None,
            json!({}),
            serde_json::to_vec(
                &json!({"name":"history","arguments":{"limit":{"type":"int","value":10}}}),
            )
            .unwrap(),
        )
        .await;
    let (_, payload) = restarted.result(60).await;
    let command: Value = serde_json::from_slice(&payload).unwrap();
    let history: Value = serde_json::from_str(command["stdout"].as_str().unwrap()).unwrap();
    assert_eq!(history["records"][0]["bindingChanged"], true);
    restarted.call(61, Stage::Management, "management.handle", None, json!({"method":"GET","path":"history","query":"limit=10&requestId=req_one","content_type":null,"headers":[]}), vec![]).await;
    let (response, payload) = restarted.result(61).await;
    assert_eq!(response["status"], 200);
    let history: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(history["records"][0]["requestId"], "req_one");
    assert_eq!(
        history["records"][0]["completedTimeDisplay"],
        "1970-01-02 00:00:00"
    );
    assert_eq!(
        history["records"][0]["completedTimeUtc"],
        "1970-01-01T16:00:00Z"
    );
    assert!(
        history["records"][0]["decisionTimeUtc"]
            .as_str()
            .unwrap()
            .ends_with('Z')
    );
    restarted.call(62, Stage::Management, "management.handle", None, json!({"method":"GET","path":"history","query":"limit=10&requestId=req_other","content_type":null,"headers":[]}), vec![]).await;
    let (_, payload) = restarted.result(62).await;
    let history: Value = serde_json::from_slice(&payload).unwrap();
    assert!(history["records"].as_array().unwrap().is_empty());
    restarted.stop().await;
}

#[tokio::test]
async fn quota_exhaustion_delegates_reports_failure_and_releases_coordination() {
    let mut host = Host::start(HashMap::new(), 0).await;
    host.model(10, "req_one", false).await;
    host.schedule(20, "req_one").await;
    sleep(Duration::from_millis(1050)).await;
    host.fail_put = true;
    assert_eq!(host.schedule(21, "req_one").await["decision"], "delegate");
    assert!(host.state.is_empty());
    let history = host.history(25, "limit=20").await;
    assert_eq!(history["pagination"]["total"], 0);
    assert!(
        history["storage"]["lastWriteError"]
            .as_str()
            .unwrap()
            .contains("配额")
    );
    host.fail_put = false;
    assert_eq!(host.schedule(22, "req_one").await["decision"], "pick");
    host.cancel(10).await;
    host.stop().await;
}

#[tokio::test]
async fn cancelling_scheduler_during_persistence_releases_intent() {
    let mut host = Host::start(HashMap::new(), 0).await;
    host.model(10, "req_one", false).await;
    host.schedule(20, "req_one").await;
    sleep(Duration::from_millis(1050)).await;
    host.hold_put = true;
    host.send_schedule(21, "req_one").await;
    while host.held_put.is_empty() {
        host.pump().await;
    }
    host.cancel(21).await;
    host.hold_put = false;
    assert_eq!(host.schedule(22, "req_one").await["decision"], "pick");
    host.cancel(10).await;
    host.stop().await;
}

#[tokio::test]
async fn all_history_survives_pagination_restart_and_index_rollover() {
    let mut host = Host::start(HashMap::new(), 0).await;
    // 每批保留的活动父调用低于 SDK 并发上限，不需要真实上游
    for batch in 0..9 {
        for offset in 0..64 {
            let n = batch * 64 + offset;
            let request = format!("req_{n:04}");
            host.model_input(10000 + n, &request, false, br#"{"model":"test-model","session_id":"session-private-test","previous_response_id":"test_response","input":"test"}"#.to_vec()).await;
            assert_eq!(
                host.schedule(20000 + n, &request).await["decision"],
                "delegate"
            );
        }
        sleep(Duration::from_millis(1050)).await;
        for offset in 0..64 {
            let n = batch * 64 + offset;
            let request = format!("req_{n:04}");
            // CAS 冲突只重试状态，不重放选号或删除记录
            if n == 128 {
                host.conflict_puts = 1;
            }
            assert_eq!(
                host.schedule(30000 + n, &request).await["decision"],
                "delegate"
            );
            host.cancel(10000 + n).await;
        }
    }
    assert_eq!(
        host.state
            .keys()
            .filter(|(namespace, key)| namespace == "history" && key.starts_with("record-"))
            .count(),
        576
    );
    let first = host.history(50000, "limit=100").await;
    assert_eq!(first["pagination"]["total"], 576);
    assert_eq!(first["records"].as_array().unwrap().len(), 100);
    let anchor = first["asOf"].as_u64().unwrap();
    let mut ids = HashSet::new();
    for page in 1..=6 {
        let history = host
            .history(
                50000 + page,
                &format!("limit=100&page={page}&asOf={anchor}"),
            )
            .await;
        for record in history["records"].as_array().unwrap() {
            assert!(ids.insert(record["id"].as_str().unwrap().to_owned()));
        }
    }
    assert_eq!(ids.len(), 576);
    let last = host.history(51000, "requestId=req_0000&limit=20").await;
    assert_eq!(last["records"][0]["requestId"], "req_0000");
    let state = host.state.clone();
    let version = host.version;
    host.stop().await;
    let mut host = Host::start(state, version).await;
    let page = host
        .history(52000, "limit=20&page=29&result=unchanged")
        .await;
    assert_eq!(page["pagination"]["total"], 576);
    assert_eq!(page["records"].as_array().unwrap().len(), 16);
    host.stop().await;
}

#[tokio::test]
async fn incomplete_append_is_visible_and_committed_record_is_recovered_without_replaying_pick() {
    for fail_record in [true, false] {
        let mut host = Host::start(HashMap::new(), 0).await;
        host.model(10, "req_one", false).await;
        host.schedule(20, "req_one").await;
        sleep(Duration::from_millis(1050)).await;
        host.fail_record = fail_record;
        host.fail_commit = !fail_record;
        assert_eq!(host.schedule(21, "req_one").await["decision"], "delegate");
        let history = host.history(30, "limit=20").await;
        assert_eq!(history["pagination"]["total"], 0);
        assert_eq!(history["storage"]["incompleteWrites"], 1);
        host.cancel(10).await;
        host.fail_record = false;
        host.fail_commit = false;
        if !fail_record {
            host.call(
                40,
                Stage::Maintenance,
                "plugin.reconcile",
                None,
                json!({}),
                vec![],
            )
            .await;
            host.result(40).await;
            let history = host.history(41, "limit=20").await;
            assert_eq!(history["pagination"]["total"], 1);
            assert_eq!(history["storage"]["incompleteWrites"], 0);
        }
        host.stop().await;
    }
}

#[tokio::test]
async fn settings_reject_incomplete_or_foreign_input_before_native_dispatch() {
    let mut host = Host::start(HashMap::new(), 0).await;
    for (id, body) in [
        (
            10,
            json!({"expectedRevision":1,"configuration":{},"bindings":[]}),
        ),
        (
            11,
            json!({"expectedRevision":1,"configuration":{"waitSeconds":0},"bindings":[]}),
        ),
        (
            12,
            json!({"expectedRevision":1,"configuration":{"waitSeconds":15},"bindings":[],"unrelated":"value"}),
        ),
    ] {
        host.call(id, Stage::Management, "management.handle", None, json!({"method":"POST","path":"settings","query":"","content_type":"application/json","headers":[]}), serde_json::to_vec(&body).unwrap()).await;
        let (response, payload) = host.result(id).await;
        assert_eq!(response["status"], 400);
        assert_eq!(
            serde_json::from_slice::<Value>(&payload).unwrap()["error"]["code"],
            "invalid_settings"
        );
    }
    host.stop().await;
}

#[tokio::test]
async fn native_queue_timeout_prompts_one_full_replay_and_selects_the_backup() {
    let mut host = Host::start(HashMap::new(), 0).await;
    let body = |turn: &str, previous: bool| {
        let mut value = json!({
            "model":"test-model",
            "client_metadata":{"session_id":"root","thread_id":"root","turn_id":turn},
            "input":[{"type":"message","role":"user","content":"test"}]
        });
        if previous {
            value["previous_response_id"] = json!("resp_A");
        }
        serde_json::to_vec(&value).unwrap()
    };
    let account_a = json!({"account_id":"A","available":false,"weight":1,"in_flight":3,"maximum_concurrency":3});
    for (offset, host_queue_failover, has_backup) in [
        (0, true, true),
        (30, true, false),
        (60, false, true),
        (90, false, false),
    ] {
        host.host_queue_failover = host_queue_failover;
        let request = format!("req_native_{offset}");
        let turn = format!("turn_{offset}");
        host.model_input_with_transport(
            10 + offset,
            &request,
            false,
            MiddlewareTransport::WebSocket,
            body(&turn, true),
        )
        .await;
        host.send_candidates(11 + offset, &request, json!([account_a]))
            .await;
        assert_eq!(host.result(11 + offset).await.0["decision"], "delegate");

        let (next, _) = host.next.remove(&(10 + offset)).unwrap();
        host.respond(next, json!({"response":"initial_error","protocol":"openai","status":429,"headers":[],"body":{"handle":"error_frame","framing":"json_document"}}), vec![]).await;
        let (result, _) = host.result(10 + offset).await;
        assert_eq!(result["body"]["kind"], "pass_through");

        let error = json!({
            "type":"error", "status":429, "error":{"code":"concurrency_queue_timeout"},
            "headers":{"x-request-id":request}
        });
        let mut unrelated = error.clone();
        unrelated["headers"]["x-request-id"] = json!("different_request");
        assert_eq!(
            host.websocket_output(12 + offset, &unrelated).await,
            unrelated
        );
        let recovery = host.websocket_output(13 + offset, &error).await;
        assert_eq!(recovery["status"], 400);
        assert_eq!(recovery["error"]["code"], "previous_response_not_found");
        assert_eq!(recovery["headers"], error["headers"]);
        assert_eq!(host.websocket_output(14 + offset, &error).await, error);

        let replay = format!("req_replay_{offset}");
        let other = format!("req_other_{offset}");
        host.model_input_with_transport(
            15 + offset,
            &other,
            false,
            MiddlewareTransport::WebSocket,
            body("different_turn", false),
        )
        .await;
        assert_eq!(
            host.schedule(16 + offset, &other).await["decision"],
            "delegate"
        );
        host.cancel(15 + offset).await;
        host.model_input_with_transport(
            17 + offset,
            &replay,
            false,
            MiddlewareTransport::WebSocket,
            body(&turn, false),
        )
        .await;
        if has_backup {
            let decision = host.schedule(18 + offset, &replay).await;
            assert_eq!(decision["account_id"], "B");
            assert_eq!(host.record(&replay)["reason"], "native_queue_replay");
            assert_eq!(host.record(&replay)["originalAccountId"], "A");
        } else {
            host.send_candidates(18 + offset, &replay, json!([account_a]))
                .await;
            assert_eq!(host.result(18 + offset).await.0["decision"], "delegate");
        }
        host.cancel(17 + offset).await;

        let repeated = format!("req_repeated_{offset}");
        host.model_input_with_transport(
            19 + offset,
            &repeated,
            false,
            MiddlewareTransport::WebSocket,
            body(&turn, false),
        )
        .await;
        assert_eq!(
            host.schedule(20 + offset, &repeated).await["decision"],
            "delegate"
        );
        host.cancel(19 + offset).await;
    }
    host.stop().await;
}
