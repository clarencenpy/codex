//! The whole bridge, in process, against a scripted engine.

mod common;

use common::Client;
use common::new_bridge;
use common::shape;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn handshake_stubs_and_unknown_methods() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, _) = new_bridge(dir.path());
    let mut client = Client::connect(&bridge);

    let early = client.request("thread/list", json!({})).await.unwrap_err();
    assert_eq!(early["message"], "Not initialized");
    client.initialize().await;
    let again = client
        .request("initialize", json!({"clientInfo": {"name": "t", "title": null, "version": "0"}, "capabilities": null}))
        .await
        .unwrap_err();
    assert_eq!(again["message"], "Already initialized");

    let list = client.request("thread/list", json!({})).await.unwrap();
    assert_eq!(list["data"], json!([]));
    let account = client.request("account/read", json!({})).await.unwrap();
    assert_eq!(account["requiresOpenaiAuth"], false);
    client.request("config/read", json!({})).await.unwrap();
    let unknown = client
        .request("fs/readFile", json!({"path": "/etc/passwd"}))
        .await
        .unwrap_err();
    assert_eq!(unknown["code"], -32601);
    let missing = client
        .request("thread/read", json!({"threadId": "nope"}))
        .await
        .unwrap_err();
    assert_eq!(missing["message"], "thread not found: nope");
    let invalid = client
        .request("turn/start", json!({"threadId": 7}))
        .await
        .unwrap_err();
    assert_eq!(invalid["code"], -32602);
}

#[tokio::test]
async fn model_list_comes_from_config() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, log) = new_bridge(dir.path());
    let mut client = Client::connect(&bridge);
    client.initialize().await;

    let models = client.request("model/list", json!({})).await.unwrap();
    let summary: Vec<(String, String, bool)> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["id"].as_str().unwrap().to_string(),
                m["displayName"].as_str().unwrap().to_string(),
                m["isDefault"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("sonnet".to_string(), "sonnet".to_string(), false),
            ("opus".to_string(), "Claude Opus".to_string(), true)
        ]
    );

    let refused = client
        .request("thread/start", json!({"cwd": dir.path(), "model": "gpt-5"}))
        .await
        .unwrap_err();
    assert_eq!(refused["code"], -32602);

    let started = client
        .request(
            "thread/start",
            json!({"cwd": dir.path(), "model": "sonnet"}),
        )
        .await
        .unwrap();
    let thread_id = started["thread"]["id"].as_str().unwrap().to_string();
    client.run_turn(&thread_id, "hi").await;
    assert_eq!(
        log.lock().unwrap().launches[0].model.as_deref(),
        Some("sonnet")
    );
}

#[tokio::test]
async fn a_turn_streams_the_reply() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, log) = new_bridge(dir.path());
    let mut client = Client::connect(&bridge);
    client.initialize().await;
    let thread_id = client.start_thread(dir.path()).await;
    client.wait_for(|m| m["method"] == "thread/started").await;

    let turn = client.run_turn(&thread_id, "hello bridge").await;
    assert_eq!(shape(&turn), "completed: userMessage, agentMessage");
    assert_eq!(turn["items"][1]["text"], "echo: hello bridge");
    let deltas: Vec<&str> = client
        .notifications("item/agentMessage/delta")
        .iter()
        .map(|m| m["params"]["delta"].as_str().unwrap())
        .collect();
    assert_eq!(deltas.len(), 2, "text arrives as it is streamed");
    assert_eq!(deltas.concat(), "echo: hello bridge");
    client.assert_ordered();

    let listed = client.request("thread/list", json!({})).await.unwrap();
    assert_eq!(listed["data"][0]["preview"], "hello bridge");
    assert_eq!(listed["data"][0]["status"]["type"], "idle");
    let log = log.lock().unwrap();
    assert_eq!(log.launches.len(), 1);
    assert!(!log.launches[0].resume);
    assert_eq!(log.prompts, vec!["hello bridge".to_string()]);
}

#[tokio::test]
async fn prompts_are_plain_text_and_queued_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, log) = new_bridge(dir.path());
    let mut client = Client::connect(&bridge);
    client.initialize().await;
    let thread_id = client.start_thread(dir.path()).await;

    let mut turn_ids = Vec::new();
    for prompt in ["first", "line one\nline two", "/clear"] {
        turn_ids.push(client.start_turn(&thread_id, prompt).await);
    }
    let mut replies = Vec::new();
    for turn_id in &turn_ids {
        let turn = client.turn_completed(turn_id).await;
        replies.push(turn["items"][1]["text"].as_str().unwrap().to_string());
    }
    // A leading slash gets a space so Claude Code does not run it as a command.
    assert_eq!(
        replies,
        vec!["echo: first", "echo: line one\nline two", "echo:  /clear"]
    );
    assert_eq!(
        log.lock().unwrap().launches.len(),
        1,
        "one process serves every turn"
    );
    client.assert_ordered();
}

#[tokio::test]
async fn tool_calls_become_command_file_and_mcp_items() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, _) = new_bridge(dir.path());
    let mut client = Client::connect(&bridge);
    client.decision = Some("accept");
    client.initialize().await;
    let thread_id = client.start_thread(dir.path()).await;

    let turn = client
        .run_turn(
            &thread_id,
            "mcp please, then run: make test\nand edit: /work/notes.txt",
        )
        .await;
    assert_eq!(
        shape(&turn),
        "completed: userMessage, commandExecution(completed), fileChange(completed), mcpToolCall(completed), agentMessage"
    );
    let command = &turn["items"][1];
    assert_eq!(command["command"], "make test");
    assert_eq!(command["aggregatedOutput"], "ran make test");
    assert_eq!(command["exitCode"], 0);
    let change = &turn["items"][2]["changes"][0];
    assert_eq!(change["path"], "/work/notes.txt");
    assert_eq!(change["diff"], "@@ -1,1 +1,1 @@\n-hi\n+hello\n");
    assert_eq!(turn["items"][3]["server"], "files");
    assert_eq!(turn["items"][3]["tool"], "search");

    let asks: Vec<&str> = client
        .messages
        .iter()
        .filter(|m| m.get("id").is_some() && m.get("method").is_some())
        .map(|m| m["method"].as_str().unwrap())
        .collect();
    assert_eq!(
        asks,
        vec![
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval"
        ]
    );
    assert_eq!(client.notifications("serverRequest/resolved").len(), 2);
    let waiting = client
        .notifications("thread/status/changed")
        .iter()
        .filter(|m| m["params"]["status"]["activeFlags"] == json!(["waitingOnApproval"]))
        .count();
    assert_eq!(waiting, 2);
    client.assert_ordered();
}

#[tokio::test]
async fn approval_answers_reach_claude_code() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, log) = new_bridge(dir.path());
    let mut client = Client::connect(&bridge);
    client.initialize().await;
    let thread_id = client.start_thread(dir.path()).await;

    client.decision = Some("acceptForSession");
    let turn = client.run_turn(&thread_id, "run: git status").await;
    assert_eq!(
        shape(&turn),
        "completed: userMessage, commandExecution(completed), agentMessage"
    );

    // Declining lets Claude Code carry on with the turn.
    client.decision = Some("decline");
    let turn = client.run_turn(&thread_id, "run: git push").await;
    assert_eq!(
        shape(&turn),
        "completed: userMessage, commandExecution(declined), agentMessage"
    );

    // Cancelling stops the turn.
    client.decision = Some("cancel");
    let turn = client.run_turn(&thread_id, "edit: /work/a.txt").await;
    assert_eq!(
        shape(&turn),
        "interrupted: userMessage, fileChange(declined)"
    );
    client.assert_ordered();

    let responses = log.lock().unwrap().permission_responses.clone();
    assert_eq!(responses[0]["behavior"], "allow");
    assert_eq!(responses[0]["updatedInput"]["command"], "git status");
    // The suggested rule is applied for this session only, never written to
    // the user's settings.
    assert_eq!(
        responses[0]["updatedPermissions"],
        json!([{"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "git *"}], "behavior": "allow", "destination": "session"}])
    );
    assert_eq!(responses[1]["behavior"], "deny");
    assert_eq!(responses[1].get("interrupt"), None);
    assert_eq!(responses[2]["behavior"], "deny");
    assert_eq!(responses[2]["interrupt"], true);
}

#[tokio::test]
async fn interrupt_stops_the_turn_and_the_thread_stays_usable() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, log) = new_bridge(dir.path());
    let mut client = Client::connect(&bridge);
    client.initialize().await;
    let thread_id = client.start_thread(dir.path()).await;

    let turn_id = client.start_turn(&thread_id, "slow job").await;
    client
        .wait_for(|m| {
            m["method"] == "item/completed" && m["params"]["item"]["type"] == "agentMessage"
        })
        .await;
    let reply = client
        .request(
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": turn_id}),
        )
        .await
        .unwrap();
    assert_eq!(reply, json!({}));
    let turn = client.turn_completed(&turn_id).await;
    assert_eq!(shape(&turn), "interrupted: userMessage, agentMessage");

    let next = client.run_turn(&thread_id, "still there?").await;
    assert_eq!(next["items"][1]["text"], "echo: still there?");
    assert_eq!(
        log.lock().unwrap().launches.len(),
        1,
        "the same process carries on"
    );
    client.assert_ordered();
}

#[tokio::test]
async fn a_turn_interrupted_before_it_starts_never_reaches_claude_code() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, log) = new_bridge(dir.path());
    let mut client = Client::connect(&bridge);
    client.initialize().await;
    let thread_id = client.start_thread(dir.path()).await;

    let running = client.start_turn(&thread_id, "slow job").await;
    let queued = client.start_turn(&thread_id, "never sent").await;
    client
        .request(
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": queued}),
        )
        .await
        .unwrap();
    client
        .request(
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": running}),
        )
        .await
        .unwrap();
    assert_eq!(
        client.turn_completed(&running).await["status"],
        "interrupted"
    );
    assert_eq!(
        shape(&client.turn_completed(&queued).await),
        "interrupted: userMessage"
    );
    assert_eq!(log.lock().unwrap().prompts, vec!["slow job".to_string()]);
    client.assert_ordered();
}

#[tokio::test]
async fn engine_failure_fails_the_turn_and_the_next_turn_resumes_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, log) = new_bridge(dir.path());
    let mut client = Client::connect(&bridge);
    client.initialize().await;
    let thread_id = client.start_thread(dir.path()).await;

    let turn = client.run_turn(&thread_id, "fail now").await;
    assert_eq!(turn["status"], "failed");
    assert_eq!(
        turn["error"]["message"],
        "Claude Code reported an error: the model is overloaded"
    );

    let turn = client.run_turn(&thread_id, "please die").await;
    assert_eq!(turn["status"], "failed");
    assert_eq!(
        turn["error"]["message"],
        "Claude Code exited (exit status: 1)"
    );
    assert_eq!(client.notifications("error").len(), 2);

    let turn = client.run_turn(&thread_id, "back again").await;
    assert_eq!(turn["items"][1]["text"], "echo: back again");
    let log = log.lock().unwrap();
    assert_eq!(log.launches.len(), 2);
    assert!(
        log.launches[1].resume,
        "a new process continues the same Claude session"
    );
    assert_eq!(log.launches[0].session_id, log.launches[1].session_id);
    client.assert_ordered();
}

#[tokio::test]
async fn threads_survive_a_bridge_restart() {
    let dir = tempfile::tempdir().unwrap();
    let thread_id;
    let session_id;
    {
        let (bridge, log) = new_bridge(dir.path());
        let mut client = Client::connect(&bridge);
        client.initialize().await;
        thread_id = client.start_thread(dir.path()).await;
        client.run_turn(&thread_id, "before").await;
        client.start_turn(&thread_id, "slow job").await;
        client
            .wait_for(|m| {
                m["method"] == "item/completed" && m["params"]["item"]["type"] == "agentMessage"
            })
            .await;
        // Let the session write the running turn down before the "crash".
        tokio::time::sleep(Duration::from_millis(50)).await;
        session_id = log.lock().unwrap().launches[0].session_id.clone();
    }

    let (bridge, log) = new_bridge(dir.path());
    let mut client = Client::connect(&bridge);
    client.initialize().await;
    let listed = client.request("thread/list", json!({})).await.unwrap();
    assert_eq!(listed["data"][0]["id"], thread_id.as_str());
    assert_eq!(listed["data"][0]["status"]["type"], "notLoaded");

    let resumed = client
        .request("thread/resume", json!({"threadId": thread_id}))
        .await
        .unwrap();
    let shapes: Vec<String> = resumed["thread"]["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(shape)
        .collect();
    // The turn that was running when the bridge stopped is kept, as interrupted.
    assert_eq!(
        shapes,
        vec![
            "completed: userMessage, agentMessage",
            "interrupted: userMessage, agentMessage"
        ]
    );

    let turn = client.run_turn(&thread_id, "after").await;
    assert_eq!(turn["items"][1]["text"], "echo: after");
    let log = log.lock().unwrap();
    assert!(log.launches[0].resume);
    assert_eq!(log.launches[0].session_id, session_id);
    client.assert_ordered();
}

#[tokio::test]
async fn an_open_approval_is_asked_again_when_a_client_reconnects() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, _) = new_bridge(dir.path());
    let mut first = Client::connect(&bridge);
    first.initialize().await;
    let thread_id = first.start_thread(dir.path()).await;
    let turn_id = first.start_turn(&thread_id, "run: deploy").await;
    let ask = first
        .wait_for(|m| m["method"] == "item/commandExecution/requestApproval")
        .await;
    bridge.disconnect(first.connection);

    let mut second = Client::connect(&bridge);
    second.initialize().await;
    let resumed = second
        .request("thread/resume", json!({"threadId": thread_id}))
        .await
        .unwrap();
    assert_eq!(
        resumed["thread"]["status"],
        json!({"type": "active", "activeFlags": ["waitingOnApproval"]})
    );
    let again = second
        .messages
        .iter()
        .find(|m| m["method"] == "item/commandExecution/requestApproval")
        .cloned();
    let again = match again {
        Some(again) => again,
        None => {
            second
                .wait_for(|m| m["method"] == "item/commandExecution/requestApproval")
                .await
        }
    };
    assert_eq!(again["params"]["itemId"], ask["params"]["itemId"]);
    second
        .send(json!({"id": again["id"], "result": {"decision": "accept"}}))
        .await;
    let turn = second.turn_completed(&turn_id).await;
    assert_eq!(
        shape(&turn),
        "completed: userMessage, commandExecution(completed), agentMessage"
    );
}

#[tokio::test]
async fn archive_ends_the_thread() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, _) = new_bridge(dir.path());
    let mut client = Client::connect(&bridge);
    client.initialize().await;
    let thread_id = client.start_thread(dir.path()).await;
    client.run_turn(&thread_id, "bye").await;

    client
        .request(
            "thread/name/set",
            json!({"threadId": thread_id, "name": "Farewell"}),
        )
        .await
        .unwrap();
    let read = client
        .request(
            "thread/read",
            json!({"threadId": thread_id, "includeTurns": true}),
        )
        .await
        .unwrap();
    assert_eq!(read["thread"]["name"], "Farewell");
    assert_eq!(read["thread"]["turns"].as_array().unwrap().len(), 1);

    client
        .request("thread/archive", json!({"threadId": thread_id}))
        .await
        .unwrap();
    assert_eq!(client.notifications("thread/archived").len(), 1);
    assert_eq!(
        client.request("thread/list", json!({})).await.unwrap()["data"],
        json!([])
    );
    let archived = client
        .request("thread/list", json!({"archived": true}))
        .await
        .unwrap();
    assert_eq!(archived["data"].as_array().unwrap().len(), 1);
    let refused = client
        .request("turn/start", json!({"threadId": thread_id, "input": [{"type": "text", "text": "x", "text_elements": []}]}))
        .await
        .unwrap_err();
    assert_eq!(
        refused["message"],
        format!("thread is archived: {thread_id}")
    );
}

#[tokio::test]
async fn newline_delimited_json_transport() {
    let dir = tempfile::tempdir().unwrap();
    let (bridge, _) = new_bridge(dir.path());
    let (mut client_side, server_side) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_side);
    let serving = tokio::spawn(codex_cc_bridge::transport::serve_lines(
        bridge,
        server_read,
        server_write,
    ));

    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    let input = concat!(
        "this is not json\n",
        r#"{"id":1,"method":"initialize","params":{"clientInfo":{"name":"t","title":null,"version":"0"},"capabilities":null}}"#,
        "\n",
        r#"{"id":2,"method":"thread/list","params":{}}"#,
        "\n",
    );
    client_side.write_all(input.as_bytes()).await.unwrap();
    client_side.shutdown().await.unwrap();
    let mut output = String::new();
    client_side.read_to_string(&mut output).await.unwrap();
    serving.await.unwrap();

    let replies: Vec<Value> = output
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(
        replies[0]["result"]["userAgent"],
        format!("cc-codex-bridge/{}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(replies[1]["id"], 2);
    assert_eq!(replies[1]["result"]["data"], json!([]));
}

#[test]
fn listen_addresses() {
    use codex_cc_bridge::transport::Listen;
    use codex_cc_bridge::transport::parse_listen;
    assert_eq!(parse_listen("stdio://").unwrap(), Listen::Stdio);
    assert_eq!(
        parse_listen("ws://127.0.0.1:4500").unwrap(),
        Listen::Tcp {
            host: "127.0.0.1".to_string(),
            port: 4500
        }
    );
    assert_eq!(
        parse_listen("unix:///tmp/ccb.sock").unwrap(),
        Listen::Unix("/tmp/ccb.sock".into())
    );
    // Single user, single machine.
    assert!(parse_listen("ws://0.0.0.0:4500").is_err());
    assert!(parse_listen("ws://example.com:4500").is_err());
}
