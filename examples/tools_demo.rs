//! 内置工具直接调用示例（core::tools）
//! 运行：cargo run --example tools_demo
//! 在临时目录中演示 read / write / edit / bash / ls / grep / find 的独立调用。

use prux::core::tools::{
    ToolResult, execute_bash, execute_edit, execute_find, execute_grep, execute_ls, execute_read,
    execute_write, tool_defs,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let cwd = dir.path().to_str().unwrap();

    // 1. 工具注册表
    println!("== tool registry ==");
    for def in tool_defs(&[
        "read".into(),
        "bash".into(),
        "edit".into(),
        "write".into(),
        "grep".into(),
        "find".into(),
        "ls".into(),
    ]) {
        println!("  - {}: {}", def.name, def.snippet);
    }

    // 2. write：创建文件（自动建目录）
    println!("\n== write ==");
    let r = execute_write("src/main.rs", "fn main() {}\n", cwd).await?;
    println!("  {}", text(&r));

    // 3. read：读取文件
    println!("\n== read ==");
    let r = execute_read("src/main.rs", None, None, cwd).await?;
    println!("  {}", text(&r).replace('\n', " "));

    // 4. edit：精确替换
    println!("\n== edit ==");
    let edits = serde_json::json!([{ "oldText": "{}", "newText": "{\n    println!(\"hi\");\n}" }]);
    let (r, detail, _) = execute_edit("src/main.rs", &edits, cwd).await?;
    println!("  {}", text(&r));
    println!("  (unified diff, {} chars)", detail.len());

    // 5. bash：执行命令
    println!("\n== bash ==");
    let r = execute_bash("echo hello && ls src", None, cwd).await?;
    println!("  {}", text(&r).replace('\n', " | "));

    // 6. grep：搜索
    println!("\n== grep ==");
    let r = execute_grep("println", Some("."), None, false, false, None, None, cwd).await?;
    println!("  {}", text(&r).replace('\n', " | "));

    // 7. find：按 glob 找文件
    println!("\n== find ==");
    let r = execute_find("*.rs", None, None, cwd).await?;
    println!("  {}", text(&r).replace('\n', " | "));

    // 8. ls：列出目录
    println!("\n== ls ==");
    let r = execute_ls(None, None, cwd).await?;
    println!("  {}", text(&r).replace('\n', " | "));

    println!("\ndone. temp dir: {}", dir.path().display());
    Ok(())
}

fn text(r: &ToolResult) -> String {
    let ToolResult { text: t, .. } = r;
    t.clone()
}
