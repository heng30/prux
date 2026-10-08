//! utils::tokens 集成测试：tiktoken BPE 计数（中英混合统计口径）。
//!
//! 期望值取自官方 tiktoken（python）对同一词表的统计，见各断言的注释。
//! 用 `cargo test --test tokens -- --nocapture` 可以看到启发式与真实计数的对照表。

use prux::utils::tokens::{TokenMeter, count, encoding_for, fallback_encoding};

/// 混合语料样本：(名称, 文本, cl100k 计数, o200k 计数)
const SAMPLES: &[(&str, &str, u64, u64)] = &[
    (
        "en_prose",
        "The quick brown fox jumps over the lazy dog.",
        10,
        10,
    ),
    (
        "zh_prose",
        "用户的问题：底栏的 token 速率统计在中英混合文本下偏差很大。",
        32,
        23,
    ),
    (
        "mixed_reasoning",
        "reasoning 输出期间没有刷新，只有 tool call 时才更新；用 BPE 词表统计更准确。",
        33,
        24,
    ),
    (
        "code",
        "fn main() { let v: Vec<u32> = (0..10).map(|i| i * 2).collect(); }",
        29,
        29,
    ),
    (
        "json_args",
        r#"{"path":"src/extensions/footer/rich.rs","offset":120,"limit":80}"#,
        18,
        18,
    ),
];

#[test]
fn real_counts_match_tiktoken_ground_truth() {
    let cl = fallback_encoding();
    let o2 = encoding_for(Some("gpt-4o"));
    for (name, text, cl_expected, o2_expected) in SAMPLES {
        assert_eq!(count(cl, text), *cl_expected, "cl100k {name}");
        assert_eq!(count(o2, text), *o2_expected, "o200k {name}");
    }
    // 中文：o200k 词表压缩率更高；代码/JSON 两表基本一致
    assert!(count(cl, SAMPLES[1].1) > count(o2, SAMPLES[1].1));
    assert_eq!(count(cl, SAMPLES[3].1), count(o2, SAMPLES[3].1));
}

#[test]
fn heuristic_error_table_is_documented() {
    // 统计对照：旧的 `字节数/4` 启发式在中英混合下的偏差
    // （这些样本上为 −45% ~ +10%；语料不同可达 −23% ~ +59%）
    let mut worst = 0.0f64;
    for (name, text, cl_expected, _) in SAMPLES {
        let bytes = text.len() as f64;
        let heuristic = (bytes / 4.0).round();
        let real = *cl_expected as f64;
        let err = (heuristic - real) / real;
        worst = worst.max(err.abs());
        println!(
            "{name:16} bytes={bytes:5.0} bytes/4={heuristic:5.0} cl100k={real:5.0} 偏差={:+.0}%",
            err * 100.0
        );
    }
    println!("最大偏差 = {:.0}%", worst * 100.0);
    // 记录现状：启发式在混合语料下最大偏差超过 20%（这正是改用 BPE 的原因）
    assert!(worst > 0.2, "启发式偏差过小，统计样本失真: {worst}");
}

#[test]
fn meter_tracks_chunked_stream_within_3_percent() {
    // provider 会把词切开、token 可能跨增量边界：按字符切碎喂入，
    // 增量计数结果应与整段计数接近（误差 < 3%）
    for (name, text, cl_expected, _) in SAMPLES {
        let mut meter = TokenMeter::new(fallback_encoding());
        let chars: Vec<char> = text.chars().collect();
        for chunk in chars.chunks(3) {
            meter.feed(&chunk.iter().collect::<String>());
        }
        let chunked = meter.finish();
        let real = *cl_expected;
        let err = (chunked as f64 - real as f64).abs() / real as f64;
        assert!(err < 0.03, "{name}: 增量计数 {chunked} vs 整段 {real}");
    }
}

#[test]
fn encoding_selection_by_model() {
    // OpenAI 系走官方映射
    assert_eq!(count(encoding_for(Some("gpt-4o-mini")), "你好，世界"), 3);
    assert_eq!(count(encoding_for(Some("o3-mini")), "你好，世界"), 3);
    assert_eq!(count(encoding_for(Some("gpt-4-turbo")), "你好，世界"), 6);
    // 非 OpenAI 模型 / 未知模型回退 cl100k
    assert_eq!(
        count(encoding_for(Some("claude-sonnet-4-5")), "你好，世界"),
        6
    );
    assert_eq!(count(encoding_for(None), "你好，世界"), 6);
}
