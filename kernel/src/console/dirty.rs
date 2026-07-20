//! 更新範囲の追跡（M3-c-1）。
//!
//! バックバッファのうち、フレームバッファへまだ転送していない範囲を
//! 外接矩形 1 個で覚えておく。転送はキャッシュ無効なフレームバッファへの
//! 書き込みであり、最も高い工程なので、変更していない部分は送らない。
//!
//! 領域のリストではなく外接矩形 1 個にしてある。逐次的なテキスト出力では
//! 外接矩形は十分に小さく収まる。左上と右下だけを更新したような最悪の
//! 場合は全画面を送ることになるが、それは素朴な全面転送と同じであり、
//! それより悪くはならない（ADR-0017）。
//!
//! 生ポインタを使わない純粋ロジックであり、ホスト上の `cargo test` で
//! 検証する。

/// 画面内の矩形。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    /// 右端（排他）。
    pub fn right(&self) -> u32 {
        self.x.saturating_add(self.width)
    }

    /// 下端（排他）。
    pub fn bottom(&self) -> u32 {
        self.y.saturating_add(self.height)
    }
}

/// 未転送の範囲。画面サイズを持ち、記録時に画面内へ切り詰める。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DirtyRegion {
    screen_width: u32,
    screen_height: u32,
    bounds: Option<Rect>,
}

impl DirtyRegion {
    pub fn new(screen_width: u32, screen_height: u32) -> Self {
        Self {
            screen_width,
            screen_height,
            bounds: None,
        }
    }

    /// 未転送の範囲が無いか。
    pub fn is_empty(&self) -> bool {
        self.bounds.is_none()
    }

    /// 現在の外接矩形。転送せずに中身だけ見たい場合に使う。
    pub fn bounds(&self) -> Option<Rect> {
        self.bounds
    }

    /// 更新範囲を記録する。画面外へはみ出した分は切り詰める。
    ///
    /// 画面外の矩形・幅または高さが 0 の矩形は、記録しても転送すべき
    /// ものが無いので無視する。
    pub fn mark(&mut self, x: u32, y: u32, width: u32, height: u32) {
        let Some(rect) = self.clip(x, y, width, height) else {
            return;
        };

        self.bounds = Some(match self.bounds {
            None => rect,
            Some(current) => {
                let x = current.x.min(rect.x);
                let y = current.y.min(rect.y);
                let right = current.right().max(rect.right());
                let bottom = current.bottom().max(rect.bottom());
                Rect {
                    x,
                    y,
                    width: right - x,
                    height: bottom - y,
                }
            }
        });
    }

    /// 画面全体を未転送として記録する。スクロールのように画面全体が
    /// 変わる場合に使う。
    pub fn mark_all(&mut self) {
        self.mark(0, 0, self.screen_width, self.screen_height);
    }

    /// 未転送の範囲を取り出し、記録を空に戻す。
    ///
    /// 転送する側はこれを呼び、返った矩形だけを送る。`None` なら送る
    /// ものが無いので、フレームバッファには一切触らない。
    pub fn take(&mut self) -> Option<Rect> {
        self.bounds.take()
    }

    /// 画面内へ切り詰める。完全に画面外、または空なら `None`。
    fn clip(&self, x: u32, y: u32, width: u32, height: u32) -> Option<Rect> {
        if width == 0 || height == 0 || x >= self.screen_width || y >= self.screen_height {
            return None;
        }
        Some(Rect {
            x,
            y,
            width: width.min(self.screen_width - x),
            height: height.min(self.screen_height - y),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN_WIDTH: u32 = 1280;
    const SCREEN_HEIGHT: u32 = 800;

    fn region() -> DirtyRegion {
        DirtyRegion::new(SCREEN_WIDTH, SCREEN_HEIGHT)
    }

    #[test]
    fn a_new_region_has_nothing_to_transfer() {
        let region = region();
        assert!(region.is_empty());
        assert_eq!(region.bounds(), None);
    }

    #[test]
    fn taking_from_an_empty_region_transfers_nothing() {
        let mut region = region();
        assert_eq!(region.take(), None);
        // 取り出した後も空のまま。
        assert!(region.is_empty());
    }

    #[test]
    fn a_single_update_is_recorded_as_is() {
        let mut region = region();
        region.mark(10, 20, 30, 40);
        assert_eq!(
            region.bounds(),
            Some(Rect {
                x: 10,
                y: 20,
                width: 30,
                height: 40
            })
        );
    }

    #[test]
    fn two_updates_are_merged_into_their_bounding_box() {
        let mut region = region();
        region.mark(10, 10, 10, 10); // 10..20, 10..20
        region.mark(100, 200, 10, 10); // 100..110, 200..210
        assert_eq!(
            region.bounds(),
            Some(Rect {
                x: 10,
                y: 10,
                width: 100,  // 110 - 10
                height: 200, // 210 - 10
            })
        );
    }

    #[test]
    fn merging_keeps_the_earlier_rect_when_the_later_one_is_inside_it() {
        let mut region = region();
        region.mark(10, 10, 100, 100);
        region.mark(20, 20, 10, 10);
        assert_eq!(
            region.bounds(),
            Some(Rect {
                x: 10,
                y: 10,
                width: 100,
                height: 100
            })
        );
    }

    #[test]
    fn merging_expands_in_every_direction() {
        let mut region = region();
        region.mark(100, 100, 10, 10); // 100..110
        region.mark(50, 50, 10, 10); // 左上へ広がる
        region.mark(200, 300, 10, 10); // 右下へ広がる
        assert_eq!(
            region.bounds(),
            Some(Rect {
                x: 50,
                y: 50,
                width: 160,  // 210 - 50
                height: 260, // 310 - 50
            })
        );
    }

    #[test]
    fn many_updates_merge_into_one_rect() {
        let mut region = region();
        for index in 0..50u32 {
            region.mark(index * 8, index * 16, 8, 16);
        }
        let bounds = region.bounds().unwrap();
        assert_eq!(bounds.x, 0);
        assert_eq!(bounds.y, 0);
        assert_eq!(bounds.right(), 49 * 8 + 8);
        assert_eq!(bounds.bottom(), 49 * 16 + 16);
    }

    #[test]
    fn taking_resets_the_region() {
        let mut region = region();
        region.mark(10, 20, 30, 40);
        assert!(!region.is_empty());

        let taken = region.take();
        assert_eq!(
            taken,
            Some(Rect {
                x: 10,
                y: 20,
                width: 30,
                height: 40
            })
        );
        // 転送し終わったので、次のフラッシュでは何も送らない。
        assert!(region.is_empty());
        assert_eq!(region.take(), None);
    }

    #[test]
    fn an_update_crossing_the_right_edge_is_clipped_inside_the_screen() {
        let mut region = region();
        region.mark(SCREEN_WIDTH - 4, 0, 100, 16);
        let bounds = region.bounds().unwrap();
        assert_eq!(bounds.width, 4);
        assert!(bounds.right() <= SCREEN_WIDTH);
    }

    #[test]
    fn an_update_crossing_the_bottom_edge_is_clipped_inside_the_screen() {
        let mut region = region();
        region.mark(0, SCREEN_HEIGHT - 5, 16, 100);
        let bounds = region.bounds().unwrap();
        assert_eq!(bounds.height, 5);
        assert!(bounds.bottom() <= SCREEN_HEIGHT);
    }

    #[test]
    fn an_update_crossing_both_edges_is_clipped_on_both_axes() {
        let mut region = region();
        region.mark(SCREEN_WIDTH - 2, SCREEN_HEIGHT - 3, u32::MAX, u32::MAX);
        let bounds = region.bounds().unwrap();
        assert_eq!(bounds.width, 2);
        assert_eq!(bounds.height, 3);
        assert!(bounds.right() <= SCREEN_WIDTH);
        assert!(bounds.bottom() <= SCREEN_HEIGHT);
    }

    #[test]
    fn a_fully_offscreen_update_is_ignored() {
        let mut region = region();
        region.mark(SCREEN_WIDTH, 0, 10, 10);
        region.mark(0, SCREEN_HEIGHT, 10, 10);
        region.mark(u32::MAX, u32::MAX, 10, 10);
        assert!(region.is_empty());
    }

    #[test]
    fn an_empty_update_is_ignored() {
        let mut region = region();
        region.mark(10, 10, 0, 10);
        region.mark(10, 10, 10, 0);
        assert!(region.is_empty());
    }

    #[test]
    fn an_offscreen_update_does_not_disturb_an_existing_rect() {
        let mut region = region();
        region.mark(10, 20, 30, 40);
        region.mark(SCREEN_WIDTH, SCREEN_HEIGHT, 10, 10);
        assert_eq!(
            region.bounds(),
            Some(Rect {
                x: 10,
                y: 20,
                width: 30,
                height: 40
            })
        );
    }

    #[test]
    fn marking_everything_covers_the_whole_screen() {
        let mut region = region();
        region.mark(10, 20, 30, 40);
        region.mark_all();
        assert_eq!(
            region.bounds(),
            Some(Rect {
                x: 0,
                y: 0,
                width: SCREEN_WIDTH,
                height: SCREEN_HEIGHT
            })
        );
    }

    /// 記録した矩形は、どう組み合わせても画面内に収まる。転送側はこの
    /// 不変条件に依存して範囲チェックを省く。
    #[test]
    fn the_bounding_box_always_stays_inside_the_screen() {
        let mut region = region();
        let updates = [
            (0u32, 0u32, 8u32, 16u32),
            (SCREEN_WIDTH - 1, SCREEN_HEIGHT - 1, 64, 64),
            (SCREEN_WIDTH / 2, SCREEN_HEIGHT / 2, u32::MAX, u32::MAX),
            (SCREEN_WIDTH + 100, 0, 10, 10),
        ];
        for (x, y, width, height) in updates {
            region.mark(x, y, width, height);
            if let Some(bounds) = region.bounds() {
                assert!(bounds.right() <= SCREEN_WIDTH, "right edge escaped");
                assert!(bounds.bottom() <= SCREEN_HEIGHT, "bottom edge escaped");
            }
        }
    }
}
