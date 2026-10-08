//! token 统计：tiktoken BPE 真实计数
//!
//! 底栏 token 速率与上下文估算共用同一口径：不再用 `chars/4` 启发式
//! （中英混合实测偏差 −23% ~ +59%），直接按模型选择 BPE 词表计数。
//! 非 OpenAI 供应商没有公开词表，回退 cl100k（实测中英混合的综合偏差最小：
//! 中文 cl100k ≈ 0.9~1.2 token/字、o200k ≈ 0.6~0.7，英文 ≈ 0.2 token/字符）。

use tiktoken_rs::{CoreBPE, bpe_for_model, bpe_for_tokenizer, tokenizer::Tokenizer};

/// 未闭合尾部（最后一个空白之后）的长度上限：长 JSON / base64 等无空白内容
/// 需要强制切分，避免 pending 无限增长
const TAIL_MAX_BYTES: usize = 512;

/// 非 OpenAI 供应商的兜底词表（cl100k）
pub fn fallback_encoding() -> &'static CoreBPE {
    bpe_for_tokenizer(Tokenizer::Cl100kBase).expect("cl100k 词表内置")
}

/// 按模型选词表：OpenAI 系走官方映射（gpt-4o/gpt-5/o1/o3/gpt-oss → o200k，
/// gpt-4/gpt-3.5 → cl100k），其余模型回退 cl100k。
pub fn encoding_for(model_id: Option<&str>) -> &'static CoreBPE {
    model_id
        .and_then(|m| bpe_for_model(m).ok())
        .unwrap_or_else(fallback_encoding)
}

/// 单段文本 token 数
pub fn count(enc: &CoreBPE, text: &str) -> u64 {
    enc.count_ordinary(text) as u64
}

/// 预热词表：首次使用要解析约 2MB 词表（数十 ms），放进后台线程避免首帧卡顿
pub fn warmup(model_id: Option<&str>) {
    _ = fallback_encoding();
    _ = encoding_for(model_id);
}

/// 流式增量 token 计数器。
///
/// provider 会把一个词切成多个 delta，BPE 合并又可能跨越增量边界，
/// 所以不能逐 delta 独立计数。这里保留「最后一个空白之后」的未闭合尾部，
/// 每帧只编码已闭合的段落（O(增量)），[`TokenMeter::finish`] 时再算上尾部。
/// 与一次性整段计数相比，误差来自跨空白边界的合并，实测 < 3%。
pub struct TokenMeter {
    /// 当前使用的 BPE 词表（随模型选择，切换时计数清零）
    enc: &'static CoreBPE,
    /// 尚未编码的尾部文本：最后一个空白之后、等待后续增量闭合的部分
    pending: String,
    /// 已编码前缀累计的 token 数（不含 pending 尾部）
    counted: u64,
}

impl TokenMeter {
    /// 以指定 BPE 词表新建计数器，初始计数为 0。
    pub fn new(enc: &'static CoreBPE) -> Self {
        TokenMeter {
            enc,
            pending: String::new(),
            counted: 0,
        }
    }

    /// 切换词表（模型切换）：已累计的计数按新词表重算代价高，
    /// 直接清零重来（调用方均在流式起点切换）
    pub fn set_encoding(&mut self, enc: &'static CoreBPE) {
        if !std::ptr::eq(self.enc, enc) {
            self.enc = enc;
            self.reset();
        }
    }

    /// 当前累计 token（不含尚未闭合的尾部）
    pub fn count(&self) -> u64 {
        self.counted
    }

    /// 追加流式增量，返回当前累计 token
    pub fn feed(&mut self, delta: &str) -> u64 {
        self.pending.push_str(delta);
        let cut = self.split_point();
        if cut > 0 {
            let head: String = self.pending.drain(..cut).collect();
            self.counted += count(self.enc, &head);
        }
        self.counted
    }

    /// 消息结束：把尾部一起计入
    pub fn finish(&mut self) -> u64 {
        if !self.pending.is_empty() {
            let tail = std::mem::take(&mut self.pending);
            self.counted += count(self.enc, &tail);
        }
        self.counted
    }

    /// 清零（新消息 / 消息落地 / 中断）
    pub fn reset(&mut self) {
        self.pending.clear();
        self.counted = 0;
    }

    /// 可编码前缀长度：最后一个空白字符之前（空白留给尾部，保留跨词合并），
    /// 无空白且过长时按字符边界强制切分
    fn split_point(&self) -> usize {
        let s = self.pending.as_str();
        if let Some(i) = s.rfind(char::is_whitespace) {
            return i;
        }
        if s.len() > TAIL_MAX_BYTES {
            let mut i = s.len() - TAIL_MAX_BYTES;
            while i > 0 && !s.is_char_boundary(i) {
                i -= 1;
            }
            return i;
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_known_texts_like_tiktoken() {
        // 期望值取自官方 tiktoken（python）对同一词表的统计
        let cl = fallback_encoding();
        assert_eq!(
            count(cl, "The quick brown fox jumps over the lazy dog."),
            10
        );
        assert_eq!(
            count(
                cl,
                "用户的问题：底栏的 token 速率统计在中英混合文本下偏差很大。"
            ),
            32
        );
        assert_eq!(
            count(
                cl,
                "reasoning 输出期间没有刷新，只有 tool call 时才更新；用 BPE 词表统计更准确。"
            ),
            33
        );
        assert_eq!(
            count(
                cl,
                "fn main() { let v: Vec<u32> = (0..10).map(|i| i * 2).collect(); }"
            ),
            29
        );
        // OpenAI 模型映射：gpt-4o → o200k（中文压缩率更高），gpt-4 → cl100k
        assert_eq!(count(encoding_for(Some("gpt-4o")), "你好，世界"), 3);
        assert_eq!(count(encoding_for(Some("gpt-4-turbo")), "你好，世界"), 6);
        // 非 OpenAI 模型回退 cl100k
        assert_eq!(
            count(encoding_for(Some("claude-sonnet-4-5")), "你好，世界"),
            6
        );
    }

    #[test]
    fn meter_accumulates_chunked_deltas_without_double_counting() {
        let mut meter = TokenMeter::new(fallback_encoding());
        let text = "streaming 输出是逐段到达的：provider 会把词切开，token 统计必须跨增量边界。";
        // 每 5 个字符一段模拟 SSE delta（含词中间切断）
        let chars: Vec<char> = text.chars().collect();
        for chunk in chars.chunks(5) {
            let s: String = chunk.iter().collect();
            meter.feed(&s);
        }
        let chunked = meter.finish();
        let whole = count(fallback_encoding(), text);
        let err = (chunked as f64 - whole as f64).abs() / whole as f64;
        assert!(err < 0.03, "增量计数偏差过大: {chunked} vs {whole}");
    }

    #[test]
    fn meter_counts_monotonically_and_resets() {
        let mut meter = TokenMeter::new(fallback_encoding());
        assert_eq!(meter.count(), 0);
        let a = meter.feed("hello world ");
        let b = meter.feed("foo bar baz ");
        assert!(b >= a, "计数单调不减: {a} -> {b}");
        assert!(b > 0);
        meter.reset();
        assert_eq!(meter.count(), 0);
        // 换词表（模型切换）后清零
        meter.feed("some text here ");
        meter.set_encoding(encoding_for(Some("gpt-4o")));
        assert_eq!(meter.count(), 0);
    }
}
