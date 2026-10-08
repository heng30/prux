//! 初始消息组装
//! stdin + @文件文本 + 第一条 CLI 消息合并为 initialMessage。

/// 合并 stdin、文件文本与 CLI 消息，返回初始文本与剩余消息
pub fn compose_initial_parts(
    stdin_content: Option<&str>,
    file_text: &str,
    cli_messages: &mut Vec<String>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(stdin_text) = stdin_content {
        parts.push(stdin_text.to_string());
    }
    if !file_text.is_empty() {
        parts.push(file_text.to_string());
    }
    if !cli_messages.is_empty() {
        parts.push(cli_messages.remove(0));
    }
    parts.join("")
}
