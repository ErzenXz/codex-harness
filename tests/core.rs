use codex_harness::{
    auth::claims,
    config::Config,
    jobs::Jobs,
    metrics::Metrics,
    protocol::{Peer, Server, read_frame, repair_calls},
    provider::{ChatStream, SseDecoder, chat_request},
    search::search,
    session::{Session, Store, Turn},
    workspace::{Workspace, read_bounded, truncate},
};
use serde_json::json;
use tempfile::tempdir;
use tokio::{io::BufReader, sync::Semaphore};
use tokio_util::sync::CancellationToken;

#[test]
fn config_rejects_insecure_urls_and_invalid_limits() {
    for url in [
        "http://example.com/v1",
        "https://user:password@example.com",
        "https://example.com?key=x",
        "https://example.com/#x",
    ] {
        let config = Config {
            base_url: url.into(),
            ..Config::default()
        };
        assert!(config.validate().is_err(), "{url}");
    }
    let mut config = Config {
        base_url: "http://127.0.0.1:1234/v1".into(),
        ..Config::default()
    };
    assert!(config.validate().is_ok());
    config.max_model_requests = 0;
    assert!(config.validate().is_err());
    assert!(toml::from_str::<Config>("unknown_typo = 1").is_err());
    for example in [
        include_str!("../examples/chatgpt.toml"),
        include_str!("../examples/ollama.toml"),
        include_str!("../examples/openrouter.toml"),
    ] {
        toml::from_str::<Config>(example)
            .unwrap()
            .validate()
            .unwrap();
    }
}

#[test]
fn sse_handles_every_split_unicode_crlf_and_multiline() {
    let stream =
        ": comment\r\ndata: {\"hello\":\"世界\"}\r\n\r\ndata: one\ndata: two\n\ndata: [DONE]\n\n";
    for split in 0..stream.len() {
        let mut decoder = SseDecoder::default();
        let mut events = decoder.push(&stream.as_bytes()[..split]).unwrap();
        events.extend(decoder.push(&stream.as_bytes()[split..]).unwrap());
        assert_eq!(events, vec!["{\"hello\":\"世界\"}", "one\ntwo", "[DONE]"]);
    }
    assert!(
        SseDecoder::default()
            .push(&vec![b'x'; 4 * 1024 * 1024 + 1])
            .is_err()
    );
    assert!(claims("not-a-token").is_err());
    assert!(claims("header.!!!.signature").is_err());
}

#[test]
fn chat_reassembles_parallel_function_deltas_and_rejects_partial_calls() {
    let mut stream = ChatStream::default();
    stream.push(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"grep","arguments":"{\"pattern\":"}},{"index":1,"id":"call-2","function":{"name":"glob","arguments":"{}"}}]}}]})).unwrap();
    stream.push(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"main\"}"}}]},"finish_reason":"tool_calls"}]})).unwrap();
    let response = stream.finish(String::new()).unwrap();
    assert!(!response.incomplete);
    assert_eq!(response.output.len(), 2);
    assert_eq!(response.output[0]["arguments"], "{\"pattern\":\"main\"}");
    assert!(ChatStream::default().finish(String::new()).is_err());
    let mut stream = ChatStream::default();
    stream.push(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{"}}]},"finish_reason":"tool_calls"}]})).unwrap();
    assert!(stream.finish(String::new()).is_err());
    let mut stream = ChatStream::default();
    stream
        .push(&json!({"choices":[{"delta":{},"finish_reason":"length"}]}))
        .unwrap();
    assert!(stream.finish("partial".into()).unwrap().incomplete);
}

#[test]
fn chat_translation_keeps_tool_pairing_and_omits_encrypted_reasoning() {
    let request = chat_request(
        "model",
        "instructions",
        &[
            json!({"role":"user","content":"go"}),
            json!({"type":"reasoning","encrypted_content":"not-for-chat"}),
            json!({"type":"function_call","call_id":"c1","name":"glob","arguments":"{}"}),
            json!({"type":"function_call","call_id":"c2","name":"grep","arguments":"{}"}),
            json!({"type":"function_call_output","call_id":"c1","output":"a"}),
            json!({"type":"function_call_output","call_id":"c2","output":"b"}),
        ],
        &[],
        100,
    )
    .unwrap();
    assert_eq!(
        request["messages"][2]["tool_calls"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(request["messages"][3]["tool_call_id"], "c1");
    assert!(!request.to_string().contains("not-for-chat"));
}

#[test]
fn usage_is_token_weighted_and_unknown_cache_details_stay_unknown() {
    let metrics = Metrics::default();
    let unknown = metrics
        .record(&json!({"input_tokens":100,"output_tokens":5}))
        .unwrap();
    assert!(unknown["cachedInputTokens"].is_null());
    assert!(metrics.snapshot()["inputTokenCacheHitRate"].is_null());
    metrics.record(&json!({"input_tokens":1000,"input_tokens_details":{"cached_tokens":900},"output_tokens":10}));
    metrics.record(&json!({"prompt_tokens":3000,"prompt_tokens_details":{"cached_tokens":3000},"completion_tokens":30}));
    let stats = metrics.snapshot();
    assert_eq!(stats["responsesWithCacheDetails"], 2);
    assert_eq!(stats["inputTokens"], 4000);
    assert_eq!(stats["cachedInputTokens"], 3900);
    assert!((stats["inputTokenCacheHitRate"].as_f64().unwrap() - 0.975).abs() < 1e-12);
    assert_eq!(truncate("a世界", 4), "a世\n[truncated]");
}

#[tokio::test]
async fn framing_is_newline_delimited_bounded_and_accepts_eof() {
    let mut reader = BufReader::new(&b"{\"a\":1}\n{\"b\":2}"[..]);
    assert_eq!(
        read_frame(&mut reader).await.unwrap().unwrap(),
        b"{\"a\":1}\n"
    );
    assert_eq!(
        read_frame(&mut reader).await.unwrap().unwrap(),
        b"{\"b\":2}"
    );
    assert!(read_frame(&mut reader).await.unwrap().is_none());
    let oversized = vec![b'x'; 1024 * 1024 + 1];
    assert!(
        read_frame(&mut BufReader::new(oversized.as_slice()))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn workspace_rejects_traversal_and_protected_paths() {
    let dir = tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).await.unwrap();
    for path in [
        "../escape",
        ".git/config",
        ".env",
        ".env.local",
        ".ssh/id_rsa",
    ] {
        assert!(workspace.resolve(path, true).await.is_err(), "{path}");
    }
    assert!(
        Workspace::new(std::path::Path::new("relative"))
            .await
            .is_err()
    );
    let target = workspace.resolve("src/new.rs", true).await.unwrap();
    assert_eq!(target, dir.path().join("src/new.rs"));
    let file = dir.path().join("binary");
    tokio::fs::write(&file, b"hello\0world").await.unwrap();
    assert!(read_bounded(&file, 100).await.is_err());
    #[cfg(unix)]
    {
        let outside = tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        assert!(workspace.resolve("link/new", true).await.is_err());
    }
}

#[tokio::test]
async fn persistence_is_atomic_workspace_scoped_and_private() {
    let dir = tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).await.unwrap();
    let config = Config::default();
    let store = Store::new(dir.path().join("state")).await.unwrap();
    let mut session = Session::new(&workspace, &config);
    for index in 0..4 {
        session.turns.push_back(Turn {
            items: vec![json!({"role":"user","content":format!("turn-{index}-{}","x".repeat(50))})],
        });
    }
    let context = session.context(180);
    assert_eq!(session.turns.len(), 2);
    assert_eq!(session.dropped_turns, 2);
    assert_eq!(context.len(), 2);
    store.save(&session).await.unwrap();
    let loaded = store.load(session.id, &workspace).await.unwrap();
    assert_eq!(loaded.dropped_turns, 2);
    let other = tempdir().unwrap();
    assert!(
        store
            .load(session.id, &Workspace::new(other.path()).await.unwrap())
            .await
            .is_err()
    );
    let mut files = tokio::fs::read_dir(dir.path().join("state")).await.unwrap();
    let file = files.next_entry().await.unwrap().unwrap();
    assert!(!file.file_name().to_string_lossy().ends_with(".tmp"));
    assert!(files.next_entry().await.unwrap().is_none());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            file.metadata().await.unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let mut items = vec![
        json!({"type":"function_call","call_id":"interrupted","name":"shell","arguments":"{}"}),
    ];
    repair_calls(&mut items);
    repair_calls(&mut items);
    assert_eq!(items.len(), 2);
}

#[tokio::test]
async fn permissions_require_explicit_allow_and_cancel_promptly() {
    for allow in [true, false] {
        let (peer, mut rx) = Peer::channel(8);
        let client = peer.clone();
        tokio::spawn(async move {
            let message = rx.recv().await.unwrap();
            client.resolve(json!({"id":message["id"],"result":{"outcome":{"outcome":"selected","optionId":if allow {"allow-once"} else {"reject-once"}}}})).await;
        });
        assert_eq!(
            peer.permission(
                "session",
                "call",
                "edit",
                "edit",
                json!({}),
                &CancellationToken::new()
            )
            .await
            .is_ok(),
            allow
        );
    }
    let (peer, _rx) = Peer::channel(8);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        peer.permission("s", "c", "title", "execute", json!({}), &cancel)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn job_admission_ownership_and_eviction_are_bounded() {
    let jobs = Jobs::new(1, 100);
    let token = CancellationToken::new();
    let job = jobs
        .reserve("agent", Some("owner".into()), token.child_token())
        .await
        .unwrap();
    assert!(
        jobs.reserve("agent", None, token.child_token())
            .await
            .is_err()
    );
    assert!(
        jobs.status(&job.id, Some("other"), 0, &token)
            .await
            .is_err()
    );
    job.finish("completed", "x".repeat(200), None, 100).await;
    assert_eq!(job.snapshot().await["truncated"], true);
    jobs.reserve("agent", None, token.child_token())
        .await
        .unwrap();
    assert!(jobs.get(&job.id).await.is_err());
    jobs.cancel_all().await;
}

#[tokio::test]
async fn search_is_ignore_aware_bounded_and_never_searches_state() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\nneedle\n").unwrap();
    std::fs::write(dir.path().join("ignored.rs"), "needle secret").unwrap();
    std::fs::write(dir.path().join(".env"), "needle secret").unwrap();
    std::fs::create_dir(dir.path().join("state")).unwrap();
    std::fs::write(dir.path().join("state/auth.json"), "needle secret").unwrap();
    let workspace = Workspace::new(dir.path()).await.unwrap();
    let gate = Semaphore::new(1);
    let result = search(
        &workspace,
        &dir.path().join("state"),
        &gate,
        "grep",
        &json!({"pattern":"needle","glob":"**/*"}),
        1024,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let text = result["content"].as_str().unwrap();
    assert!(text.contains("main.rs:2"), "{result}");
    assert!(!text.contains("secret"), "{result}");
    let files = search(
        &workspace,
        &dir.path().join("state"),
        &gate,
        "glob",
        &json!({"pattern":"**/*"}),
        1024,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(files["content"].as_str().unwrap().trim(), "main.rs");
    std::fs::write(dir.path().join("large.rs"), "needle\n".repeat(10000)).unwrap();
    let capped = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        search(
            &workspace,
            &dir.path().join("state"),
            &gate,
            "grep",
            &json!({"pattern":"needle","glob":null}),
            100,
            &CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(capped["truncated"], true);
}

#[tokio::test]
async fn acp_initialization_modes_limits_and_resumption() {
    let dir = tempdir().unwrap();
    let config = Config {
        state_dir: dir.path().join("state"),
        max_sessions: 1,
        ..Config::default()
    };
    let (peer, mut updates) = Peer::channel(64);
    let server = Server::new(config, peer).await.unwrap();
    assert!(
        server
            .handle("session/new", json!({"cwd":dir.path(),"mcpServers":[]}))
            .await
            .is_err()
    );
    assert_eq!(
        server
            .handle("initialize", json!({"protocolVersion":1}))
            .await
            .unwrap()["protocolVersion"],
        1
    );
    let new = server
        .handle("session/new", json!({"cwd":dir.path(),"mcpServers":[]}))
        .await
        .unwrap();
    let id = new["sessionId"].clone();
    assert!(
        server
            .handle("session/new", json!({"cwd":dir.path(),"mcpServers":[]}))
            .await
            .is_err()
    );
    server
        .handle("session/set_mode", json!({"sessionId":id,"modeId":"plan"}))
        .await
        .unwrap();
    assert_eq!(
        updates.recv().await.unwrap()["params"]["update"]["currentModeId"],
        "plan"
    );
    server
        .handle("_harness/session/unload", json!({"sessionId":id}))
        .await
        .unwrap();
    let loaded = server
        .handle(
            "session/load",
            json!({"cwd":dir.path(),"sessionId":id,"mcpServers":[]}),
        )
        .await
        .unwrap();
    assert_eq!(loaded["modes"]["currentModeId"], "plan");
    assert!(
        server
            .handle(
                "session/new",
                json!({"cwd":dir.path(),"mcpServers":[{"name":"not-implemented"}]})
            )
            .await
            .is_err()
    );
    assert!(
        server.handle("_harness/stats", json!({})).await.unwrap()["inputTokenCacheHitRate"]
            .is_null()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn shell_output_timeout_and_environment_are_bounded() {
    let dir = tempdir().unwrap();
    let jobs = Jobs::new(8, 1024);
    let (peer, mut rx) = Peer::channel(32);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let cancel = CancellationToken::new();
    let output = jobs
        .shell(
            dir.path(),
            "printf 'ok'; test -z \"$OPENAI_API_KEY\"",
            2,
            false,
            None,
            &cancel,
            &peer,
            "s",
        )
        .await
        .unwrap();
    assert_eq!(output["status"], "completed");
    assert_eq!(output["output"], "ok");
    let output = jobs
        .shell(
            dir.path(),
            "sleep 30 & wait",
            1,
            false,
            None,
            &cancel,
            &peer,
            "s",
        )
        .await
        .unwrap();
    assert_eq!(output["status"], "timed_out");
    let output = jobs
        .shell(
            dir.path(),
            "yes x | head -c 100000",
            2,
            false,
            None,
            &cancel,
            &peer,
            "s",
        )
        .await
        .unwrap();
    assert_eq!(output["truncated"], true);
    assert!(output["output"].as_str().unwrap().len() <= 1024);
}
