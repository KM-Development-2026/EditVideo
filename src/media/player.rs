//! プレビュー用デコードワーカースレッド
//!
//! `ffmpeg -ss <素材時刻> -i <file> ... -f rawvideo pipe:1` の標準出力から RGBA フレームを読む。
//! 操作・通知はすべてタイムライン時刻で行う。

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::ffmpeg;
use crate::export::command::secs_string;
use crate::export::runner::read_tail;
use crate::model::timeline::{Segment, Timeline};
use crate::model::{MediaInfo, Rational, Timestamp};

/// プレビューの既定の幅上限
pub const DEFAULT_PREVIEW_WIDTH: u32 = 960;
/// 再生中に連続して読み捨てるフレーム数の上限（これを超えたら遅れていても表示する）
const MAX_CONSECUTIVE_DROPS: u32 = 5;
/// 再生中、UI が受け取っていないフレームがこれ以上あれば送らない（最小化中のメモリ増加防止）
const MAX_FRAMES_IN_FLIGHT: usize = 2;
/// フレーム待ちの最大時間（この間隔でコマンドを確認する）
const MAX_WAIT: Duration = Duration::from_millis(100);
/// 末尾付近で1フレームも出ない場合に遡る時間
const FALLBACK_BACKOFF: Timestamp = Timestamp::from_secs(1);
/// 一時停止中のシークで、保持中のプロセスを読み進めて済ませる最大フレーム数
const MAX_FORWARD_SKIP: i64 = 15;
/// エラーメッセージに含める stderr の行数
const STDERR_TAIL_LINES: usize = 10;

/// UI からワーカーへのコマンド
#[derive(Clone, Debug, PartialEq)]
pub enum PlayerCommand {
    /// タイムライン時刻へシーク
    Seek(Timestamp),
    Play,
    Pause,
    /// ±N フレーム移動（再生中なら一時停止してから）
    StepFrame(i64),
    /// タイムラインのスナップショットを差し替える
    SetTimeline(Timeline),
    /// プレビューの幅上限（px）
    SetPreviewWidth(u32),
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackState {
    Playing,
    Paused,
}

/// ワーカーから UI への通知
#[derive(Clone, Debug, PartialEq)]
pub enum PlayerEvent {
    Frame {
        /// タイムライン時刻
        time: Timestamp,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    State(PlaybackState),
    /// 末尾に達した（この後 State(Paused) が届く）
    Ended,
    Error(String),
}

/// プレビュー再生（デコードワーカースレッドの操作窓口）
pub struct Player {
    tx: Sender<PlayerCommand>,
    rx: Receiver<PlayerEvent>,
    /// 送信済みで UI が未受信のフレーム数
    in_flight: Arc<AtomicUsize>,
    handle: Option<JoinHandle<()>>,
    /// スレッド異常終了を通知済みか
    dead: std::cell::Cell<bool>,
}

impl Player {
    /// timeline を再生するワーカースレッドを起動する（一時停止状態・フレームは未表示）。
    /// notify は通知を送るたびに呼ばれる（UI の再描画要求用）
    pub fn start(
        ffmpeg_exe: &Path,
        timeline: Timeline,
        notify: impl Fn() + Send + 'static,
    ) -> Result<Self> {
        let (tx, cmd_rx) = mpsc::channel();
        let (ev_tx, rx) = mpsc::channel();
        let in_flight = Arc::new(AtomicUsize::new(0));
        let worker = Worker {
            ffmpeg: ffmpeg_exe.to_path_buf(),
            rx: cmd_rx,
            tx: ev_tx,
            notify: Box::new(notify),
            in_flight: in_flight.clone(),
            timeline,
            preview_width: DEFAULT_PREVIEW_WIDTH,
            state: PlaybackState::Paused,
            position: Timestamp::ZERO,
            decoder: None,
            pending: VecDeque::new(),
            clock: None,
            drops: 0,
            unsent: None,
        };
        let handle = std::thread::Builder::new()
            .name("player".into())
            .spawn(move || worker.run())
            .context("プレビュースレッドを起動できません")?;
        Ok(Self {
            tx,
            rx,
            in_flight,
            handle: Some(handle),
            dead: std::cell::Cell::new(false),
        })
    }

    pub fn send(&self, cmd: PlayerCommand) {
        let _ = self.tx.send(cmd);
    }

    pub fn seek(&self, t: Timestamp) {
        self.send(PlayerCommand::Seek(t));
    }

    pub fn play(&self) {
        self.send(PlayerCommand::Play);
    }

    pub fn pause(&self) {
        self.send(PlayerCommand::Pause);
    }

    pub fn step_frame(&self, n: i64) {
        self.send(PlayerCommand::StepFrame(n));
    }

    pub fn set_timeline(&self, timeline: Timeline) {
        self.send(PlayerCommand::SetTimeline(timeline));
    }

    pub fn set_preview_width(&self, width: u32) {
        self.send(PlayerCommand::SetPreviewWidth(width));
    }

    /// 届いている通知を1件取り出す
    pub fn try_recv(&self) -> Option<PlayerEvent> {
        match self.rx.try_recv() {
            Ok(ev) => Some(self.received(ev)),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => self.disconnected(),
        }
    }

    /// 通知を最大 timeout だけ待って取り出す（テスト用）
    pub fn recv_timeout(&self, timeout: Duration) -> Option<PlayerEvent> {
        match self.rx.recv_timeout(timeout) {
            Ok(ev) => Some(self.received(ev)),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => self.disconnected(),
        }
    }

    fn received(&self, ev: PlayerEvent) -> PlayerEvent {
        if matches!(ev, PlayerEvent::Frame { .. }) {
            self.in_flight.fetch_sub(1, Ordering::Relaxed);
        }
        ev
    }

    fn disconnected(&self) -> Option<PlayerEvent> {
        // Shutdown 前に切断されるのはスレッド異常終了時のみ。通知は1回だけ
        if self.dead.replace(true) {
            None
        } else {
            Some(PlayerEvent::Error(
                "プレビュースレッドが異常終了しました".into(),
            ))
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            self.send(PlayerCommand::Shutdown);
            let _ = h.join();
        }
    }
}

/// シーク先の計算結果
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekPlan {
    /// セグメント番号
    pub seg: usize,
    /// セグメント先頭のタイムライン時刻
    pub seg_start: Timestamp,
    pub fps: Rational,
    /// 表示するフレームの素材上のフレーム番号
    pub frame: i64,
    /// ffmpeg の -ss に渡す素材時刻
    pub ss: Timestamp,
    /// 表示するフレームのタイムライン時刻
    pub time: Timestamp,
}

/// タイムライン時刻 t に表示すべきフレームと ffmpeg の開始位置を求める。
/// t は [0, 長さ) にクランプする（末尾ちょうどは最後のフレーム）。
pub fn plan_seek(tl: &Timeline, t: Timestamp) -> Option<SeekPlan> {
    let duration = tl.duration();
    if duration <= Timestamp::ZERO {
        return None;
    }
    let t = t.clamp_to(Timestamp::ZERO, duration - Timestamp(1));
    let (seg, src) = tl.locate(t)?;
    let segment = &tl.segments[seg];
    let fps = tl.source(segment.source)?.fps;
    let (seg_start, _) = tl.segment_range(seg)?;
    // t を含むフレーム（floor）
    let frame = fps.frame_at(src);
    // 入力シークは「pts >= ss の最初のフレーム」を出すので、半フレーム手前を指定して
    // 素材側の時刻の丸め（mkv の 1ms 単位など）に左右されないようにする
    let half = Timestamp(fps.frame_duration().as_micros() / 2);
    let ss = (fps.frame_start(frame) - half).max(Timestamp::ZERO);
    let time = frame_time(segment, seg_start, fps, frame)?;
    Some(SeekPlan {
        seg,
        seg_start,
        fps,
        frame,
        ss,
        time,
    })
}

/// 素材フレーム番号 frame のタイムライン時刻。セグメント終端以降なら None。
/// セグメント先頭がフレームの途中にある場合はセグメント先頭の時刻にする。
pub fn frame_time(
    seg: &Segment,
    seg_start: Timestamp,
    fps: Rational,
    frame: i64,
) -> Option<Timestamp> {
    let src = fps.frame_start(frame);
    if src >= seg.end {
        return None;
    }
    Some(seg_start + (src.max(seg.start) - seg.start))
}

/// 最後のフレームのタイムライン時刻
pub fn last_frame_time(tl: &Timeline) -> Option<Timestamp> {
    plan_seek(tl, tl.duration()).map(|p| p.time)
}

/// position から ±n フレーム移動した先のタイムライン時刻（フレーム境界・範囲内にクランプ）
pub fn step_target(tl: &Timeline, position: Timestamp, n: i64) -> Option<Timestamp> {
    let plan = plan_seek(tl, position)?;
    let seg = &tl.segments[plan.seg];
    let target_src = plan.fps.frame_start(plan.frame + n);
    let t = plan.seg_start + (target_src - seg.start);
    Some(t.clamp_to(Timestamp::ZERO, last_frame_time(tl)?))
}

/// idx 以降で最初の有効セグメント
pub fn next_enabled(tl: &Timeline, idx: usize) -> Option<usize> {
    (idx..tl.segments.len()).find(|&i| tl.segments[i].enabled)
}

/// t から再生を始める位置。t が無効セグメント内なら次の有効セグメント先頭。
/// 以降に有効セグメントが無い・範囲外なら None
pub fn playable_from(tl: &Timeline, t: Timestamp) -> Option<Timestamp> {
    if t >= tl.duration() {
        return None;
    }
    let (idx, _) = tl.locate(t)?;
    if tl.segments[idx].enabled {
        return Some(t);
    }
    let next = next_enabled(tl, idx + 1)?;
    tl.segment_range(next).map(|(s, _)| s)
}

/// アスペクト比を保ったプレビューサイズ（幅は max_width 以下、幅・高さとも偶数で 2 以上）
pub fn preview_size(src_width: u32, src_height: u32, max_width: u32) -> (u32, u32) {
    let even = |v: u32| (v & !1).max(2);
    if src_width == 0 || src_height == 0 {
        // 解像度不明なら 16:9 とみなす
        return preview_size(1280, 720, max_width);
    }
    let w = even(src_width.min(max_width));
    let h = (src_height as u64 * w as u64 + src_width as u64 / 2) / src_width as u64;
    (w, even(h as u32))
}

/// プレビュー用 ffmpeg の引数
pub fn decode_args(file: &Path, ss: Timestamp, width: u32, height: u32) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-ss"]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.push(secs_string(ss).into());
    args.push("-i".into());
    args.push(file.into());
    for a in ["-map", "0:V:0", "-an", "-sn", "-dn", "-vf"] {
        args.push(a.into());
    }
    args.push(format!("scale={width}:{height}").into());
    for a in ["-pix_fmt", "rgba", "-f", "rawvideo", "pipe:1"] {
        args.push(a.into());
    }
    args
}

/// 溜まったコマンドを整理する。連続する Seek は最後だけ、連続する StepFrame は合算、
/// Shutdown があればそれだけにする
pub fn coalesce(cmds: impl IntoIterator<Item = PlayerCommand>) -> Vec<PlayerCommand> {
    let mut out: Vec<PlayerCommand> = Vec::new();
    for cmd in cmds {
        match (out.last_mut(), cmd) {
            (_, PlayerCommand::Shutdown) => return vec![PlayerCommand::Shutdown],
            (Some(PlayerCommand::Seek(last)), PlayerCommand::Seek(t)) => *last = t,
            (Some(PlayerCommand::StepFrame(last)), PlayerCommand::StepFrame(n)) => *last += n,
            (_, cmd) => out.push(cmd),
        }
    }
    out
}

/// 再生開始時刻 origin から elapsed 経過したときのタイムライン時刻
pub fn clock_time(origin: Timestamp, elapsed: Duration) -> Timestamp {
    origin + Timestamp(elapsed.as_micros().min(i64::MAX as u128) as i64)
}

/// 再生中のフレームを読み捨てるか（表示期間が過ぎている・連続読み捨てが上限未満）
pub fn should_drop(
    frame_time: Timestamp,
    now: Timestamp,
    frame_duration: Timestamp,
    consecutive_drops: u32,
) -> bool {
    frame_time + frame_duration <= now && consecutive_drops < MAX_CONSECUTIVE_DROPS
}

/// 起動中の ffmpeg（1セグメント分の連続デコード）
struct Decoder {
    child: Child,
    stdout: ChildStdout,
    stderr: Option<JoinHandle<Vec<String>>>,
    seg: usize,
    seg_start: Timestamp,
    fps: Rational,
    /// 次に読むフレームの素材フレーム番号
    next_frame: i64,
    width: u32,
    height: u32,
}

impl Decoder {
    fn spawn(
        ffmpeg_exe: &Path,
        info: &MediaInfo,
        plan: &SeekPlan,
        ss: Timestamp,
        frame: i64,
        max_width: u32,
    ) -> Result<Self, String> {
        let (width, height) = preview_size(info.width, info.height, max_width);
        let mut child = ffmpeg::command(ffmpeg_exe)
            .args(decode_args(&info.path, ss, width, height))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("ffmpeg を起動できません（{}）: {e}", ffmpeg_exe.display()))?;
        let stdout = child.stdout.take().expect("stdout piped");
        // stderr は読み続けないとパイプが詰まる
        let stderr = child.stderr.take().expect("stderr piped");
        let stderr = std::thread::spawn(move || read_tail(stderr, STDERR_TAIL_LINES));
        Ok(Self {
            child,
            stdout,
            stderr: Some(stderr),
            seg: plan.seg,
            seg_start: plan.seg_start,
            fps: plan.fps,
            next_frame: frame,
            width,
            height,
        })
    }

    /// 1フレーム読む。終端なら None
    fn read_frame(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; self.width as usize * self.height as usize * 4];
        match self.stdout.read_exact(&mut buf) {
            Ok(()) => {
                self.next_frame += 1;
                Ok(Some(buf))
            }
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// プロセスを止め、stderr の末尾を返す
    fn close(&mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.stderr
            .take()
            .and_then(|h| h.join().ok())
            .map(|lines| lines.join("\n"))
            .unwrap_or_default()
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.close();
    }
}

/// 読んだが UI に送っていないフレーム
struct Unsent {
    time: Timestamp,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// 再生クロック（origin_time を origin で表示した基準）
#[derive(Clone, Copy)]
struct Clock {
    origin: Instant,
    origin_time: Timestamp,
}

impl Clock {
    fn start(t: Timestamp) -> Self {
        Self {
            origin: Instant::now(),
            origin_time: t,
        }
    }

    fn now(&self) -> Timestamp {
        clock_time(self.origin_time, self.origin.elapsed())
    }
}

struct Worker {
    ffmpeg: PathBuf,
    rx: Receiver<PlayerCommand>,
    tx: Sender<PlayerEvent>,
    notify: Box<dyn Fn() + Send>,
    in_flight: Arc<AtomicUsize>,
    timeline: Timeline,
    preview_width: u32,
    state: PlaybackState,
    /// 最後に表示（または読み出し）したフレームのタイムライン時刻
    position: Timestamp,
    decoder: Option<Decoder>,
    /// 未処理のコマンド
    pending: VecDeque<PlayerCommand>,
    clock: Option<Clock>,
    /// 連続して読み捨てたフレーム数
    drops: u32,
    unsent: Option<Unsent>,
}

impl Worker {
    fn send(&self, ev: PlayerEvent) {
        if matches!(ev, PlayerEvent::Frame { .. }) {
            self.in_flight.fetch_add(1, Ordering::Relaxed);
        }
        let _ = self.tx.send(ev);
        (self.notify)();
    }

    fn run(mut self) {
        loop {
            if self.state == PlaybackState::Paused && self.pending.is_empty() {
                match self.rx.recv() {
                    Ok(cmd) => self.pending.push_back(cmd),
                    Err(_) => return,
                }
            }
            loop {
                match self.rx.try_recv() {
                    Ok(cmd) => self.pending.push_back(cmd),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.pending.push_back(PlayerCommand::Shutdown);
                        break;
                    }
                }
            }
            for cmd in coalesce(self.pending.drain(..).collect::<Vec<_>>()) {
                if !self.handle(cmd) {
                    return;
                }
            }
            if self.state == PlaybackState::Playing && self.pending.is_empty() {
                self.tick();
            }
        }
    }

    /// コマンドを処理する。終了なら false
    fn handle(&mut self, cmd: PlayerCommand) -> bool {
        match cmd {
            PlayerCommand::Seek(t) => {
                if self.state == PlaybackState::Playing {
                    self.restart_playing_at(t);
                } else {
                    self.show_at(t);
                }
            }
            PlayerCommand::Play => self.play(),
            PlayerCommand::Pause => self.pause(),
            PlayerCommand::StepFrame(n) => {
                self.pause();
                if let Some(t) = step_target(&self.timeline, self.position, n) {
                    self.show_at(t);
                }
            }
            PlayerCommand::SetTimeline(tl) => {
                self.timeline = tl;
                self.reload();
            }
            PlayerCommand::SetPreviewWidth(w) => {
                let w = w.max(2);
                if w != self.preview_width {
                    self.preview_width = w;
                    self.reload();
                }
            }
            PlayerCommand::Shutdown => return false,
        }
        true
    }

    /// タイムライン・プレビューサイズ変更後に現在位置を表示し直す
    fn reload(&mut self) {
        self.decoder = None;
        self.unsent = None;
        let t = self
            .position
            .clamp_to(Timestamp::ZERO, self.timeline.duration());
        if self.state == PlaybackState::Playing {
            self.restart_playing_at(t);
        } else {
            self.show_at(t);
        }
    }

    fn set_state(&mut self, state: PlaybackState) {
        if self.state != state {
            self.state = state;
            self.send(PlayerEvent::State(state));
        }
    }

    fn pause(&mut self) {
        if self.state != PlaybackState::Playing {
            return;
        }
        self.clock = None;
        self.flush_unsent();
        self.set_state(PlaybackState::Paused);
    }

    /// 読み捨て・未送信のまま止まった場合、最後のフレームを UI に届ける
    fn flush_unsent(&mut self) {
        if let Some(u) = self.unsent.take() {
            self.send(PlayerEvent::Frame {
                time: u.time,
                width: u.width,
                height: u.height,
                rgba: u.rgba,
            });
        }
    }

    fn play(&mut self) {
        if self.state == PlaybackState::Playing {
            return;
        }
        let at_end = last_frame_time(&self.timeline).is_none_or(|last| self.position >= last);
        let start = if at_end {
            None
        } else {
            playable_from(&self.timeline, self.position)
        };
        // 末尾・有効区間なしなら先頭から
        let Some(start) = start.or_else(|| playable_from(&self.timeline, Timestamp::ZERO)) else {
            return;
        };
        if start != self.position {
            self.decoder = None;
        }
        self.position = start;
        self.drops = 0;
        // プロセスを起動し直す場合は最初のフレームを読んだ時点からクロックを進める
        self.clock = self.decoder.as_ref().map(|_| Clock::start(start));
        self.set_state(PlaybackState::Playing);
    }

    /// 再生中のシーク（無効セグメント内なら次の有効セグメント先頭へ）
    fn restart_playing_at(&mut self, t: Timestamp) {
        self.decoder = None;
        self.unsent = None;
        match playable_from(&self.timeline, t) {
            Some(start) => {
                self.position = start;
                self.drops = 0;
                self.clock = None;
            }
            None => {
                // 末尾以降: 指定位置を表示して止める
                self.state = PlaybackState::Paused;
                self.clock = None;
                self.show_at(t);
                self.send(PlayerEvent::Ended);
                self.send(PlayerEvent::State(PlaybackState::Paused));
            }
        }
    }

    fn spawn_decoder(&self, plan: &SeekPlan, ss: Timestamp, frame: i64) -> Result<Decoder, String> {
        let seg = &self.timeline.segments[plan.seg];
        let info = self
            .timeline
            .source(seg.source)
            .ok_or_else(|| "素材が見つかりません".to_owned())?;
        Decoder::spawn(&self.ffmpeg, info, plan, ss, frame, self.preview_width)
    }

    /// 一時停止中のシーク: t のフレームを1枚表示する（プロセスは次の操作用に保持）
    fn show_at(&mut self, t: Timestamp) {
        let Some(plan) = plan_seek(&self.timeline, t) else {
            return;
        };
        // 保持中のプロセスで少し先へ進むだけなら読み進める
        let reusable = self.decoder.as_ref().is_some_and(|d| {
            d.seg == plan.seg
                && d.next_frame <= plan.frame
                && plan.frame - d.next_frame <= MAX_FORWARD_SKIP
        });
        if !reusable {
            self.decoder = None;
            match self.spawn_decoder(&plan, plan.ss, plan.frame) {
                Ok(d) => self.decoder = Some(d),
                Err(e) => return self.send(PlayerEvent::Error(e)),
            }
        }
        let dec = self.decoder.as_mut().expect("decoder");
        let mut frame = None;
        while dec.next_frame <= plan.frame {
            match dec.read_frame() {
                Ok(Some(rgba)) => frame = Some(rgba),
                Ok(None) => {
                    frame = None;
                    break;
                }
                Err(e) => {
                    let msg = format!("プレビューの読み込みに失敗しました: {e}");
                    self.decoder = None;
                    return self.send(PlayerEvent::Error(msg));
                }
            }
        }
        match frame {
            Some(rgba) => {
                let (width, height) = (dec.width, dec.height);
                self.position = plan.time;
                self.send(PlayerEvent::Frame {
                    time: plan.time,
                    width,
                    height,
                    rgba,
                });
            }
            None => self.show_fallback(&plan),
        }
    }

    /// 指定フレームが出ない（素材の映像が表記の長さより短い等）場合、少し前から読んで
    /// 最後に得られたフレームを表示する
    fn show_fallback(&mut self, plan: &SeekPlan) {
        let stderr = self
            .decoder
            .take()
            .map(|mut d| d.close())
            .unwrap_or_default();
        let back = plan.fps.frame_at(FALLBACK_BACKOFF);
        let first = (plan.frame - back).max(0);
        let half = Timestamp(plan.fps.frame_duration().as_micros() / 2);
        let ss = (plan.fps.frame_start(first) - half).max(Timestamp::ZERO);
        let mut dec = match self.spawn_decoder(plan, ss, first) {
            Ok(d) => d,
            Err(e) => return self.send(PlayerEvent::Error(e)),
        };
        let mut last = None;
        while dec.next_frame <= plan.frame {
            match dec.read_frame() {
                Ok(Some(rgba)) => last = Some((dec.next_frame - 1, rgba)),
                _ => break,
            }
        }
        let seg = &self.timeline.segments[plan.seg];
        let time = last
            .as_ref()
            .and_then(|(f, _)| frame_time(seg, plan.seg_start, plan.fps, *f));
        match (last, time) {
            (Some((_, rgba)), Some(time)) => {
                self.position = time;
                self.send(PlayerEvent::Frame {
                    time,
                    width: dec.width,
                    height: dec.height,
                    rgba,
                });
            }
            _ => {
                let detail = dec.close();
                let detail = if detail.is_empty() { stderr } else { detail };
                let mut msg = format!("{} のフレームを取得できません", plan.time.to_hms());
                if !detail.is_empty() {
                    msg.push('\n');
                    msg.push_str(&detail);
                }
                self.send(PlayerEvent::Error(msg));
            }
        }
    }

    /// 再生中の1ステップ: 次のフレームを読み、時刻が来たら送る
    fn tick(&mut self) {
        if self.decoder.is_none() {
            let Some(plan) = plan_seek(&self.timeline, self.position) else {
                return self.end();
            };
            match self.spawn_decoder(&plan, plan.ss, plan.frame) {
                Ok(d) => self.decoder = Some(d),
                Err(e) => {
                    self.send(PlayerEvent::Error(e));
                    return self.stop_playing();
                }
            }
        }
        let dec = self.decoder.as_ref().expect("decoder");
        let seg = &self.timeline.segments[dec.seg];
        let Some(ft) = frame_time(seg, dec.seg_start, dec.fps, dec.next_frame) else {
            return self.advance_segment(false);
        };
        let frame_duration = dec.fps.frame_duration();
        // 表示時刻まで待つ（その間もコマンドを受け付ける）
        if let Some(clock) = self.clock
            && ft > clock.now()
        {
            let wait =
                Duration::from_micros((ft - clock.now()).as_micros().max(0) as u64).min(MAX_WAIT);
            match self.rx.recv_timeout(wait) {
                Ok(cmd) => return self.pending.push_back(cmd),
                Err(RecvTimeoutError::Timeout) => {
                    if clock.now() < ft {
                        return;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return self.pending.push_back(PlayerCommand::Shutdown);
                }
            }
        }
        let dec = self.decoder.as_mut().expect("decoder");
        let rgba = match dec.read_frame() {
            Ok(Some(rgba)) => rgba,
            // 素材の映像が先に尽きた
            Ok(None) => return self.advance_segment(true),
            Err(e) => {
                self.send(PlayerEvent::Error(format!(
                    "プレビューの読み込みに失敗しました: {e}"
                )));
                return self.stop_playing();
            }
        };
        let frame = Unsent {
            time: ft,
            width: dec.width,
            height: dec.height,
            rgba,
        };
        self.position = ft;
        let clock = *self.clock.get_or_insert_with(|| Clock::start(ft));
        let busy = self.in_flight.load(Ordering::Relaxed) >= MAX_FRAMES_IN_FLIGHT;
        if should_drop(ft, clock.now(), frame_duration, self.drops) || busy {
            self.drops += 1;
            self.unsent = Some(frame);
        } else {
            self.drops = 0;
            self.unsent = Some(frame);
            self.flush_unsent();
        }
    }

    /// セグメント終端（または素材の終端 eof）に達したので次の有効セグメントへ進む
    fn advance_segment(&mut self, eof: bool) {
        let Some(dec) = self.decoder.as_mut() else {
            return;
        };
        let cur = dec.seg;
        let next = cur + 1;
        let tl = &self.timeline;
        // 同じ素材で素材時刻が連続する有効セグメントならプロセスを使い続ける
        if !eof
            && let Some(n) = tl.segments.get(next)
            && n.enabled
            && n.source == tl.segments[cur].source
            && n.start == tl.segments[cur].end
        {
            dec.seg = next;
            dec.seg_start = tl.segment_range(next).map_or(dec.seg_start, |(s, _)| s);
            return;
        }
        self.decoder = None;
        let Some(idx) = next_enabled(tl, next) else {
            return self.end();
        };
        let Some((start, _)) = tl.segment_range(idx) else {
            return self.end();
        };
        self.position = start;
        if idx != next {
            // 無効セグメントを飛ばしたので、次の最初のフレームからクロックを合わせ直す
            self.clock = None;
            self.drops = 0;
        }
    }

    /// 末尾に達した
    fn end(&mut self) {
        self.decoder = None;
        self.flush_unsent();
        self.clock = None;
        self.send(PlayerEvent::Ended);
        self.set_state(PlaybackState::Paused);
    }

    fn stop_playing(&mut self) {
        self.decoder = None;
        self.clock = None;
        self.flush_unsent();
        self.set_state(PlaybackState::Paused);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::timeline::SourceId;

    fn ts(us: i64) -> Timestamp {
        Timestamp(us)
    }

    fn info(name: &str, secs: i64, fps: Rational) -> MediaInfo {
        MediaInfo {
            path: PathBuf::from(name),
            duration: Timestamp::from_secs(secs),
            width: 1920,
            height: 1080,
            fps,
            has_audio: false,
            video_codec: None,
            audio_codec: None,
        }
    }

    fn fps30() -> Rational {
        Rational::new(30, 1).unwrap()
    }

    fn single(secs: i64) -> Timeline {
        let mut tl = Timeline::new();
        tl.add_source(info("a.mp4", secs, fps30()));
        tl
    }

    #[test]
    fn preview_size_keeps_aspect_and_even() {
        assert_eq!(preview_size(1920, 1080, 960), (960, 540));
        assert_eq!(preview_size(320, 240, 960), (320, 240), "拡大しない");
        assert_eq!(preview_size(1080, 1920, 960), (960, 1706));
        // 奇数は偶数に丸める
        assert_eq!(preview_size(641, 481, 960), (640, 480));
        assert_eq!(preview_size(1440, 1080, 641), (640, 480));
        assert_eq!(preview_size(1000, 1, 960), (960, 2), "最小 2");
        assert_eq!(preview_size(0, 0, 960), (960, 540), "不明は 16:9");
    }

    #[test]
    fn plan_seek_floor_frame_and_half_frame_ss() {
        let tl = single(10);
        let p = plan_seek(&tl, ts(1_500_000)).unwrap();
        assert_eq!((p.seg, p.frame, p.time), (0, 45, ts(1_500_000)));
        // 33334/2 = 16667 手前
        assert_eq!(p.ss, ts(1_500_000 - 16_667));
        // フレーム途中はそのフレームの先頭
        let p = plan_seek(&tl, ts(1_520_000)).unwrap();
        assert_eq!((p.frame, p.time), (45, ts(1_500_000)));
        // 先頭は 0 未満にしない
        let p = plan_seek(&tl, Timestamp::ZERO).unwrap();
        assert_eq!(
            (p.frame, p.ss, p.time),
            (0, Timestamp::ZERO, Timestamp::ZERO)
        );
        // 範囲外・末尾は最後のフレーム
        let last = plan_seek(&tl, Timestamp::from_secs(10)).unwrap();
        assert_eq!((last.frame, last.time), (299, fps30().frame_start(299)));
        assert_eq!(plan_seek(&tl, Timestamp::from_secs(99)), Some(last));
        assert_eq!(plan_seek(&tl, Timestamp(-5)).unwrap().frame, 0);
        assert_eq!(plan_seek(&Timeline::new(), Timestamp::ZERO), None);
    }

    #[test]
    fn plan_seek_maps_segments_to_source_time() {
        // 素材 [0,10) の [4,10) を先頭に、[0,4) を後ろに並べ替え
        let mut tl = single(10);
        tl.split_at(Timestamp::from_secs(4));
        tl.segments.swap(0, 1);
        let p = plan_seek(&tl, Timestamp::from_secs(1)).unwrap();
        assert_eq!((p.seg, p.frame), (0, 150), "素材 5 秒");
        assert_eq!(p.time, Timestamp::from_secs(1));
        let p = plan_seek(&tl, Timestamp::from_secs(7)).unwrap();
        assert_eq!(
            (p.seg, p.seg_start, p.frame),
            (1, Timestamp::from_secs(6), 30)
        );
    }

    #[test]
    fn plan_seek_ntsc_two_sources() {
        let ntsc = Rational::new(30000, 1001).unwrap();
        let mut tl = Timeline::new();
        tl.add_source(info("a.mp4", 2, fps30()));
        tl.add_source(info("b.mp4", 2, ntsc));
        let p = plan_seek(&tl, Timestamp::from_secs(3)).unwrap();
        assert_eq!(p.seg, 1);
        assert_eq!(p.fps, ntsc);
        assert_eq!(p.frame, 29);
        assert_eq!(p.time, Timestamp::from_secs(2) + ntsc.frame_start(29));
    }

    #[test]
    fn frame_time_within_segment() {
        let fps = fps30();
        // 素材 [1.01, 2.0) をタイムライン 5 秒から
        let seg = Segment {
            source: SourceId(0),
            start: ts(1_010_000),
            end: Timestamp::from_secs(2),
            enabled: true,
        };
        let start = Timestamp::from_secs(5);
        // フレーム30（1.0 秒）はセグメント先頭より前なので先頭に寄せる
        assert_eq!(frame_time(&seg, start, fps, 30), Some(start));
        assert_eq!(
            frame_time(&seg, start, fps, 31),
            Some(start + ts(1_033_334 - 1_010_000))
        );
        assert_eq!(
            frame_time(&seg, start, fps, 59).map(|t| t > start),
            Some(true)
        );
        assert_eq!(frame_time(&seg, start, fps, 60), None, "終端以降");
    }

    #[test]
    fn step_target_snaps_and_clamps() {
        let tl = single(10);
        let f = |n| fps30().frame_start(n);
        assert_eq!(step_target(&tl, f(45), 1), Some(f(46)));
        assert_eq!(step_target(&tl, f(45), -1), Some(f(44)));
        assert_eq!(step_target(&tl, f(45), 5), Some(f(50)));
        // フレーム途中からは所属フレーム基準
        assert_eq!(step_target(&tl, ts(1_510_000), 1), Some(f(46)));
        // 範囲クランプ
        assert_eq!(step_target(&tl, f(1), -5), Some(Timestamp::ZERO));
        assert_eq!(step_target(&tl, f(298), 5), Some(f(299)));
        assert_eq!(step_target(&Timeline::new(), Timestamp::ZERO, 1), None);
    }

    #[test]
    fn step_target_across_segment_boundary() {
        let mut tl = single(10);
        tl.split_at(Timestamp::from_secs(4));
        tl.segments.swap(0, 1);
        // タイムライン 6 秒 = 2つ目のセグメント（素材 0 秒）の先頭
        let six = Timestamp::from_secs(6);
        let back = step_target(&tl, six, -1).unwrap();
        assert_eq!(back, six - fps30().frame_duration() + ts(1));
        assert_eq!(tl.locate(back).map(|l| l.0), Some(0));
        assert_eq!(step_target(&tl, six, 1), Some(six + fps30().frame_start(1)));
    }

    #[test]
    fn playable_from_skips_disabled() {
        let mut tl = single(10);
        tl.delete_range(Timestamp::from_secs(2), Timestamp::from_secs(4));
        tl.delete_range(Timestamp::from_secs(8), Timestamp::from_secs(10));
        let s = Timestamp::from_secs;
        assert_eq!(playable_from(&tl, s(1)), Some(s(1)));
        assert_eq!(playable_from(&tl, s(3)), Some(s(4)));
        assert_eq!(playable_from(&tl, s(2)), Some(s(4)), "無効区間の先頭");
        assert_eq!(playable_from(&tl, s(9)), None, "以降に有効区間なし");
        assert_eq!(playable_from(&tl, s(10)), None);
        assert_eq!(next_enabled(&tl, 1), Some(2));
        assert_eq!(next_enabled(&tl, 3), None);
    }

    #[test]
    fn last_frame_time_cases() {
        assert_eq!(last_frame_time(&single(1)), Some(fps30().frame_start(29)));
        assert_eq!(last_frame_time(&Timeline::new()), None);
    }

    #[test]
    fn decode_args_shape() {
        let args = decode_args(Path::new("in.mp4"), ts(1_500_000), 640, 360);
        let s: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let pos = |x: &str| s.iter().position(|a| a == x).unwrap();
        // 入力シーク（-i より前）
        assert!(pos("-ss") < pos("-i"));
        assert_eq!(s[pos("-ss") + 1], "1.500000");
        assert_eq!(s[pos("-i") + 1], "in.mp4");
        assert_eq!(s[pos("-vf") + 1], "scale=640:360");
        assert_eq!(s[pos("-pix_fmt") + 1], "rgba");
        assert_eq!(s[pos("-f") + 1], "rawvideo");
        assert_eq!(s.last().unwrap(), "pipe:1");
        assert!(s.contains(&"-an".to_owned()));
    }

    #[test]
    fn coalesce_keeps_last_seek() {
        use PlayerCommand::*;
        let s = |ms| Seek(Timestamp::from_millis(ms));
        assert_eq!(coalesce([s(1), s(2), s(3)]), vec![s(3)]);
        assert_eq!(
            coalesce([s(1), s(2), Play, s(4), s(5)]),
            vec![s(2), Play, s(5)]
        );
        assert_eq!(
            coalesce([StepFrame(1), StepFrame(1), StepFrame(-3), Pause]),
            vec![StepFrame(-1), Pause]
        );
        assert_eq!(coalesce([s(1), Shutdown, s(2)]), vec![Shutdown]);
        assert_eq!(coalesce(Vec::new()), vec![]);
        // 順序を保つ
        assert_eq!(
            coalesce([Pause, SetPreviewWidth(640), s(1)]),
            vec![Pause, SetPreviewWidth(640), s(1)]
        );
    }

    #[test]
    fn clock_and_drop() {
        let t = clock_time(Timestamp::from_secs(2), Duration::from_millis(1_500));
        assert_eq!(t, ts(3_500_000));
        let fd = fps30().frame_duration();
        let now = Timestamp::from_secs(1);
        // 表示期間内なら表示
        assert!(!should_drop(now - ts(10_000), now, fd, 0));
        // 1フレーム以上遅れたら読み捨て
        assert!(should_drop(now - fd, now, fd, 0));
        // 連続読み捨ての上限
        assert!(!should_drop(
            now - Timestamp::from_secs(1),
            now,
            fd,
            MAX_CONSECUTIVE_DROPS
        ));
        // 進んでいるフレームは捨てない
        assert!(!should_drop(now + fd, now, fd, 0));
    }
}
