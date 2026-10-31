//! `bouncyballs-banner`：小球落位式启动横幅。
//!
//! 左侧 art 不直接显示，而是拆成一颗颗球从画布上方落下：球体字符按 out-bounce 缓动
//! 落到各自的格子，落位后显形为该格原本的 art 字符，全部落完即定格成静态 art。
//! 右侧信息区、竖线与宽高三档规则完全复用 [`super::render_art`]。
//!
//! 小球位置与起始帧在构造时按 art 内容确定（不用随机数），渲染只用帧号算画面，
//! 因此状态只有一个帧原子计数：`render` 每调一次推进一帧（按一轮「下落 + 静置」取模），
//! `wants_redraw` 恒为 `true` 让空闲的 TUI 持续重绘（动画 tick 80ms/帧，
//! 全片约 3 秒，落完静置 3 秒后再从头循环）。

use super::{art_lines, render_art, render_wordmark};
use crate::{
    core::extensions::{BannerCtx, BannerExtension, BannerLine, ExtensionMode},
    extensions::{BANNER_FACTORIES, BannerFactory, PRIORITY_BANNER_BOUNCY},
    utils::{display::display_width, terminal_caps},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// 每颗球从目标行上方多少行开始下落
const FALL_ROWS: usize = 3;
/// 一颗球从起始点落到目标行所用的帧数
const BALL_FALL_FRAMES: usize = 10;
/// 同一批放球覆盖的 art 列数：列越靠右批次越晚，形成自左向右的波浪下落
const BALL_BATCH_COLS: usize = 3;
/// 相邻批次之间相差的帧数
const BALL_BATCH_DELAY: usize = 2;
/// 下落途中的球体字符（按落位列与行轮换，避免整片同字符）
const BALL_SYMBOLS: [char; 5] = ['*', 'o', 'O', '0', '.'];
/// 定格成静态 art 后到下一轮下落之间的静置时长
const LOOP_PAUSE_MS: usize = 3000;
/// 空闲 TUI 的动画 tick（`src/modes/tui.rs` 的 80ms 定时分支），用来把静置时长换算成帧数
const FRAME_MS: usize = 80;
/// 静置帧数：3 秒按 80ms/帧取整
const LOOP_PAUSE_FRAMES: usize = LOOP_PAUSE_MS / FRAME_MS;

/// 向扩展注册表登记小球横幅工厂：priority 比 [`crate::extensions::PRIORITY_BANNER`]
/// 小一档，后注册于静态版，因此默认生效（banner 互斥取最后注册的启用者）。
#[linkme::distributed_slice(BANNER_FACTORIES)]
static BOUNCY_BALLS_FACTORY: BannerFactory = BannerFactory {
    priority: PRIORITY_BANNER_BOUNCY,
    make: || Arc::new(BouncyBallsBanner::new()),
};

/// 小球落位式启动横幅：art 由小球自上落下、回弹落位后显形。
pub struct BouncyBallsBanner {
    /// art 画布列数（= art 最长行显示宽）
    width: usize,
    /// art 画布行数
    rows: usize,
    /// 全部小球（与 art 非空格 1:1）
    balls: Vec<Ball>,
    /// 落位动画总帧数（末球落位后再留一帧定格）
    total: usize,
    /// 一轮循环的帧数 = `total`（下落）+ 静置帧数
    cycle: usize,
    /// 已渲染的帧数：每渲染一帧 +1，对 `cycle` 取模得当前帧
    frame: AtomicUsize,
}

/// 小球横幅里的一颗小球（对应 art 的一个非空格）
struct Ball {
    /// 落位后的行号（0 起，相对 art 顶部）
    row: usize,
    /// 落位后的列号（0 起，相对 art 左边缘）
    col: usize,
    /// 落位后显示的 art 字符
    ch: char,
    /// 下落途中显示的球体字符
    symbol: char,
    /// 开始下落的帧号
    start: usize,
    /// 从 [`FALL_ROWS`] 行高处落到 `row` 所用的帧数
    dur: usize,
}

impl Ball {
    /// 第 `f` 帧时的画布行号；未开始、已落位或仍在画布上方（负数）时返 `None`。
    fn row_at(&self, f: usize) -> Option<usize> {
        if f < self.start || f >= self.start + self.dur {
            return None;
        }
        let p = (f - self.start) as f64 / self.dur as f64;
        let target = self.row as f64;
        let pos = target - (1.0 - out_bounce(p)) * (target + FALL_ROWS as f64);
        (pos >= 0.0).then(|| pos.round() as usize)
    }
}

impl BouncyBallsBanner {
    /// 按当前 art 内容生成小球（构造要便宜：只跑一遍 art 字符）。
    pub fn new() -> Self {
        let art = art_lines();
        let width = art.iter().map(|l| display_width(l)).max().unwrap_or(0);
        let mut balls = Vec::new();
        for (r, line) in art.iter().enumerate() {
            for (c, ch) in line.chars().enumerate() {
                if ch.is_whitespace() {
                    continue;
                }
                balls.push(Ball {
                    row: r,
                    col: c,
                    ch,
                    symbol: BALL_SYMBOLS[(c + r) % BALL_SYMBOLS.len()],
                    start: (c / BALL_BATCH_COLS) * BALL_BATCH_DELAY + (c * 7 + r) % 3,
                    dur: BALL_FALL_FRAMES,
                });
            }
        }
        let total = balls.iter().map(|b| b.start + b.dur).max().unwrap_or(0) + 1;
        Self {
            width,
            rows: art.len(),
            balls,
            total,
            cycle: total + LOOP_PAUSE_FRAMES,
            frame: AtomicUsize::new(0),
        }
    }

    /// 第 `f` 帧的 art 行：已落位的格显原字符，下落中的球显球体字符（可压过已落位的格）。
    fn frame_lines(&self, f: usize) -> Vec<String> {
        let mut grid = vec![vec![' '; self.width]; self.rows];
        for b in &self.balls {
            if f >= b.start + b.dur {
                grid[b.row][b.col] = b.ch;
            }
        }
        for b in &self.balls {
            if let Some(r) = b.row_at(f)
                && r < self.rows
            {
                grid[r][b.col] = b.symbol;
            }
        }
        grid.into_iter()
            .map(|row| row.into_iter().collect())
            .collect()
    }
}

impl Default for BouncyBallsBanner {
    /// 默认构造等价于 [`BouncyBallsBanner::new`]。
    fn default() -> Self {
        Self::new()
    }
}

impl BannerExtension for BouncyBallsBanner {
    /// 固定返回 `"bouncyballs-banner"`。
    fn name(&self) -> &str {
        "bouncyballs-banner"
    }

    /// 返回 `/extension` 面板展示的一句话说明。
    fn description(&self) -> &str {
        "Startup banner whose left-hand art drops in as bouncing balls."
    }

    /// 只在 Minimal 模式注册。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Minimal]
    }

    /// 恒为 `true`：循环播放没有终点，空闲 TUI 只在 80ms tick 里问一次，
    /// 不持续请求重绘就既不会下落也不会在静置后重新开始。
    /// “界面真在显示 banner”由调用方筛（提交过输入/有历史消息就不再问本方法）。
    fn wants_redraw(&self) -> bool {
        true
    }

    /// 渲染下一帧（每调一次推进一帧，静置期停在末帧，然后回到首帧重播）；
    /// 宽高不足时与静态版同样隐藏。
    fn render(&self, ctx: &BannerCtx) -> Vec<BannerLine> {
        if terminal_caps::is_apple_terminal() {
            return render_wordmark(ctx);
        }
        let f = (self.frame.fetch_add(1, Ordering::Relaxed) % self.cycle).min(self.total - 1);
        render_art(ctx, &self.frame_lines(f))
    }
}

/// 标准 ease-out-bounce：`p` 在 `0..=1` 时返回 `0..=1`（带三次回弹，不超调）。
fn out_bounce(p: f64) -> f64 {
    const N: f64 = 7.5625;
    const D: f64 = 2.75;
    if p < 1.0 / D {
        N * p * p
    } else if p < 2.0 / D {
        let t = p - 1.5 / D;
        N * t * t + 0.75
    } else if p < 2.5 / D {
        let t = p - 2.25 / D;
        N * t * t + 0.9375
    } else {
        let t = p - 2.625 / D;
        N * t * t + 0.984375
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::{
        PRIORITY_BANNER,
        banner::{BOTTOM_PAD, TOP_PAD, art_lines},
    };

    /// 一行横幅的纯文本
    fn row_text(line: &BannerLine) -> String {
        line.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn metadata() {
        let b = BouncyBallsBanner::new();
        assert_eq!(b.name(), "bouncyballs-banner");
        assert_eq!(b.modes(), vec![ExtensionMode::Minimal]);
        assert!(!b.description().is_empty());
        assert!(
            BOUNCY_BALLS_FACTORY.priority < PRIORITY_BANNER,
            "priority 更小才会后注册、默认生效"
        );
    }

    #[test]
    fn out_bounce_spans_zero_to_one_without_overshoot() {
        assert_eq!(out_bounce(0.0), 0.0);
        assert!((out_bounce(1.0) - 1.0).abs() < 1e-9, "末点应回落正好");
        assert!(
            (0..=100).all(|i| {
                let v = out_bounce(i as f64 / 100.0);
                (0.0..=1.0).contains(&v)
            }),
            "回弹曲线不超调也不为负"
        );
    }

    #[test]
    fn falls_in_then_settles_as_art() {
        let banner = BouncyBallsBanner::new();
        let art = art_lines();
        assert_eq!(banner.rows, art.len());
        assert_eq!(
            banner.balls.len(),
            art.iter()
                .map(|l| l.chars().filter(|c| !c.is_whitespace()).count())
                .sum::<usize>(),
            "每个 art 非空格一颗球"
        );

        // 首帧：球都还在画布上沿之外
        let first = banner.frame_lines(0);
        assert!(
            first.iter().all(|l| l.trim().is_empty()),
            "首帧画布应为空: {first:?}"
        );

        // 中段：画布内是球体字符，行宽不超画布
        let mid = banner.frame_lines(banner.total / 3);
        assert!(
            mid.iter()
                .any(|l| BALL_SYMBOLS.iter().any(|s| l.contains(*s))),
            "中段应有球: {mid:?}"
        );
        assert!(mid.iter().all(|l| display_width(l) <= banner.width));

        // 末帧：全部落位成 art 原文（右侧补空格到画布宽）
        let last = banner.frame_lines(banner.total - 1);
        let want: Vec<String> = art
            .iter()
            .map(|l| format!("{l:<w$}", w = banner.width))
            .collect();
        assert_eq!(last, want, "末帧应定格为 art");
    }

    #[test]
    fn keeps_requesting_redraw_and_loops_with_pause() {
        let banner = BouncyBallsBanner::new();
        let ctx = BannerCtx {
            width: 100,
            height: 40,
        };
        assert!(
            banner.wants_redraw(),
            "启动即请求重绘（否则空闲时只有一帧）"
        );
        let art_area = |lines: &[BannerLine]| -> Vec<String> {
            lines[TOP_PAD..TOP_PAD + banner.rows]
                .iter()
                .map(row_text)
                .collect()
        };
        let first_frame = art_area(&banner.render(&ctx));
        assert!(
            banner.frame_lines(0).iter().all(|l| l.trim().is_empty()),
            "首帧画布应为空"
        );
        for _ in 1..banner.total {
            assert_eq!(
                banner.render(&ctx).len(),
                TOP_PAD + banner.rows + BOTTOM_PAD,
                "动画期间布局不变"
            );
        }
        assert!(banner.wants_redraw(), "循环播放需要一直重绘");

        // 静置期：末帧逐帧完全相同（且含 art 首行）
        let baseline = art_area(&banner.render(&ctx));
        assert!(
            baseline[0].contains(art_lines()[0].as_str()),
            "定格帧含 art 首行: {:?}",
            baseline[0]
        );
        assert_eq!(banner.cycle - banner.total, LOOP_PAUSE_FRAMES);
        for i in 1..LOOP_PAUSE_FRAMES {
            assert_eq!(
                art_area(&banner.render(&ctx)),
                baseline,
                "静置第 {i} 帧应与定格帧相同"
            );
        }

        // 静置结束后回到首帧：画布重新清空，与开场首帧一致
        let restart = art_area(&banner.render(&ctx));
        assert_ne!(restart, baseline, "循环后应离开定格帧");
        assert_eq!(restart, first_frame, "循环回第一帧应重新开始下落");
    }

    #[test]
    fn narrow_or_short_terminal_hides_banner() {
        let banner = BouncyBallsBanner::new();
        let wide = BannerCtx {
            width: 200,
            height: 40,
        };
        assert!(!banner.render(&wide).is_empty());
        assert!(
            banner
                .render(&BannerCtx {
                    width: super::super::LEFT_PAD + banner.width - 1,
                    height: 40,
                })
                .is_empty(),
            "宽度不足时与静态版同规则隐藏"
        );
        assert!(
            banner
                .render(&BannerCtx {
                    width: 200,
                    height: TOP_PAD + banner.rows + BOTTOM_PAD + 4,
                })
                .is_empty(),
            "高度不足时隐藏"
        );
    }
}
