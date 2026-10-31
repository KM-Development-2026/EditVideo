//! キーフレーム取得とスナップの結合テスト（ffmpeg が無い環境ではスキップ）

mod common;

use common::{TestDir, make_video, open, tools};
use edit_video::media::probe;
use edit_video::model::Timestamp;

#[test]
fn probe_keyframes_and_snap() {
    let Some(ff) = tools() else { return };
    let dir = TestDir::new("keyframes");
    let src = dir.0.join("in.mp4");
    // キーフレームは1秒ごと
    make_video(&ff, &src, "320x240", 30, 5, false);

    let kfs = probe::probe_keyframes(&ff.ffprobe, &src).unwrap();
    let fps = edit_video::model::Rational::new(30, 1).unwrap();
    for s in 0..5 {
        let frame = fps.nearest_frame(Timestamp::from_secs(s));
        assert!(
            kfs.iter().any(|&k| fps.nearest_frame(k) == frame),
            "{s} 秒にキーフレームが無い: {kfs:?}"
        );
    }

    // 0〜1.4 秒を削除 → 区間開始 1.4 秒はキーフレームでない。最寄りは 1 秒
    let mut tl = open(&ff, &[&src]);
    tl.delete_range(Timestamp::ZERO, Timestamp::from_millis(1_400));
    let all = vec![kfs];
    let issues = tl.keyframe_issues(&all);
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].at, Timestamp::from_millis(1_400));
    let to = issues[0].snap_to.expect("寄せ先がある");
    assert_eq!(fps.nearest_frame(to), 30);

    assert!(tl.snap_to_keyframes(&all));
    assert!(tl.keyframe_issues(&all).is_empty());
    assert_eq!(tl.export_ranges()[0].start, to);
}
