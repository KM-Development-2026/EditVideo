//! テスト動画を実際にデコードしてプレビュープレイヤーを検証する（ffmpeg が無ければスキップ）

mod common;

use std::time::{Duration, Instant};

use common::{TestDir, make_video, open, tools};
use edit_video::media::player::{self, PlaybackState, Player, PlayerEvent};
use edit_video::model::timeline::Timeline;
use edit_video::model::{Rational, Timestamp};

const TIMEOUT: Duration = Duration::from_secs(10);

struct Frame {
    time: Timestamp,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

fn fps30() -> Rational {
    Rational::new(30, 1).unwrap()
}

fn ms(v: i64) -> Timestamp {
    Timestamp::from_millis(v)
}

/// 320x240・30fps・4秒のテスト動画を開く
fn setup(name: &str, audio: bool) -> Option<(TestDir, Timeline, Player)> {
    let ff = tools()?;
    let dir = TestDir::new(name);
    let src = dir.0.join("src.mp4");
    make_video(&ff, &src, "320x240", 30, 4, audio);
    let tl = open(&ff, &[&src]);
    let player = Player::start(&ff.ffmpeg, tl.clone(), || {}).unwrap();
    Some((dir, tl, player))
}

/// 次の Frame を待つ（Error なら失敗）
fn next_frame(p: &Player) -> Frame {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        match p.recv_timeout(Duration::from_millis(50)) {
            Some(PlayerEvent::Frame {
                time,
                width,
                height,
                rgba,
            }) => {
                return Frame {
                    time,
                    width,
                    height,
                    rgba,
                };
            }
            Some(PlayerEvent::Error(e)) => panic!("エラー: {e}"),
            _ => {}
        }
    }
    panic!("フレームが届きません");
}

/// 指定時間イベントを集める（until が真になったら終了）
fn collect(p: &Player, dur: Duration, until: impl Fn(&PlayerEvent) -> bool) -> Vec<PlayerEvent> {
    let deadline = Instant::now() + dur;
    let mut v = Vec::new();
    while Instant::now() < deadline {
        if let Some(ev) = p.recv_timeout(Duration::from_millis(20)) {
            if let PlayerEvent::Error(e) = &ev {
                panic!("エラー: {e}");
            }
            let stop = until(&ev);
            v.push(ev);
            if stop {
                break;
            }
        }
    }
    v
}

fn frame_times(events: &[PlayerEvent]) -> Vec<Timestamp> {
    events
        .iter()
        .filter_map(|e| match e {
            PlayerEvent::Frame { time, .. } => Some(*time),
            _ => None,
        })
        .collect()
}

fn fmt_times(ts: &[Timestamp]) -> String {
    ts.iter()
        .map(|t| format!("{:.3}", t.as_secs_f64()))
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn seek_returns_frame_on_boundary() {
    let Some((_dir, tl, p)) = setup("player_seek", false) else {
        return;
    };
    let fps = fps30();

    // フレーム途中の時刻は含むフレームの先頭
    p.seek(ms(1_510));
    let f = next_frame(&p);
    eprintln!("Seek(1.510) -> {:?}", f.time);
    assert_eq!(f.time, fps.frame_start(45));
    assert_eq!((f.width, f.height), (320, 240));
    assert_eq!(f.rgba.len(), 320 * 240 * 4);

    // 時刻によってフレーム内容が異なる
    p.seek(Timestamp::ZERO);
    let a = next_frame(&p);
    p.seek(ms(2_000));
    let b = next_frame(&p);
    assert_eq!((a.time, b.time), (Timestamp::ZERO, ms(2_000)));
    assert_ne!(a.rgba, b.rgba);

    // コマ送り（保持中のプロセスを読み進める）と、直接シークした結果が一致する
    p.seek(ms(1_000));
    assert_eq!(next_frame(&p).time, ms(1_000));
    p.step_frame(1);
    let stepped = next_frame(&p);
    eprintln!("StepFrame(+1) -> {:?}", stepped.time);
    assert_eq!(stepped.time, fps.frame_start(31));
    p.seek(fps.frame_start(31));
    let direct = next_frame(&p);
    assert_eq!(direct.time, fps.frame_start(31));
    assert!(
        direct.rgba == stepped.rgba,
        "シーク結果が逐次デコードと異なる"
    );
    p.step_frame(-2);
    let back = next_frame(&p);
    eprintln!("StepFrame(-2) -> {:?}", back.time);
    assert_eq!(back.time, fps.frame_start(29));

    // 末尾は最後のフレーム
    p.seek(tl.duration());
    let last = next_frame(&p);
    eprintln!("Seek(末尾 {:?}) -> {:?}", tl.duration(), last.time);
    assert_eq!(Some(last.time), player::last_frame_time(&tl));

    // スクラブ: 連続シークは最後の位置が表示される
    for i in 0..30 {
        p.seek(ms(100 * i));
    }
    let mut got = vec![next_frame(&p)];
    while let Some(ev) = p.recv_timeout(Duration::from_millis(500)) {
        if let PlayerEvent::Frame { time, rgba, .. } = ev {
            got.push(Frame {
                time,
                width: 0,
                height: 0,
                rgba,
            });
        }
    }
    eprintln!("30回のシークで受信したフレーム: {}", got.len());
    assert_eq!(got.last().unwrap().time, ms(2_900));

    // プレビュー幅の変更
    p.set_preview_width(160);
    let small = next_frame(&p);
    assert_eq!((small.width, small.height), (160, 120));
    assert_eq!(small.time, ms(2_900));
}

#[test]
fn seek_to_end_when_audio_is_longer() {
    // 音声が映像より長いと format の長さの末尾に映像フレームが無い（少し前から読んで救済する）
    let Some(ff) = tools() else { return };
    let dir = TestDir::new("player_end_audio");
    let src = dir.0.join("src.mp4");
    let status = std::process::Command::new(&ff.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=30:duration=2",
        ])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=3"])
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
        ])
        .arg(&src)
        .status()
        .unwrap();
    assert!(status.success());
    let tl = open(&ff, &[&src]);
    let p = Player::start(&ff.ffmpeg, tl.clone(), || {}).unwrap();
    p.seek(tl.duration());
    let f = next_frame(&p);
    eprintln!("長さ {:?} / 末尾シーク -> {:?}", tl.duration(), f.time);
    assert!(tl.duration() > ms(2_500));
    // rawvideo 出力は既定で CFR のため、映像終端で最後のフレームが1枚複製されることがある
    assert!(
        f.time >= fps30().frame_start(59) && f.time <= ms(2_000),
        "映像の最後のフレーム付近: {:?}",
        f.time
    );
    // 映像の無い区間を再生してもすぐ末尾になる
    p.seek(ms(2_500));
    next_frame(&p);
    p.play();
    let events = collect(&p, TIMEOUT, |e| {
        matches!(e, PlayerEvent::State(PlaybackState::Paused))
    });
    assert!(events.contains(&PlayerEvent::Ended), "{:?}", events.len());
}

#[test]
fn play_advances_time() {
    let Some((_dir, _tl, p)) = setup("player_play", false) else {
        return;
    };
    p.seek(Timestamp::ZERO);
    next_frame(&p);
    p.play();
    let started = Instant::now();
    let events = collect(&p, Duration::from_millis(600), |_| false);
    p.pause();
    let rest = collect(&p, TIMEOUT, |e| {
        matches!(e, PlayerEvent::State(PlaybackState::Paused))
    });
    let elapsed = started.elapsed();
    assert_eq!(
        events.first(),
        Some(&PlayerEvent::State(PlaybackState::Playing))
    );
    assert_eq!(
        rest.last(),
        Some(&PlayerEvent::State(PlaybackState::Paused))
    );
    let times = frame_times(&events);
    eprintln!(
        "再生 {:?} で {} フレーム: {}",
        elapsed,
        times.len(),
        fmt_times(&times)
    );
    assert!(
        times.windows(2).all(|w| w[0] < w[1]),
        "時刻が単調増加しない"
    );
    let last = *times.last().expect("再生中のフレームがない");
    assert!(last >= ms(300) && last <= ms(900), "{last:?}");
    assert!(times.len() >= 8, "フレームが少なすぎる: {}", times.len());
}

#[test]
fn play_reaches_end() {
    let Some((_dir, tl, p)) = setup("player_ended", false) else {
        return;
    };
    p.seek(ms(3_500));
    next_frame(&p);
    p.play();
    let events = collect(&p, TIMEOUT, |e| {
        matches!(e, PlayerEvent::State(PlaybackState::Paused))
    });
    let times = frame_times(&events);
    eprintln!("末尾まで: {}", fmt_times(&times));
    let ended = events
        .iter()
        .position(|e| *e == PlayerEvent::Ended)
        .expect("Ended が来ない");
    assert_eq!(
        events.get(ended + 1),
        Some(&PlayerEvent::State(PlaybackState::Paused))
    );
    assert_eq!(times.last().copied(), player::last_frame_time(&tl));

    // 末尾で再生すると先頭から
    p.play();
    let again = collect(&p, Duration::from_millis(300), |_| false);
    p.pause();
    let t = frame_times(&again);
    eprintln!("末尾から再生: {}", fmt_times(&t));
    assert_eq!(t.first(), Some(&Timestamp::ZERO));
}

#[test]
fn play_skips_disabled_segment() {
    let Some((_dir, mut tl, p)) = setup("player_skip", false) else {
        return;
    };
    tl.delete_range(ms(1_000), ms(2_500));
    // 差し替えると現在位置（先頭）を表示し直す
    p.set_timeline(tl.clone());
    assert_eq!(next_frame(&p).time, Timestamp::ZERO);
    p.seek(ms(600));
    assert_eq!(next_frame(&p).time, ms(600));
    p.play();
    let events = collect(&p, TIMEOUT, |e| {
        matches!(e, PlayerEvent::State(PlaybackState::Paused))
    });
    assert!(events.contains(&PlayerEvent::Ended));
    let times = frame_times(&events);
    eprintln!("無効区間 [1.0, 2.5) を含む再生: {}", fmt_times(&times));
    assert!(
        times.iter().all(|&t| t < ms(1_000) || t >= ms(2_500)),
        "無効区間のフレームが表示された"
    );
    assert!(times.iter().any(|&t| t < ms(1_000)));
    assert!(
        times.contains(&ms(2_500)),
        "有効区間の先頭から再開していない"
    );
    assert!(times.windows(2).all(|w| w[0] < w[1]));

    // 一時停止中は無効区間内も表示できる。そこから再生すると次の有効区間へ
    p.seek(ms(1_200));
    assert_eq!(next_frame(&p).time, ms(1_200));
    p.play();
    let first = next_frame(&p);
    eprintln!("無効区間内から再生 -> 最初のフレーム {:?}", first.time);
    assert_eq!(first.time, ms(2_500));
    p.pause();

    // タイムラインを差し替えると現在位置を表示し直す
    collect(&p, TIMEOUT, |e| {
        matches!(e, PlayerEvent::State(PlaybackState::Paused))
    });
    p.set_timeline(tl);
    assert!(next_frame(&p).time >= ms(2_500));
}

#[test]
fn drop_while_playing_returns() {
    let Some((_dir, _tl, p)) = setup("player_drop", false) else {
        return;
    };
    p.seek(Timestamp::ZERO);
    p.play();
    std::thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    drop(p);
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
}

#[test]
fn shuttle_double_speed() {
    let Some((_dir, _tl, p)) = setup("player_2x", false) else {
        return;
    };
    p.seek(Timestamp::ZERO);
    next_frame(&p);
    p.shuttle(2);
    let started = Instant::now();
    let events = collect(&p, Duration::from_millis(600), |_| false);
    p.pause();
    collect(&p, TIMEOUT, |e| {
        matches!(e, PlayerEvent::State(PlaybackState::Paused))
    });
    let elapsed = started.elapsed();
    let times = frame_times(&events);
    eprintln!("2x 再生 {:?}: {}", elapsed, fmt_times(&times));
    assert!(
        times.windows(2).all(|w| w[0] < w[1]),
        "時刻が単調増加しない"
    );
    // 2 フレームおきに間引いている
    let fd = fps30().frame_duration();
    assert!(
        times
            .windows(2)
            .all(|w| w[1] - w[0] >= fd + fd - Timestamp(2)),
        "間引かれていない"
    );
    // 実時間の約 2 倍進む（起動の遅れを見込んで下限は緩め）
    let last = *times.last().expect("再生中のフレームがない");
    assert!(last >= ms(700) && last <= ms(1_800), "{last:?}");
}

#[test]
fn shuttle_reverse_to_start() {
    let Some((_dir, _tl, p)) = setup("player_reverse", false) else {
        return;
    };
    p.seek(ms(1_500));
    assert_eq!(next_frame(&p).time, ms(1_500));
    p.shuttle(-1);
    let events = collect(&p, TIMEOUT, |e| {
        matches!(e, PlayerEvent::State(PlaybackState::Paused))
    });
    let times = frame_times(&events);
    eprintln!("逆再生: {}", fmt_times(&times));
    assert_eq!(
        events.first(),
        Some(&PlayerEvent::State(PlaybackState::Playing))
    );
    assert!(events.contains(&PlayerEvent::Ended), "先頭で止まらない");
    assert!(
        times.windows(2).all(|w| w[0] > w[1]),
        "時刻が単調減少しない"
    );
    assert!(
        times.first().is_some_and(|&t| t < ms(1_500)),
        "現在フレームより前から始まらない"
    );
    assert_eq!(
        times.last(),
        Some(&Timestamp::ZERO),
        "先頭のフレームを表示していない"
    );

    // 先頭で逆再生しても再生状態にならない（停止状態を通知する）
    p.shuttle(-1);
    let again = collect(&p, Duration::from_millis(300), |_| false);
    assert_eq!(again, vec![PlayerEvent::State(PlaybackState::Paused)]);
}

#[test]
fn shuttle_reverse_skips_disabled_segment() {
    let Some((_dir, mut tl, p)) = setup("player_reverse_skip", false) else {
        return;
    };
    tl.delete_range(ms(1_000), ms(2_500));
    p.set_timeline(tl);
    next_frame(&p);
    p.seek(ms(3_000));
    assert_eq!(next_frame(&p).time, ms(3_000));
    p.shuttle(-4);
    let events = collect(&p, TIMEOUT, |e| {
        matches!(e, PlayerEvent::State(PlaybackState::Paused))
    });
    let times = frame_times(&events);
    eprintln!(
        "無効区間 [1.0, 2.5) を含む 4x 逆再生: {}",
        fmt_times(&times)
    );
    assert!(events.contains(&PlayerEvent::Ended));
    assert!(
        times.windows(2).all(|w| w[0] > w[1]),
        "時刻が単調減少しない"
    );
    assert!(
        times.iter().all(|&t| t < ms(1_000) || t >= ms(2_500)),
        "無効区間のフレームが表示された"
    );
    assert!(times.iter().any(|&t| t >= ms(2_500)));
    assert!(
        times.iter().any(|&t| t < ms(1_000)),
        "直前の有効区間へ移っていない"
    );
}

#[test]
fn shuttle_switches_direction_while_playing() {
    let Some((_dir, _tl, p)) = setup("player_switch", false) else {
        return;
    };
    p.seek(ms(1_000));
    next_frame(&p);
    p.shuttle(1);
    let forward = frame_times(&collect(&p, Duration::from_millis(400), |_| false));
    p.shuttle(-2);
    let backward = collect(&p, Duration::from_millis(600), |_| false);
    p.pause();
    let backward = frame_times(&backward);
    eprintln!("順 {} / 逆 {}", fmt_times(&forward), fmt_times(&backward));
    let turn = *forward.last().expect("順再生のフレームがない");
    assert!(turn > ms(1_000));
    // 切り替え直後に溜まっていた順方向の1枚が届くことがあるので、それ以降で判定
    let back: Vec<_> = backward.into_iter().skip_while(|&t| t >= turn).collect();
    assert!(back.len() >= 3, "逆再生のフレームが少ない");
    assert!(back.windows(2).all(|w| w[0] > w[1]), "時刻が単調減少しない");
}
