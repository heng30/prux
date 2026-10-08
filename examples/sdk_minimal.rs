//! SDK 最小示例（对应 docs/sdk.md 快速开始）
//! 运行：cargo run --example sdk_minimal -- --dry-run
use prux::core::agent_session::{Agent, ToolSelection};
use prux::core::model_resolver::find_model;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dry_run = std::env::args().any(|a| a == "--dry-run");
    let model_entry = find_model("opencode-go", "deepseek-v4-flash")?;
    let cwd = std::env::current_dir()?.to_string_lossy().to_string();

    let mut agent = Agent::new(
        model_entry,
        cwd,
        ToolSelection::tools(vec![
            "read".into(),
            "bash".into(),
            "edit".into(),
            "write".into(),
        ]),
        Vec::new(),
        Some("high".into()),
        None,
        None,
        Vec::new(),
        None,
        None,
        false,
        Vec::new(),
    )?;

    if dry_run {
        println!(
            "[dry-run] Agent built successfully, model: {}",
            agent.model.model_id
        );
        return Ok(());
    }
    let reply = agent
        .prompt("What files are in the current directory?")
        .await?;
    println!("{}", reply);
    Ok(())
}
