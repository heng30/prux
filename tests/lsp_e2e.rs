//! `lsp` 扩展的端到端验证：真的拉起 `rust-analyzer`、在临时 crate 上跑一遍
//! 诊断 / 符号 / 定义查询。
//!
//! 默认 `#[ignore]`：需要本机已安装 `rust-analyzer`，且首次索引要几秒到几十秒，
//! 不适合放进常规测试。手动跑：
//!
//! ```text
//! cargo test --test lsp_e2e -- --ignored --nocapture
//! ```

use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;

use prux::core::extensions::{Extension, ToolExecCtx};
use prux::extensions::lsp::Lsp;
use prux::test_support::AgentDirGuard;
use serde_json::json;

/// 源文件：一个结构体、一个实现、一个函数、一处调用，够覆盖三种查询。
const SOURCE: &str = r#"pub struct Foo;

impl Foo {
    pub fn new() -> Self {
        Foo
    }
}

pub fn helper() -> u32 {
    let _f = Foo::new();
    42
}

fn main() {
    let _ = helper();
}
"#;

/// 只填 cwd 的测试上下文；`lsp` 不碰子代理与嵌套工具。
fn ctx(cwd: &str) -> ToolExecCtx {
    ToolExecCtx {
        cwd: cwd.to_string(),
        make_sub_agent: Arc::new(|_spec| Err("subagents unavailable in tests".to_string())),
        parent_abort: Arc::new(AtomicBool::new(false)),
        agent_id: None,
        depth: 0,
        parent_model: None,
        execute_tool: prux::core::extensions::unavailable_tool_exec(),
        parent_tool_call_id: None,
        nested_calls: Default::default(),
        script_tools: Default::default(),
        session_branch_entries: Default::default(),
        script_call: false,
    }
}

/// 跑一个 `lsp` 工具调用并返回文本结果。
async fn call(cwd: &str, args: serde_json::Value) -> String {
    let result = Lsp
        .execute_tool_async("lsp".to_string(), args, ctx(cwd))
        .await
        .expect("lsp tool failed");
    result.text
}

#[tokio::test]
#[ignore = "requires rust-analyzer and a few seconds of indexing"]
async fn queries_a_real_rust_workspace() {
    let _guard = AgentDirGuard::temp();
    let root = std::env::temp_dir().join(format!("prux-lsp-e2e-{}", std::process::id()));
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"lsp-e2e\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(src.join("lib.rs"), SOURCE).unwrap();

    let cwd = root.to_string_lossy().to_string();
    let file = src.join("lib.rs").to_string_lossy().to_string();

    let work = async {
        let symbols = call(&cwd, json!({ "operation": "symbols", "path": file })).await;
        println!("--- symbols ---\n{symbols}");
        assert!(symbols.contains("struct Foo"), "{symbols}");
        assert!(symbols.contains("function helper"), "{symbols}");

        let diagnostics = call(&cwd, json!({ "operation": "diagnostics", "path": file })).await;
        println!("--- diagnostics ---\n{diagnostics}");
        assert!(
            diagnostics.contains("no diagnostics") || diagnostics.contains("diagnostic"),
            "{diagnostics}"
        );

        let definition = call(
            &cwd,
            json!({
                "operation": "definition",
                "path": file,
                "line": 15,
                "character": 13
            }),
        )
        .await;
        println!("--- definition ---\n{definition}");
        assert!(definition.contains(":9:8"), "{definition}");
    };

    tokio::time::timeout(Duration::from_secs(240), work)
        .await
        .expect("lsp e2e timed out");

    let _ = std::fs::remove_dir_all(&root);
}
