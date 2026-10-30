use std::fmt;

/// 1秒あたりのマイクロ秒数
pub const MICROS_PER_SEC: i64 = 1_000_000;

/// マイクロ秒単位の時刻（浮動小数の誤差を避ける）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub i64);

impl Timestamp {
    pub const ZERO: Timestamp = Timestamp(0);

    pub const fn from_micros(us: i64) -> Self {
        Timestamp(us)
    }

    pub const fn from_millis(ms: i64) -> Self {
        Timestamp(ms * 1_000)
    }

    pub const fn from_secs(s: i64) -> Self {
        Timestamp(s * MICROS_PER_SEC)
    }

    pub const fn as_micros(self) -> i64 {
        self.0
    }

    /// 表示・UI 用の秒（計算には使わない）
    pub fn as_secs_f64(self) -> f64 {
        self.0 as f64 / MICROS_PER_SEC as f64
    }

    pub fn clamp_to(self, lo: Timestamp, hi: Timestamp) -> Timestamp {
        Timestamp(self.0.clamp(lo.0, hi.0))
    }

    /// `HH:MM:SS.mmm` 形式（ミリ秒未満は切り捨て）
    pub fn to_hms(self) -> String {
        let sign = if self.0 < 0 { "-" } else { "" };
        let total_ms = self.0.unsigned_abs() / 1_000;
        let ms = total_ms % 1_000;
        let total_s = total_ms / 1_000;
        let s = total_s % 60;
        let m = (total_s / 60) % 60;
        let h = total_s / 3_600;
        format!("{sign}{h:02}:{m:02}:{s:02}.{ms:03}")
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hms())
    }
}

impl std::ops::Add for Timestamp {
    type Output = Timestamp;
    fn add(self, rhs: Timestamp) -> Timestamp {
        Timestamp(self.0 + rhs.0)
    }
}

impl std::ops::Sub for Timestamp {
    type Output = Timestamp;
    fn sub(self, rhs: Timestamp) -> Timestamp {
        Timestamp(self.0 - rhs.0)
    }
}

impl std::ops::AddAssign for Timestamp {
    fn add_assign(&mut self, rhs: Timestamp) {
        self.0 += rhs.0;
    }
}

impl std::ops::SubAssign for Timestamp {
    fn sub_assign(&mut self, rhs: Timestamp) {
        self.0 -= rhs.0;
    }
}

/// 有理数（fps 等）。常に約分済み・分母は正
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rational {
    pub num: i64,
    pub den: i64,
}

impl Rational {
    /// 分母 0 の場合は None
    pub fn new(num: i64, den: i64) -> Option<Self> {
        if den == 0 {
            return None;
        }
        let g = gcd(num.unsigned_abs(), den.unsigned_abs()).max(1) as i64;
        let sign = if den < 0 { -1 } else { 1 };
        Some(Rational {
            num: sign * num / g,
            den: sign * den / g,
        })
    }

    /// 正の値か（fps として使えるか）
    pub fn is_positive(self) -> bool {
        self.num > 0 && self.den > 0
    }

    /// 表示用
    pub fn as_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    /// 時刻 t を含むフレーム番号（floor）。fps = num/den
    pub fn frame_at(self, t: Timestamp) -> i64 {
        let n = t.0 as i128 * self.num as i128;
        let d = self.den as i128 * MICROS_PER_SEC as i128;
        n.div_euclid(d) as i64
    }

    /// フレーム n の開始時刻。`frame_at(frame_start(n)) == n` となる最小のマイクロ秒（ceil）
    pub fn frame_start(self, frame: i64) -> Timestamp {
        let n = frame as i128 * self.den as i128 * MICROS_PER_SEC as i128;
        let d = self.num as i128;
        Timestamp(div_ceil_i128(n, d) as i64)
    }

    /// 最寄りのフレーム番号（四捨五入）
    pub fn nearest_frame(self, t: Timestamp) -> i64 {
        let n = t.0 as i128 * self.num as i128;
        let d = self.den as i128 * MICROS_PER_SEC as i128;
        // floor((2n + d) / 2d) で四捨五入
        (2 * n + d).div_euclid(2 * d) as i64
    }

    /// 最寄りのフレーム開始時刻へスナップ
    pub fn snap(self, t: Timestamp) -> Timestamp {
        self.frame_start(self.nearest_frame(t))
    }

    /// 1フレームの長さ（切り上げマイクロ秒）
    pub fn frame_duration(self) -> Timestamp {
        self.frame_start(1)
    }
}

impl fmt::Display for Rational {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn div_ceil_i128(n: i128, d: i128) -> i128 {
    -((-n).div_euclid(d))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ntsc() -> Rational {
        Rational::new(30000, 1001).unwrap()
    }

    #[test]
    fn rational_new_reduces_and_normalizes_sign() {
        assert_eq!(Rational::new(60, 2), Some(Rational { num: 30, den: 1 }));
        assert_eq!(Rational::new(30, -1), Some(Rational { num: -30, den: 1 }));
        assert_eq!(Rational::new(1, 0), None);
        assert_eq!(Rational::new(0, 5), Some(Rational { num: 0, den: 1 }));
        assert!(!Rational::new(0, 1).unwrap().is_positive());
    }

    #[test]
    fn frame_at_integer_fps() {
        let fps = Rational::new(25, 1).unwrap();
        assert_eq!(fps.frame_at(Timestamp(0)), 0);
        assert_eq!(fps.frame_at(Timestamp(39_999)), 0);
        assert_eq!(fps.frame_at(Timestamp(40_000)), 1);
        assert_eq!(fps.frame_at(Timestamp::from_secs(10)), 250);
        assert_eq!(fps.frame_start(250), Timestamp::from_secs(10));
    }

    #[test]
    fn frame_roundtrip_ntsc_no_drift() {
        let fps = ntsc();
        // 1時間分を超えるフレームでも往復が一致する
        for n in [0i64, 1, 2, 29, 30, 1000, 107_892, 107_893, 1_000_000] {
            let t = fps.frame_start(n);
            assert_eq!(fps.frame_at(t), n, "frame {n}");
            // 1マイクロ秒前は前フレーム
            if n > 0 {
                assert_eq!(fps.frame_at(Timestamp(t.0 - 1)), n - 1, "frame {n}");
            }
        }
        // 30000 フレーム = 1001 秒ちょうど
        assert_eq!(fps.frame_start(30000), Timestamp::from_secs(1001));
        // 1フレーム = 33366.66..us → 切り上げ 33367
        assert_eq!(fps.frame_duration(), Timestamp(33_367));
    }

    #[test]
    fn snap_to_nearest_frame() {
        let fps = ntsc();
        let f1 = fps.frame_start(1); // 33367
        assert_eq!(fps.snap(Timestamp(0)), Timestamp(0));
        assert_eq!(fps.snap(Timestamp(16_000)), Timestamp(0));
        assert_eq!(fps.snap(Timestamp(17_000)), f1);
        assert_eq!(fps.snap(f1), f1);
        let fps25 = Rational::new(25, 1).unwrap();
        // ちょうど半分は切り上げ
        assert_eq!(fps25.snap(Timestamp(20_000)), Timestamp(40_000));
        assert_eq!(fps25.snap(Timestamp(19_999)), Timestamp(0));
    }

    #[test]
    fn negative_time_frame() {
        let fps = Rational::new(25, 1).unwrap();
        assert_eq!(fps.frame_at(Timestamp(-1)), -1);
        assert_eq!(fps.frame_start(-1), Timestamp(-40_000));
    }

    #[test]
    fn format_hms() {
        assert_eq!(Timestamp(0).to_hms(), "00:00:00.000");
        assert_eq!(Timestamp(83_456_789).to_hms(), "00:01:23.456");
        assert_eq!(
            Timestamp::from_secs(36_000 + 59 * 60 + 59).to_hms(),
            "10:59:59.000"
        );
        assert_eq!(Timestamp(-1_500_000).to_hms(), "-00:00:01.500");
        assert_eq!(Timestamp(999).to_hms(), "00:00:00.000");
        // 29.97fps の 1時間目付近のフレーム
        let t = ntsc().frame_start(107_892);
        assert_eq!(t.to_hms(), "00:59:59.996");
        assert_eq!(format!("{}", Timestamp::from_millis(1_234)), "00:00:01.234");
    }

    #[test]
    fn timestamp_ops() {
        let a = Timestamp::from_secs(3);
        let b = Timestamp::from_millis(500);
        assert_eq!(a + b, Timestamp(3_500_000));
        assert_eq!(a - b, Timestamp(2_500_000));
        assert_eq!(
            Timestamp(5).clamp_to(Timestamp(0), Timestamp(3)),
            Timestamp(3)
        );
    }
}
