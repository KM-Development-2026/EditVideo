//! タイムラインと編集操作
//!
//! 時刻は2系統ある:
//! - 素材時刻: 各素材ファイル内の時刻（`Segment.start/end`）
//! - タイムライン時刻: セグメントを並べた先頭からの時刻（再生ヘッド・IN/OUT）
//!
//! 削除済みセグメントもタイムライン上の幅を持つ（書き出し時に詰める）。

use super::MediaInfo;
use super::time::Timestamp;

/// 読み込んだ素材の識別子（`Timeline::sources` の添字）
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceId(pub usize);

/// 素材上の区間 [start, end)（素材時刻）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub source: SourceId,
    pub start: Timestamp,
    pub end: Timestamp,
    /// false = 削除扱い（書き出し対象外）
    pub enabled: bool,
}

impl Segment {
    pub fn duration(&self) -> Timestamp {
        self.end - self.start
    }
}

/// 書き出し対象の区間（素材時刻 [start, end)）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExportRange {
    pub source: SourceId,
    pub start: Timestamp,
    pub end: Timestamp,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Timeline {
    pub sources: Vec<MediaInfo>,
    /// タイムライン上の並び順
    pub segments: Vec<Segment>,
    /// タイムライン時刻
    pub in_point: Option<Timestamp>,
    pub out_point: Option<Timestamp>,
}

impl Timeline {
    pub fn new() -> Self {
        Self::default()
    }

    /// 素材を追加し、素材全体を覆うセグメントを末尾に追加する
    pub fn add_source(&mut self, info: MediaInfo) -> SourceId {
        let id = SourceId(self.sources.len());
        let duration = info.duration;
        self.sources.push(info);
        if duration > Timestamp::ZERO {
            self.segments.push(Segment {
                source: id,
                start: Timestamp::ZERO,
                end: duration,
                enabled: true,
            });
        }
        id
    }

    pub fn source(&self, id: SourceId) -> Option<&MediaInfo> {
        self.sources.get(id.0)
    }

    /// タイムライン全体の長さ（削除済みセグメントも含む）
    pub fn duration(&self) -> Timestamp {
        self.segments
            .iter()
            .fold(Timestamp::ZERO, |acc, s| acc + s.duration())
    }

    /// セグメント idx のタイムライン時刻 [start, end)
    pub fn segment_range(&self, idx: usize) -> Option<(Timestamp, Timestamp)> {
        let seg = self.segments.get(idx)?;
        let start = self.segments[..idx]
            .iter()
            .fold(Timestamp::ZERO, |acc, s| acc + s.duration());
        Some((start, start + seg.duration()))
    }

    /// セグメント境界のタイムライン時刻（0 と末尾を含む、昇順）
    pub fn boundaries(&self) -> Vec<Timestamp> {
        let mut v = Vec::with_capacity(self.segments.len() + 1);
        let mut t = Timestamp::ZERO;
        v.push(t);
        for s in &self.segments {
            t += s.duration();
            v.push(t);
        }
        v
    }

    /// タイムライン時刻 → (セグメント番号, 素材時刻)
    ///
    /// 境界ちょうどは後ろのセグメントに属する。末尾ちょうどは最後のセグメントの終端を返す。
    /// 範囲外（t < 0, t > 長さ）やセグメントが無い場合は None。
    pub fn locate(&self, t: Timestamp) -> Option<(usize, Timestamp)> {
        if t < Timestamp::ZERO {
            return None;
        }
        let mut start = Timestamp::ZERO;
        for (i, seg) in self.segments.iter().enumerate() {
            let end = start + seg.duration();
            if t < end {
                return Some((i, seg.start + (t - start)));
            }
            start = end;
        }
        if t == start {
            let last = self.segments.len().checked_sub(1)?;
            return Some((last, self.segments[last].end));
        }
        None
    }

    /// タイムライン時刻 t を含むセグメントを2分割する。
    /// 境界ちょうど・範囲外では何もしない（長さ0のセグメントを作らない）。
    /// 分割した場合 true。
    pub fn split_at(&mut self, t: Timestamp) -> bool {
        let Some((idx, src_t)) = self.locate(t) else {
            return false;
        };
        let seg = &self.segments[idx];
        if src_t <= seg.start || src_t >= seg.end {
            return false;
        }
        let mut right = seg.clone();
        right.start = src_t;
        self.segments[idx].end = src_t;
        self.segments.insert(idx + 1, right);
        true
    }

    /// 有効/無効を切り替える。idx が範囲外なら false
    pub fn toggle(&mut self, idx: usize) -> bool {
        match self.segments.get_mut(idx) {
            Some(s) => {
                s.enabled = !s.enabled;
                true
            }
            None => false,
        }
    }

    /// 無効化する。状態が変わった場合 true
    pub fn delete(&mut self, idx: usize) -> bool {
        self.set_enabled(idx, false)
    }

    /// 有効化（復元）する。状態が変わった場合 true
    pub fn restore(&mut self, idx: usize) -> bool {
        self.set_enabled(idx, true)
    }

    fn set_enabled(&mut self, idx: usize, enabled: bool) -> bool {
        match self.segments.get_mut(idx) {
            Some(s) if s.enabled != enabled => {
                s.enabled = enabled;
                true
            }
            _ => false,
        }
    }

    /// タイムライン区間 [a, b) を分割して無効化する。a > b は入れ替える。
    /// 何か変化した場合 true。
    pub fn delete_range(&mut self, a: Timestamp, b: Timestamp) -> bool {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let total = self.duration();
        let a = a.clamp_to(Timestamp::ZERO, total);
        let b = b.clamp_to(Timestamp::ZERO, total);
        if a >= b {
            return false;
        }
        let mut changed = self.split_at(a);
        changed |= self.split_at(b);
        let mut start = Timestamp::ZERO;
        for seg in &mut self.segments {
            let end = start + seg.duration();
            if start >= a && end <= b && seg.enabled {
                seg.enabled = false;
                changed = true;
            }
            start = end;
        }
        changed
    }

    /// IN 点を設定（タイムライン範囲にクランプ）。OUT 点以降なら OUT を消去する
    pub fn set_in(&mut self, t: Timestamp) {
        let t = t.clamp_to(Timestamp::ZERO, self.duration());
        if self.out_point.is_some_and(|o| t >= o) {
            self.out_point = None;
        }
        self.in_point = Some(t);
    }

    /// OUT 点を設定（タイムライン範囲にクランプ）。IN 点以前なら IN を消去する
    pub fn set_out(&mut self, t: Timestamp) {
        let t = t.clamp_to(Timestamp::ZERO, self.duration());
        if self.in_point.is_some_and(|i| t <= i) {
            self.in_point = None;
        }
        self.out_point = Some(t);
    }

    pub fn clear_in(&mut self) {
        self.in_point = None;
    }

    pub fn clear_out(&mut self) {
        self.out_point = None;
    }

    pub fn clear_in_out(&mut self) {
        self.in_point = None;
        self.out_point = None;
    }

    /// IN/OUT のどちらかが設定されていれば、その範囲 [in, out)（未設定側は先頭/末尾）
    pub fn in_out_range(&self) -> Option<(Timestamp, Timestamp)> {
        if self.in_point.is_none() && self.out_point.is_none() {
            return None;
        }
        let i = self.in_point.unwrap_or(Timestamp::ZERO);
        let o = self.out_point.unwrap_or_else(|| self.duration());
        Some((i, o))
    }

    /// t より前で最も近いセグメント境界
    pub fn prev_edit(&self, t: Timestamp) -> Option<Timestamp> {
        self.boundaries().into_iter().rev().find(|&b| b < t)
    }

    /// t より後で最も近いセグメント境界
    pub fn next_edit(&self, t: Timestamp) -> Option<Timestamp> {
        self.boundaries().into_iter().find(|&b| b > t)
    }

    /// 書き出し対象区間（素材時刻）の列。
    /// 有効セグメントのうち、IN/OUT があればその範囲と交差する部分を返す。
    /// 同じ素材で連続する区間は1つにまとめる。長さ0の区間は含めない。
    pub fn export_ranges(&self) -> Vec<ExportRange> {
        let (lo, hi) = self
            .in_out_range()
            .unwrap_or((Timestamp::ZERO, self.duration()));
        let mut out: Vec<ExportRange> = Vec::new();
        let mut start = Timestamp::ZERO;
        for seg in &self.segments {
            let end = start + seg.duration();
            let a = start.max(lo);
            let b = end.min(hi);
            if seg.enabled && a < b {
                let r = ExportRange {
                    source: seg.source,
                    start: seg.start + (a - start),
                    end: seg.start + (b - start),
                };
                match out.last_mut() {
                    Some(last) if last.source == r.source && last.end == r.start => {
                        last.end = r.end;
                    }
                    _ => out.push(r),
                }
            }
            start = end;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::time::Rational;
    use std::path::PathBuf;

    fn ts(s: i64) -> Timestamp {
        Timestamp::from_secs(s)
    }

    fn info(name: &str, secs: i64) -> MediaInfo {
        MediaInfo {
            path: PathBuf::from(name),
            duration: ts(secs),
            width: 1920,
            height: 1080,
            fps: Rational::new(30, 1).unwrap(),
            has_audio: true,
            video_codec: Some("h264".into()),
            audio_codec: Some("aac".into()),
            keyframes: Vec::new(),
        }
    }

    fn single(secs: i64) -> Timeline {
        let mut tl = Timeline::new();
        tl.add_source(info("a.mp4", secs));
        tl
    }

    /// 素材 a(10秒) + b(5秒)
    fn two_sources() -> Timeline {
        let mut tl = Timeline::new();
        tl.add_source(info("a.mp4", 10));
        tl.add_source(info("b.mp4", 5));
        tl
    }

    /// 不変条件: 各セグメントの長さが正で素材範囲内
    fn assert_invariants(tl: &Timeline) {
        for (i, s) in tl.segments.iter().enumerate() {
            assert!(
                s.start < s.end,
                "segment {i} has non-positive length: {s:?}"
            );
            assert!(s.start >= Timestamp::ZERO, "segment {i} starts before 0");
            let src = tl.source(s.source).expect("source exists");
            assert!(s.end <= src.duration, "segment {i} exceeds source");
        }
    }

    fn range(source: usize, a: i64, b: i64) -> ExportRange {
        ExportRange {
            source: SourceId(source),
            start: ts(a),
            end: ts(b),
        }
    }

    #[test]
    fn add_source_creates_full_segment() {
        let tl = two_sources();
        assert_eq!(tl.segments.len(), 2);
        assert_eq!(tl.segments[1].source, SourceId(1));
        assert_eq!(tl.segments[1].end, ts(5));
        assert_eq!(tl.duration(), ts(15));
        assert_eq!(tl.segment_range(1), Some((ts(10), ts(15))));
        assert_eq!(tl.segment_range(2), None);
        assert_invariants(&tl);
    }

    #[test]
    fn add_source_zero_duration_has_no_segment() {
        let mut tl = Timeline::new();
        let id = tl.add_source(info("empty.mp4", 0));
        assert_eq!(id, SourceId(0));
        assert!(tl.segments.is_empty());
        assert_eq!(tl.locate(Timestamp::ZERO), None);
        assert!(!tl.split_at(Timestamp::ZERO));
        assert!(tl.export_ranges().is_empty());
    }

    #[test]
    fn locate_across_sources() {
        let tl = two_sources();
        assert_eq!(tl.locate(ts(0)), Some((0, ts(0))));
        assert_eq!(
            tl.locate(Timestamp(9_999_999)),
            Some((0, Timestamp(9_999_999)))
        );
        // 境界ちょうどは後ろのセグメント
        assert_eq!(tl.locate(ts(10)), Some((1, ts(0))));
        assert_eq!(tl.locate(ts(12)), Some((1, ts(2))));
        // 末尾ちょうど
        assert_eq!(tl.locate(ts(15)), Some((1, ts(5))));
        assert_eq!(tl.locate(Timestamp(15_000_001)), None);
        assert_eq!(tl.locate(Timestamp(-1)), None);
    }

    #[test]
    fn locate_after_split_maps_source_time() {
        let mut tl = single(10);
        tl.split_at(ts(4));
        tl.delete(0);
        // 削除済みでもタイムライン上の幅は残る
        assert_eq!(tl.locate(ts(4)), Some((1, ts(4))));
        assert_eq!(tl.locate(ts(7)), Some((1, ts(7))));
    }

    #[test]
    fn split_in_middle() {
        let mut tl = single(10);
        assert!(tl.split_at(ts(3)));
        assert_eq!(tl.segments.len(), 2);
        assert_eq!((tl.segments[0].start, tl.segments[0].end), (ts(0), ts(3)));
        assert_eq!((tl.segments[1].start, tl.segments[1].end), (ts(3), ts(10)));
        assert_eq!(tl.duration(), ts(10));
        assert_invariants(&tl);
    }

    #[test]
    fn split_at_boundaries_is_noop() {
        let mut tl = single(10);
        assert!(!tl.split_at(ts(0)), "先頭");
        assert!(!tl.split_at(ts(10)), "末尾");
        assert!(!tl.split_at(ts(11)), "範囲外");
        assert!(!tl.split_at(Timestamp(-1)), "負");
        assert!(tl.split_at(ts(5)));
        assert!(!tl.split_at(ts(5)), "既存境界ちょうど");
        assert_eq!(tl.segments.len(), 2);
        assert_invariants(&tl);
    }

    #[test]
    fn split_keeps_enabled_state_and_source() {
        let mut tl = two_sources();
        tl.delete(1);
        assert!(tl.split_at(ts(12)));
        assert_eq!(tl.segments.len(), 3);
        assert_eq!(tl.segments[1].source, SourceId(1));
        assert_eq!((tl.segments[1].start, tl.segments[1].end), (ts(0), ts(2)));
        assert_eq!((tl.segments[2].start, tl.segments[2].end), (ts(2), ts(5)));
        assert!(!tl.segments[1].enabled && !tl.segments[2].enabled);
        assert_invariants(&tl);
    }

    #[test]
    fn split_one_microsecond_from_edge() {
        let mut tl = single(10);
        assert!(tl.split_at(Timestamp(1)));
        assert!(tl.split_at(Timestamp(9_999_999)));
        assert_eq!(tl.segments.len(), 3);
        assert_eq!(tl.segments[0].duration(), Timestamp(1));
        assert_eq!(tl.segments[2].duration(), Timestamp(1));
        assert_invariants(&tl);
    }

    #[test]
    fn toggle_delete_restore() {
        let mut tl = single(10);
        assert!(tl.delete(0));
        assert!(!tl.delete(0), "既に無効");
        assert!(!tl.segments[0].enabled);
        assert!(tl.restore(0));
        assert!(!tl.restore(0));
        assert!(tl.toggle(0));
        assert!(!tl.segments[0].enabled);
        assert!(tl.toggle(0));
        assert!(tl.segments[0].enabled);
        assert!(!tl.toggle(5));
        assert!(!tl.delete(5));
    }

    #[test]
    fn delete_range_middle() {
        let mut tl = single(10);
        assert!(tl.delete_range(ts(3), ts(6)));
        assert_eq!(tl.segments.len(), 3);
        let enabled: Vec<bool> = tl.segments.iter().map(|s| s.enabled).collect();
        assert_eq!(enabled, vec![true, false, true]);
        assert_eq!(tl.export_ranges(), vec![range(0, 0, 3), range(0, 6, 10)]);
        assert_invariants(&tl);
    }

    #[test]
    fn delete_range_head_tail_and_whole() {
        let mut tl = single(10);
        assert!(tl.delete_range(ts(0), ts(2)));
        assert!(tl.delete_range(ts(8), ts(10)));
        assert_eq!(tl.segments.len(), 3);
        assert_eq!(tl.export_ranges(), vec![range(0, 2, 8)]);
        assert_invariants(&tl);

        let mut tl = single(10);
        assert!(tl.delete_range(ts(0), ts(10)));
        assert_eq!(tl.segments.len(), 1);
        assert!(tl.export_ranges().is_empty());
    }

    #[test]
    fn delete_range_degenerate_and_reversed() {
        let mut tl = single(10);
        assert!(!tl.delete_range(ts(4), ts(4)), "空区間");
        assert!(!tl.delete_range(ts(10), ts(20)), "範囲外");
        assert_eq!(tl.segments.len(), 1);
        // 逆順は入れ替え
        assert!(tl.delete_range(ts(6), ts(3)));
        assert_eq!(tl.export_ranges(), vec![range(0, 0, 3), range(0, 6, 10)]);
        // 範囲外にはみ出す指定はクランプ
        let mut tl = single(10);
        assert!(tl.delete_range(Timestamp(-5), ts(2)));
        assert_eq!(tl.export_ranges(), vec![range(0, 2, 10)]);
        assert_invariants(&tl);
    }

    #[test]
    fn delete_range_already_deleted_is_unchanged() {
        let mut tl = single(10);
        tl.split_at(ts(5));
        tl.delete(0);
        assert!(!tl.delete_range(ts(0), ts(5)));
    }

    #[test]
    fn delete_range_across_sources() {
        let mut tl = two_sources();
        assert!(tl.delete_range(ts(8), ts(12)));
        assert_eq!(tl.segments.len(), 4);
        assert_eq!(tl.export_ranges(), vec![range(0, 0, 8), range(1, 2, 5)]);
        assert_eq!(tl.duration(), ts(15));
        assert_invariants(&tl);
    }

    #[test]
    fn in_out_set_and_clear() {
        let mut tl = single(10);
        assert_eq!(tl.in_out_range(), None);
        tl.set_in(ts(2));
        assert_eq!(tl.in_out_range(), Some((ts(2), ts(10))));
        tl.set_out(ts(7));
        assert_eq!(tl.in_out_range(), Some((ts(2), ts(7))));
        tl.clear_in();
        assert_eq!(tl.in_out_range(), Some((ts(0), ts(7))));
        tl.clear_out();
        assert_eq!(tl.in_out_range(), None);
        // クランプ
        tl.set_out(ts(99));
        assert_eq!(tl.out_point, Some(ts(10)));
        tl.set_in(Timestamp(-3));
        assert_eq!(tl.in_point, Some(ts(0)));
        tl.clear_in_out();
        assert_eq!((tl.in_point, tl.out_point), (None, None));
    }

    #[test]
    fn in_after_out_clears_out() {
        let mut tl = single(10);
        tl.set_out(ts(5));
        tl.set_in(ts(7));
        assert_eq!((tl.in_point, tl.out_point), (Some(ts(7)), None));
        tl.set_out(ts(3));
        assert_eq!((tl.in_point, tl.out_point), (None, Some(ts(3))));
        // IN == OUT も不正なので片方を消す
        tl.set_in(ts(3));
        assert_eq!((tl.in_point, tl.out_point), (Some(ts(3)), None));
    }

    #[test]
    fn prev_next_edit() {
        let mut tl = two_sources();
        tl.split_at(ts(4));
        // 境界: 0, 4, 10, 15
        assert_eq!(tl.prev_edit(ts(0)), None);
        assert_eq!(tl.next_edit(ts(0)), Some(ts(4)));
        assert_eq!(tl.prev_edit(ts(4)), Some(ts(0)), "境界ちょうどは含まない");
        assert_eq!(tl.next_edit(ts(4)), Some(ts(10)));
        assert_eq!(tl.prev_edit(ts(7)), Some(ts(4)));
        assert_eq!(tl.next_edit(ts(7)), Some(ts(10)));
        assert_eq!(tl.next_edit(ts(15)), None);
        assert_eq!(tl.prev_edit(ts(15)), Some(ts(10)));
    }

    #[test]
    fn export_ranges_merges_contiguous_enabled() {
        let mut tl = single(10);
        tl.split_at(ts(3));
        tl.split_at(ts(6));
        assert_eq!(tl.export_ranges(), vec![range(0, 0, 10)]);
    }

    #[test]
    fn export_ranges_with_in_out() {
        let mut tl = single(10);
        tl.delete_range(ts(4), ts(6));
        tl.set_in(ts(2));
        tl.set_out(ts(8));
        assert_eq!(tl.export_ranges(), vec![range(0, 2, 4), range(0, 6, 8)]);
        // IN/OUT が削除区間の内側だけなら空
        tl.set_in(ts(4));
        tl.set_out(ts(6));
        assert!(tl.export_ranges().is_empty());
        // IN/OUT が境界ちょうどで長さ0の区間を作らない
        tl.set_in(ts(6));
        tl.set_out(ts(10));
        assert_eq!(tl.export_ranges(), vec![range(0, 6, 10)]);
    }

    #[test]
    fn export_ranges_in_out_across_sources() {
        let mut tl = two_sources();
        tl.set_in(ts(9));
        tl.set_out(ts(11));
        assert_eq!(tl.export_ranges(), vec![range(0, 9, 10), range(1, 0, 1)]);
        // 同じ素材でも素材時刻が連続しなければまとめない
        let mut tl = Timeline::new();
        tl.add_source(info("a.mp4", 10));
        tl.split_at(ts(5));
        tl.segments.swap(0, 1);
        assert_eq!(tl.export_ranges(), vec![range(0, 5, 10), range(0, 0, 5)]);
    }

    #[test]
    fn export_ranges_same_source_twice_not_merged_across_end() {
        // 同じ素材を2回追加: [0,10) の後に [0,10) は連続しないので別区間
        let mut tl = Timeline::new();
        tl.add_source(info("a.mp4", 10));
        let second = tl.segments[0].clone();
        tl.segments.push(second);
        assert_eq!(tl.export_ranges(), vec![range(0, 0, 10), range(0, 0, 10)]);
        assert_invariants(&tl);
    }
}
