//! 临时复现：prux bash 工具执行 make debug 的异常退出。
use prux::core::tools::{BashSessionEnv, execute_bash_with_env};

#[tokio::main]
async fn main() {
    let cmd = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "make debug".into());
    let out = execute_bash_with_env(
        &cmd,
        Some(60.0),
        "/data/Code/rust/prux",
        None,
        None,
        &BashSessionEnv::default(),
    )
    .await;
    println!(
        "=== exit_code={:?} cancelled={} timed_out={}",
        out.exit_code, out.cancelled, out.timed_out
    );
    println!("=== text:\n{}", out.text);
}
