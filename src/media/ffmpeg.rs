//! ffmpeg / ffprobe の探索とプロセス起動の共通処理

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, anyhow};
use serde::Deserialize;

/// コンソール窓を出さない（Windows の CREATE_NO_WINDOW）
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 見つかった実行ファイルのパス
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FfmpegPaths {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

/// `%APPDATA%\EditVideo\config.json` の内容
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Config {
    /// ffmpeg.exe のパス
    pub ffmpeg_path: Option<PathBuf>,
    /// ffprobe.exe のパス
    pub ffprobe_path: Option<PathBuf>,
}

impl Config {
    pub fn parse(json: &str) -> Result<Self> {
        Ok(serde_json::from_str(json)?)
    }

    /// 設定ファイルを読む。存在しなければ既定値
    pub fn load() -> Result<Self> {
        let Some(path) = config_path() else {
            return Ok(Self::default());
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => Self::parse(&s)
                .map_err(|e| anyhow!("設定ファイル {} の読み込みに失敗: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(anyhow!("設定ファイル {} を開けません: {e}", path.display())),
        }
    }
}

/// 設定ファイルのパス（%APPDATA% が無ければ None）
pub fn config_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("EditVideo").join("config.json"))
}

/// 設定ファイル → PATH の順に ffmpeg / ffprobe を探す
pub fn locate() -> Result<FfmpegPaths> {
    let config = Config::load()?;
    let path_env = std::env::var_os("PATH");
    let path_env = path_env.as_deref();
    let ffmpeg = resolve(config.ffmpeg_path.as_deref(), "ffmpeg", path_env).ok_or_else(|| {
        anyhow!("ffmpeg が見つかりません。PATH に追加するか設定ファイルで指定してください")
    })?;
    let ffprobe =
        resolve(config.ffprobe_path.as_deref(), "ffprobe", path_env).ok_or_else(|| {
            anyhow!("ffprobe が見つかりません。PATH に追加するか設定ファイルで指定してください")
        })?;
    Ok(FfmpegPaths { ffmpeg, ffprobe })
}

/// 指定パスが存在すればそれを、無ければ PATH から name を探す
pub fn resolve(configured: Option<&Path>, name: &str, path_env: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(p) = configured.filter(|p| p.is_file()) {
        return Some(p.to_path_buf());
    }
    find_in_path(name, path_env?)
}

fn find_in_path(name: &str, path_env: &OsStr) -> Option<PathBuf> {
    let file_name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(path_env)
        .map(|dir| dir.join(&file_name))
        .find(|p| p.is_file())
}

/// コンソール窓を出さない設定済みの Command を作る
pub fn command(program: &Path) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config() {
        let c = Config::parse(r#"{"ffmpeg_path":"C:\\ff\\ffmpeg.exe"}"#).unwrap();
        assert_eq!(c.ffmpeg_path, Some(PathBuf::from(r"C:\ff\ffmpeg.exe")));
        assert_eq!(c.ffprobe_path, None);
        assert_eq!(Config::parse("{}").unwrap(), Config::default());
        assert!(Config::parse("not json").is_err());
    }

    #[test]
    fn resolve_prefers_existing_configured_path() {
        let dir = std::env::temp_dir().join(format!("edit_video_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(format!("fake{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&exe, b"").unwrap();

        // 設定パスが存在すればそれ
        assert_eq!(resolve(Some(&exe), "nope", None), Some(exe.clone()));
        // 設定パスが無ければ PATH から探す
        let path_env = std::env::join_paths([&dir]).unwrap();
        assert_eq!(
            resolve(Some(Path::new("Z:\\missing.exe")), "fake", Some(&path_env)),
            Some(exe.clone())
        );
        assert_eq!(resolve(None, "not_exist_cmd", Some(&path_env)), None);
        assert_eq!(resolve(None, "fake", None), None);

        std::fs::remove_dir_all(&dir).ok();
    }
}
