//! 分类器示例（core::provider::classify）
//! 运行：cargo run --example classify -- --text "The change works, thanks!" [--provider openrouter] [--model <id>]
//!
//! 说明：prux 的分类器与 pi 的 `Models.classify()` 同形——一次调用可问多道题
//! （choice / score / bool，其中 bool 在线上是 TypeSafe 的 `noul`），
//! 失败不抛错（返回 `stopReason: "error"`），`usage` 按目录单价计费。
//! 需要 provider 凭据（openrouter 走 OPENROUTER_API_KEY / `prux login openrouter`）。

use prux::core::{
    model_resolver::{find_model_of_type, list_models_of_type, model_config_from_entry},
    provider::{
        BoolCriteria, ClassifierAnswer, ClassifierContext, ClassifierQuestion, ModelType, classify,
    },
};
use serde_json::json;
use std::collections::BTreeMap;

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
    let model_id = arg("--model").unwrap_or_else(|| "~typesafe/jev-latest".to_string());
    let text = arg("--text").unwrap_or_else(|| "The change works perfectly, thanks.".to_string());

    // 1. 目录里该 provider 的分类器模型（type: classifier 条目；chat 模型不在其中）
    println!("== classifier models ({provider}) ==");
    let models = list_models_of_type(&provider, ModelType::Classifier);
    if models.is_empty() {
        println!("  (none)");
    }
    for (id, name) in &models {
        println!("  - {id}  ({name})");
    }

    // 2. 按类型取条目（分类器与 chat 条目互不覆盖）并组装请求配置
    let entry = find_model_of_type(&provider, &model_id, ModelType::Classifier)?;
    let model = model_config_from_entry(&entry, None, None);
    println!("\n== classify ==");
    println!(
        "  model: {}/{} ({})",
        model.provider, model.model_id, model.api
    );

    // 3. 三道题一起问：选择 / 评分 / 判断
    let context = ClassifierContext {
        state: json!({ "text": text }),
        images: None,
        questions: BTreeMap::from([
            (
                "sentiment".to_string(),
                ClassifierQuestion::Choice {
                    instructions: "Classify the sentiment of the text.".into(),
                    criteria: BTreeMap::from([
                        ("positive".to_string(), "The user is satisfied".to_string()),
                        ("negative".to_string(), "The user is unhappy".to_string()),
                    ]),
                },
            ),
            (
                "confidence".to_string(),
                ClassifierQuestion::Score {
                    instructions: "How confident is the user?".into(),
                    criteria: vec!["low".to_string(), "medium".to_string(), "high".to_string()],
                },
            ),
            (
                "approved".to_string(),
                ClassifierQuestion::Bool {
                    instructions: "Does the user approve of the result?".into(),
                    criteria: BoolCriteria {
                        yes: "Approval".to_string(),
                        no: "No approval".to_string(),
                    },
                },
            ),
        ]),
    };

    // 4. 一次性调用：失败不抛错，看 stop_reason
    let result = classify(&model, &context).await;
    if result.stop_reason != "stop" {
        anyhow::bail!(
            "classification failed: {}",
            result
                .error_message
                .unwrap_or_else(|| result.stop_reason.clone())
        );
    }

    // 5. 答案按问题 id 对号入座
    for id in context.questions.keys() {
        let answer = match result.answers.get(id) {
            Some(ClassifierAnswer::Choice {
                choice,
                probabilities,
                confidence,
            }) => {
                let ranked: Vec<String> = probabilities
                    .iter()
                    .map(|(option, p)| format!("{option}={p:.2}"))
                    .collect();
                format!(
                    "{choice} (confidence {confidence:.2}; {})",
                    ranked.join(" ")
                )
            }
            Some(ClassifierAnswer::Score { score, confidence }) => {
                format!("{score} (confidence {confidence:.2})")
            }
            Some(ClassifierAnswer::Bool { probability }) => format!("p(true) = {probability:.2}"),
            None => "(no answer)".to_string(),
        };
        println!("  {id}: {answer}");
    }

    if let Some(usage) = &result.usage {
        println!(
            "  usage: in={} out={} total={} cost=${:.6}",
            usage.input, usage.output, usage.total_tokens, usage.cost.total
        );
    }

    Ok(())
}
