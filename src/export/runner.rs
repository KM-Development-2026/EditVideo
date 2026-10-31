//! 書き出しワーカースレッド（ffmpeg 実行・進捗・キャンセル）

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::command::{self, ExportPlan, ExportSettings, FilterGraph, PlanKind};
use crate::media::ffmpeg;
use crate::model::Timestamp;
use crate::model::timeline::Timeline;

/// 失敗時メッセージに含める stderr の行数
const STDERR_TAIL_LINES: usize = 30;
/// キャンセル確認の間隔
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// ワーカーから UI への通知
#[derive(Clone, Debug, PartialEq)]
pub enum ExportEvent {
    /// 0.0..=1.0
    Progress(f32),
    Finished(PathBuf),
    Failed(String),
    Cancelled,
}

impl ExportEvent {
    /// 最後の通知（これ以降は届かない）か
    pub fn is_terminal(&self) -> bool {
        !matches!(self, ExportEvent::Progress(_))
    }
}

/// 実行中の書き出し
pub struct ExportJob {
    rx: Receiver<ExportEvent>,
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ExportJob {
    /// timeline の書き出し対象区間を settings で書き出すスレッドを起動する。
    /// notify は通知を送るたびに呼ばれる（UI の再描画要求用）
    pub fn start(
        ffmpeg_exe: &Path,
        timeline: &Timeline,
        settings: ExportSettings,
        notify: impl Fn() + Send + 'static,
    ) -> Result<Self> {
        let ranges = timeline.export_ranges();
        for r in &ranges {
            if let Some(src) = timeline.source(r.source)
                && is_same_file(&src.path, &settings.output)
            {
                bail!("出力先が入力素材と同じです: {}", settings.output.display());
            }
        }
        let temp_dir = create_temp_dir()?;
        let plan = match command::build(&ranges, &timeline.sources, &settings, &temp_dir) {
            Ok(p) => p,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&temp_dir);
                return Err(e);
            }
        };

        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker = Worker {
            ffmpeg: ffmpeg_exe.to_path_buf(),
            cancel: cancel.clone(),
            tx,
            notify: Box::new(notify),
        };
        let handle = std::thread::Builder::new()
            .name("export".into())
            .spawn(move || worker.run(plan, temp_dir))
            .context("書き出しスレッドを起動できません")?;
        Ok(Self {
            rx,
            cancel,
            handle: Some(handle),
        })
    }

    /// キャンセルを要求する（完了は Cancelled イベントで通知）
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// 届いている通知を1件取り出す
    pub fn try_recv(&self) -> Option<ExportEvent> {
        match self.rx.try_recv() {
            Ok(ev) => Some(ev),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                // 最終通知の後に切断されるので、ここに来るのはスレッド異常終了時のみ
                Some(ExportEvent::Failed(
                    "書き出しスレッドが異常終了しました".into(),
                ))
            }
        }
    }

    /// 完了まで待ち、最終通知を返す（テスト・CLI 用）
    pub fn wait(mut self) -> ExportEvent {
        let mut last = ExportEvent::Failed("書き出しスレッドが異常終了しました".into());
        while let Ok(ev) = self.rx.recv() {
            let done = ev.is_terminal();
            last = ev;
            if done {
                break;
            }
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        last
    }
}

impl Drop for ExportJob {
    fn drop(&mut self) {
        // 画面を閉じた場合などは ffmpeg を止めて後始末を待つ
        if let Some(h) = self.handle.take() {
            self.cancel();
            let _ = h.join();
        }
    }
}

/// ffmpeg 1回分の実行
struct Step {
    args: Vec<OsString>,
    duration: Timestamp,
    /// 最終出力ファイルを書くか（失敗時に削除する対象か）
    writes_output: bool,
}

enum StepError {
    Cancelled,
    Failed(String),
}

struct Worker {
    ffmpeg: PathBuf,
    cancel: Arc<AtomicBool>,
    tx: Sender<ExportEvent>,
    notify: Box<dyn Fn() + Send>,
}

impl Worker {
    fn send(&self, ev: ExportEvent) {
        let _ = self.tx.send(ev);
        (self.notify)();
    }

    fn run(self, plan: ExportPlan, temp_dir: PathBuf) {
        let mut output_touched = false;
        let result = self.execute(&plan, &mut output_touched);
        let _ = std::fs::remove_dir_all(&temp_dir);
        if result.is_err() && output_touched {
            let _ = std::fs::remove_file(&plan.output);
        }
        let ev = match result {
            Ok(()) => ExportEvent::Finished(plan.output.clone()),
            Err(StepError::Cancelled) => ExportEvent::Cancelled,
            Err(StepError::Failed(msg)) => ExportEvent::Failed(msg),
        };
        self.send(ev);
    }

    fn execute(&self, plan: &ExportPlan, output_touched: &mut bool) -> Result<(), StepError> {
        let (files, steps) = steps_of(plan);
        for (path, content) in files {
            std::fs::write(path, content).map_err(|e| {
                StepError::Failed(format!(
                    "一時ファイル {} を書き込めません: {e}",
                    path.display()
                ))
            })?;
        }
        let grand_total = steps
            .iter()
            .fold(Timestamp::ZERO, |acc, s| acc + s.duration);
        let mut done = Timestamp::ZERO;
        self.send(ExportEvent::Progress(0.0));
        for step in &steps {
            *output_touched |= step.writes_output;
            self.run_step(step, done, grand_total)?;
            done += step.duration;
            self.send(ExportEvent::Progress(progress_ratio(done, grand_total)));
        }
        Ok(())
    }

    fn run_step(&self, step: &Step, done: Timestamp, total: Timestamp) -> Result<(), StepError> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(StepError::Cancelled);
        }
        let mut child = ffmpeg::command(&self.ffmpeg)
            .args(&step.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                StepError::Failed(format!(
                    "ffmpeg を起動できません（{}）: {e}",
                    self.ffmpeg.display()
                ))
            })?;

        // stderr は読み続けないとパイプが詰まって ffmpeg が止まる
        let stderr = child.stderr.take().expect("stderr piped");
        let stderr_thread = std::thread::spawn(move || read_tail(stderr, STDERR_TAIL_LINES));

        let stdout = child.stdout.take().expect("stdout piped");
        let (ptx, prx) = mpsc::channel();
        let stdout_thread = std::thread::spawn(move || {
            for_each_line(stdout, |line| {
                if let Some(t) = parse_out_time(line) {
                    let _ = ptx.send(t);
                }
            });
        });

        let mut cancelled = false;
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            match prx.recv_timeout(POLL_INTERVAL) {
                Ok(t) => {
                    let t = t.clamp_to(Timestamp::ZERO, step.duration);
                    self.send(ExportEvent::Progress(progress_ratio(done + t, total)));
                }
                Err(RecvTimeoutError::Timeout) => {}
                // stdout が閉じた = ffmpeg が終了しつつある
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        let status = finish(&mut child, cancelled);
        let _ = stdout_thread.join();
        let tail = stderr_thread.join().unwrap_or_default();

        if cancelled {
            return Err(StepError::Cancelled);
        }
        match status {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(StepError::Failed(failure_message(&s.to_string(), &tail))),
            Err(e) => Err(StepError::Failed(format!("ffmpeg の終了を待てません: {e}"))),
        }
    }
}

/// キャンセル時は kill してから終了を待つ
fn finish(child: &mut Child, kill: bool) -> std::io::Result<ExitStatus> {
    if kill {
        let _ = child.kill();
    }
    child.wait()
}

/// 計画を「事前に書き出すファイル」と「実行ステップ列」に展開する
fn steps_of(plan: &ExportPlan) -> (Vec<(&Path, &str)>, Vec<Step>) {
    match &plan.kind {
        PlanKind::Accurate(a) => {
            let files = match &a.filter {
                FilterGraph::Inline(_) => vec![],
                FilterGraph::Script { path, content } => vec![(path.as_path(), content.as_str())],
            };
            let steps = vec![Step {
                args: a.args.clone(),
                duration: plan.total,
                writes_output: true,
            }];
            (files, steps)
        }
        PlanKind::Fast(f) => {
            let files = vec![(f.list_path.as_path(), f.list_content.as_str())];
            let mut steps: Vec<Step> = f
                .parts
                .iter()
                .map(|p| Step {
                    args: p.args.clone(),
                    duration: p.duration,
                    writes_output: false,
                })
                .collect();
            // 連結はコピーのみで速いが、区間数ぶんと同じ重みにして段階的に進める
            steps.push(Step {
                args: f.concat_args.clone(),
                duration: plan.total,
                writes_output: true,
            });
            (files, steps)
        }
    }
}

fn failure_message(status: &str, stderr_tail: &[String]) -> String {
    let mut msg = format!("ffmpeg が失敗しました（{status}）");
    if !stderr_tail.is_empty() {
        msg.push('\n');
        msg.push_str(&stderr_tail.join("\n"));
    }
    msg
}

/// `-progress` 出力の1行から out_time を読む（out_time_ms も実際はマイクロ秒）
pub fn parse_out_time(line: &str) -> Option<Timestamp> {
    let (key, value) = line.trim().split_once('=')?;
    match key.trim() {
        "out_time_us" | "out_time_ms" => value.trim().parse().ok().map(Timestamp::from_micros),
        _ => None,
    }
}

/// done / total を 0.0..=1.0 に
pub fn progress_ratio(done: Timestamp, total: Timestamp) -> f32 {
    if total <= Timestamp::ZERO {
        return 0.0;
    }
    (done.as_micros() as f64 / total.as_micros() as f64).clamp(0.0, 1.0) as f32
}

/// 1行ずつ読む（UTF-8 でない出力でも止まらないよう lossy 変換）
fn for_each_line(r: impl Read, mut f: impl FnMut(&str)) {
    let mut reader = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => f(String::from_utf8_lossy(&buf).trim_end()),
        }
    }
}

/// 最後の n 行を保持しつつ最後まで読む
pub(crate) fn read_tail(r: impl Read, n: usize) -> Vec<String> {
    let mut lines = VecDeque::with_capacity(n);
    for_each_line(r, |line| {
        if line.is_empty() {
            return;
        }
        if lines.len() == n {
            lines.pop_front();
        }
        lines.push_back(line.to_owned());
    });
    lines.into()
}

/// 同じファイルを指すか（存在するなら正規化して比較）
fn is_same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// `temp_dir()` 配下に一意な作業ディレクトリを作る
fn create_temp_dir() -> Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!(
            "edit_video_export_{}_{nanos}_{n}",
            std::process::id()
        ));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("一時ディレクトリを作成できません: {}", dir.display())
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_out_time_lines() {
        assert_eq!(
            parse_out_time("out_time_us=1234567"),
            Some(Timestamp(1_234_567))
        );
        assert_eq!(
            parse_out_time("out_time_ms=1234567\r"),
            Some(Timestamp(1_234_567))
        );
        assert_eq!(
            parse_out_time("out_time_us=-23220"),
            Some(Timestamp(-23_220))
        );
        assert_eq!(parse_out_time("out_time_us=N/A"), None);
        assert_eq!(parse_out_time("out_time=00:00:01.234567"), None);
        assert_eq!(parse_out_time("progress=continue"), None);
        assert_eq!(parse_out_time("frame=30"), None);
        assert_eq!(parse_out_time(""), None);
    }

    #[test]
    fn progress_ratio_clamps() {
        let t = Timestamp::from_secs;
        assert_eq!(progress_ratio(t(0), t(10)), 0.0);
        assert_eq!(progress_ratio(t(5), t(10)), 0.5);
        assert_eq!(progress_ratio(t(20), t(10)), 1.0);
        assert_eq!(progress_ratio(Timestamp(-1), t(10)), 0.0);
        assert_eq!(progress_ratio(t(1), Timestamp::ZERO), 0.0);
    }

    #[test]
    fn read_tail_keeps_last_lines() {
        let text = (1..=40)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = read_tail(text.as_bytes(), 30);
        assert_eq!(tail.len(), 30);
        assert_eq!(tail[0], "line11");
        assert_eq!(tail[29], "line40");
        // UTF-8 でない行でも読み進める
        let bytes: &[u8] = b"ok\n\xff\xfe\nlast\r\n";
        assert_eq!(read_tail(bytes, 30).last().unwrap(), "last");
    }

    #[test]
    fn failure_message_includes_tail() {
        let m = failure_message("exit code: 1", &["a".into(), "b".into()]);
        assert_eq!(m, "ffmpeg が失敗しました（exit code: 1）\na\nb");
    }

    #[test]
    fn temp_dirs_are_unique() {
        let a = create_temp_dir().unwrap();
        let b = create_temp_dir().unwrap();
        assert_ne!(a, b);
        assert!(a.is_dir() && b.is_dir());
        std::fs::remove_dir(&a).unwrap();
        std::fs::remove_dir(&b).unwrap();
    }

    #[test]
    fn same_file_detection() {
        let dir = create_temp_dir().unwrap();
        let f = dir.join("in.mp4");
        std::fs::write(&f, b"").unwrap();
        // 別表記でも同じファイル
        let alt = dir.join(".").join("in.mp4");
        assert!(is_same_file(&f, &alt));
        assert!(!is_same_file(&f, &dir.join("out.mp4")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn start_rejects_output_same_as_input() {
        let dir = create_temp_dir().unwrap();
        let f = dir.join("in.mp4");
        std::fs::write(&f, b"").unwrap();
        let mut tl = Timeline::new();
        tl.add_source(crate::model::MediaInfo {
            path: f.clone(),
            duration: Timestamp::from_secs(1),
            width: 320,
            height: 240,
            fps: crate::model::Rational::new(30, 1).unwrap(),
            has_audio: false,
            video_codec: None,
            audio_codec: None,
        });
        let settings = ExportSettings {
            output: f.clone(),
            ..Default::default()
        };
        let err = ExportJob::start(Path::new("ffmpeg"), &tl, settings, || {})
            .err()
            .expect("same path must fail");
        assert!(err.to_string().contains("同じ"), "{err}");
        // 区間0件も起動前にエラー
        tl.delete(0);
        let settings = ExportSettings {
            output: dir.join("out.mp4"),
            ..Default::default()
        };
        assert!(ExportJob::start(Path::new("ffmpeg"), &tl, settings, || {}).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
