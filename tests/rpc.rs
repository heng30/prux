//! `prux rpc` 的端到端测试：真的起子进程，走 stdin/stdout 的 JSONL 协议。
//!
//! 覆盖点（对应协议契约）：
//! - 分帧与错误：坏 JSON 回 `parse` 响应、未知命令回 `Unknown command`、`id` 原样回传；
//! - stdout 只放协议记录（每一行都必须能解析成 JSON）；
//! - 状态面：`get_state` / `get_available_models` / `get_available_thinking_levels` 的形状，
//!   且模型对象**不含凭据**；
//! - 队列：空闲期 `steer` / `follow_up` 只排队（`pendingMessageCount` 跟着涨），
//!   `clear_queue` 把文本原样退回；扩展斜杠命令不许排队，走 `prompt` 则回 `handled`；
//! - `bash`：真跑命令并回 `output` / `exitCode`，两条并发各自回包；
//! - 会话：`new_session` 换 id、`set_session_name` / 空名字报错、`export_html` 真的落盘、
//!   `switch_session` 装载历史、`get_entries` / `get_tree` / `get_fork_messages`、`fork` 分叉；
//! - 子命令挂载：扩展启用时 `prux --help` 列得出 `rpc`；
//! - 关停：关掉 stdin 后进程正常退出，且在途 bash 的响应先写完。
//!
//! 不打网络：所有命令都是本地动作。**不测真正跑模型的路径**（`prompt` 的 `started`、
//! `compact`、`agent_settled` 之后的续跑），那需要真模型或假上游服务。

use prux::{
    core::{
        provider::AgentMessage,
        session_manager::{Session, default_session_dir},
        settings_manager::agent_dir,
    },
    test_support::{AgentDirGuard, run_with_timeout},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

/// 单个测试的超时：起进程 + 十几条往返，60s 足够；挂起要能被诊断出来而不是卡住 CI。
const TEST_TIMEOUT: Duration = Duration::from_secs(60);

/// 一条命令的响应等待上限（读 stdout 是阻塞的，超时由 [`TEST_TIMEOUT`] 兜底）。
struct Rpc {
    /// 子进程（drop 时强制杀，避免测试失败留下僵尸）。
    child: Child,
    /// 子进程 stdin；`None` 表示已关闭（关停测试用）。
    stdin: Option<ChildStdin>,
    /// 子进程 stdout，按行读协议记录。
    stdout: BufReader<ChildStdout>,
    /// stderr 累积（子进程写满管道会阻塞，必须有人读）。
    stderr: Arc<Mutex<String>>,
    /// 临时 agent 目录：`PRUX_AGENT_DIR` 指向它，里面只开 `rpc` 扩展。
    /// 只为了持有目录（drop 时删），测试体不直接读它。
    _agent: tempfile::TempDir,
    /// 临时项目目录：子进程 cwd（`export_html` / 会话都落在这里）。
    project: tempfile::TempDir,
    /// 比目标响应更早到达的其它响应（并发命令会乱序回来，先收着别丢）。
    stash: Vec<Value>,
}

impl Rpc {
    /// 起一个子进程，并把 `rpc` 扩展写进它自己的 settings.json。
    fn spawn() -> Rpc {
        let agent = tempfile::tempdir().expect("temp agent dir");
        fs::write(
            agent.path().join("settings.json"),
            r#"{"enabledExtensions":["rpc"]}"#,
        )
        .expect("write settings");

        let project = tempfile::tempdir().expect("temp project dir");
        let mut child = Command::new(env!("CARGO_BIN_EXE_prux"))
            .arg("rpc")
            .current_dir(project.path())
            .env("PRUX_AGENT_DIR", agent.path())
            // 别让开发者机器上的 provider 环境变量影响模型解析结果
            .env_remove("PRUX_PROVIDER")
            .env_remove("PRUX_MODEL")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn `prux rpc`");

        let stderr = Arc::new(Mutex::new(String::new()));
        let child_stderr = child.stderr.take().expect("piped stderr");
        let mut child_stderr = BufReader::new(child_stderr);
        let sink = Arc::clone(&stderr);
        thread::spawn(move || {
            let mut buffer = String::new();
            while matches!(child_stderr.read_line(&mut buffer), Ok(n) if n > 0) {
                sink.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_str(&buffer);
                buffer.clear();
            }
        });

        Rpc {
            stdin: child.stdin.take(),
            stdout: BufReader::new(child.stdout.take().expect("piped stdout")),
            child,
            stderr,
            _agent: agent,
            project,
            stash: Vec::new(),
        }
    }

    /// 临时项目目录（子进程 cwd）。
    fn project_dir(&self) -> &Path {
        self.project.path()
    }

    /// 子进程的 `PRUX_AGENT_DIR` 路径（测试要往里写 models.json 时用）。
    fn agent_path(&self) -> PathBuf {
        self._agent.path().to_path_buf()
    }

    /// 子进程 stderr 快照（断言失败时贴出来，定位启动期报错）。
    fn stderr(&self) -> String {
        self.stderr
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 写一行命令进 stdin（立刻 flush，否则子进程收不到）。
    fn send(&mut self, command: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        writeln!(stdin, "{command}").expect("write command");
        stdin.flush().expect("flush command");
    }

    /// 直接写一行原始文本（测坏 JSON 用）。
    fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        writeln!(stdin, "{line}").expect("write raw line");
        stdin.flush().expect("flush raw line");
    }

    /// 读下一条记录；stdout 上出现非 JSON 行说明协议被污染，直接失败。
    fn read_record(&mut self) -> Value {
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).unwrap_or(0);
        assert!(
            read > 0,
            "子进程 stdout 提前关闭，stderr:\n{}",
            self.stderr()
        );
        serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("stdout 出现非 JSON 行（{e}）: {line}"))
    }

    /// 一直读到指定 `id` 的响应。
    ///
    /// 并发命令的响应可能乱序回来：属于别人的响应先存进 `stash`，别丢；
    /// 事件记录（没有 `type: "response"`）直接跳过——本套测试不打网络，正常不会出现。
    fn response(&mut self, id: &str) -> Value {
        if let Some(index) = self
            .stash
            .iter()
            .position(|record| record.get("id") == Some(&json!(id)))
        {
            return self.stash.remove(index);
        }
        loop {
            let record = self.read_record();
            if record.get("type") != Some(&json!("response")) {
                continue;
            }
            if record.get("id") == Some(&json!(id)) {
                return record;
            }
            self.stash.push(record);
        }
    }

    /// 发一条带 `id` 的命令并等它的响应。
    fn call(&mut self, id: &str, command: Value) -> Value {
        let mut command = command;
        command["id"] = json!(id);
        self.send(&command);
        self.response(id)
    }

    /// 关掉 stdin（协议里的关停请求）；进程可能还没退。
    fn close_stdin(&mut self) {
        drop(self.stdin.take());
    }

    /// 等进程退出并返回退出码（调用前先关 stdin）。
    fn wait(mut self) -> ExitStatus {
        self.close_stdin();
        self.child.wait().expect("wait for exit")
    }
}

impl Drop for Rpc {
    /// 测试失败也要收尸：先关 stdin 给一次优雅退出的机会，再强制杀掉。
    fn drop(&mut self) {
        drop(self.stdin.take());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        _ = self.child.kill();
        _ = self.child.wait();
    }
}

/// 往子进程的 agent 目录写一份自定义 provider：不联网，只为了让"已配置模型"这件事确定下来。
///
/// 不写它的话，`get_available_models` 的有无取决于开发者机器上的 API key 环境变量，
/// 断言会时绿时红；写了它，`rpc-test/rpc-test-model` 一定在目录里。
fn write_test_provider(agent_dir: &Path) {
    let models = json!({
        "providers": {
            "rpc-test": {
                "baseUrl": "http://127.0.0.1:1/v1",
                "api": "openai-completions",
                "apiKey": "test-key",
                "models": [{
                    "id": "rpc-test-model",
                    "name": "RPC Test Model",
                    "reasoning": true,
                    "contextWindow": 128000,
                    "maxTokens": 4096
                }]
            }
        }
    });
    fs::write(
        agent_dir.join("models.json"),
        serde_json::to_string_pretty(&models).expect("serialize models.json"),
    )
    .expect("write models.json");
}

/// 响应的 `success` 必须为 true，失败时把 error 贴出来。
fn assert_ok(response: &Value) -> &Value {
    assert_eq!(
        response["success"],
        json!(true),
        "命令失败: {}",
        response["error"]
    );
    &response["data"]
}

/// 响应的 `success` 必须为 false，返回错误文本。
fn assert_err(response: &Value) -> String {
    assert_eq!(
        response["success"],
        json!(false),
        "命令本应失败: {response}"
    );
    response["error"].as_str().unwrap_or_default().to_string()
}

/// 协议层：坏 JSON、未知命令、`id` 原样回传，stdout 只有 JSON。
#[test]
fn protocol_records_echo_ids_and_report_bad_input() {
    run_with_timeout(TEST_TIMEOUT, || {
        let mut rpc = Rpc::spawn();

        // 坏 JSON：没有 id 的 parse 响应
        rpc.send_raw("{not json");
        let parse = rpc.read_record();
        assert_eq!(parse["type"], json!("response"));
        assert_eq!(parse["command"], json!("parse"));
        assert_eq!(parse["success"], json!(false));
        assert!(
            parse.get("id").is_none(),
            "解析错误不知道怎么对齐，不该带 id"
        );

        // 未知命令：id 原样带回
        let unknown = rpc.call("req-unknown", json!({"type": "no_such_command"}));
        assert_eq!(unknown["id"], json!("req-unknown"));
        assert!(assert_err(&unknown).contains("Unknown command: no_such_command"));

        // 缺字段的命令同样回同一 id 的失败响应
        let missing = rpc.call("req-steer", json!({"type": "steer"}));
        assert_eq!(missing["id"], json!("req-steer"));
        assert!(assert_err(&missing).contains("message is required"));

        // 正常响应：id + command + success + data
        let state = rpc.call("req-state", json!({"type": "get_state"}));
        assert_eq!(state["id"], json!("req-state"));
        assert_eq!(state["command"], json!("get_state"));
        let data = assert_ok(&state);
        for key in [
            "model",
            "thinkingLevel",
            "isStreaming",
            "isCompacting",
            "steeringMode",
            "followUpMode",
            "sessionId",
            "messageCount",
            "pendingMessageCount",
        ] {
            assert!(data.get(key).is_some(), "get_state 缺 {key}: {data}");
        }
        assert_eq!(data["isStreaming"], json!(false));
        assert_eq!(data["messageCount"], json!(0));

        // 函数名与响应里的 command 一致：`set_session_name` 的失败也要挂在同一个名字上
        let empty_name = rpc.call(
            "req-name",
            json!({"type": "set_session_name", "name": "  "}),
        );
        assert_eq!(empty_name["command"], json!("set_session_name"));
        assert!(assert_err(&empty_name).contains("cannot be empty"));
    });
}

/// 模型与思考档位：形状正确、**不含凭据**、非法值被拒。
#[test]
fn model_and_thinking_commands_expose_catalog_shapes() {
    run_with_timeout(TEST_TIMEOUT, || {
        let mut rpc = Rpc::spawn();
        write_test_provider(&rpc.agent_path());

        let models = rpc.call("m1", json!({"type": "get_available_models"}));
        let models = assert_ok(&models)["models"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let ours = models.iter().find(|model| {
            model["provider"] == json!("rpc-test") && model["id"] == json!("rpc-test-model")
        });
        assert!(
            ours.is_some(),
            "自定义 provider 的模型应出现在可用目录: {models:?}"
        );
        for model in &models {
            assert!(model.get("provider").is_some() && model.get("id").is_some());
            assert!(model.get("apiKey").is_none(), "协议里不许出现凭据: {model}");
            assert!(
                model.get("api_key").is_none(),
                "协议里不许出现凭据: {model}"
            );
        }

        // 切到自定义模型：回该模型对象（同样不带凭据），状态跟着变
        let switched = rpc.call(
            "m2",
            json!({"type": "set_model", "provider": "rpc-test", "modelId": "rpc-test-model"}),
        );
        let switched = assert_ok(&switched).clone();
        assert_eq!(switched["provider"], json!("rpc-test"));
        assert_eq!(switched["id"], json!("rpc-test-model"));
        assert_eq!(switched["contextWindow"], json!(128000));
        assert!(switched.get("apiKey").is_none() && switched.get("api_key").is_none());

        let state = rpc.call("m2b", json!({"type": "get_state"}));
        let state = assert_ok(&state);
        assert_eq!(state["model"]["id"], json!("rpc-test-model"));
        assert!(state["model"].get("apiKey").is_none());

        // 切到自定义模型后仍能循环（至少不报错）
        let cycled = rpc.call("m2c", json!({"type": "cycle_model"}));
        assert_ok(&cycled);

        // 不存在的模型：与 pi 同一句文案
        let missing = rpc.call(
            "m3",
            json!({"type": "set_model", "provider": "nope", "modelId": "nope"}),
        );
        assert_eq!(
            assert_err(&missing),
            "Model not found: nope/nope".to_string()
        );

        // 思考档位：列表 → 设置 → 状态跟着变
        let levels = rpc.call("t1", json!({"type": "get_available_thinking_levels"}));
        let levels = assert_ok(&levels)["levels"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(!levels.is_empty(), "思考档位列表不该为空（至少含 off）");
        let target = levels
            .iter()
            .filter_map(Value::as_str)
            .find(|level| *level != "off")
            .or_else(|| levels[0].as_str().map(|_| "off"))
            .expect("挑一个档位");
        assert_ok(&rpc.call("t2", json!({"type": "set_thinking_level", "level": target})));
        let state = rpc.call("t3", json!({"type": "get_state"}));
        assert_eq!(assert_ok(&state)["thinkingLevel"], json!(target));

        // 缺 level 直接拒绝
        assert!(
            assert_err(&rpc.call("t4", json!({"type": "set_thinking_level"}))).contains("level")
        );

        // 队列模式：只认这两个取值
        assert!(
            assert_err(&rpc.call(
                "q1",
                json!({"type": "set_steering_mode", "mode": "sometimes"})
            ))
            .contains("one-at-a-time")
        );
        assert_ok(&rpc.call("q2", json!({"type": "set_steering_mode", "mode": "all"})));
        let state = rpc.call("q3", json!({"type": "get_state"}));
        assert_eq!(assert_ok(&state)["steeringMode"], json!("all"));
    });
}

/// 队列：空闲期 steer / follow_up 只排队，`pendingMessageCount` 跟着涨，`clear_queue` 原样退回。
#[test]
fn queued_input_accumulates_until_cleared() {
    run_with_timeout(TEST_TIMEOUT, || {
        let mut rpc = Rpc::spawn();

        let steer = rpc.call("q1", json!({"type": "steer", "message": "先说这个"}));
        assert_eq!(assert_ok(&steer)["disposition"], json!("queued"));

        let follow = rpc.call("q2", json!({"type": "follow_up", "message": "再补一句"}));
        assert_eq!(assert_ok(&follow)["disposition"], json!("queued"));

        let state = rpc.call("q3", json!({"type": "get_state"}));
        assert_eq!(
            assert_ok(&state)["pendingMessageCount"],
            json!(2),
            "两条队列各有一条未投递消息"
        );

        let cleared = rpc.call("q4", json!({"type": "clear_queue"}));
        let cleared = assert_ok(&cleared).clone();
        assert_eq!(cleared["steering"], json!(["先说这个"]));
        assert_eq!(cleared["followUp"], json!(["再补一句"]));

        let state = rpc.call("q5", json!({"type": "get_state"}));
        assert_eq!(assert_ok(&state)["pendingMessageCount"], json!(0));

        // 命令表：内置扩展（默认启用）至少提供一条扩展命令
        let commands = rpc.call("q6", json!({"type": "get_commands"}));
        let commands = assert_ok(&commands)["commands"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let extension = commands
            .iter()
            .find(|command| command.get("source").and_then(Value::as_str) == Some("extension"))
            .cloned()
            .expect("默认启用的扩展应提供至少一条斜杠命令");
        let name = extension["name"].as_str().expect("命令名").to_string();

        // 扩展斜杠命令不能排队：要执行请走 prompt
        let rejected = rpc.call(
            "q7",
            json!({"type": "steer", "message": format!("/{name}")}),
        );
        let error = assert_err(&rejected);
        assert!(
            error.contains("cannot be queued") && error.contains(&format!("\"/{name}\"")),
            "扩展命令排队要报错并点名命令，实际: {error}"
        );

        // prompt 里的斜杠命令就地执行，回 handled（不排队、也不发进模型）
        let handled = rpc.call(
            "q8",
            json!({"type": "prompt", "message": format!("/{name}")}),
        );
        assert_eq!(
            assert_ok(&handled)["disposition"],
            json!("handled"),
            "斜杠命令应被认领而不是起回合"
        );
    });
}

/// `bash`：真跑、回结构化结果、空命令拒绝、多条并发各自回包。
///
/// 只在 Unix 上跑：命令（`echo` / `exit 3`）与行尾都按 POSIX 假定。
#[cfg(unix)]
#[test]
fn bash_runs_commands_out_of_band() {
    run_with_timeout(TEST_TIMEOUT, || {
        let mut rpc = Rpc::spawn();

        let single = rpc.call("b1", json!({"type": "bash", "command": "echo rpc-hello"}));
        let result = assert_ok(&single);
        assert_eq!(result["output"], json!("rpc-hello\n"));
        assert_eq!(result["exitCode"], json!(0));
        assert_eq!(result["cancelled"], json!(false));

        // 失败命令照样回结果（exitCode 非 0），不当作命令错误
        let failed = rpc.call("b2", json!({"type": "bash", "command": "exit 3"}));
        assert_eq!(assert_ok(&failed)["exitCode"], json!(3));

        // 空命令直接拒绝
        assert!(
            assert_err(&rpc.call("b3", json!({"type": "bash", "command": " "})))
                .contains("command")
        );

        // 两条并发：两条响应都要回来（结果不串号）
        rpc.send(&json!({"id": "c1", "type": "bash", "command": "echo one"}));
        rpc.send(&json!({"id": "c2", "type": "bash", "command": "echo two"}));
        let first = rpc.response("c1");
        let second = rpc.response("c2");
        assert_eq!(assert_ok(&first)["output"], json!("one\n"));
        assert_eq!(assert_ok(&second)["output"], json!("two\n"));
    });
}

/// 会话：新会话换 id、改名、统计、导出落盘、条目与树都能取。
#[test]
fn session_commands_manage_the_active_session() {
    run_with_timeout(TEST_TIMEOUT, || {
        let mut rpc = Rpc::spawn();

        let before = rpc.call("s1", json!({"type": "get_state"}));
        let before = assert_ok(&before)["sessionId"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        let created = rpc.call("s2", json!({"type": "new_session"}));
        assert_eq!(assert_ok(&created)["cancelled"], json!(false));

        let after = rpc.call("s3", json!({"type": "get_state"}));
        let after_data = assert_ok(&after);
        assert_ne!(
            after_data["sessionId"].as_str().unwrap_or_default(),
            before,
            "new_session 必须换一个会话 id"
        );
        assert_eq!(after_data["messageCount"], json!(0));

        // 改名：成功则状态里能看到
        assert_ok(&rpc.call(
            "s4",
            json!({"type": "set_session_name", "name": "rpc 端到端"}),
        ));
        let state = rpc.call("s5", json!({"type": "get_state"}));
        assert_eq!(assert_ok(&state)["sessionName"], json!("rpc 端到端"));

        // 空会话统计：全零，但字段齐全
        let stats = rpc.call("s6", json!({"type": "get_session_stats"}));
        let stats = assert_ok(&stats);
        for key in [
            "sessionFile",
            "sessionId",
            "userMessages",
            "assistantMessages",
            "toolCalls",
            "toolResults",
            "totalMessages",
            "tokens",
            "cost",
        ] {
            assert!(
                stats.get(key).is_some(),
                "get_session_stats 缺 {key}: {stats}"
            );
        }
        assert_eq!(stats["userMessages"], json!(0));
        assert_eq!(stats["tokens"]["total"], json!(0));

        // 条目与树：空会话也有条目（工具快照），两份的 leafId 一致
        let entries = rpc.call("s7", json!({"type": "get_entries"}));
        let entries = assert_ok(&entries).clone();
        assert!(entries["entries"].is_array());
        let tree = rpc.call("s8", json!({"type": "get_tree"}));
        let tree = assert_ok(&tree);
        assert!(tree["tree"].is_array());
        assert_eq!(tree["leafId"], entries["leafId"]);

        // since 指向不存在的条目：明确报错
        let missing = rpc.call(
            "s9",
            json!({"type": "get_entries", "since": "no-such-entry"}),
        );
        assert!(assert_err(&missing).contains("Entry not found: no-such-entry"));

        // 导出：路径必须真的存在，且落在会话工作目录下
        let exported = rpc.call("s10", json!({"type": "export_html"}));
        let path = assert_ok(&exported)["path"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(Path::new(&path).is_file(), "导出文件不存在: {path}");
        assert!(
            Path::new(&path).ends_with(PathBuf::from(path.clone()).file_name().unwrap_or_default())
        );
        assert!(
            path.contains(&rpc.project_dir().to_string_lossy().to_string()),
            "默认导出路径应落在会话工作目录: {path}"
        );
    });
}

/// 切会话与分叉：装载历史、`get_fork_messages` 给出可分叉点、`fork` 换出新会话。
#[test]
fn switch_session_loads_history_and_fork_creates_a_branch() {
    run_with_timeout(TEST_TIMEOUT, || {
        // 先在测试进程里造一份带用户消息的会话文件（不碰网络）
        let _agent = AgentDirGuard::temp();
        let cwd = std::env::temp_dir().to_string_lossy().to_string();
        let mut session =
            Session::create(&cwd, Some(default_session_dir(&cwd, &agent_dir())), true)
                .expect("create session");
        session.append_message(&AgentMessage::user_text("历史里的一句用户消息"));
        let source_path = session
            .get_session_file()
            .expect("会话已落盘")
            .to_string_lossy()
            .to_string();
        drop(session);
        assert!(Path::new(&source_path).is_file(), "会话文件应已写出");

        let mut rpc = Rpc::spawn();
        let switched = rpc.call(
            "w1",
            json!({"type": "switch_session", "sessionPath": source_path}),
        );
        assert_eq!(assert_ok(&switched)["cancelled"], json!(false));

        // 历史进来了
        let messages = rpc.call("w2", json!({"type": "get_messages"}));
        let messages = assert_ok(&messages)["messages"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            messages.iter().any(|message| message
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| blocks.iter().any(|block| block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains("历史里的一句用户消息"))))),
            "切换后应装载历史: {messages:?}"
        );

        // 可分叉点就是那条用户消息
        let forks = rpc.call("w3", json!({"type": "get_fork_messages"}));
        let forks = assert_ok(&forks)["messages"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let entry_id = forks
            .iter()
            .find(|message| {
                message["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("历史里的一句用户消息"))
            })
            .and_then(|message| message["entryId"].as_str())
            .expect("历史的用户消息应可作分叉点")
            .to_string();

        let before = rpc.call("w4", json!({"type": "get_state"}));
        let before = assert_ok(&before)["sessionId"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        // 分叉：回 fork 条目的正文，并切到新会话
        let forked = rpc.call("w5", json!({"type": "fork", "entryId": entry_id}));
        let forked = assert_ok(&forked);
        assert_eq!(forked["cancelled"], json!(false));
        assert!(
            forked["text"]
                .as_str()
                .is_some_and(|text| text.contains("历史里的一句用户消息")),
            "fork 应回该条目的正文: {forked}"
        );

        let after = rpc.call("w6", json!({"type": "get_state"}));
        let after_data = assert_ok(&after);
        assert_ne!(
            after_data["sessionId"].as_str().unwrap_or_default(),
            before,
            "fork 必须换一个会话 id"
        );
        assert!(
            after_data["sessionFile"].as_str().is_some(),
            "分叉出来的会话应当已经落盘: {after_data}"
        );

        // 缺 entryId 直接拒绝
        assert!(assert_err(&rpc.call("w7", json!({"type": "fork"}))).contains("entryId"));
    });
}

/// 关停：关 stdin 后进程正常退出，且在途 bash 的响应要先写完。
///
/// 「先收尾再退」是协议承诺：客户端关掉 stdin 之后不该丢掉已经发出去的请求结果。
#[cfg(unix)]
#[test]
fn closing_stdin_drains_in_flight_work_then_exits() {
    run_with_timeout(TEST_TIMEOUT, || {
        let mut rpc = Rpc::spawn();
        assert_ok(&rpc.call("e1", json!({"type": "get_state"})));

        // 起一条慢命令后立刻关 stdin：结果仍必须写出来
        rpc.send(&json!({"id": "e2", "type": "bash", "command": "sleep 1; echo drained"}));
        rpc.close_stdin();
        let bash = rpc.response("e2");
        assert_eq!(assert_ok(&bash)["output"], json!("drained\n"));

        let status = rpc.wait();
        assert!(status.success(), "关 stdin 应正常退出，实际: {status:?}");
    });
}

/// 子命令挂载：扩展启用时 `prux --help` 里能看到 `rpc`。
#[test]
fn subcommand_shows_up_in_help_when_enabled() {
    run_with_timeout(TEST_TIMEOUT, || {
        let agent = tempfile::tempdir().expect("temp agent dir");
        fs::write(
            agent.path().join("settings.json"),
            r#"{"enabledExtensions":["rpc"]}"#,
        )
        .expect("write settings");

        let output = Command::new(env!("CARGO_BIN_EXE_prux"))
            .arg("--help")
            .env("PRUX_AGENT_DIR", agent.path())
            .output()
            .expect("run `prux --help`");
        assert!(
            output.status.success(),
            "--help 应成功: {:?}",
            output.status
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("rpc") && stdout.contains("JSON-lines RPC protocol"),
            "Commands 段应列出 rpc 子命令:\n{stdout}"
        );
    });
}
