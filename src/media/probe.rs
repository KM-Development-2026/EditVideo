//! ffprobe によるメディア情報取得

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use super::ffmpeg;
use crate::model::time::MICROS_PER_SEC;
use crate::model::{MediaInfo, Rational, Timestamp};

#[derive(Deserialize)]
struct ProbeOutput {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    format: Option<ProbeFormat>,
}

#[derive(Deserialize)]
struct ProbeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    r_frame_rate: Option<String>,
    avg_frame_rate: Option<String>,
    duration: Option<String>,
    disposition: Option<Disposition>,
}

#[derive(Deserialize)]
struct Disposition {
    #[serde(default)]
    attached_pic: i32,
}

#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
}

/// ffprobe を実行して素材情報を得る
pub fn probe(ffprobe: &Path, file: &Path) -> Result<MediaInfo> {
    let output = ffmpeg::command(ffprobe)
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(file)
        .output()
        .with_context(|| format!("ffprobe を起動できません: {}", ffprobe.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "ffprobe が失敗しました（{}）: {}",
            output.status,
            stderr.trim()
        );
    }
    let json = String::from_utf8_lossy(&output.stdout);
    parse_probe_json(file, &json)
}

/// ffprobe の JSON 出力をパースする（純粋関数）
pub fn parse_probe_json(path: &Path, json: &str) -> Result<MediaInfo> {
    let out: ProbeOutput = serde_json::from_str(json).context("ffprobe の出力を解析できません")?;

    // カバー画像（attached_pic）は映像ストリームとして扱わない
    let video = out
        .streams
        .iter()
        .find(|s| {
            s.codec_type.as_deref() == Some("video")
                && s.disposition.as_ref().is_none_or(|d| d.attached_pic == 0)
        })
        .ok_or_else(|| anyhow!("映像ストリームがありません"))?;
    let audio = out
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("audio"));

    let fps = [&video.r_frame_rate, &video.avg_frame_rate]
        .into_iter()
        .filter_map(|r| r.as_deref().and_then(parse_rational))
        .find(|r| r.is_positive())
        .ok_or_else(|| anyhow!("フレームレートを取得できません"))?;

    let duration = out
        .format
        .as_ref()
        .and_then(|f| f.duration.as_deref())
        .and_then(parse_seconds)
        .or_else(|| video.duration.as_deref().and_then(parse_seconds))
        .ok_or_else(|| anyhow!("長さを取得できません"))?;

    Ok(MediaInfo {
        path: PathBuf::from(path),
        duration,
        width: video.width.unwrap_or(0),
        height: video.height.unwrap_or(0),
        fps,
        has_audio: audio.is_some(),
        video_codec: video.codec_name.clone(),
        audio_codec: audio.and_then(|a| a.codec_name.clone()),
    })
}

/// 映像のキーフレーム位置（素材時刻・昇順）を取得する。
/// パケット情報だけを読む（デコードしない）が、ファイル全体を読むので長尺では時間がかかる
pub fn probe_keyframes(ffprobe: &Path, file: &Path) -> Result<Vec<Timestamp>> {
    let output = ffmpeg::command(ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "V:0",
            "-show_entries",
            "format=start_time:packet=pts_time,flags",
            "-of",
            "csv",
        ])
        .arg(file)
        .output()
        .with_context(|| format!("ffprobe を起動できません: {}", ffprobe.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "キーフレームを取得できません（{}）: {}",
            output.status,
            stderr.trim()
        );
    }
    let keyframes = parse_keyframes_csv(&String::from_utf8_lossy(&output.stdout));
    if keyframes.is_empty() {
        bail!("キーフレームが見つかりません");
    }
    Ok(keyframes)
}

/// ffprobe の CSV（`packet,<pts_time>,<flags>` と `format,<start_time>`）からキーフレーム位置を得る。
/// ffmpeg の入力シークはファイルの開始時刻を基準にするので、start_time を引いて素材時刻にする
pub fn parse_keyframes_csv(csv: &str) -> Vec<Timestamp> {
    let mut start = Timestamp::ZERO;
    let mut pts = Vec::new();
    for line in csv.lines() {
        let mut fields = line.trim().split(',');
        match fields.next() {
            Some("packet") => {
                let (Some(t), Some(flags)) = (fields.next(), fields.next()) else {
                    continue;
                };
                if flags.contains('K')
                    && let Some(t) = parse_seconds(t)
                {
                    pts.push(t);
                }
            }
            Some("format") => {
                if let Some(t) = fields.next().and_then(parse_seconds) {
                    start = t;
                }
            }
            _ => {}
        }
    }
    let mut keyframes: Vec<Timestamp> = pts
        .into_iter()
        .map(|t| (t - start).max(Timestamp::ZERO))
        .collect();
    keyframes.sort();
    keyframes.dedup();
    keyframes
}

/// "30000/1001" や "25" をパース
fn parse_rational(s: &str) -> Option<Rational> {
    let (n, d) = s.split_once('/').unwrap_or((s, "1"));
    Rational::new(n.trim().parse().ok()?, d.trim().parse().ok()?)
}

/// "12.345678" 形式の秒をマイクロ秒に（浮動小数を経由しない。7桁目以降は切り捨て）
fn parse_seconds(s: &str) -> Option<Timestamp> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (int_part, frac_part) = s.split_once('.').unwrap_or((s, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    let all_digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    if !all_digits(int_part) || !all_digits(frac_part) {
        return None;
    }
    let secs: i64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().ok()?
    };
    let mut frac: i64 = 0;
    for (i, b) in frac_part.bytes().take(6).enumerate() {
        frac += (b - b'0') as i64 * 10i64.pow(5 - i as u32);
    }
    let us = secs.checked_mul(MICROS_PER_SEC)?.checked_add(frac)?;
    Some(Timestamp(if neg { -us } else { us }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<MediaInfo> {
        parse_probe_json(Path::new("in.mp4"), json)
    }

    #[test]
    fn parse_ntsc_with_audio() {
        let json = r#"{
            "streams": [
                {"index":0,"codec_type":"video","codec_name":"h264","width":1920,"height":1080,
                 "r_frame_rate":"30000/1001","avg_frame_rate":"30000/1001","duration":"60.026633",
                 "disposition":{"default":1,"attached_pic":0}},
                {"index":1,"codec_type":"audio","codec_name":"aac","sample_rate":"48000",
                 "r_frame_rate":"0/0","duration":"60.032000"}
            ],
            "format": {"filename":"in.mp4","duration":"60.032000","size":"123456"}
        }"#;
        let info = parse(json).unwrap();
        assert_eq!(info.path, PathBuf::from("in.mp4"));
        assert_eq!(info.width, 1920);
        assert_eq!(info.height, 1080);
        assert_eq!(info.fps, Rational::new(30000, 1001).unwrap());
        assert!(info.has_audio);
        assert_eq!(info.video_codec.as_deref(), Some("h264"));
        assert_eq!(info.audio_codec.as_deref(), Some("aac"));
        assert_eq!(info.duration, Timestamp(60_032_000));
    }

    #[test]
    fn parse_keyframes_subtracts_start_time() {
        // mpegts は開始時刻が 0 でない。パケットはデコード順なので並べ替える
        let csv = "packet,2.466667,K__,\n\
                   packet,1.466667,K__,\n\
                   packet,1.566667,___,\n\
                   packet,N/A,K__,\n\
                   packet,3.466667,K_D,\n\
                   format,1.466667\n";
        assert_eq!(
            parse_keyframes_csv(csv),
            vec![Timestamp(0), Timestamp(1_000_000), Timestamp(2_000_000)]
        );
        // 開始時刻が N/A なら 0 とみなす
        let csv = "packet,0.000000,K__\npacket,0.500000,K__\nformat,N/A\n";
        assert_eq!(
            parse_keyframes_csv(csv),
            vec![Timestamp(0), Timestamp(500_000)]
        );
        assert!(parse_keyframes_csv("").is_empty());
    }

    #[test]
    fn parse_no_audio() {
        let json = r#"{
            "streams": [{"codec_type":"video","width":640,"height":480,"r_frame_rate":"25/1"}],
            "format": {"duration":"5.000000"}
        }"#;
        let info = parse(json).unwrap();
        assert!(!info.has_audio);
        assert_eq!(info.video_codec, None);
        assert_eq!(info.audio_codec, None);
        assert_eq!(info.fps, Rational::new(25, 1).unwrap());
        assert_eq!(info.duration, Timestamp::from_secs(5));
    }

    #[test]
    fn duration_only_in_format() {
        // mkv / webm はストリームに duration が無いことが多い
        let json = r#"{
            "streams": [{"codec_type":"video","width":1280,"height":720,"r_frame_rate":"60/1"},
                        {"codec_type":"audio"}],
            "format": {"duration":"3723.5"}
        }"#;
        let info = parse(json).unwrap();
        assert_eq!(info.duration, Timestamp(3_723_500_000));
        assert_eq!(info.duration.to_hms(), "01:02:03.500");
    }

    #[test]
    fn duration_fallback_to_stream() {
        let json = r#"{
            "streams": [{"codec_type":"video","width":320,"height":240,"r_frame_rate":"24000/1001","duration":"1.001000"}],
            "format": {}
        }"#;
        let info = parse(json).unwrap();
        assert_eq!(info.duration, Timestamp(1_001_000));
    }

    #[test]
    fn fps_fallback_to_avg_frame_rate() {
        let json = r#"{
            "streams": [{"codec_type":"video","width":320,"height":240,"r_frame_rate":"0/0","avg_frame_rate":"15/1"}],
            "format": {"duration":"1"}
        }"#;
        assert_eq!(parse(json).unwrap().fps, Rational::new(15, 1).unwrap());
    }

    #[test]
    fn skips_attached_pic() {
        let json = r#"{
            "streams": [
                {"codec_type":"video","codec_name":"mjpeg","width":500,"height":500,"r_frame_rate":"90000/1",
                 "disposition":{"attached_pic":1}},
                {"codec_type":"video","width":1920,"height":1080,"r_frame_rate":"30/1"}
            ],
            "format": {"duration":"2.0"}
        }"#;
        let info = parse(json).unwrap();
        assert_eq!((info.width, info.height), (1920, 1080));
    }

    #[test]
    fn errors() {
        assert!(parse("not json").is_err());
        // 音声のみ
        let audio_only = r#"{"streams":[{"codec_type":"audio"}],"format":{"duration":"1.0"}}"#;
        assert!(parse(audio_only).is_err());
        // 長さ不明
        let no_dur = r#"{"streams":[{"codec_type":"video","r_frame_rate":"30/1"}],"format":{"duration":"N/A"}}"#;
        assert!(parse(no_dur).is_err());
        // fps 不明
        let no_fps = r#"{"streams":[{"codec_type":"video","r_frame_rate":"0/0"}],"format":{"duration":"1"}}"#;
        assert!(parse(no_fps).is_err());
    }

    #[test]
    fn parse_seconds_cases() {
        assert_eq!(parse_seconds("0"), Some(Timestamp(0)));
        assert_eq!(parse_seconds("1.5"), Some(Timestamp(1_500_000)));
        assert_eq!(parse_seconds("0.0333667"), Some(Timestamp(33_366)));
        assert_eq!(parse_seconds(".25"), Some(Timestamp(250_000)));
        assert_eq!(parse_seconds("-0.5"), Some(Timestamp(-500_000)));
        assert_eq!(parse_seconds("N/A"), None);
        assert_eq!(parse_seconds(""), None);
        assert_eq!(parse_seconds("1e3"), None);
    }

    #[test]
    fn parse_rational_cases() {
        assert_eq!(parse_rational("30000/1001"), Rational::new(30000, 1001));
        assert_eq!(parse_rational("25"), Rational::new(25, 1));
        assert_eq!(parse_rational("0/0"), None);
        assert_eq!(parse_rational("abc"), None);
    }
}
