//! 結合テストの共通ヘルパー（テスト動画の生成など）
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use edit_video::media::ffmpeg::{self, FfmpegPaths};
use edit_video::media::probe;
use edit_video::model::timeline::Timeline;

/// テストごとの一時ディレクトリ（終了時に削除）
pub struct TestDir(pub PathBuf);

impl TestDir {
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("edit_video_it_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TestDir(dir)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// ffmpeg / ffprobe を探す。無ければ None（テストはスキップ）
pub fn tools() -> Option<FfmpegPaths> {
    match ffmpeg::locate() {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("ffmpeg が見つからないためスキップ: {e:#}");
            None
        }
    }
}

/// testsrc2（+ sine）から動画を作る。キーフレームは1秒ごと
pub fn make_video(ff: &FfmpegPaths, out: &Path, size: &str, rate: u32, secs: u32, audio: bool) {
    let mut cmd = Command::new(&ff.ffmpeg);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
    ])
    .arg(format!("testsrc2=size={size}:rate={rate}:duration={secs}"));
    if audio {
        cmd.args(["-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=440:duration={secs}"));
    }
    cmd.args([
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
    ])
    .args(["-g", &rate.to_string()]);
    if audio {
        cmd.args(["-c:a", "aac"]);
    }
    let status = cmd.arg(out).status().expect("ffmpeg を起動できません");
    assert!(status.success(), "テスト動画の生成に失敗");
}

/// ファイルを ffprobe してタイムラインを作る
pub fn open(ff: &FfmpegPaths, files: &[&Path]) -> Timeline {
    let mut tl = Timeline::new();
    for f in files {
        tl.add_source(probe::probe(&ff.ffprobe, f).unwrap());
    }
    tl
}
