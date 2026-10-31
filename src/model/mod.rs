//! 純粋ロジック（UI・ffmpeg に依存しない）

pub mod history;
pub mod time;
pub mod timeline;

use std::path::PathBuf;

pub use time::{Rational, Timestamp};

/// 素材ファイルの情報（ffprobe の結果）
#[derive(Clone, Debug, PartialEq)]
pub struct MediaInfo {
    pub path: PathBuf,
    pub duration: Timestamp,
    pub width: u32,
    pub height: u32,
    /// 例: 30000/1001
    pub fps: Rational,
    pub has_audio: bool,
    /// 映像コーデック名（ffprobe の codec_name。例: "h264"）
    pub video_codec: Option<String>,
    /// 音声コーデック名（音声なしなら None）
    pub audio_codec: Option<String>,
}
