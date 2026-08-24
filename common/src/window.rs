//! 見えている窓——どの行から何行を見せるか（VIEW-a）。
//!
//! # なぜ `common` に在るのか
//!
//! **ここは `common` の中で唯一、ユーザープログラムからも取り込まれる
//! モジュールである**（`ADR-0045`）。**`kernel/userland/zi.rs` が
//! `#[path]` で取り込む。** **`less` と `more` も同じものを使う。**
//!
//! **理由はホストで検算できることである。** **ユーザープログラムは cargo の
//! パッケージに属さないので、`cargo test --workspace` から見えない**
//! （`kernel/build.rs` が `rustc` を直に呼び、`--extern` を1つも渡さない）。
//! **窓の計算は純粋な論理で、ホストテストが最も安く効く種類である。**
//!
//! # 自己完結でなければならない
//!
//! **`crate::` を1つも参照しない。** **`#[path]` で取り込まれた側では
//! `crate` が `zi` になる**ので、参照があるとそこで壊れる。
//! **`core` だけを使う。**
//!
//! # 描画は持たない
//!
//! **「どの行から何行を見せるか」だけを決める。** **どう描くかは使う側の
//! 話である**——`zi` は編集を描き、`less` は読み取り専用で状態行の中身も違う。
//! **共通にするのは、両方でまったく同じ規則になる部分だけである。**

/// 見えている窓（VIEW-a）。
///
/// **`top` は先頭に見えている行、`height` は一度に見せる行数である。**
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Window {
    top: usize,
    height: usize,
}

impl Window {
    /// 先頭を見ている窓を作る。
    ///
    /// **`height` が 0 なら 1 として扱う。** **0 だと `follow` の算術が
    /// 破綻する**（引く数が引かれる数より大きくなる）。**呼ぶ側で
    /// 場当たりに守る形にしない**（`userlib::WindowSize::or_default` と同じ判断）。
    pub const fn new(height: usize) -> Self {
        Self {
            top: 0,
            height: if height == 0 { 1 } else { height },
        }
    }

    /// 先頭に見えている行。
    pub const fn top(&self) -> usize {
        self.top
    }

    /// 一度に見せる行数。
    pub const fn height(&self) -> usize {
        self.height
    }

    /// 見えている行の範囲（全体が `total` 行のとき）。
    ///
    /// **`total` を越えない。** **窓が末尾より下に在るときは空になる。**
    pub fn visible(&self, total: usize) -> core::ops::Range<usize> {
        let start = if self.top > total { total } else { self.top };
        let end = match start.checked_add(self.height) {
            Some(end) if end < total => end,
            _ => total,
        };
        start..end
    }

    /// `line` が見えるように窓を動かす。**動いたら真。**
    ///
    /// # 規則は2つだけである
    ///
    /// **窓より上に出たら、その行を先頭にする。**
    /// **窓より下に出たら、その行が最後に見えるようにする。**
    /// **中に居るなら動かさない。**
    ///
    /// **`vi` と同じ形である。** **1行ずつしか動かさない形にはしない**
    /// ——**跳んだ先が窓から遠いとき、1行ずつでは追いつかない。**
    pub fn follow(&mut self, line: usize) -> bool {
        if line < self.top {
            self.top = line;
            return true;
        }
        let last = self.top + self.height - 1;
        if line > last {
            self.top = line + 1 - self.height;
            return true;
        }
        false
    }

    /// 窓そのものを下へ動かす（VIEW-b）。**動いたら真。**
    ///
    /// # 追うのではなく、動かす
    ///
    /// **[`Self::follow`] はカーソルを追う形で、`zi` が使う。**
    /// **`less` と `more` はカーソルを持たない**——**窓そのものを動かす。**
    /// **規則が違うので、別の口にしてある。**
    ///
    /// # 末尾より先へは行かない
    ///
    /// **最後の1画面ぶんで止まる**（`top` の上限は `total - height`）。
    /// **全体が1画面に収まるなら動かない。** **`less` と同じ振る舞いである**
    /// ——**末尾の先の空白へ落ちていく形にはしない。**
    pub fn scroll_down(&mut self, lines: usize, total: usize) -> bool {
        let max = total.saturating_sub(self.height);
        let want = self.top.saturating_add(lines).min(max);
        if want == self.top {
            return false;
        }
        self.top = want;
        true
    }

    /// 窓そのものを上へ動かす（VIEW-b）。**動いたら真。**
    ///
    /// **先頭で止まる。** **`total` を要らない**——**上端は 0 で、全体の
    /// 行数に依らない。**
    pub fn scroll_up(&mut self, lines: usize) -> bool {
        let want = self.top.saturating_sub(lines);
        if want == self.top {
            return false;
        }
        self.top = want;
        true
    }

    /// `line` が画面の何行目に出るか。**窓の外なら `None`。**
    pub fn screen_row(&self, line: usize) -> Option<usize> {
        if line < self.top || line >= self.top + self.height {
            return None;
        }
        Some(line - self.top)
    }
}

#[cfg(test)]
mod tests {
    use super::Window;

    #[test]
    fn scrolling_down_stops_at_the_last_screenful() {
        let mut window = Window::new(10);
        assert!(window.scroll_down(5, 100));
        assert_eq!(window.top(), 5);
        // **上限は `total - height` である。** それより先へは行かない。
        assert!(window.scroll_down(1000, 100));
        assert_eq!(window.top(), 90);
        assert!(!window.scroll_down(1, 100), "端に着いたら動かない");
    }

    #[test]
    fn scrolling_does_not_move_when_everything_fits() {
        let mut window = Window::new(10);
        assert!(!window.scroll_down(1, 10), "全体が1画面に収まる");
        assert!(!window.scroll_down(1, 3), "画面より短い");
        assert_eq!(window.top(), 0);
    }

    #[test]
    fn scrolling_up_stops_at_the_top() {
        let mut window = Window::new(10);
        window.scroll_down(20, 100);
        assert_eq!(window.top(), 20);
        assert!(window.scroll_up(5));
        assert_eq!(window.top(), 15);
        assert!(window.scroll_up(1000));
        assert_eq!(window.top(), 0);
        assert!(!window.scroll_up(1), "先頭では動かない");
    }

    #[test]
    fn a_page_down_then_a_page_up_returns_to_where_it_started() {
        let mut window = Window::new(48);
        assert!(window.scroll_down(48, 100));
        assert_eq!(window.top(), 48);
        assert!(window.scroll_up(48));
        assert_eq!(window.top(), 0, "同じ量で戻る");
    }

    #[test]
    fn a_new_window_shows_the_top() {
        let window = Window::new(10);
        assert_eq!(window.top(), 0);
        assert_eq!(window.height(), 10);
        assert_eq!(window.visible(100), 0..10);
    }

    #[test]
    fn a_zero_height_window_becomes_one() {
        // **0 を通すと `follow` の算術が破綻する**（doc の理由）。
        let window = Window::new(0);
        assert_eq!(window.height(), 1);
        assert_eq!(window.visible(100), 0..1);
    }

    #[test]
    fn visible_never_passes_the_end() {
        let window = Window::new(10);
        assert_eq!(window.visible(4), 0..4);
        assert_eq!(window.visible(0), 0..0);
    }

    #[test]
    fn a_line_inside_the_window_does_not_move_it() {
        let mut window = Window::new(10);
        assert!(!window.follow(0));
        assert!(!window.follow(9));
        assert_eq!(window.top(), 0);
    }

    #[test]
    fn a_line_below_the_window_pulls_it_down_by_the_least() {
        let mut window = Window::new(10);
        assert!(window.follow(10));
        // **1 行ぶんだけ動く。** 10 行目が最後に見える位置である。
        assert_eq!(window.top(), 1);
        assert_eq!(window.visible(100), 1..11);
    }

    #[test]
    fn a_far_jump_lands_the_line_at_the_bottom() {
        let mut window = Window::new(10);
        assert!(window.follow(99));
        assert_eq!(window.top(), 90);
        assert_eq!(window.screen_row(99), Some(9));
    }

    #[test]
    fn a_line_above_the_window_becomes_the_top() {
        let mut window = Window::new(10);
        window.follow(99);
        assert!(window.follow(50));
        assert_eq!(window.top(), 50);
        assert_eq!(window.screen_row(50), Some(0));
    }

    #[test]
    fn the_screen_row_is_none_outside_the_window() {
        let mut window = Window::new(10);
        window.follow(99);
        assert_eq!(window.screen_row(89), None);
        assert_eq!(window.screen_row(100), None);
    }

    #[test]
    fn a_window_of_one_line_still_follows() {
        let mut window = Window::new(1);
        assert!(window.follow(7));
        assert_eq!(window.top(), 7);
        assert_eq!(window.screen_row(7), Some(0));
    }
}
