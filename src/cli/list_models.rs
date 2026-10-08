//! --list-models CLI 输出

use crate::core::model_resolver::list_models;

/// 列出 provider 的模型目录，支持搜索过滤
pub fn run_list_models(provider: &str, search: &str) {
    let models = list_models(provider);
    if models.is_empty() {
        eprintln!("provider {} has no model catalog", provider);
        return;
    }
    let search = search.to_lowercase();
    for (id, name) in models {
        if !search.is_empty()
            && !id.to_lowercase().contains(&search)
            && !name.to_lowercase().contains(&search)
        {
            continue;
        }
        println!("{}: {}", id, name);
    }
}
