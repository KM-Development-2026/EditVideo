//! Undo/Redo（Timeline のスナップショット方式）

use super::timeline::Timeline;

/// 既定の Undo 履歴上限
const DEFAULT_LIMIT: usize = 200;

#[derive(Debug)]
pub struct History {
    undo: Vec<Timeline>,
    redo: Vec<Timeline>,
    limit: usize,
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

impl History {
    pub fn new() -> Self {
        Self::with_limit(DEFAULT_LIMIT)
    }

    /// limit: 保持する Undo 段数（0 は 1 として扱う）
    pub fn with_limit(limit: usize) -> Self {
        History {
            undo: Vec::new(),
            redo: Vec::new(),
            limit: limit.max(1),
        }
    }

    /// 編集前のスナップショットを積む（Redo は破棄）
    pub fn push(&mut self, before: Timeline) {
        self.undo.push(before);
        if self.undo.len() > self.limit {
            let excess = self.undo.len() - self.limit;
            self.undo.drain(..excess);
        }
        self.redo.clear();
    }

    /// 編集操作 f を適用し、変化があれば（f が true を返せば）履歴に積む
    pub fn apply(
        &mut self,
        timeline: &mut Timeline,
        f: impl FnOnce(&mut Timeline) -> bool,
    ) -> bool {
        let before = timeline.clone();
        if f(timeline) {
            self.push(before);
            true
        } else {
            false
        }
    }

    /// 取り消し。できた場合 true
    pub fn undo(&mut self, current: &mut Timeline) -> bool {
        match self.undo.pop() {
            Some(prev) => {
                self.redo.push(std::mem::replace(current, prev));
                true
            }
            None => false,
        }
    }

    /// やり直し。できた場合 true
    pub fn redo(&mut self, current: &mut Timeline) -> bool {
        match self.redo.pop() {
            Some(next) => {
                self.undo.push(std::mem::replace(current, next));
                true
            }
            None => false,
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MediaInfo;
    use crate::model::time::{Rational, Timestamp};
    use std::path::PathBuf;

    fn timeline() -> Timeline {
        let mut tl = Timeline::new();
        tl.add_source(MediaInfo {
            path: PathBuf::from("a.mp4"),
            duration: Timestamp::from_secs(10),
            width: 640,
            height: 480,
            fps: Rational::new(30, 1).unwrap(),
            has_audio: false,
            video_codec: None,
            audio_codec: None,
            keyframes: Vec::new(),
        });
        tl
    }

    #[test]
    fn undo_redo_roundtrip() {
        let mut tl = timeline();
        let mut h = History::new();
        let s0 = tl.clone();
        assert!(h.apply(&mut tl, |t| t.split_at(Timestamp::from_secs(3))));
        let s1 = tl.clone();
        assert!(h.apply(&mut tl, |t| t.delete(0)));
        let s2 = tl.clone();

        assert!(h.undo(&mut tl));
        assert_eq!(tl, s1);
        assert!(h.undo(&mut tl));
        assert_eq!(tl, s0);
        assert!(!h.undo(&mut tl));
        assert_eq!(tl, s0);

        assert!(h.redo(&mut tl));
        assert_eq!(tl, s1);
        assert!(h.redo(&mut tl));
        assert_eq!(tl, s2);
        assert!(!h.redo(&mut tl));
    }

    #[test]
    fn noop_edit_is_not_recorded() {
        let mut tl = timeline();
        let mut h = History::new();
        assert!(!h.apply(&mut tl, |t| t.split_at(Timestamp::ZERO)));
        assert!(!h.can_undo());
    }

    #[test]
    fn new_edit_clears_redo() {
        let mut tl = timeline();
        let mut h = History::new();
        h.apply(&mut tl, |t| t.split_at(Timestamp::from_secs(3)));
        h.undo(&mut tl);
        assert!(h.can_redo());
        h.apply(&mut tl, |t| t.delete(0));
        assert!(!h.can_redo());
        assert!(h.can_undo());
    }

    #[test]
    fn limit_drops_oldest() {
        let mut tl = timeline();
        let mut h = History::with_limit(2);
        for s in 1..=3 {
            h.apply(&mut tl, |t| t.split_at(Timestamp::from_secs(s)));
        }
        assert_eq!(tl.segments.len(), 4);
        assert!(h.undo(&mut tl));
        assert!(h.undo(&mut tl));
        assert!(!h.undo(&mut tl));
        // 最古の1段は破棄されているので分割1回分が残る
        assert_eq!(tl.segments.len(), 2);
        h.clear();
        assert!(!h.can_undo() && !h.can_redo());
    }
}
