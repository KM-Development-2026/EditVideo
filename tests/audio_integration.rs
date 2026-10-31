//! 音声プレビューの結合テスト（ffmpeg・出力デバイスが無い環境ではスキップ）

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::{TestDir, make_video, open, tools};
use edit_video::media::audio::{self, AudioOutput};
use edit_video::media::player::{PlaybackState, Player, PlayerEvent};
use edit_video::model::Timestamp;

#[test]
fn pcm_args_output_sine() {
    let Some(ff) = tools() else { return };
    let dir = TestDir::new("audio_pcm");
    let src = dir.0.join("in.mp4");
    make_video(&ff, &src, "320x240", 30, 3, true);

    // 1 秒目から 1 秒分（48kHz ステレオ f32）。apad で無音が続くので出力指定の前に -t を入れて止める
    let mut args = audio::pcm_args(&src, Timestamp::from_secs(1), 48_000, 2);
    let output = args.pop().expect("出力指定");
    args.extend(["-t".into(), "1".into(), output]);
    let out = Command::new(&ff.ffmpeg).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let samples: Vec<f32> = out
        .stdout
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    let frames = samples.len() / 2;
    assert!(
        (47_000..=49_000).contains(&frames),
        "1 秒分にならない: {frames}"
    );
    let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.05, "無音になっている: {peak}");
}

/// 出力デバイスの代わりに音源を読み切る。(実データのサンプル列, 途中で記録したタイムライン時刻)
fn drain(
    source: &mut audio::PcmSource,
    clock: &audio::AudioClock,
    mark_at: usize,
) -> (Vec<f32>, Option<Timestamp>) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut real = Vec::new();
    let mut mark = None;
    let mut last_played = clock.played();
    for s in source.by_ref() {
        let played = clock.played();
        // 鳴らした量が増えたときだけ実データ（データ待ちの無音は数えない）
        if played != last_played {
            real.push(s);
            last_played = played;
        }
        if mark.is_none() && real.len() == mark_at {
            mark = Some(clock.now());
        }
        assert!(Instant::now() < deadline, "音源が終わらない");
    }
    (real, mark)
}

#[test]
fn feed_covers_enabled_segments_exactly() {
    let Some(ff) = tools() else { return };
    let dir = TestDir::new("audio_feed");
    let src = dir.0.join("in.mp4");
    make_video(&ff, &src, "320x240", 30, 4, true);
    let mut tl = open(&ff, &[&src]);
    tl.delete_range(Timestamp::from_millis(1_000), Timestamp::from_millis(2_500));

    // 0.5 秒から: [0.5, 1.0) と [2.5, 4.0) の計 2 秒 = 48kHz ステレオで 192000 サンプル
    let (mut source, playback) =
        audio::start_feed(&ff.ffmpeg, &tl, Timestamp::from_millis(500), 48_000, 2);
    let clock = playback.clock();
    let (real, mark) = drain(&mut source, &clock, 48_000);
    assert_eq!(real.len(), 192_000, "区間長ちょうどにならない");
    // 0.5 秒分鳴らした時点で削除区間を飛ばして 2.5 秒
    assert_eq!(mark, Some(Timestamp::from_millis(2_500)));
    assert_eq!(clock.now(), Timestamp::from_secs(4));
    let peak = real.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.05, "無音になっている: {peak}");
}

#[test]
fn feed_stops_when_dropped() {
    let Some(ff) = tools() else { return };
    let dir = TestDir::new("audio_feed_stop");
    let src = dir.0.join("in.mp4");
    make_video(&ff, &src, "320x240", 30, 4, true);
    let tl = open(&ff, &[&src]);
    let (mut source, playback) = audio::start_feed(&ff.ffmpeg, &tl, Timestamp::ZERO, 48_000, 2);
    assert!(source.next().is_some());
    drop(playback);
    assert_eq!(source.next(), None);
}

#[test]
fn audio_clock_advances_while_playing() {
    let Some(ff) = tools() else { return };
    let Ok(out) = AudioOutput::open() else {
        eprintln!("音声出力デバイスが無いためスキップ");
        return;
    };
    let dir = TestDir::new("audio_clock");
    let src = dir.0.join("in.mp4");
    make_video(&ff, &src, "320x240", 30, 4, true);
    let mut tl = open(&ff, &[&src]);
    tl.delete_range(Timestamp::from_millis(1_000), Timestamp::from_millis(2_500));

    let playback = out.play(&ff.ffmpeg, &tl, Timestamp::from_millis(500));
    let clock = playback.clock();
    let started = Instant::now();
    std::thread::sleep(Duration::from_millis(900));
    let played = clock.played();
    let now = clock.now();
    eprintln!(
        "{:?} 経過で {:?} 鳴らした → タイムライン {:?}",
        started.elapsed(),
        played,
        now
    );
    // ffmpeg の起動待ちの分だけ遅れる。実時間を大きく超えない
    assert!(played >= Timestamp::from_millis(400), "{played:?}");
    assert!(played <= Timestamp::from_millis(1_000), "{played:?}");
    // 0.5 秒から鳴らして 0.5 秒を超えたら削除区間 [1.0, 2.5) を飛ばす
    assert!(now >= Timestamp::from_millis(900), "{now:?}");
    if played > Timestamp::from_millis(500) {
        assert!(
            now >= Timestamp::from_millis(2_500),
            "削除区間を飛ばしていない: {now:?}"
        );
    }
    drop(playback);
}

#[test]
fn player_with_audio_keeps_time() {
    let Some(ff) = tools() else { return };
    if AudioOutput::open().is_err() {
        eprintln!("音声出力デバイスが無いためスキップ");
        return;
    }
    let dir = TestDir::new("audio_player");
    let src = dir.0.join("in.mp4");
    make_video(&ff, &src, "320x240", 30, 4, true);
    let tl = open(&ff, &[&src]);
    let p = Player::start(&ff.ffmpeg, tl, || {}).unwrap();
    p.seek(Timestamp::ZERO);
    // 最初のフレーム
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(PlayerEvent::Frame { .. }) = p.recv_timeout(Duration::from_millis(50)) {
            break;
        }
    }
    p.play();
    let started = Instant::now();
    let mut times = Vec::new();
    while started.elapsed() < Duration::from_millis(1_000) {
        match p.recv_timeout(Duration::from_millis(20)) {
            Some(PlayerEvent::Frame { time, .. }) => times.push(time),
            Some(PlayerEvent::Error(e)) => panic!("エラー: {e}"),
            _ => {}
        }
    }
    p.pause();
    let paused = Instant::now() + Duration::from_secs(10);
    while Instant::now() < paused {
        if let Some(PlayerEvent::State(PlaybackState::Paused)) =
            p.recv_timeout(Duration::from_millis(50))
        {
            break;
        }
    }
    eprintln!(
        "音声クロックで 1 秒再生: {} フレーム, 最後 {:?}",
        times.len(),
        times.last()
    );
    assert!(
        times.windows(2).all(|w| w[0] < w[1]),
        "時刻が単調増加しない"
    );
    let last = *times.last().expect("再生中のフレームがない");
    // 音声の起動待ちで少し遅れるが、実時間程度は進む
    assert!(
        last >= Timestamp::from_millis(600) && last <= Timestamp::from_millis(1_200),
        "{last:?}"
    );
    assert!(times.len() >= 15, "フレームが少なすぎる: {}", times.len());
}
