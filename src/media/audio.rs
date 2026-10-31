//! 音声プレビュー（ffmpeg の PCM 出力を rodio で鳴らす）
//!
//! 鳴らした量（出力へ渡したサンプル数）を、削除済みセグメントを飛ばしたタイムライン時刻に換算して
//! 映像の基準クロックにする（音声クロックがマスター）。

use std::ffi::OsString;
use std::io::{ErrorKind, Read};
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};

use anyhow::{Context, Result};
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Source};

use super::ffmpeg;
use crate::export::command::secs_string;
use crate::model::Timestamp;
use crate::model::time::MICROS_PER_SEC;
use crate::model::timeline::Timeline;

/// 1回に渡す PCM のフレーム数（48kHz で約 43ms）
const CHUNK_FRAMES: usize = 2048;
/// 先読みする長さ（秒）。カット位置で ffmpeg を起動する間も途切れないように
const PREFETCH_SECS: usize = 2;

/// 鳴らす区間（タイムライン上で連続して再生される1セグメント分）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioPart {
    /// 区間先頭のタイムライン時刻
    pub at: Timestamp,
    /// 素材ファイル（音声が無い素材は None = 無音）
    pub file: Option<PathBuf>,
    /// 素材時刻の開始位置
    pub start: Timestamp,
    pub duration: Timestamp,
}

/// start（タイムライン時刻）から再生したときに鳴らす区間の列。削除済みセグメントは飛ばす。
/// start が削除済みセグメント内なら次の有効セグメントから
pub fn audio_parts(tl: &Timeline, start: Timestamp) -> Vec<AudioPart> {
    let mut parts = Vec::new();
    let mut seg_at = Timestamp::ZERO;
    for seg in &tl.segments {
        let seg_end = seg_at + seg.duration();
        if seg.enabled && seg_end > start {
            let at = seg_at.max(start);
            let info = tl.source(seg.source);
            parts.push(AudioPart {
                at,
                file: info.filter(|i| i.has_audio).map(|i| i.path.clone()),
                start: seg.start + (at - seg_at),
                duration: seg_end - at,
            });
        }
        seg_at = seg_end;
    }
    parts
}

/// 区間の列の先頭から played だけ鳴らしたときのタイムライン時刻（末尾を越えたら最後の区間の終端）
pub fn timeline_time(parts: &[AudioPart], played: Timestamp) -> Option<Timestamp> {
    let mut rest = played.max(Timestamp::ZERO);
    for p in parts {
        if rest < p.duration {
            return Some(p.at + rest);
        }
        rest -= p.duration;
    }
    parts.last().map(|p| p.at + p.duration)
}

/// 長さ d のフレーム数（四捨五入）
pub fn frames_for(d: Timestamp, rate: u32) -> usize {
    let n = d.as_micros().max(0) as i128 * i128::from(rate);
    ((n + i128::from(MICROS_PER_SEC) / 2) / i128::from(MICROS_PER_SEC)) as usize
}

/// 区間の音声を PCM（f32 リトルエンディアン・インターリーブ）で出力する ffmpeg の引数。
/// 音声が途中で尽きても apad で無音を足す（長さは読み出し側で区間長ちょうどに揃える）
pub fn pcm_args(file: &Path, start: Timestamp, rate: u32, channels: u16) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-ss"]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.push(secs_string(start).into());
    args.push("-i".into());
    args.push(file.into());
    for a in ["-map", "0:a:0", "-vn", "-sn", "-dn", "-af", "apad", "-ac"] {
        args.push(a.into());
    }
    args.push(channels.to_string().into());
    args.push("-ar".into());
    args.push(rate.to_string().into());
    for a in ["-f", "f32le", "pipe:1"] {
        args.push(a.into());
    }
    args
}

/// 出力デバイス（開いている間だけ音が出る）
pub struct AudioOutput {
    sink: MixerDeviceSink,
}

impl AudioOutput {
    /// 既定の出力デバイスを開く
    pub fn open() -> Result<Self> {
        let mut sink =
            DeviceSinkBuilder::open_default_sink().context("音声出力デバイスを開けません")?;
        sink.log_on_drop(false);
        Ok(Self { sink })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sink.config().sample_rate().get()
    }

    pub fn channels(&self) -> u16 {
        self.sink.config().channel_count().get()
    }

    /// timeline を start から鳴らし始める（返り値を破棄すると止まる）
    pub fn play(&self, ffmpeg_exe: &Path, tl: &Timeline, start: Timestamp) -> AudioPlayback {
        let (source, playback) =
            start_feed(ffmpeg_exe, tl, start, self.sample_rate(), self.channels());
        self.sink.mixer().add(source);
        playback
    }
}

/// start から鳴らす PCM の供給を始め、出力に渡す音源と再生の操作窓口を返す
/// （出力デバイスと切り離してあるのはテストで音源を直接読むため）
pub fn start_feed(
    ffmpeg_exe: &Path,
    tl: &Timeline,
    start: Timestamp,
    rate: u32,
    channels: u16,
) -> (PcmSource, AudioPlayback) {
    let parts = Arc::new(audio_parts(tl, start));
    let capacity = (rate as usize * PREFETCH_SECS / CHUNK_FRAMES).max(4);
    let (tx, rx) = mpsc::sync_channel(capacity);
    let consumed = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let source = PcmSource::new(rx, channels, rate, consumed.clone(), stop.clone());
    let feeder = Feeder {
        ffmpeg: ffmpeg_exe.to_path_buf(),
        parts: parts.clone(),
        rate,
        channels,
        tx,
        stop: stop.clone(),
    };
    std::thread::spawn(move || feeder.run());
    let playback = AudioPlayback {
        clock: AudioClock {
            parts,
            consumed,
            samples_per_sec: u64::from(rate) * u64::from(channels),
        },
        stop,
    };
    (source, playback)
}

/// 鳴らした量からタイムライン時刻を求める（映像の基準クロック）
#[derive(Clone)]
pub struct AudioClock {
    parts: Arc<Vec<AudioPart>>,
    /// 出力へ渡したサンプル数（全チャンネル分）
    consumed: Arc<AtomicU64>,
    samples_per_sec: u64,
}

impl AudioClock {
    /// 再生開始から鳴らした長さ
    pub fn played(&self) -> Timestamp {
        let n = self.consumed.load(Ordering::Relaxed) as u128 * MICROS_PER_SEC as u128;
        Timestamp((n / u128::from(self.samples_per_sec.max(1))) as i64)
    }

    /// 現在のタイムライン時刻
    pub fn now(&self) -> Timestamp {
        timeline_time(&self.parts, self.played()).unwrap_or(Timestamp::ZERO)
    }
}

/// 再生中の音声（破棄すると止まる）
pub struct AudioPlayback {
    clock: AudioClock,
    stop: Arc<AtomicBool>,
}

impl AudioPlayback {
    pub fn clock(&self) -> AudioClock {
        self.clock.clone()
    }
}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        // Source は次の読み出しで終わり、Feeder は送信失敗・停止フラグで終わる
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// 区間ごとに ffmpeg で PCM を作って送るスレッド
struct Feeder {
    ffmpeg: PathBuf,
    parts: Arc<Vec<AudioPart>>,
    rate: u32,
    channels: u16,
    tx: SyncSender<Vec<f32>>,
    stop: Arc<AtomicBool>,
}

impl Feeder {
    fn run(self) {
        for part in self.parts.iter() {
            let frames = frames_for(part.duration, self.rate);
            let ok = match &part.file {
                Some(file) => self.send_file(file, part.start, frames),
                None => self.send_silence(frames),
            };
            if !ok {
                return;
            }
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// 送れなくなった（停止・受信側の終了）なら false
    fn send(&self, chunk: Vec<f32>) -> bool {
        !self.stopped() && self.tx.send(chunk).is_ok()
    }

    fn send_silence(&self, frames: usize) -> bool {
        let ch = usize::from(self.channels);
        let mut left = frames;
        while left > 0 {
            let n = left.min(CHUNK_FRAMES);
            if !self.send(vec![0.0; n * ch]) {
                return false;
            }
            left -= n;
        }
        true
    }

    /// ffmpeg の出力をちょうど frames フレーム送る（足りなければ無音で埋める。起動失敗も無音）
    fn send_file(&self, file: &Path, start: Timestamp, frames: usize) -> bool {
        let child = ffmpeg::command(&self.ffmpeg)
            .args(pcm_args(file, start, self.rate, self.channels))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            return self.send_silence(frames);
        };
        let mut stdout = child.stdout.take().expect("stdout piped");
        let ch = usize::from(self.channels);
        let mut left = frames;
        let mut ok = true;
        while left > 0 {
            let n = left.min(CHUNK_FRAMES);
            let mut bytes = vec![0u8; n * ch * 4];
            // 読めなかった分は 0（= 無音の 0.0）のまま
            read_full(&mut stdout, &mut bytes);
            let chunk = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect();
            if !self.send(chunk) {
                ok = false;
                break;
            }
            left -= n;
        }
        let _ = child.kill();
        let _ = child.wait();
        ok
    }
}

/// buf をできるだけ埋める（終端・エラーで止まる）。読めたバイト数
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> usize {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    filled
}

/// 受け取った PCM を順に出す rodio の音源。
/// データが届いていない間は無音を出すが、鳴らした量には数えない（クロックが止まり映像も待つ）
pub struct PcmSource {
    rx: Receiver<Vec<f32>>,
    buf: Vec<f32>,
    pos: usize,
    /// データ不足で出している無音の残りサンプル数（フレーム単位で出してチャンネルをずらさない）
    silence_left: u16,
    channels: u16,
    rate: u32,
    consumed: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
}

impl PcmSource {
    pub fn new(
        rx: Receiver<Vec<f32>>,
        channels: u16,
        rate: u32,
        consumed: Arc<AtomicU64>,
        stop: Arc<AtomicBool>,
    ) -> Self {
        Self {
            rx,
            buf: Vec::new(),
            pos: 0,
            silence_left: 0,
            channels: channels.max(1),
            rate: rate.max(1),
            consumed,
            stop,
        }
    }
}

impl Iterator for PcmSource {
    type Item = rodio::Sample;

    fn next(&mut self) -> Option<rodio::Sample> {
        if self.stop.load(Ordering::Relaxed) {
            return None;
        }
        if self.silence_left > 0 {
            self.silence_left -= 1;
            return Some(0.0);
        }
        // チャンクはフレーム単位なので、ここはフレームの境目
        while self.pos >= self.buf.len() {
            match self.rx.try_recv() {
                Ok(chunk) => {
                    self.buf = chunk;
                    self.pos = 0;
                }
                Err(TryRecvError::Empty) => {
                    self.silence_left = self.channels - 1;
                    return Some(0.0);
                }
                Err(TryRecvError::Disconnected) => return None,
            }
        }
        let s = self.buf[self.pos];
        self.pos += 1;
        self.consumed.fetch_add(1, Ordering::Relaxed);
        Some(s as rodio::Sample)
    }
}

impl Source for PcmSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> rodio::ChannelCount {
        NonZero::new(self.channels).expect("channels > 0")
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        NonZero::new(self.rate).expect("rate > 0")
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MediaInfo;
    use crate::model::time::Rational;

    fn ts(s: i64) -> Timestamp {
        Timestamp::from_secs(s)
    }

    fn info(name: &str, secs: i64, has_audio: bool) -> MediaInfo {
        MediaInfo {
            path: PathBuf::from(name),
            duration: ts(secs),
            width: 640,
            height: 360,
            fps: Rational::new(30, 1).unwrap(),
            has_audio,
            video_codec: Some("h264".into()),
            audio_codec: has_audio.then(|| "aac".into()),
        }
    }

    fn part(at: i64, file: Option<&str>, start: i64, dur: i64) -> AudioPart {
        AudioPart {
            at: ts(at),
            file: file.map(PathBuf::from),
            start: ts(start),
            duration: ts(dur),
        }
    }

    #[test]
    fn parts_skip_disabled_and_start_mid_segment() {
        let mut tl = Timeline::new();
        tl.add_source(info("a.mp4", 10, true));
        tl.delete_range(ts(3), ts(6));
        assert_eq!(
            audio_parts(&tl, ts(1)),
            vec![part(1, Some("a.mp4"), 1, 2), part(6, Some("a.mp4"), 6, 4)]
        );
        // 削除区間内から始めると次の有効区間から
        assert_eq!(audio_parts(&tl, ts(4)), vec![part(6, Some("a.mp4"), 6, 4)]);
        assert!(audio_parts(&tl, ts(10)).is_empty());
    }

    #[test]
    fn parts_silence_for_source_without_audio() {
        let mut tl = Timeline::new();
        tl.add_source(info("a.mp4", 2, true));
        tl.add_source(info("b.mp4", 3, false));
        assert_eq!(
            audio_parts(&tl, ts(0)),
            vec![part(0, Some("a.mp4"), 0, 2), part(2, None, 0, 3)]
        );
    }

    #[test]
    fn timeline_time_maps_across_skips() {
        let parts = vec![part(1, None, 1, 2), part(6, None, 6, 4)];
        assert_eq!(timeline_time(&parts, ts(0)), Some(ts(1)));
        assert_eq!(
            timeline_time(&parts, Timestamp::from_millis(1_500)),
            Some(Timestamp::from_millis(2_500))
        );
        // 1つ目の区間を鳴らし終えたら削除区間を飛ばして 6 秒
        assert_eq!(timeline_time(&parts, ts(2)), Some(ts(6)));
        assert_eq!(timeline_time(&parts, ts(5)), Some(ts(9)));
        // 末尾を越えたら終端
        assert_eq!(timeline_time(&parts, ts(99)), Some(ts(10)));
        assert_eq!(timeline_time(&[], ts(1)), None);
    }

    #[test]
    fn frames_for_rounds() {
        assert_eq!(frames_for(ts(1), 48_000), 48_000);
        assert_eq!(frames_for(Timestamp(33_333), 48_000), 1_600);
        assert_eq!(frames_for(Timestamp(-5), 48_000), 0);
    }

    #[test]
    fn pcm_args_shape() {
        let args: Vec<String> = pcm_args(Path::new("in.mp4"), Timestamp(1_500_000), 48_000, 2)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let pos = |x: &str| args.iter().position(|a| a == x).unwrap();
        assert!(pos("-ss") < pos("-i"));
        assert_eq!(args[pos("-ss") + 1], "1.500000");
        assert_eq!(args[pos("-map") + 1], "0:a:0");
        assert_eq!(args[pos("-af") + 1], "apad");
        assert_eq!(args[pos("-ac") + 1], "2");
        assert_eq!(args[pos("-ar") + 1], "48000");
        assert_eq!(args[pos("-f") + 1], "f32le");
        assert_eq!(args.last().unwrap(), "pipe:1");
    }

    fn source(
        channels: u16,
    ) -> (
        SyncSender<Vec<f32>>,
        PcmSource,
        Arc<AtomicU64>,
        Arc<AtomicBool>,
    ) {
        let (tx, rx) = mpsc::sync_channel(8);
        let consumed = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let src = PcmSource::new(rx, channels, 48_000, consumed.clone(), stop.clone());
        (tx, src, consumed, stop)
    }

    #[test]
    fn source_counts_only_real_samples() {
        let (tx, mut src, consumed, _stop) = source(2);
        // データ不足: 1フレーム（2サンプル）の無音を出し、数えない
        assert_eq!(src.next(), Some(0.0));
        assert_eq!(src.next(), Some(0.0));
        assert_eq!(consumed.load(Ordering::Relaxed), 0);
        tx.send(vec![0.1, 0.2, 0.3, 0.4]).unwrap();
        let got: Vec<_> = (0..4).map(|_| src.next().unwrap()).collect();
        assert_eq!(got, vec![0.1, 0.2, 0.3, 0.4]);
        assert_eq!(consumed.load(Ordering::Relaxed), 4);
        // 送り手が終わったら終了
        drop(tx);
        assert_eq!(src.next(), None);
    }

    #[test]
    fn source_stops_on_flag() {
        let (tx, mut src, _consumed, stop) = source(2);
        tx.send(vec![0.5; 4]).unwrap();
        assert_eq!(src.next(), Some(0.5));
        stop.store(true, Ordering::Relaxed);
        assert_eq!(src.next(), None);
    }

    #[test]
    fn clock_converts_consumed_samples() {
        let clock = AudioClock {
            parts: Arc::new(vec![part(1, None, 1, 2), part(6, None, 6, 4)]),
            consumed: Arc::new(AtomicU64::new(0)),
            samples_per_sec: 96_000,
        };
        assert_eq!(clock.now(), ts(1));
        // 2.5 秒分（48kHz ステレオ）
        clock.consumed.store(240_000, Ordering::Relaxed);
        assert_eq!(clock.played(), Timestamp::from_millis(2_500));
        assert_eq!(clock.now(), Timestamp::from_millis(6_500));
    }
}
