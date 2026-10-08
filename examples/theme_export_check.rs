// 临时验证：生成 dark 导出的关键 CSS 行
fn main() {
    let (vars, page_bg, card_bg, info_bg) = prux::core::export_html::load_theme_vars("dark");
    println!("page_bg={page_bg} card_bg={card_bg} info_bg={info_bg}");
    let keys = [
        "--text:",
        "--dim:",
        "--muted:",
        "--selectedBg:",
        "--userMessageBg:",
        "--userMessageText:",
        "--toolOutput:",
        "--mdHeading:",
        "--accent:",
    ];
    for k in keys {
        if let Some(l) = vars.lines().find(|l| l.trim_start().starts_with(k)) {
            println!("{}", l.trim());
        }
    }
}
