//! 拟人化鼠标轨迹:三次贝塞尔路径 + 缓入缓出速度分布 + 高斯抖动。
//! 纯数学模块(无 Win32 依赖),供前台输入注入的移动插值使用。
//! 全部随机性由显式传入的伪随机源产生:运行时用时间播种,单测注入固定
//! 种子完全复现。不引入 rand 依赖 —— xorshift64* + Box-Muller 已足够。

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};

/// 二维点(浮点数学坐标)。
pub type Point2 = (f64, f64);

// ---------------------------------------------------------------------------
// 伪随机源
// ---------------------------------------------------------------------------

/// xorshift64*:周期 2^64-1,质量对"拟人抖动"绰绰有余,零依赖。
#[derive(Clone, Copy, Debug)]
pub struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    /// 种子 0 会退化为全零序列,内部钳到 1。
    pub fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    /// 均匀 [0, 1) 的 f64。
    pub fn next_f64(&mut self) -> f64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        ((v >> 11) as f64) / (1u64 << 53) as f64
    }

    /// 标准正态分布 N(0,1)(Box-Muller 极坐标法),钳制到 ±4σ 保证有界:
    /// 轨迹抖动绝不能因极端样本跳出一屏。
    pub fn next_gaussian(&mut self) -> f64 {
        loop {
            let u = 2.0 * self.next_f64() - 1.0;
            let v = 2.0 * self.next_f64() - 1.0;
            let s = u * u + v * v;
            if s > 0.0 && s < 1.0 {
                let f = (-2.0 * s.ln() / s).sqrt();
                return (u * f).clamp(-4.0, 4.0);
            }
        }
    }

    /// 均匀闭区间 [min, max] 的整数。
    pub fn next_range_i32(&mut self, min: i32, max: i32) -> i32 {
        debug_assert!(min <= max);
        let span = (max as i64 - min as i64 + 1) as f64;
        (self.next_f64() * span).floor() as i64 as i32 + min
    }
}

// 线程局部随机源:时间 + 进程级原子计数播种,不同线程互不干扰、无锁竞争
// (ThreadId::as_u64 尚不稳定,计数器的低位混合已足以区分线程)。
static RNG_COUNTER: AtomicU64 = AtomicU64::new(1);
thread_local! {
    static RNG: RefCell<XorShift64> = RefCell::new(XorShift64::new({
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9);
        nanos ^ RNG_COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9E37_79B9)
    }));
}

/// 在线程局部随机源上执行一次操作(调用方在闭包内完成全部抽样)。
pub fn with_rng<T>(f: impl FnOnce(&mut XorShift64) -> T) -> T {
    RNG.with(|rng| f(&mut rng.borrow_mut()))
}

// ---------------------------------------------------------------------------
// 贝塞尔曲线与轨迹
// ---------------------------------------------------------------------------

/// 控制点垂直偏移 σ = 距离 × curvature(距离很小时偏移自然趋零)。
/// 0.08 是拟人弯曲的温和档:路径弧高约为距离的 4-12%。
pub const DEFAULT_CURVATURE: f64 = 0.08;
/// 每步抖动 σ(像素),输出钳制 ±2 像素 —— 拟人微调,不破坏落点。
pub const DEFAULT_JITTER_PX: f64 = 0.9;
/// 单点抖动硬上限(像素)。
const JITTER_CLAMP_PX: f64 = 2.0;
/// 控制点垂直偏移上限 = 距离 × 0.4(超弧会穿出起终连线太远)。
const CURVATURE_CLAMP_FACTOR: f64 = 0.4;

/// 三次贝塞尔曲线:P0(起点)、P1/P2(控制点)、P3(终点)。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CubicBezier {
    pub p0: Point2,
    pub p1: Point2,
    pub p2: Point2,
    pub p3: Point2,
}

impl CubicBezier {
    /// t ∈ [0,1] 处的曲线点(伯恩斯坦形式)。t 越界按端点钳制。
    pub fn point(&self, t: f64) -> Point2 {
        let t = t.clamp(0.0, 1.0);
        let u = 1.0 - t;
        let a = u * u * u;
        let b = 3.0 * u * u * t;
        let c = 3.0 * u * t * t;
        let d = t * t * t;
        (
            a * self.p0.0 + b * self.p1.0 + c * self.p2.0 + d * self.p3.0,
            a * self.p0.1 + b * self.p1.1 + c * self.p2.1 + d * self.p3.1,
        )
    }

    /// 生成带随机弯曲的曲线:控制点位于起终连线的 1/3、2/3 处,垂直方向
    /// 各加一个高斯偏移(σ = 距离 × curvature,上限 距离 × 0.4)。
    /// 弯曲方向随机,两条控制点偏移独立 —— 路径呈自然的 S 或 C 弧。
    pub fn with_random_curvature(
        from: Point2,
        to: Point2,
        rng: &mut XorShift64,
        curvature: f64,
    ) -> Self {
        let dx = to.0 - from.0;
        let dy = to.1 - from.1;
        let dist = (dx * dx + dy * dy).sqrt();
        // 单位垂直向量(起终连线顺时针旋转 90°);距离为零时退化为原地。
        let (nx, ny) = if dist > 1e-6 {
            (-dy / dist, dx / dist)
        } else {
            (0.0, 0.0)
        };
        let clamp = dist * CURVATURE_CLAMP_FACTOR;
        let sigma = dist * curvature.clamp(0.0, 1.0);
        let off1 = (rng.next_gaussian() * sigma).clamp(-clamp, clamp);
        let off2 = (rng.next_gaussian() * sigma).clamp(-clamp, clamp);
        CubicBezier {
            p0: from,
            p1: (from.0 + dx / 3.0 + nx * off1, from.1 + dy / 3.0 + ny * off1),
            p2: (to.0 - dx / 3.0 + nx * off2, to.1 - dy / 3.0 + ny * off2),
            p3: to,
        }
    }
}

/// 缓入缓出参数化(平滑阶跃 3t²-2t³):启动加速、中段近似匀速、临近目标减速。
/// 在曲线上按该分布采样,步点自然向两端聚拢 —— 与真人"瞄准-落点"节奏一致。
pub fn ease_in_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// 生成 steps 个点的拟人化轨迹(含终点):
/// 贝塞尔路径 + 缓入缓出分布 + 每点高斯抖动(钳制 ±2px)。
/// 终点强制精确落在目标坐标 —— 纯 move 动作必须确定性落地,
/// 后续 click 动作另有 SetCursorPos 精确定位,落点误差不被放大。
/// 返回整型物理像素坐标。
pub fn human_trajectory(
    from: Point2,
    to: Point2,
    steps: usize,
    rng: &mut XorShift64,
    curvature: f64,
    jitter_px: f64,
) -> Vec<(i32, i32)> {
    debug_assert!(steps >= 1);
    let curve = CubicBezier::with_random_curvature(from, to, rng, curvature);
    let mut points = Vec::with_capacity(steps);
    for step in 1..=steps {
        let t = ease_in_out(step as f64 / steps as f64);
        let (mut x, mut y) = curve.point(t);
        x += (rng.next_gaussian() * jitter_px).clamp(-JITTER_CLAMP_PX, JITTER_CLAMP_PX);
        y += (rng.next_gaussian() * jitter_px).clamp(-JITTER_CLAMP_PX, JITTER_CLAMP_PX);
        points.push((x.round() as i32, y.round() as i32));
    }
    // 终点精确落地(替换最后一点,丢弃其抖动)。
    *points.last_mut().expect("steps >= 1") = (to.0.round() as i32, to.1.round() as i32);
    points
}

/// 拟人化动作间隔:均值 mean_ms,高斯 σ = 0.15×mean,钳制 [0.5, 1.2]×mean。
/// 钳制保证计划延迟最多比预算高 20%,60 秒总预算护栏不被抖动击穿。
pub fn human_delay_ms(rng: &mut XorShift64, mean_ms: u64) -> u64 {
    if mean_ms == 0 {
        return 0;
    }
    let sigma = (mean_ms as f64 * 0.15).max(1.0);
    let value = mean_ms as f64 + rng.next_gaussian() * sigma;
    (value.round() as i64).clamp((mean_ms as f64 * 0.5) as i64, (mean_ms as f64 * 1.2) as i64)
        as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xorshift_is_deterministic_and_bounded() {
        let mut a = XorShift64::new(42);
        let mut b = XorShift64::new(42);
        for _ in 0..1000 {
            let va = a.next_f64();
            assert!((0.0..1.0).contains(&va));
            assert_eq!(va, b.next_f64(), "相同种子序列必须一致");
        }
    }

    #[test]
    fn gaussian_is_zero_mean_and_clamped() {
        let mut rng = XorShift64::new(7);
        let mut sum = 0.0;
        const N: usize = 200_000;
        for _ in 0..N {
            let v = rng.next_gaussian();
            assert!((-4.0..=4.0).contains(&v), "高斯样本钳制 ±4σ");
            sum += v;
        }
        assert!((sum / N as f64).abs() < 0.05, "样本均值应趋近 0");
    }

    #[test]
    fn bezier_endpoints_are_exact() {
        let curve = CubicBezier {
            p0: (10.0, 20.0),
            p1: (30.0, 5.0),
            p2: (-5.0, 40.0),
            p3: (100.0, 80.0),
        };
        assert_eq!(curve.point(0.0), curve.p0);
        assert_eq!(curve.point(1.0), curve.p3);
        // t 越界钳制
        assert_eq!(curve.point(-1.0), curve.p0);
        assert_eq!(curve.point(2.0), curve.p3);
        // 中点 = 伯恩斯坦组合
        let mid = curve.point(0.5);
        let expect = (
            (10.0 + 3.0 * 30.0 + 3.0 * -5.0 + 100.0) / 8.0,
            (20.0 + 3.0 * 5.0 + 3.0 * 40.0 + 80.0) / 8.0,
        );
        assert!((mid.0 - expect.0).abs() < 1e-9 && (mid.1 - expect.1).abs() < 1e-9);
    }

    #[test]
    fn random_curvature_preserves_endpoints_and_bounds_controls() {
        let mut rng = XorShift64::new(3);
        for _ in 0..200 {
            let curve =
                CubicBezier::with_random_curvature((0.0, 0.0), (300.0, 400.0), &mut rng, 0.08);
            assert_eq!(curve.p0, (0.0, 0.0));
            assert_eq!(curve.p3, (300.0, 400.0));
            // 控制点在起终点 1/3 处 ± 0.4×距离的包络内
            let dist = 500.0;
            assert!((curve.p1.0 - 100.0).abs() <= dist * 0.4 + 1e-9);
            assert!((curve.p2.0 - 200.0).abs() <= dist * 0.4 + 1e-9);
        }
        // 零距离:原地轨迹不产生 NaN
        let curve = CubicBezier::with_random_curvature((5.0, 5.0), (5.0, 5.0), &mut rng, 0.08);
        assert!(curve.point(0.5).0.is_finite());
        assert_eq!(curve.point(0.5), (5.0, 5.0));
    }

    #[test]
    fn ease_in_out_is_monotonic_smoothstep() {
        assert_eq!(ease_in_out(0.0), 0.0);
        assert_eq!(ease_in_out(1.0), 1.0);
        assert!((ease_in_out(0.5) - 0.5).abs() < 1e-9);
        let mut prev = -1.0;
        for i in 0..=100 {
            let v = ease_in_out(i as f64 / 100.0);
            assert!(v >= prev, "缓入缓出必须单调");
            prev = v;
        }
    }

    #[test]
    fn trajectory_lands_exactly_and_stays_in_bounds() {
        let mut rng = XorShift64::new(11);
        let path = human_trajectory((10.0, 10.0), (210.0, 110.0), 12, &mut rng, 0.08, 0.9);
        assert_eq!(path.len(), 12);
        // 终点精确落地
        assert_eq!(*path.last().unwrap(), (210, 110));
        // 全程在 [起点-抖动, 终点+弧线余量] 的包络内:起终连线 ± (0.4×距离 + 抖动)
        let margin = (210.0f64 * 0.4).ceil() as i32 + 3;
        for &(x, y) in &path {
            assert!((10 - margin..=210 + margin).contains(&x), "x={x} 超出包络");
            assert!((10 - margin..=110 + margin).contains(&y), "y={y} 超出包络");
        }
    }

    #[test]
    fn trajectory_is_deterministic_for_fixed_seed() {
        let run = |seed| {
            human_trajectory(
                (0.0, 0.0),
                (500.0, 300.0),
                30,
                &mut XorShift64::new(seed),
                0.08,
                0.9,
            )
        };
        assert_eq!(run(99), run(99), "固定种子轨迹可复现");
        assert_ne!(run(99), run(100), "不同种子轨迹应有差异");
    }

    #[test]
    fn single_step_trajectory_is_the_destination() {
        let mut rng = XorShift64::new(5);
        let path = human_trajectory((0.0, 0.0), (7.3, 9.6), 1, &mut rng, 0.08, 0.9);
        assert_eq!(path, vec![(7, 10)]);
    }

    #[test]
    fn delay_jitter_stays_within_budget_window() {
        let mut rng = XorShift64::new(21);
        for _ in 0..10_000 {
            let d = human_delay_ms(&mut rng, 100);
            assert!((50..=120).contains(&d), "d={d} 超出 [0.5,1.2]×mean");
        }
        assert_eq!(human_delay_ms(&mut rng, 0), 0);
    }
}
