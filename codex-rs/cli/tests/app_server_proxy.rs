use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use app_test_support::MockResponsesConfig;
use app_test_support::create_escalated_command_execution_sse_response;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::process::Command;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::Message;

const DEADLINE: Duration = Duration::from_secs(60);

struct Daemon {
    home: TempDir,
    binary: PathBuf,
}

impl Daemon {
    fn new() -> Result<Self> {
        let home = tempfile::tempdir()?;
        let binary = codex_utils_cargo_bin::cargo_bin("codex")?.canonicalize()?;
        let managed = home.path().join("packages/app-server-daemon/current/bin");
        std::fs::create_dir_all(&managed)?;
        let executable = managed.join(if cfg!(windows) { "codex.exe" } else { "codex" });
        #[cfg(unix)]
        std::os::unix::fs::symlink(&binary, executable)?;
        #[cfg(not(unix))]
        codex_utils_cargo_bin::copy_executable(&binary, &executable)?;
        std::fs::create_dir(home.path().join("app-server-daemon"))?;
        std::fs::write(
            home.path().join("app-server-daemon/settings.json"),
            r#"{"remoteControlEnabled":false,"shutdownGraceSeconds":0,"updater":{"autoUpdateEnabled":false}}"#,
        )?;
        MockResponsesConfig::new("http://127.0.0.1:1")
            .with_root_config("analytics.enabled = false")
            .write(home.path())?;
        Ok(Self { home, binary })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .env("CODEX_HOME", self.home.path())
            .env("CODEX_SQLITE_HOME", self.home.path())
            .env("CODEX_APP_SERVER_DISABLE_MANAGED_CONFIG", "1")
            .env_remove("CODEX_EXEC_SERVER_URL")
            .env_remove("CODEX_ACCESS_TOKEN")
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY")
            .env_remove("OPENAI_FEDERATION_RULE_ID")
            .env_remove("OPENAI_IDENTITY_TOKEN_FILE")
            .env_remove("OPENAI_WORKLOAD_IDENTITY_CONTEXT")
            .current_dir(self.home.path());
        command
    }

    fn pid_record(&self) -> Result<Vec<u8>> {
        Ok(std::fs::read(
            self.home.path().join("app-server-daemon/app-server.pid"),
        )?)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self
            .command()
            .as_std_mut()
            .args(["app-server", "daemon", "stop"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

struct Client {
    child: Child,
    ws: WebSocketStream<tokio::io::Join<ChildStdout, ChildStdin>>,
    pending: VecDeque<Value>,
}

impl Client {
    async fn connect(daemon: &Daemon) -> Result<Self> {
        let mut child = daemon
            .command()
            .args(["app-server", "proxy", "--start-daemon"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let pipes = tokio::io::join(
            child.stdout.take().context("stdout")?,
            child.stdin.take().context("stdin")?,
        );
        let (ws, _) = timeout(DEADLINE, client_async("ws://localhost/", pipes)).await??;
        let mut client = Self {
            child,
            ws,
            pending: VecDeque::new(),
        };
        client
            .rpc(
                "initialize",
                json!({
                    "clientInfo": {"name": "managed-proxy-test", "version": "1.0"},
                    "capabilities": {"experimentalApi": true}
                }),
            )
            .await?;
        client.send(json!({"method": "initialized"})).await?;
        Ok(client)
    }

    async fn send(&mut self, value: Value) -> Result<()> {
        self.ws
            .send(Message::Text(value.to_string().into()))
            .await?;
        Ok(())
    }

    async fn receive(&mut self, matches: impl Fn(&Value) -> bool) -> Result<Value> {
        if let Some(index) = self.pending.iter().position(&matches) {
            return Ok(self.pending.remove(index).context("buffered message")?);
        }
        timeout(DEADLINE, async {
            loop {
                let message = self.ws.next().await.context("proxy disconnected")??;
                if let Message::Text(text) = message {
                    let value: Value = serde_json::from_str(&text)?;
                    if matches(&value) {
                        return Ok(value);
                    }
                    self.pending.push_back(value);
                }
            }
        })
        .await?
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        // Reusing an ID across connections exercises connection-scoped responses.
        self.send(json!({"id": 1, "method": method, "params": params}))
            .await?;
        let response = self
            .receive(|value| value["id"] == 1 && value.get("method").is_none())
            .await?;
        ensure!(response.get("error").is_none(), "{method}: {response}");
        Ok(response["result"].clone())
    }

    async fn event(&mut self, method: &str) -> Result<Value> {
        Ok(self.receive(|value| value["method"] == method).await?["params"].clone())
    }

    async fn disconnect(mut self) -> Result<()> {
        self.ws.close(/*msg*/ None).await?;
        drop(self.ws);
        ensure!(
            timeout(DEADLINE, self.child.wait()).await??.success(),
            "proxy exit"
        );
        Ok(())
    }
}

#[tokio::test]
async fn managed_proxy_starts_once_and_disconnect_leaves_other_client_alive() -> Result<()> {
    let daemon = Daemon::new()?;
    // Both clients attempt startup before either waits for readiness.
    let (first, second) = tokio::try_join!(Client::connect(&daemon), Client::connect(&daemon))?;
    let mut first = first;
    let mut second = second;
    let pid = daemon.pid_record()?;
    let thread = first.rpc("thread/start", json!({})).await?["thread"]["id"].clone();
    second
        .rpc("thread/resume", json!({"threadId": thread}))
        .await?;
    first
        .rpc(
            "thread/name/set",
            json!({"threadId": thread, "name": "shared"}),
        )
        .await?;
    assert_eq!(
        first.event("thread/name/updated").await?,
        second.event("thread/name/updated").await?
    );
    first.disconnect().await?;
    assert_eq!(daemon.pid_record()?, pid);
    second
        .rpc(
            "thread/name/set",
            json!({"threadId": thread, "name": "still running"}),
        )
        .await?;
    let mut third = Client::connect(&daemon).await?;
    assert_eq!(daemon.pid_record()?, pid);
    assert_eq!(
        third
            .rpc("thread/resume", json!({"threadId": thread}))
            .await?["thread"]["name"],
        "still running"
    );
    let output = daemon
        .command()
        .args(["-c", "model=other", "app-server", "proxy", "--start-daemon"])
        .output()
        .await?;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("does not accept configuration overrides"));
    assert_eq!(daemon.pid_record()?, pid);
    second
        .rpc(
            "thread/name/set",
            json!({"threadId": thread, "name": "unchanged owner"}),
        )
        .await?;
    second.disconnect().await?;
    third.disconnect().await?;
    let fourth = Client::connect(&daemon).await?;
    assert_eq!(daemon.pid_record()?, pid);
    fourth.disconnect().await?;
    Ok(())
}

#[tokio::test]
async fn managed_proxy_startup_failure_is_not_a_protocol_response() -> Result<()> {
    let daemon = Daemon::new()?;
    std::fs::write(
        daemon.home.path().join("app-server-daemon/settings.json"),
        "invalid json",
    )?;
    let output = timeout(
        DEADLINE,
        daemon
            .command()
            .args(["app-server", "proxy", "--start-daemon"])
            .output(),
    )
    .await??;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr)?.contains("failed to start shared app-server daemon"));
    assert!(daemon.pid_record().is_err());
    Ok(())
}

#[tokio::test]
async fn managed_proxy_rejects_configuration_before_starting() -> Result<()> {
    let daemon = Daemon::new()?;
    for options in [
        vec!["-c", "model=other"],
        vec!["--enable", "daemon_auto_start"],
        vec!["--disable", "daemon_auto_start"],
        vec!["--model", "other"],
        vec!["--sandbox", "read-only"],
        vec!["--ask-for-approval", "never"],
        vec!["--no-daemon"],
        vec!["--cd", "."],
    ] {
        let output = daemon
            .command()
            .args(options)
            .args(["app-server", "proxy", "--start-daemon"])
            .output()
            .await?;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8(output.stderr)?.contains("does not accept configuration overrides")
        );
        assert!(daemon.pid_record().is_err());
    }
    Ok(())
}

#[tokio::test]
async fn managed_proxy_rejects_process_specific_environment() -> Result<()> {
    let daemon = Daemon::new()?;
    for key in [
        "CODEX_EXEC_SERVER_URL",
        "OPENAI_FEDERATION_RULE_ID",
        "OPENAI_IDENTITY_TOKEN_FILE",
    ] {
        let output = daemon
            .command()
            .env(key, "test-value")
            .args(["app-server", "proxy", "--start-daemon"])
            .output()
            .await?;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8(output.stderr)?
                .contains("process-specific executor or workload identity")
        );
        assert!(daemon.pid_record().is_err());
    }
    Ok(())
}

#[tokio::test]
async fn shared_proxy_queue_approvals_and_reconnect_use_one_owner() -> Result<()> {
    let server = create_mock_responses_server_sequence(vec![
        create_escalated_command_execution_sse_response(
            vec!["echo".into(), "approval".into()],
            /*workdir*/ None,
            /*timeout_ms*/ None,
            "approval-call",
        )?,
        create_final_assistant_message_sse_response("active done")?,
        create_final_assistant_message_sse_response("queued one done")?,
        create_final_assistant_message_sse_response("queued two done")?,
    ])
    .await;
    let daemon = Daemon::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_approval_policy("on-request")
        .with_root_config("approvals_reviewer = \"user\"\nanalytics.enabled = false")
        .write(daemon.home.path())?;
    let mut first = Client::connect(&daemon).await?;
    let thread = first.rpc("thread/start", json!({})).await?["thread"]["id"].clone();
    let mut second = Client::connect(&daemon).await?;
    second
        .rpc("thread/resume", json!({"threadId": thread}))
        .await?;
    let started = first
        .rpc(
            "turn/start",
            json!({"threadId": thread, "input": [{"type": "text", "text": "start"}]}),
        )
        .await?;
    assert_eq!(
        first.event("turn/started").await?,
        second.event("turn/started").await?
    );
    let approval = first
        .receive(|v| v["method"] == "item/commandExecution/requestApproval")
        .await?;
    assert_eq!(
        approval,
        second
            .receive(|v| v["method"] == "item/commandExecution/requestApproval")
            .await?
    );
    let enqueue = |id: &str| json!({"threadId": thread, "input": [{"type": "text", "text": id}], "clientUserMessageId": id});
    let (a, b) = tokio::try_join!(
        first.rpc("thread/queue/add", enqueue("first")),
        second.rpc("thread/queue/add", enqueue("second"))
    )?;
    assert_ne!(a["queuedSubmission"]["id"], b["queuedSubmission"]["id"]);
    let queue = first
        .rpc("thread/queue/list", json!({"threadId": thread}))
        .await?;
    assert_eq!(
        queue,
        second
            .rpc("thread/queue/list", json!({"threadId": thread}))
            .await?
    );
    assert_eq!(queue["data"].as_array().context("queue data")?.len(), 2);
    for _ in 0..2 {
        assert_eq!(
            first.event("thread/queue/changed").await?,
            second.event("thread/queue/changed").await?
        );
    }
    first.disconnect().await?;
    let mut first = Client::connect(&daemon).await?;
    first
        .rpc("thread/resume", json!({"threadId": thread}))
        .await?;
    assert_eq!(
        queue,
        first
            .rpc("thread/queue/list", json!({"threadId": thread}))
            .await?
    );
    assert_eq!(
        approval,
        first
            .receive(|v| v["method"] == "item/commandExecution/requestApproval")
            .await?
    );
    let steered = first.rpc("turn/steer", json!({"threadId": thread, "expectedTurnId": started["turn"]["id"], "input": [{"type": "text", "text": "explicit steer"}]})).await?;
    assert_eq!(steered["turnId"], started["turn"]["id"]);
    let answer = json!({"id": approval["id"], "result": {"decision": "decline"}});
    tokio::try_join!(first.send(answer.clone()), second.send(answer))?;
    assert_eq!(
        first.event("serverRequest/resolved").await?,
        second.event("serverRequest/resolved").await?
    );
    // Inspect lifecycle events in arrival order, not just completions: each queued
    // turn must start only after its predecessor completes.
    let mut turns = Vec::new();
    for index in 0..3 {
        if index > 0 {
            let event = second
                .receive(|v| v["method"] == "turn/started" || v["method"] == "turn/completed")
                .await?;
            assert_eq!(event["method"], "turn/started");
            assert_eq!(event["params"], first.event("turn/started").await?);
        }
        let event = first.event("turn/completed").await?;
        let other = second
            .receive(|v| v["method"] == "turn/started" || v["method"] == "turn/completed")
            .await?;
        assert_eq!(other["method"], "turn/completed");
        assert_eq!(event, other["params"]);
        turns.push(event["turn"]["id"].clone());
    }
    assert_eq!(turns[0], started["turn"]["id"]);
    assert!(turns[0] != turns[1] && turns[1] != turns[2] && turns[0] != turns[2]);
    assert_eq!(
        first
            .rpc("thread/queue/list", json!({"threadId": thread}))
            .await?["data"],
        json!([])
    );
    let history = first
        .rpc(
            "thread/read",
            json!({"threadId": thread, "includeTurns": true}),
        )
        .await?;
    let mut accepted = Vec::new();
    for turn in history["thread"]["turns"].as_array().context("turns")? {
        for item in turn["items"].as_array().context("items")? {
            if item["type"] == "userMessage"
                && (item["clientId"] == "first" || item["clientId"] == "second")
            {
                accepted.push(item["clientId"].clone());
            }
        }
    }
    let expected: Vec<_> = queue["data"]
        .as_array()
        .context("queue")?
        .iter()
        .map(|v| v["clientUserMessageId"].clone())
        .collect();
    assert_eq!(accepted, expected);
    first.disconnect().await?;
    second.disconnect().await?;
    Ok(())
}
