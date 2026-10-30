//! ffmpeg で生成したテスト動画を実際に書き出して尺を検証する（ffmpeg が無ければスキップ）

mod common;

use std::path::Path;
use std::process::Command;

use common::{TestDir, make_video, open, tools};
use edit_video::export::{ExportEvent, ExportJob, ExportMode, ExportSettings, Quality};
use edit_video::media::ffmpeg::FfmpegPaths;
use edit_video::media::probe;
use edit_video::model::Timestamp;
use edit_video::model::timeline::Timeline;

/// 30fps の1フレーム
const FRAME_US: i64 = 33_334;

/// ffprobe で映像ストリームの尺を得る
fn video_duration(ff: &FfmpegPaths, file: &Path) -> Timestamp {
    let out = Command::new(&ff.ffprobe)
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=duration", "-of", "csv=p=0"])
        .arg(file)
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    let secs: f64 = s
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("尺を読めません: {s:?}"));
    Timestamp((secs * 1e6).round() as i64)
}

fn export(ff: &FfmpegPaths, tl: &Timeline, mode: ExportMode, output: &Path) -> Vec<f32> {
    let settings = ExportSettings {
        mode,
        quality: Quality::Light,
        output: output.to_path_buf(),
    };
    let job = ExportJob::start(&ff.ffmpeg, tl, settings, || {}).unwrap();
    // 進捗を集めつつ完了を待つ
    let mut progress = Vec::new();
    let last = loop {
        match job.try_recv() {
            Some(ExportEvent::Progress(p)) => progress.push(p),
            Some(ev) => break ev,
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    };
    assert_eq!(last, ExportEvent::Finished(output.to_path_buf()));
    assert!(output.is_file());
    progress
}

fn assert_near(actual: Timestamp, expected: Timestamp, tol_us: i64) {
    let diff = (actual.as_micros() - expected.as_micros()).abs();
    assert!(
        diff <= tol_us,
        "尺 {actual:?} が期待値 {expected:?} から {diff}us ずれています"
    );
    eprintln!(
        "尺: {:.6} 秒（期待 {:.6}）",
        actual.as_secs_f64(),
        expected.as_secs_f64()
    );
}

#[test]
fn accurate_delete_middle() {
    let Some(ff) = tools() else { return };
    let dir = TestDir::new("accurate");
    let src = dir.0.join("src.mp4");
    make_video(&ff, &src, "320x240", 30, 10, true);

    let mut tl = open(&ff, &[&src]);
    tl.delete_range(Timestamp::from_secs(2), Timestamp::from_secs(4));
    let out = dir.0.join("out.mp4");
    let progress = export(&ff, &tl, ExportMode::Accurate, &out);
    assert_eq!(progress.last().copied(), Some(1.0));
    assert!(progress.windows(2).all(|w| w[0] <= w[1]), "{progress:?}");
    assert_near(video_duration(&ff, &out), Timestamp::from_secs(8), FRAME_US);
}

#[test]
fn accurate_in_out() {
    let Some(ff) = tools() else { return };
    let dir = TestDir::new("inout");
    let src = dir.0.join("src.mp4");
    make_video(&ff, &src, "320x240", 30, 10, true);

    let mut tl = open(&ff, &[&src]);
    tl.set_in(Timestamp::from_millis(1_500));
    tl.set_out(Timestamp::from_millis(4_500));
    let out = dir.0.join("out.mp4");
    export(&ff, &tl, ExportMode::Accurate, &out);
    assert_near(video_duration(&ff, &out), Timestamp::from_secs(3), FRAME_US);
}

#[test]
fn accurate_mixed_sources() {
    // 解像度・fps 違い + 音声なし素材の混在
    let Some(ff) = tools() else { return };
    let dir = TestDir::new("mixed");
    let a = dir.0.join("a.mp4");
    let b = dir.0.join("b.mp4");
    make_video(&ff, &a, "320x240", 30, 3, true);
    make_video(&ff, &b, "256x144", 25, 2, false);

    let tl = open(&ff, &[&a, &b]);
    let out = dir.0.join("out.mp4");
    export(&ff, &tl, ExportMode::Accurate, &out);
    assert_near(video_duration(&ff, &out), Timestamp::from_secs(5), FRAME_US);
    let info = probe::probe(&ff.ffprobe, &out).unwrap();
    assert_eq!((info.width, info.height), (320, 240));
    assert!(info.has_audio);
}

#[test]
fn fast_delete_middle() {
    let Some(ff) = tools() else { return };
    let dir = TestDir::new("fast");
    let src = dir.0.join("src.mp4");
    make_video(&ff, &src, "320x240", 30, 10, true);

    let mut tl = open(&ff, &[&src]);
    // キーフレーム（1秒ごと）に合わせた削除
    tl.delete_range(Timestamp::from_secs(2), Timestamp::from_secs(4));
    let out = dir.0.join("out.mp4");
    let progress = export(&ff, &tl, ExportMode::Fast, &out);
    assert_eq!(progress.last().copied(), Some(1.0));
    // キーフレーム単位なので許容幅は広め
    assert_near(video_duration(&ff, &out), Timestamp::from_secs(8), 300_000);
}

#[test]
fn cancel_removes_output() {
    let Some(ff) = tools() else { return };
    let dir = TestDir::new("cancel");
    let src = dir.0.join("src.mp4");
    make_video(&ff, &src, "320x240", 30, 10, true);

    let tl = open(&ff, &[&src]);
    let out = dir.0.join("out.mp4");
    let settings = ExportSettings {
        output: out.clone(),
        ..Default::default()
    };
    let job = ExportJob::start(&ff.ffmpeg, &tl, settings, || {}).unwrap();
    job.cancel();
    assert_eq!(job.wait(), ExportEvent::Cancelled);
    assert!(!out.exists());
}

#[test]
fn failure_reports_stderr() {
    let Some(ff) = tools() else { return };
    let dir = TestDir::new("fail");
    let src = dir.0.join("src.mp4");
    make_video(&ff, &src, "320x240", 30, 2, false);

    let tl = open(&ff, &[&src]);
    // 存在しないディレクトリへの出力は失敗する
    let out = dir.0.join("no_such_dir").join("out.mp4");
    let settings = ExportSettings {
        output: out.clone(),
        ..Default::default()
    };
    let job = ExportJob::start(&ff.ffmpeg, &tl, settings, || {}).unwrap();
    match job.wait() {
        ExportEvent::Failed(msg) => {
            assert!(msg.contains("ffmpeg が失敗しました"), "{msg}");
            assert!(msg.lines().count() > 1, "stderr が含まれていない: {msg}");
        }
        ev => panic!("失敗するはず: {ev:?}"),
    }
}
