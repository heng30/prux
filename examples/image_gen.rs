//! 图片生成示例（core::provider::generate_images）
//! 运行：cargo run --example image_gen -- --prompt "a red circle on white" [--model <id>] [--out <dir>]
//!
//! 说明：prux 的图片生成与 pi 的 `Models.generateImages()` 同形——一次性非流式调用，
//! 失败不抛错（返回 `stopReason: "error"`），只把 base64 结果交给调用方；
//! **落盘/展示是调用方的事**，本示例把生成的图片写到 `--out`（默认 `./.prux-images`）演示这一层。
//! 需要 provider 凭据（openrouter 走 OPENROUTER_API_KEY / `prux login openrouter`）。

use prux::core::{
    model_resolver::{find_model_of_type, list_models_of_type, model_config_from_entry},
    provider::{ImageContent, ModelType, generate_images},
};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    let provider = arg("--provider").unwrap_or_else(|| "openrouter".to_string());
    let model_id = arg("--model").unwrap_or_else(|| "google/gemini-3-pro-image".to_string());
    let prompt =
        arg("--prompt").unwrap_or_else(|| "a red circle on a plain white background".to_string());
    let out_dir = PathBuf::from(arg("--out").unwrap_or_else(|| ".prux-images".to_string()));

    // 1. 目录里该 provider 的图片模型（type: image 条目；chat 模型不在其中）
    println!("== image models ({provider}) ==");
    let models = list_models_of_type(&provider, ModelType::Image);
    if models.is_empty() {
        println!("  (none)");
    }
    for (id, name) in models.iter().take(10) {
        println!("  - {id}  ({name})");
    }
    if models.len() > 10 {
        println!("  … {} more", models.len() - 10);
    }

    // 2. 按类型取条目（同一上游 ID 的 chat / image 条目互不覆盖）并组装请求配置
    let entry = find_model_of_type(&provider, &model_id, ModelType::Image)?;
    let model = model_config_from_entry(&entry, None, None);
    println!("\n== generate ==");
    println!(
        "  model: {}/{} ({})",
        model.provider, model.model_id, model.api
    );
    println!("  output modalities: {:?}", model.output);
    println!("  prompt: {prompt}");

    // 3. 一次性调用：失败不抛错，看 stop_reason
    let result = generate_images(&model, &[ImageContent::Text { text: prompt }]).await;
    if result.stop_reason != "stop" {
        anyhow::bail!(
            "image generation failed: {}",
            result
                .error_message
                .unwrap_or_else(|| result.stop_reason.clone())
        );
    }
    if let Some(usage) = &result.usage {
        println!(
            "  usage: in={} out={} cacheRead={} cacheWrite={} total={} cost=${:.6}",
            usage.input,
            usage.output,
            usage.cache_read,
            usage.cache_write,
            usage.total_tokens,
            usage.cost.total
        );
    }

    // 4. 输出：文本直接打印，图片由调用方落盘（本示例写文件）
    std::fs::create_dir_all(&out_dir)?;
    for (i, block) in result.output.iter().enumerate() {
        match block {
            ImageContent::Text { text } => println!("  text: {text}"),
            ImageContent::Image { data, mime_type } => {
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD.decode(data)?;
                let ext = mime_type.rsplit('/').next().unwrap_or("bin");
                let path = out_dir.join(format!("{}-{i}.{ext}", model.model_id.replace('/', "_")));
                std::fs::write(&path, &bytes)?;
                println!(
                    "  image: {} ({} bytes, {mime_type})",
                    path.display(),
                    bytes.len()
                );
            }
        }
    }

    Ok(())
}
