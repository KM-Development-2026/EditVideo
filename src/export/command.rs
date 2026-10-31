//! 書き出し用 ffmpeg 引数の生成（純粋関数。ffmpeg は実行しない）

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};

use crate::model::time::MICROS_PER_SEC;
use crate::model::timeline::ExportRange;
use crate::model::{MediaInfo, Rational, Timestamp};

/// フィルタ文字列がこれより長ければファイル渡しにする
pub const FILTER_INLINE_MAX: usize = 4000;
/// コマンドライン長の上限（Windows の 32767 文字から実行ファイルパス分の余裕を引いた値）
pub const COMMAND_LINE_MAX: usize = 32767 - 1024;
/// 音声なし区間に埋める無音の形式
const SILENCE: &str = "anullsrc=r=48000:cl=stereo";

/// 書き出しモード
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExportMode {
    /// 再エンコード（フレーム単位で正確）
    #[default]
    Accurate,
    /// ストリームコピー（キーフレーム単位）
    Fast,
}

impl ExportMode {
    pub fn label(self) -> &'static str {
        match self {
            ExportMode::Accurate => "正確（再エンコード）",
            ExportMode::Fast => "高速（無劣化・キーフレーム単位）",
        }
    }
}

/// 画質プリセット（正確モードのみ有効）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Quality {
    High,
    #[default]
    Standard,
    Light,
}

impl Quality {
    pub fn crf(self) -> u32 {
        match self {
            Quality::High => 18,
            Quality::Standard => 20,
            Quality::Light => 23,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Quality::High => "高",
            Quality::Standard => "標準",
            Quality::Light => "軽量",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportSettings {
    pub mode: ExportMode,
    pub quality: Quality,
    pub output: PathBuf,
}

/// filter_complex の渡し方
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FilterGraph {
    /// コマンドラインに直接書く
    Inline(String),
    /// path に content を書き出し `-/filter_complex <path>` で渡す（runner が書き出す）
    Script { path: PathBuf, content: String },
}

/// 正確モード: 1回の ffmpeg 実行
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccuratePlan {
    /// ffmpeg の引数（filter 指定も含む完成形）
    pub args: Vec<OsString>,
    pub filter: FilterGraph,
}

/// 高速モードの区間1つ分（一時ファイルへのコピー）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FastPart {
    pub args: Vec<OsString>,
    pub path: PathBuf,
    pub duration: Timestamp,
}

/// 高速モード: 区間ごとのコピー → concat demuxer で連結
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FastPlan {
    pub parts: Vec<FastPart>,
    /// concat demuxer 用リストファイル（runner が書き出す）
    pub list_path: PathBuf,
    pub list_content: String,
    pub concat_args: Vec<OsString>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanKind {
    Accurate(AccuratePlan),
    Fast(FastPlan),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportPlan {
    pub kind: PlanKind,
    /// 書き出し総尺（進捗計算用）
    pub total: Timestamp,
    pub output: PathBuf,
}

/// 区間列から書き出し計画を作る。temp_dir は一時ファイル（フィルタ・リスト・中間ファイル）の置き場所
pub fn build(
    ranges: &[ExportRange],
    sources: &[MediaInfo],
    settings: &ExportSettings,
    temp_dir: &Path,
) -> Result<ExportPlan> {
    if ranges.is_empty() {
        bail!("書き出す区間がありません");
    }
    let mut used = Vec::with_capacity(ranges.len());
    for r in ranges {
        let info = sources
            .get(r.source.0)
            .ok_or_else(|| anyhow!("素材 {} が見つかりません", r.source.0))?;
        if r.start < Timestamp::ZERO || r.start >= r.end {
            bail!("不正な区間です: {}–{}", r.start, r.end);
        }
        used.push(info);
    }
    let total = ranges
        .iter()
        .fold(Timestamp::ZERO, |acc, r| acc + (r.end - r.start));
    let kind = match settings.mode {
        ExportMode::Accurate => PlanKind::Accurate(accurate(ranges, &used, settings, temp_dir)?),
        ExportMode::Fast => PlanKind::Fast(fast(ranges, &used, settings, temp_dir)?),
    };
    Ok(ExportPlan {
        kind,
        total,
        output: settings.output.clone(),
    })
}

fn accurate(
    ranges: &[ExportRange],
    used: &[&MediaInfo],
    settings: &ExportSettings,
    temp_dir: &Path,
) -> Result<AccuratePlan> {
    let mut inputs: Vec<OsString> = vec!["-hide_banner".into()];
    for (r, info) in ranges.iter().zip(used) {
        inputs.extend(input_args(r));
        inputs.push(info.path.clone().into());
    }

    let filter = accurate_filter(ranges, used);
    let has_audio = used.iter().any(|i| i.has_audio);

    let mut outputs: Vec<OsString> = Vec::new();
    outputs.extend(["-map", "[v]"].map(OsString::from));
    if has_audio {
        outputs.extend(["-map", "[a]"].map(OsString::from));
    }
    let crf = settings.quality.crf().to_string();
    outputs.extend(
        [
            "-c:v", "libx264", "-preset", "medium", "-crf", &crf, "-pix_fmt", "yuv420p",
        ]
        .map(OsString::from),
    );
    if has_audio {
        outputs.extend(["-c:a", "aac"].map(OsString::from));
    }
    outputs.extend(tail_args(&settings.output));

    let assemble = |filter_args: [OsString; 2]| -> Vec<OsString> {
        let mut v = inputs.clone();
        v.extend(filter_args);
        v.extend(outputs.iter().cloned());
        v
    };

    let inline = assemble(["-filter_complex".into(), filter.clone().into()]);
    if filter.len() <= FILTER_INLINE_MAX && command_line_len(&inline) <= COMMAND_LINE_MAX {
        return Ok(AccuratePlan {
            args: inline,
            filter: FilterGraph::Inline(filter),
        });
    }

    let path = temp_dir.join("filter.txt");
    let args = assemble(["-/filter_complex".into(), path.clone().into()]);
    if command_line_len(&args) > COMMAND_LINE_MAX {
        bail!(
            "区間数（{}）が多すぎてコマンドラインの長さ制限を超えます",
            ranges.len()
        );
    }
    Ok(AccuratePlan {
        args,
        filter: FilterGraph::Script {
            path,
            content: filter,
        },
    })
}

/// 正確モードの filter_complex 文字列
fn accurate_filter(ranges: &[ExportRange], used: &[&MediaInfo]) -> String {
    let first = used[0];
    // yuv420p は偶数サイズが必要なので揃え先は偶数に切り下げる
    let (w, h, fps) = (first.width & !1, first.height & !1, first.fps);
    let normalize = used
        .iter()
        .any(|i| i.width != w || i.height != h || i.fps != fps);
    let has_audio = used.iter().any(|i| i.has_audio);

    let mut chains = Vec::new();
    let mut concat_in = String::new();
    for (i, (r, info)) in ranges.iter().zip(used).enumerate() {
        if normalize {
            chains.push(format!(
                "[{i}:V]scale={w}:{h}:force_original_aspect_ratio=decrease,\
                 pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={}[v{i}]",
                fps_str(fps)
            ));
            concat_in.push_str(&format!("[v{i}]"));
        } else {
            concat_in.push_str(&format!("[{i}:V]"));
        }
        if has_audio {
            if info.has_audio {
                concat_in.push_str(&format!("[{i}:a]"));
            } else {
                // 音声なし素材の区間は区間長の無音で埋める
                chains.push(format!(
                    "{SILENCE}:d={}[s{i}]",
                    secs_string(r.end - r.start)
                ));
                concat_in.push_str(&format!("[s{i}]"));
            }
        }
    }
    let (a, out) = if has_audio { (1, "[v][a]") } else { (0, "[v]") };
    chains.push(format!(
        "{concat_in}concat=n={}:v=1:a={a}{out}",
        ranges.len()
    ));
    chains.join(";")
}

fn fast(
    ranges: &[ExportRange],
    used: &[&MediaInfo],
    settings: &ExportSettings,
    temp_dir: &Path,
) -> Result<FastPlan> {
    let first = used[0];
    if let Some(other) = used.iter().find(|i| !fast_compatible(first, i)) {
        bail!(
            "高速モードは形式（コーデック・解像度・fps・音声有無）が同じ素材のみ連結できます: {} と {}",
            first.path.display(),
            other.path.display()
        );
    }
    let ext = settings
        .output
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty())
        .unwrap_or("mp4")
        .to_ascii_lowercase();

    let mut parts = Vec::with_capacity(ranges.len());
    let mut list_content = String::from("ffconcat version 1.0\n");
    for (i, (r, info)) in ranges.iter().zip(used).enumerate() {
        let name = format!("part_{i:04}.{ext}");
        let path = temp_dir.join(&name);
        let mut args: Vec<OsString> = vec!["-hide_banner".into()];
        args.extend(input_args(r));
        args.push(info.path.clone().into());
        // 添付画像・字幕・データストリームは連結時に失敗しやすいので映像と音声だけ
        args.extend(
            [
                "-map",
                "0:V",
                "-map",
                "0:a?",
                "-c",
                "copy",
                "-avoid_negative_ts",
                "make_zero",
                "-progress",
                "pipe:1",
                "-nostats",
                "-y",
            ]
            .map(OsString::from),
        );
        args.push(path.clone().into());
        // リストファイルからの相対パス（同じディレクトリに置く）
        list_content.push_str(&format!("file {}\n", concat_quote(&name)));
        parts.push(FastPart {
            args,
            path,
            duration: r.end - r.start,
        });
    }

    let list_path = temp_dir.join("list.txt");
    let mut concat_args: Vec<OsString> = ["-hide_banner", "-f", "concat", "-safe", "0", "-i"]
        .map(OsString::from)
        .to_vec();
    concat_args.push(list_path.clone().into());
    concat_args.extend(["-map", "0", "-c", "copy"].map(OsString::from));
    concat_args.extend(tail_args(&settings.output));

    Ok(FastPlan {
        parts,
        list_path,
        list_content,
        concat_args,
    })
}

fn fast_compatible(a: &MediaInfo, b: &MediaInfo) -> bool {
    a.width == b.width
        && a.height == b.height
        && a.fps == b.fps
        && a.has_audio == b.has_audio
        && a.video_codec == b.video_codec
        && a.audio_codec == b.audio_codec
}

/// `-ss a -to b -i`（入力パスは呼び出し側で追加）
fn input_args(r: &ExportRange) -> [OsString; 5] {
    [
        "-ss".into(),
        secs_string(r.start).into(),
        "-to".into(),
        secs_string(r.end).into(),
        "-i".into(),
    ]
}

/// 出力共通の末尾引数
fn tail_args(output: &Path) -> Vec<OsString> {
    let mut v: Vec<OsString> = [
        "-movflags",
        "+faststart",
        "-progress",
        "pipe:1",
        "-nostats",
        "-y",
    ]
    .map(OsString::from)
    .to_vec();
    v.push(output.into());
    v
}

/// マイクロ秒精度の秒文字列（例 "12.345678"。浮動小数を通さない）
pub fn secs_string(t: Timestamp) -> String {
    let us = t.as_micros();
    let sign = if us < 0 { "-" } else { "" };
    let abs = us.unsigned_abs();
    let per = MICROS_PER_SEC as u64;
    format!("{sign}{}.{:06}", abs / per, abs % per)
}

fn fps_str(fps: Rational) -> String {
    format!("{}/{}", fps.num, fps.den)
}

/// concat demuxer のリスト用にシングルクォートで囲む（' は '\'' に置換）
pub fn concat_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Windows のコマンドライン長の見積もり（引用符・区切り・エスケープ分を多めに数える）
pub fn command_line_len(args: &[OsString]) -> usize {
    args.iter()
        .map(|a| {
            let s = a.to_string_lossy();
            s.len() + 3 + s.chars().filter(|&c| c == '"' || c == '\\').count()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::timeline::SourceId;

    fn ts(s: i64) -> Timestamp {
        Timestamp::from_secs(s)
    }

    fn info(name: &str, w: u32, h: u32, fps: i64, audio: bool) -> MediaInfo {
        MediaInfo {
            path: PathBuf::from(name),
            duration: ts(600),
            width: w,
            height: h,
            fps: Rational::new(fps, 1).unwrap(),
            has_audio: audio,
            video_codec: Some("h264".into()),
            audio_codec: audio.then(|| "aac".into()),
        }
    }

    fn range(source: usize, a: Timestamp, b: Timestamp) -> ExportRange {
        ExportRange {
            source: SourceId(source),
            start: a,
            end: b,
        }
    }

    fn settings(mode: ExportMode) -> ExportSettings {
        ExportSettings {
            mode,
            quality: Quality::default(),
            output: PathBuf::from(r"C:\out\out.mp4"),
        }
    }

    fn strs(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn accurate_plan(ranges: &[ExportRange], sources: &[MediaInfo]) -> (ExportPlan, AccuratePlan) {
        let plan = build(
            ranges,
            sources,
            &settings(ExportMode::Accurate),
            Path::new(r"C:\tmp\job"),
        )
        .unwrap();
        let PlanKind::Accurate(a) = plan.kind.clone() else {
            panic!("accurate expected");
        };
        (plan, a)
    }

    fn inline_filter(a: &AccuratePlan) -> &str {
        match &a.filter {
            FilterGraph::Inline(f) => f,
            FilterGraph::Script { .. } => panic!("inline expected"),
        }
    }

    #[test]
    fn defaults() {
        assert_eq!(ExportMode::default(), ExportMode::Accurate);
        assert_eq!(Quality::default(), Quality::Standard);
        assert_eq!(
            [Quality::High, Quality::Standard, Quality::Light].map(Quality::crf),
            [18, 20, 23]
        );
    }

    #[test]
    fn secs_string_cases() {
        assert_eq!(secs_string(Timestamp(0)), "0.000000");
        assert_eq!(secs_string(Timestamp(12_345_678)), "12.345678");
        assert_eq!(secs_string(Timestamp(1)), "0.000001");
        assert_eq!(secs_string(ts(3600)), "3600.000000");
        assert_eq!(secs_string(Timestamp(-1_500_000)), "-1.500000");
        // 29.97fps のフレーム境界も丸めずそのまま
        let t = Rational::new(30000, 1001).unwrap().frame_start(1);
        assert_eq!(secs_string(t), "0.033367");
    }

    #[test]
    fn accurate_single_range_with_audio() {
        let src = [info(r"C:\v\in.mp4", 1920, 1080, 30, true)];
        let (plan, a) = accurate_plan(&[range(0, Timestamp(1_500_000), ts(5))], &src);
        assert_eq!(plan.total, Timestamp(3_500_000));
        assert_eq!(plan.output, PathBuf::from(r"C:\out\out.mp4"));
        assert_eq!(inline_filter(&a), "[0:V][0:a]concat=n=1:v=1:a=1[v][a]");
        let expected = [
            "-hide_banner",
            "-ss",
            "1.500000",
            "-to",
            "5.000000",
            "-i",
            r"C:\v\in.mp4",
            "-filter_complex",
            "[0:V][0:a]concat=n=1:v=1:a=1[v][a]",
            "-map",
            "[v]",
            "-map",
            "[a]",
            "-c:v",
            "libx264",
            "-preset",
            "medium",
            "-crf",
            "20",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-movflags",
            "+faststart",
            "-progress",
            "pipe:1",
            "-nostats",
            "-y",
            r"C:\out\out.mp4",
        ];
        assert_eq!(strs(&a.args), expected);
    }

    #[test]
    fn accurate_multiple_ranges() {
        let src = [info("in.mp4", 1920, 1080, 30, true)];
        let (plan, a) = accurate_plan(&[range(0, ts(0), ts(5)), range(0, ts(10), ts(20))], &src);
        assert_eq!(plan.total, ts(15));
        assert_eq!(
            inline_filter(&a),
            "[0:V][0:a][1:V][1:a]concat=n=2:v=1:a=1[v][a]"
        );
        let args = strs(&a.args);
        assert_eq!(
            &args[1..13],
            [
                "-ss",
                "0.000000",
                "-to",
                "5.000000",
                "-i",
                "in.mp4",
                "-ss",
                "10.000000",
                "-to",
                "20.000000",
                "-i",
                "in.mp4"
            ]
        );
    }

    #[test]
    fn accurate_no_audio() {
        let src = [info("in.mp4", 640, 480, 25, false)];
        let (_, a) = accurate_plan(&[range(0, ts(0), ts(2)), range(0, ts(4), ts(6))], &src);
        assert_eq!(inline_filter(&a), "[0:V][1:V]concat=n=2:v=1:a=0[v]");
        let args = strs(&a.args);
        assert!(!args.iter().any(|s| s == "[a]" || s == "-c:a"));
        assert_eq!(args.iter().filter(|s| *s == "-map").count(), 1);
    }

    #[test]
    fn accurate_mixed_audio_fills_silence() {
        let src = [
            info("a.mp4", 1280, 720, 30, true),
            info("b.mp4", 1280, 720, 30, false),
        ];
        let (_, a) = accurate_plan(
            &[
                range(0, ts(0), ts(2)),
                range(1, ts(1), Timestamp(3_250_000)),
            ],
            &src,
        );
        assert_eq!(
            inline_filter(&a),
            "anullsrc=r=48000:cl=stereo:d=2.250000[s1];\
             [0:V][0:a][1:V][s1]concat=n=2:v=1:a=1[v][a]"
        );
        let args = strs(&a.args);
        assert!(args.windows(2).any(|w| w == ["-map", "[a]"]));
        // 無音は入力を増やさない
        assert_eq!(args.iter().filter(|s| *s == "-i").count(), 2);
    }

    #[test]
    fn accurate_different_resolution_normalizes_to_first() {
        let src = [
            info("a.mp4", 1280, 720, 30, true),
            info("b.mp4", 1920, 1080, 60, true),
        ];
        let (_, a) = accurate_plan(&[range(0, ts(0), ts(2)), range(1, ts(0), ts(2))], &src);
        let norm = |i: usize| {
            format!(
                "[{i}:V]scale=1280:720:force_original_aspect_ratio=decrease,\
                 pad=1280:720:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=30/1[v{i}]"
            )
        };
        assert_eq!(
            inline_filter(&a),
            format!(
                "{};{};[v0][0:a][v1][1:a]concat=n=2:v=1:a=1[v][a]",
                norm(0),
                norm(1)
            )
        );
    }

    #[test]
    fn accurate_odd_size_rounds_to_even() {
        let src = [info("a.mp4", 321, 241, 30, false)];
        let (_, a) = accurate_plan(&[range(0, ts(0), ts(2))], &src);
        let f = inline_filter(&a);
        assert!(f.starts_with("[0:V]scale=320:240:"), "{f}");
    }

    #[test]
    fn accurate_long_filter_uses_script() {
        let src = [info("in.mp4", 1920, 1080, 30, true)];
        let ranges: Vec<_> = (0..300)
            .map(|i| range(0, ts(i * 2), ts(i * 2 + 1)))
            .collect();
        let (plan, a) = accurate_plan(&ranges, &src);
        assert_eq!(plan.total, ts(300));
        let FilterGraph::Script { path, content } = &a.filter else {
            panic!("script expected");
        };
        assert_eq!(path, &PathBuf::from(r"C:\tmp\job\filter.txt"));
        assert!(content.len() > FILTER_INLINE_MAX);
        assert!(content.ends_with("concat=n=300:v=1:a=1[v][a]"));
        let args = strs(&a.args);
        let pos = args.iter().position(|s| s == "-/filter_complex").unwrap();
        assert_eq!(args[pos + 1], r"C:\tmp\job\filter.txt");
        assert!(!args.iter().any(|s| s == "-filter_complex"));
        assert!(command_line_len(&a.args) <= COMMAND_LINE_MAX);
        // 区間が少なければインライン
        let (_, a) = accurate_plan(&ranges[..3], &src);
        assert!(matches!(a.filter, FilterGraph::Inline(_)));
    }

    #[test]
    fn accurate_long_paths_use_script_or_error() {
        // フィルタは短くても入力パスが長いとコマンドライン全体が制限を超える
        let long = format!(r"C:\{}\in.mp4", "d".repeat(200));
        let src = [info(&long, 1920, 1080, 30, true)];
        let ranges: Vec<_> = (0..150)
            .map(|i| range(0, ts(i * 2), ts(i * 2 + 1)))
            .collect();
        let err = build(
            &ranges,
            &src,
            &settings(ExportMode::Accurate),
            Path::new("tmp"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("長さ制限"), "{err}");
    }

    #[test]
    fn empty_ranges_is_error() {
        let src = [info("in.mp4", 1920, 1080, 30, true)];
        for mode in [ExportMode::Accurate, ExportMode::Fast] {
            assert!(build(&[], &src, &settings(mode), Path::new("tmp")).is_err());
        }
    }

    #[test]
    fn invalid_source_or_range_is_error() {
        let src = [info("in.mp4", 1920, 1080, 30, true)];
        let s = settings(ExportMode::Accurate);
        assert!(build(&[range(1, ts(0), ts(1))], &src, &s, Path::new("tmp")).is_err());
        assert!(build(&[range(0, ts(2), ts(2))], &src, &s, Path::new("tmp")).is_err());
    }

    #[test]
    fn fast_plan_commands_and_list() {
        let src = [info(r"C:\v\in.mp4", 1920, 1080, 30, true)];
        let plan = build(
            &[
                range(0, ts(0), ts(5)),
                range(0, ts(10), Timestamp(12_500_000)),
            ],
            &src,
            &settings(ExportMode::Fast),
            Path::new(r"C:\tmp\job"),
        )
        .unwrap();
        assert_eq!(plan.total, Timestamp(7_500_000));
        let PlanKind::Fast(f) = plan.kind else {
            panic!("fast expected");
        };
        assert_eq!(f.parts.len(), 2);
        assert_eq!(f.parts[1].duration, Timestamp(2_500_000));
        assert_eq!(f.parts[1].path, PathBuf::from(r"C:\tmp\job\part_0001.mp4"));
        assert_eq!(
            strs(&f.parts[1].args),
            [
                "-hide_banner",
                "-ss",
                "10.000000",
                "-to",
                "12.500000",
                "-i",
                r"C:\v\in.mp4",
                "-map",
                "0:V",
                "-map",
                "0:a?",
                "-c",
                "copy",
                "-avoid_negative_ts",
                "make_zero",
                "-progress",
                "pipe:1",
                "-nostats",
                "-y",
                r"C:\tmp\job\part_0001.mp4",
            ]
        );
        assert_eq!(f.list_path, PathBuf::from(r"C:\tmp\job\list.txt"));
        assert_eq!(
            f.list_content,
            "ffconcat version 1.0\nfile 'part_0000.mp4'\nfile 'part_0001.mp4'\n"
        );
        assert_eq!(
            strs(&f.concat_args),
            [
                "-hide_banner",
                "-f",
                "concat",
                "-safe",
                "0",
                "-i",
                r"C:\tmp\job\list.txt",
                "-map",
                "0",
                "-c",
                "copy",
                "-movflags",
                "+faststart",
                "-progress",
                "pipe:1",
                "-nostats",
                "-y",
                r"C:\out\out.mp4",
            ]
        );
    }

    #[test]
    fn fast_uses_output_extension() {
        let src = [info("in.mkv", 1920, 1080, 30, true)];
        let mut s = settings(ExportMode::Fast);
        s.output = PathBuf::from("out.MKV");
        let plan = build(&[range(0, ts(0), ts(1))], &src, &s, Path::new("t")).unwrap();
        let PlanKind::Fast(f) = plan.kind else {
            panic!()
        };
        assert_eq!(f.parts[0].path, Path::new("t").join("part_0000.mkv"));
    }

    #[test]
    fn concat_quote_escapes_single_quote() {
        assert_eq!(concat_quote("a.mp4"), "'a.mp4'");
        assert_eq!(
            concat_quote(r"C:\Users\O'Brien\a.mp4"),
            r"'C:\Users\O'\''Brien\a.mp4'"
        );
        assert_eq!(concat_quote("''"), r"''\'''\'''");
    }

    #[test]
    fn fast_rejects_incompatible_sources() {
        let s = settings(ExportMode::Fast);
        let ranges = [range(0, ts(0), ts(1)), range(1, ts(0), ts(1))];
        let cases = [
            info("b.mp4", 1280, 720, 30, true),
            info("b.mp4", 1920, 1080, 60, true),
            info("b.mp4", 1920, 1080, 30, false),
            MediaInfo {
                video_codec: Some("hevc".into()),
                ..info("b.mp4", 1920, 1080, 30, true)
            },
        ];
        for b in cases {
            let src = [info("a.mp4", 1920, 1080, 30, true), b];
            assert!(build(&ranges, &src, &s, Path::new("t")).is_err());
        }
        // 同じ形式なら可
        let src = [
            info("a.mp4", 1920, 1080, 30, true),
            info("b.mp4", 1920, 1080, 30, true),
        ];
        assert!(build(&ranges, &src, &s, Path::new("t")).is_ok());
    }
}
