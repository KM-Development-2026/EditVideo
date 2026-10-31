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

    /// タイムライン時刻 t を、そのセグメントの素材で最寄りのフレーム開始時刻へ寄せる
    /// （セグメント範囲内にクランプ）。範囲外ならそのまま返す。
    pub fn snap_to_frame(&self, t: Timestamp) -> Timestamp {
        let Some((idx, src)) = self.locate(t) else {
            return t;
        };
        let seg = &self.segments[idx];
        let (Some(info), Some((seg_start, _))) = (self.source(seg.source), self.segment_range(idx))
        else {
            return t;
        };
        let snapped = info.fps.snap(src).clamp_to(seg.start, seg.end);
        seg_start + (snapped - seg.start)
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
        self.set_range_enabled(a, b, false)
    }

    /// タイムライン区間 [a, b) を分割して有効化（復元）する。何か変化した場合 true。
    pub fn restore_range(&mut self, a: Timestamp, b: Timestamp) -> bool {
        self.set_range_enabled(a, b, true)
    }

    fn set_range_enabled(&mut self, a: Timestamp, b: Timestamp, enabled: bool) -> bool {
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
            if start >= a && end <= b && seg.enabled != enabled {
                seg.enabled = enabled;
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
        self.export_ranges_with_start()
            .into_iter()
            .map(|(r, _)| r)
            .collect()
    }

    /// `export_ranges` と、各区間の開始のタイムライン時刻
    fn export_ranges_with_start(&self) -> Vec<(ExportRange, Timestamp)> {
        let (lo, hi) = self
            .in_out_range()
            .unwrap_or((Timestamp::ZERO, self.duration()));
        let mut out: Vec<(ExportRange, Timestamp)> = Vec::new();
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
                    Some((last, _)) if last.source == r.source && last.end == r.start => {
                        last.end = r.end;
                    }
                    _ => out.push((r, a)),
                }
            }
            start = end;
        }
        out
    }

    /// 書き出し区間の開始がキーフレームでない箇所（高速モードの警告用）。
    /// keyframes は素材ごとのキーフレーム位置（素材時刻・昇順。`SourceId` の添字）。
    /// 未取得（空）の素材は判定しない。フレーム番号が同じならキーフレーム上とみなす。
    pub fn keyframe_issues(&self, keyframes: &[Vec<Timestamp>]) -> Vec<KeyframeIssue> {
        let mut issues = Vec::new();
        for (r, at) in self.export_ranges_with_start() {
            let Some(kfs) = keyframes.get(r.source.0).filter(|k| !k.is_empty()) else {
                continue;
            };
            let Some(fps) = self.source(r.source).map(|s| s.fps) else {
                continue;
            };
            let frame = fps.nearest_frame(r.start);
            if kfs.iter().any(|&k| fps.nearest_frame(k) == frame) {
                continue;
            }
            let lo = self.snap_lower_bound(at, r.start);
            let snap_to = kfs
                .iter()
                .copied()
                .filter(|&k| k >= lo && k < r.end)
                .min_by_key(|&k| (k - r.start).as_micros().abs())
                .map(|k| at + (k - r.start));
            issues.push(KeyframeIssue { at, snap_to });
        }
        issues
    }

    /// 区間開始（タイムライン時刻 at・素材時刻 src）を前へ寄せられる下限（素材時刻）。
    /// 開始がセグメント境界で、直前が同じ素材の連続した削除済みセグメントならその先頭まで
    /// （削除を一部取り消して寄せる）。それ以外は開始を含むセグメントの先頭まで。
    /// 開始が IN 点でない（IN が削除部分の中にある）場合、IN より前には寄せない。
    fn snap_lower_bound(&self, at: Timestamp, src: Timestamp) -> Timestamp {
        let Some((idx, _)) = self.locate(at) else {
            return src;
        };
        let seg = &self.segments[idx];
        let seg_start = self.segment_range(idx).map_or(at, |(s, _)| s);
        let mut lo = seg.start;
        if at == seg_start
            && let Some(prev) = idx.checked_sub(1).map(|i| &self.segments[i])
            && !prev.enabled
            && prev.source == seg.source
            && prev.end == seg.start
        {
            lo = prev.start;
        }
        if let Some(i) = self.in_point.filter(|&i| i < at) {
            lo = lo.max(src - (at - i));
        }
        lo
    }

    /// 書き出し区間の開始を最寄りのキーフレームへ寄せる。
    /// 開始が IN 点なら IN を動かし、後ろへ寄せる場合はその間を削除、
    /// 前へ寄せる場合は直前の削除済み部分を復元する。何か変化した場合 true。
    pub fn snap_to_keyframes(&mut self, keyframes: &[Vec<Timestamp>]) -> bool {
        let mut changed = false;
        // 後ろの区間から処理する（削除・復元してもタイムライン時刻はずれない）
        for issue in self.keyframe_issues(keyframes).into_iter().rev() {
            let Some(to) = issue.snap_to else { continue };
            let at = issue.at;
            let by_in = self.in_point == Some(at);
            if by_in {
                self.set_in(to);
                changed = true;
            }
            if to > at && !by_in {
                changed |= self.delete_range(at, to);
            } else if to < at {
                let seg_start = self
                    .locate(at)
                    .and_then(|(i, _)| self.segment_range(i))
                    .map_or(at, |(s, _)| s);
                if to < seg_start {
                    changed |= self.restore_range(to, seg_start);
                }
            }
        }
        changed
    }
}

/// 書き出し区間の開始がキーフレームでない箇所
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyframeIssue {
    /// 区間開始のタイムライン時刻
    pub at: Timestamp,
    /// 寄せ先の最寄りキーフレーム（タイムライン時刻）。区間内・直前の削除部分に無ければ None
    pub snap_to: Option<Timestamp>,
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
    fn snap_to_frame_uses_segment_source() {
        // 30fps: 1フレーム = 33333.3us（開始時刻は切り上げ）
        let mut tl = single(10);
        assert_eq!(tl.snap_to_frame(Timestamp(40_000)), Timestamp(33_334));
        assert_eq!(tl.snap_to_frame(Timestamp(10_000)), Timestamp(0));
        // 後ろのセグメントは素材時刻で丸めてからタイムライン時刻に戻す
        tl.split_at(ts(4));
        tl.segments.swap(0, 1);
        // タイムライン [0,6) = 素材 [4,10)
        assert_eq!(tl.snap_to_frame(Timestamp(40_000)), Timestamp(33_334));
        // 範囲外はそのまま
        assert_eq!(tl.snap_to_frame(ts(20)), ts(20));
        assert_eq!(tl.snap_to_frame(ts(10)), ts(10));
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
    fn restore_range_reenables_part() {
        let mut tl = single(10);
        tl.delete_range(ts(2), ts(8));
        assert!(tl.restore_range(ts(6), ts(8)));
        assert_eq!(tl.export_ranges(), vec![range(0, 0, 2), range(0, 6, 10)]);
        assert!(!tl.restore_range(ts(0), ts(2)), "既に有効");
        assert_invariants(&tl);
    }

    fn ms(v: i64) -> Timestamp {
        Timestamp::from_millis(v)
    }

    /// キーフレーム 0, 2, 4, 6, 8 秒（素材 0）
    fn kf_every_2s() -> Vec<Vec<Timestamp>> {
        vec![(0..5).map(|i| ts(i * 2)).collect()]
    }

    #[test]
    fn keyframe_issues_detects_non_keyframe_starts() {
        let mut tl = single(10);
        let kfs = kf_every_2s();
        assert!(tl.keyframe_issues(&kfs).is_empty(), "先頭はキーフレーム");
        // 未取得なら判定しない
        tl.delete_range(ts(0), ms(3200));
        assert!(tl.keyframe_issues(&[]).is_empty());
        assert!(tl.keyframe_issues(&[Vec::new()]).is_empty());
        // 3.2 秒開始 → 最寄りは 4 秒
        assert_eq!(
            tl.keyframe_issues(&kfs),
            vec![KeyframeIssue {
                at: ms(3200),
                snap_to: Some(ts(4)),
            }]
        );
        // 同じフレーム内の誤差（30fps で 1us ずれ）はキーフレーム上とみなす
        let mut tl = single(10);
        tl.delete_range(ts(0), ts(2));
        assert!(tl.keyframe_issues(&[vec![Timestamp(1_999_999)]]).is_empty());
    }

    #[test]
    fn snap_forward_deletes_gap() {
        let mut tl = single(10);
        tl.delete_range(ts(0), ms(3200));
        assert!(tl.snap_to_keyframes(&kf_every_2s()));
        assert_eq!(tl.export_ranges(), vec![range(0, 4, 10)]);
        assert!(tl.keyframe_issues(&kf_every_2s()).is_empty());
        assert_invariants(&tl);
    }

    #[test]
    fn snap_backward_restores_deleted_part() {
        let mut tl = single(10);
        tl.delete_range(ts(0), ms(2600));
        assert!(tl.snap_to_keyframes(&kf_every_2s()));
        assert_eq!(tl.export_ranges(), vec![range(0, 2, 10)]);
        assert_invariants(&tl);
    }

    #[test]
    fn snap_moves_in_point() {
        let mut tl = single(10);
        tl.set_in(ms(4600));
        assert_eq!(
            tl.keyframe_issues(&kf_every_2s()),
            vec![KeyframeIssue {
                at: ms(4600),
                snap_to: Some(ts(4)),
            }]
        );
        assert!(tl.snap_to_keyframes(&kf_every_2s()));
        assert_eq!(tl.in_point, Some(ts(4)));
        assert_eq!(tl.segments.len(), 1, "分割しない");
        assert_eq!(tl.export_ranges(), vec![range(0, 4, 10)]);
    }

    #[test]
    fn snap_does_not_go_before_in_point() {
        // IN が削除区間内 → 書き出しは削除の直後から。IN より前には寄せない
        let mut tl = single(10);
        tl.delete_range(ts(0), ts(3));
        tl.set_in(ts(1));
        let kfs = vec![vec![ts(0), ms(2500), ts(5)]];
        assert_eq!(
            tl.keyframe_issues(&kfs),
            vec![KeyframeIssue {
                at: ts(3),
                snap_to: Some(ms(2500)),
            }]
        );
        assert!(tl.snap_to_keyframes(&kfs));
        assert_eq!(tl.in_point, Some(ts(1)));
        assert_eq!(
            tl.export_ranges(),
            vec![ExportRange {
                source: SourceId(0),
                start: ms(2500),
                end: ts(10),
            }]
        );
        // 削除部分のキーフレームが IN より前なら使わず、後ろのキーフレームへ寄せる
        let mut tl = single(10);
        tl.delete_range(ts(0), ts(3));
        tl.set_in(ts(1));
        let kfs = vec![vec![ts(0), ms(500), ts(9)]];
        assert_eq!(tl.keyframe_issues(&kfs)[0].snap_to, Some(ts(9)));
    }

    #[test]
    fn snap_none_when_no_keyframe_in_reach() {
        // 区間 [3, 5) の範囲にも直前の削除部分 [0, 3) にも使えるキーフレームが無い
        let mut tl = single(10);
        tl.delete_range(ts(0), ts(3));
        tl.set_out(ts(5));
        tl.set_in(ms(3500));
        let kfs = vec![vec![ts(0), ts(6)]];
        assert_eq!(
            tl.keyframe_issues(&kfs),
            vec![KeyframeIssue {
                at: ms(3500),
                snap_to: None,
            }]
        );
        assert!(!tl.snap_to_keyframes(&kfs));
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
