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
    /// 高速モードのスナップ・警告用（遅延取得。未取得なら空）
    pub keyframes: Vec<Timestamp>,
}
