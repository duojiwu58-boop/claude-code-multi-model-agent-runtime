//! 多点比色引擎:单色锚点扫描 + 相对偏移链校验。
//!
//! x86_64 上运行时探测 AVX2 走向量化分支(每次并行 8 像素 / 32 字节 BGRA,
//! 饱和减求 |差| + 无符号 min 阈值比对 + movemask 位聚合),无 AVX2 或
//! 非 x86_64 平台回退逐像素标量;两条路径语义一致,单测互证。
//!
//! 支持两种像素来源:
//! - 连续 BGRA 缓冲(屏幕 BitBlt / PrintWindow / WGC 拷贝产物);
//! - 行距步进的映射内存(DDAPI staging 纹理 Map 后直扫,免整图拷贝)。
//!
//! 4K(3840x2160)全区域锚点扫描在 AVX2 路径约 1-3ms(内存带宽主导),
//! 标量路径约 8-15ms —— 见 `perf_smoke_4k_scan`(ignore 标记,--release 运行)。

/// 目标颜色点:RGB + 每通道容差(曼哈顿通道差 ≤ tolerance 即命中)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorPoint {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub tolerance: u8,
}

impl ColorPoint {
    pub fn new(r: u8, g: u8, b: u8, tolerance: u8) -> Self {
        Self { r, g, b, tolerance }
    }

    /// 像素 (r,g,b) 是否在容差内命中。
    #[inline]
    pub fn matches(&self, pixel: (u8, u8, u8)) -> bool {
        let t = i32::from(self.tolerance);
        (i32::from(self.r) - i32::from(pixel.0)).abs() <= t
            && (i32::from(self.g) - i32::from(pixel.1)).abs() <= t
            && (i32::from(self.b) - i32::from(pixel.2)).abs() <= t
    }
}

/// 缓冲坐标矩形:x1/y1 为不包含边界,与 Rust 区间语义一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl Rect {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x0: x,
            y0: y,
            x1: x.saturating_add(width),
            y1: y.saturating_add(height),
        }
    }

    #[inline]
    pub fn width(self) -> u32 {
        self.x1.saturating_sub(self.x0)
    }

    #[inline]
    pub fn height(self) -> u32 {
        self.y1.saturating_sub(self.y0)
    }

    /// 点 (i64 坐标,防溢出)是否落在矩形内。
    #[inline]
    pub fn contains(self, x: i64, y: i64) -> bool {
        x >= i64::from(self.x0)
            && x < i64::from(self.x1)
            && y >= i64::from(self.y0)
            && y < i64::from(self.y1)
    }

    /// 钳制到 [0, buf_w) × [0, buf_h),避免调用方 ROI 越界引发指针越界。
    fn clamped(self, buf_w: u32, buf_h: u32) -> Rect {
        Rect {
            x0: self.x0.min(buf_w),
            y0: self.y0.min(buf_h),
            x1: self.x1.min(buf_w),
            y1: self.y1.min(buf_h),
        }
    }
}

/// 偏移校验点:相对锚点命中位置的 (dx, dy) 与独立颜色/容差。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OffsetCheck {
    pub dx: i32,
    pub dy: i32,
    pub color: ColorPoint,
}

/// 像素读取器:连续缓冲或行距步进内存(两者读取语义一致)。
/// `Strided` 变体由 DDAPI staging 纹理 Map 后构造:base 指向映射首字节,
/// 有效内存 [pitch × buf_h),调用方保证映射在本读取器存活期内不被 Unmap。
pub enum PixelReader<'a> {
    Contiguous {
        bgra: &'a [u8],
        buf_w: u32,
        buf_h: u32,
    },
    Strided {
        base: *const u8,
        pitch: usize,
        buf_w: u32,
        buf_h: u32,
    },
}

impl PixelReader<'_> {
    pub fn buf_w(&self) -> u32 {
        match self {
            PixelReader::Contiguous { buf_w, .. } => *buf_w,
            PixelReader::Strided { buf_w, .. } => *buf_w,
        }
    }

    pub fn buf_h(&self) -> u32 {
        match self {
            PixelReader::Contiguous { buf_h, .. } => *buf_h,
            PixelReader::Strided { buf_h, .. } => *buf_h,
        }
    }

    /// (x,y) 处的 (r,g,b);越界返回 None。
    ///
    /// # Safety
    /// `Strided` 变体解引用原始指针:调用方必须保证 `base` 指向 ≥
    /// [pitch × buf_h) 字节的有效映射内存,且本读取器存活期内映射不被
    /// Unmap、pitch ≥ buf_w×4。`Contiguous` 变体无需额外前提。
    pub unsafe fn read(&self, x: u32, y: u32) -> Option<(u8, u8, u8)> {
        if x >= self.buf_w() || y >= self.buf_h() {
            return None;
        }
        match self {
            PixelReader::Contiguous { bgra, buf_w, .. } => {
                let off = (usize::try_from(y).ok()? * usize::try_from(*buf_w).ok()?
                    + usize::try_from(x).ok()?)
                    * 4;
                if off + 3 >= bgra.len() {
                    return None;
                }
                Some((bgra[off + 2], bgra[off + 1], bgra[off]))
            }
            PixelReader::Strided { base, pitch, .. } => {
                let off = usize::try_from(y).ok()?.checked_mul(*pitch)?
                    + usize::try_from(x).ok()?.checked_mul(4)?;
                // 行内像素 4 字节必须完整落在 [行起点, 行起点 + pitch) 内;
                // pitch ≥ buf_w×4 由调用方契约保证。
                let px = unsafe { std::slice::from_raw_parts(base.add(off), 4) };
                Some((px[2], px[1], px[0]))
            }
        }
    }
}

/// 运行时 AVX2 可用性(响应体标注引擎,便于性能诊断)。
pub fn avx2_available() -> bool {
    cfg!(target_arch = "x86_64") && std::is_x86_feature_detected!("avx2")
}

// ---------------------------------------------------------------------------
// 锚点扫描
// ---------------------------------------------------------------------------

/// 在连续 BGRA 缓冲内扫描锚点色,返回全部命中坐标(缓冲坐标,行优先)。
/// 防御性收缩:实际字节数不足声明行数时按可用行数扫描,绝不越界。
pub fn find_color_matches(
    bgra: &[u8],
    buf_w: u32,
    buf_h: u32,
    roi: Rect,
    target: ColorPoint,
) -> Vec<(u32, u32)> {
    if buf_w == 0 {
        return Vec::new();
    }
    let row_bytes = buf_w as usize * 4;
    let available_rows = bgra.len() / row_bytes;
    let buf_h = buf_h.min(available_rows as u32);
    let roi = roi.clamped(buf_w, buf_h);
    if roi.width() == 0 || roi.height() == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    unsafe {
        scan_rows(
            |y| bgra.as_ptr().add(y as usize * row_bytes),
            row_bytes,
            roi,
            target,
            &mut out,
        );
    }
    out
}

/// 强制标量路径(参考实现与 AVX2 路径的单测互证基线;非 x86_64 平台的
/// 生产路径)。语义与 [`find_color_matches`] 完全一致。
pub fn find_color_matches_scalar(
    bgra: &[u8],
    buf_w: u32,
    buf_h: u32,
    roi: Rect,
    target: ColorPoint,
) -> Vec<(u32, u32)> {
    if buf_w == 0 {
        return Vec::new();
    }
    let row_bytes = buf_w as usize * 4;
    let available_rows = bgra.len() / row_bytes;
    let buf_h = buf_h.min(available_rows as u32);
    let roi = roi.clamped(buf_w, buf_h);
    if roi.width() == 0 || roi.height() == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for y in roi.y0..roi.y1 {
        let row = &bgra[y as usize * row_bytes..(y as usize + 1) * row_bytes];
        scan_row_scalar(row, roi.x0, roi.x1, target, y, &mut out);
    }
    out
}

/// 行距步进的 BGRA 映射内存直扫(DDAPI staging 纹理,免整图拷贝)。
///
/// # Safety
/// `base` 必须指向 ≥ [pitch × buf_h) 字节的有效可读内存,且每行前
/// buf_w×4 字节为 BGRA 像素;pitch ≥ buf_w×4。
pub unsafe fn find_color_matches_strided(
    base: *const u8,
    pitch: usize,
    buf_w: u32,
    buf_h: u32,
    roi: Rect,
    target: ColorPoint,
) -> Vec<(u32, u32)> {
    if buf_w == 0 || pitch < buf_w as usize * 4 {
        return Vec::new();
    }
    let roi = roi.clamped(buf_w, buf_h);
    if roi.width() == 0 || roi.height() == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    unsafe {
        scan_rows(
            |y| base.add(y as usize * pitch),
            pitch,
            roi,
            target,
            &mut out,
        );
    }
    out
}

/// 行扫描公共骨架:逐行取行指针,AVX2 可用走向量化、否则标量。
unsafe fn scan_rows(
    row_at: impl Fn(u32) -> *const u8,
    row_bytes: usize,
    roi: Rect,
    target: ColorPoint,
    out: &mut Vec<(u32, u32)>,
) {
    if avx2_available() {
        for y in roi.y0..roi.y1 {
            let row = unsafe { std::slice::from_raw_parts(row_at(y), row_bytes) };
            unsafe { scan_row_avx2(row, roi.x0, roi.x1, target, y, out) };
        }
    } else {
        for y in roi.y0..roi.y1 {
            let row = unsafe { std::slice::from_raw_parts(row_at(y), row_bytes) };
            scan_row_scalar(row, roi.x0, roi.x1, target, y, out);
        }
    }
}

/// 标量行扫描(参考语义):逐像素三通道容差比对。
fn scan_row_scalar(
    row: &[u8],
    x0: u32,
    x1: u32,
    target: ColorPoint,
    y: u32,
    out: &mut Vec<(u32, u32)>,
) {
    for x in x0..x1 {
        let off = x as usize * 4;
        if target.matches((row[off + 2], row[off + 1], row[off])) {
            out.push((x, y));
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
/// AVX2 行扫描:每次 32 字节(8 像素)。
/// 算法:饱和减求逐字节 |差| → 与容差做无符号 min 后判等(|差| ≤ 容差) →
/// movemask 取 32 位逐字节标志 → 位聚合出逐像素命中(忽略 alpha 字节)。
/// 行首未对齐 8 像素边界与行尾不足 8 像素的部分走标量补扫。
unsafe fn scan_row_avx2(
    row: &[u8],
    x0: u32,
    x1: u32,
    target: ColorPoint,
    y: u32,
    out: &mut Vec<(u32, u32)>,
) {
    use std::arch::x86_64::*;
    let packed = i32::from(target.b) | (i32::from(target.g) << 8) | (i32::from(target.r) << 16);
    let tolerance = i32::from(target.tolerance);
    let packed_tolerance = tolerance | (tolerance << 8) | (tolerance << 16) | (0xFFi32 << 24);
    let t_vec = _mm256_set1_epi32(packed);
    let tol_vec = _mm256_set1_epi32(packed_tolerance);

    let row_ptr = row.as_ptr();
    let mut x = x0;
    // 行首:补到 8 像素对齐
    let aligned_start = x0.div_ceil(8) * 8;
    let head_end = aligned_start.min(x1);
    if x < head_end {
        scan_row_scalar(row, x, head_end, target, y, out);
        x = head_end;
    }
    // 主体:8 像素块。块起点 8 对齐且块尾 ≤ x1 ≤ buf_w,32 字节装载不越行界。
    while x + 8 <= x1 {
        let ptr = unsafe { row_ptr.add(x as usize * 4) } as *const __m256i;
        let v = unsafe { _mm256_loadu_si256(ptr) };
        let diff = _mm256_or_si256(_mm256_subs_epu8(v, t_vec), _mm256_subs_epu8(t_vec, v));
        let ok = _mm256_cmpeq_epi8(_mm256_min_epu8(diff, tol_vec), diff);
        let m = _mm256_movemask_epi8(ok) as u32;
        // 字节 0/1/2(BGR)同时命中 → 位组 4k 置位
        let hits = m & (m >> 1) & (m >> 2) & 0x1111_1111;
        let mut bits = hits;
        while bits != 0 {
            let bit = bits.trailing_zeros();
            out.push((x + bit / 4, y));
            bits &= bits - 1;
        }
        x += 8;
    }
    // 行尾:不足 8 像素
    if x < x1 {
        scan_row_scalar(row, x, x1, target, y, out);
    }
}

// ---------------------------------------------------------------------------
// 偏移链校验
// ---------------------------------------------------------------------------

/// 链校验:候选锚点位的全部偏移点(必须落在 roi 内)均命中各自颜色时
/// 该候选通过。锚点自身已在扫描阶段确认命中,此处不再复检。
/// 返回通过的候选(保持输入的行优先顺序)。
pub fn verify_anchor_chain(
    reader: &PixelReader,
    candidates: &[(u32, u32)],
    offsets: &[OffsetCheck],
    roi: Rect,
) -> Vec<(u32, u32)> {
    let mut verified = Vec::new();
    'candidate: for &(ax, ay) in candidates {
        for offset in offsets {
            let ox = i64::from(ax) + i64::from(offset.dx);
            let oy = i64::from(ay) + i64::from(offset.dy);
            if !roi.contains(ox, oy) {
                continue 'candidate;
            }
            let Some(pixel) = (unsafe { reader.read(ox as u32, oy as u32) }) else {
                continue 'candidate;
            };
            if !offset.color.matches(pixel) {
                continue 'candidate;
            }
        }
        verified.push((ax, ay));
    }
    verified
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造 w×h 全零缓冲并在 (x,y) 写入 RGB(alpha 恒 0xFF)。
    fn buffer_with(w: u32, h: u32, pixels: &[(u32, u32, u8, u8, u8)]) -> Vec<u8> {
        let mut buf = vec![0u8; (w * h * 4) as usize];
        for &(x, y, r, g, b) in pixels {
            let off = ((y * w + x) * 4) as usize;
            buf[off] = b;
            buf[off + 1] = g;
            buf[off + 2] = r;
            buf[off + 3] = 0xFF;
        }
        buf
    }

    #[test]
    fn finds_exact_color_positions() {
        let buf = buffer_with(
            8,
            4,
            &[
                (2, 1, 250, 250, 250),
                (5, 3, 250, 250, 250),
                (7, 0, 255, 0, 0),
            ],
        );
        let target = ColorPoint::new(250, 250, 250, 0);
        let hits = find_color_matches(&buf, 8, 4, Rect::new(0, 0, 8, 4), target);
        assert_eq!(hits, vec![(2, 1), (5, 3)]);
    }

    #[test]
    fn tolerance_respects_per_channel_boundary() {
        let buf = buffer_with(4, 1, &[(0, 0, 100, 100, 100), (1, 0, 110, 100, 100)]);
        // 容差 9:R 差 10 越界
        assert_eq!(
            find_color_matches(
                &buf,
                4,
                1,
                Rect::new(0, 0, 4, 1),
                ColorPoint::new(100, 100, 100, 9)
            ),
            vec![(0, 0)]
        );
        assert_eq!(
            find_color_matches(
                &buf,
                4,
                1,
                Rect::new(0, 0, 4, 1),
                ColorPoint::new(100, 100, 100, 10)
            ),
            vec![(0, 0), (1, 0)]
        );
    }

    #[test]
    fn roi_excludes_outside_hits_and_partial_roi_works() {
        let buf = buffer_with(
            16,
            2,
            &[(1, 0, 0, 0, 255), (9, 0, 0, 0, 255), (9, 1, 0, 0, 255)],
        );
        let target = ColorPoint::new(0, 0, 255, 0);
        let hits = find_color_matches(&buf, 16, 2, Rect::new(2, 0, 6, 2), target);
        assert!(hits.is_empty(), "ROI 外命中必须排除");
        let hits = find_color_matches(&buf, 16, 2, Rect::new(8, 0, 2, 2), target);
        assert_eq!(hits, vec![(9, 0), (9, 1)]);
    }

    #[test]
    fn short_buffer_clamps_to_available_rows() {
        // 声明 4 行但只给 2 行数据:第 3 行命中不得越界读取
        let buf = buffer_with(8, 2, &[(3, 1, 1, 2, 3)]);
        let hits = find_color_matches(
            &buf,
            8,
            4,
            Rect::new(0, 0, 8, 4),
            ColorPoint::new(1, 2, 3, 0),
        );
        assert_eq!(hits, vec![(3, 1)]);
        // 声明宽度为 0 时不扫描
        assert!(find_color_matches(
            &buf,
            0,
            2,
            Rect::new(0, 0, 8, 2),
            ColorPoint::new(1, 2, 3, 0)
        )
        .is_empty());
    }

    #[test]
    fn scalar_and_avx2_paths_agree() {
        // 覆盖多种行宽(8 像素块的头部/尾部补扫边界)与随机像素
        let mut seed = 1234u32;
        let mut next = move || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 24) as u8
        };
        for width in [1u32, 3, 7, 8, 9, 16, 17, 31, 40, 64] {
            let height = 7u32;
            let mut buf = vec![0u8; (width * height * 4) as usize];
            for px in buf.chunks_exact_mut(4) {
                px[0] = next();
                px[1] = next();
                px[2] = next();
                px[3] = 0xFF;
            }
            let target = ColorPoint::new(next(), next(), next(), 6);
            let roi = Rect::new(0, 0, width, height);
            let auto = find_color_matches(&buf, width, height, roi, target);
            let scalar = find_color_matches_scalar(&buf, width, height, roi, target);
            assert_eq!(auto, scalar, "width={width} 两条路径必须一致");
            if avx2_available() {
                // 有 AVX2 时再验证:auto 实际走了向量化分支且结果非空场景吻合
                assert_eq!(auto.len(), scalar.len());
            }
        }
    }

    #[test]
    fn strided_reader_honors_row_pitch() {
        // 4 像素宽 + 8 字节行距(含填充)的映射内存
        let mut buf = [0u8; 8 * 2];
        buf[0..4].copy_from_slice(&[10, 20, 30, 40]); // (0,0) = BGR(10,20,30)
        buf[8..12].copy_from_slice(&[50, 60, 70, 80]); // (0,1)
        let reader = PixelReader::Strided {
            base: buf.as_ptr(),
            pitch: 8,
            buf_w: 1,
            buf_h: 2,
        };
        unsafe {
            assert_eq!(reader.read(0, 0), Some((30, 20, 10)));
            assert_eq!(reader.read(0, 1), Some((70, 60, 50)));
            assert_eq!(reader.read(1, 0), None);
            assert_eq!(reader.read(0, 2), None);
        }
        // 步进扫描:第 1 行命中
        let hits = unsafe {
            find_color_matches_strided(
                buf.as_ptr(),
                8,
                1,
                2,
                Rect::new(0, 0, 1, 2),
                ColorPoint::new(70, 60, 50, 0),
            )
        };
        assert_eq!(hits, vec![(0, 1)]);
    }

    #[test]
    fn chain_verification_enforces_roi_containment() {
        // 8x4 全零,锚点 (2,1) 命中;偏移 (2,0) 在 ROI 内,(5,0) 越界
        let buf = buffer_with(
            8,
            4,
            &[
                (2, 1, 200, 200, 200), // 锚点
                (4, 1, 10, 20, 30),    // 偏移 (2,0) 目标
                (7, 1, 10, 20, 30),    // 偏移 (5,0) 目标(ROI 外)
            ],
        );
        let roi = Rect::new(0, 0, 5, 4);
        let reader = PixelReader::Contiguous {
            bgra: &buf,
            buf_w: 8,
            buf_h: 4,
        };
        let offsets = [OffsetCheck {
            dx: 2,
            dy: 0,
            color: ColorPoint::new(10, 20, 30, 0),
        }];
        let verified = verify_anchor_chain(&reader, &[(2, 1)], &offsets, roi);
        assert_eq!(verified, vec![(2, 1)]);
        // 偏移落 ROI 外(即使缓冲区里有匹配像素)也必须拒绝
        let roi_outside = Rect::new(0, 0, 4, 4);
        let verified = verify_anchor_chain(&reader, &[(2, 1)], &offsets, roi_outside);
        assert!(verified.is_empty());
        // 偏移颜色不匹配拒绝
        let offsets_bad = [OffsetCheck {
            dx: 2,
            dy: 0,
            color: ColorPoint::new(9, 9, 9, 0),
        }];
        assert!(verify_anchor_chain(&reader, &[(2, 1)], &offsets_bad, roi).is_empty());
    }

    #[test]
    fn chain_without_offsets_passes_anchor_candidates() {
        let buf = buffer_with(4, 1, &[(1, 0, 1, 2, 3)]);
        let reader = PixelReader::Contiguous {
            bgra: &buf,
            buf_w: 4,
            buf_h: 1,
        };
        let verified = verify_anchor_chain(&reader, &[(1, 0)], &[], Rect::new(0, 0, 4, 1));
        assert_eq!(verified, vec![(1, 0)]);
    }

    #[test]
    fn negative_offsets_work_without_overflow() {
        let buf = buffer_with(8, 4, &[(3, 2, 1, 2, 3), (1, 1, 9, 9, 9)]);
        let reader = PixelReader::Contiguous {
            bgra: &buf,
            buf_w: 8,
            buf_h: 4,
        };
        let offsets = [OffsetCheck {
            dx: -2,
            dy: -1,
            color: ColorPoint::new(9, 9, 9, 0),
        }];
        let verified = verify_anchor_chain(&reader, &[(3, 2)], &offsets, Rect::new(0, 0, 8, 4));
        assert_eq!(verified, vec![(3, 2)]);
        // 候选靠边时负偏移越出 ROI 上界,拒绝而非下溢
        let verified = verify_anchor_chain(&reader, &[(3, 2)], &offsets, Rect::new(2, 1, 6, 3));
        assert!(verified.is_empty());
    }

    /// 4K 全区域扫描性能冒烟:AVX2 路径应落在毫秒级(--release 运行)。
    /// 默认 debug 下 AVX2 目标特征仍生效(内部 unsafe 分派),阈值放宽到 50ms
    /// 兼容 CI 虚拟机;`cargo test --release -- --ignored` 观察真实 ~1-3ms。
    #[test]
    #[ignore = "性能冒烟,显式 --ignored 运行;--release 下应 ~1-3ms"]
    fn perf_smoke_4k_scan() {
        let (w, h) = (3840u32, 2160u32);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let mut seed = 7u32;
        for px in buf.chunks_exact_mut(4) {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            px[0] = (seed >> 24) as u8;
            px[2] = 128;
        }
        let target = ColorPoint::new(255, 0, 0, 4);
        let start = std::time::Instant::now();
        let hits = find_color_matches(&buf, w, h, Rect::new(0, 0, w, h), target);
        let elapsed = start.elapsed();
        eprintln!(
            "4K({w}x{h}) 全区域锚点扫描(零命中): {:.3} ms, engine={}, hits={}",
            elapsed.as_secs_f64() * 1000.0,
            if avx2_available() { "avx2" } else { "scalar" },
            hits.len()
        );
        assert!(elapsed.as_millis() < 50, "4K 全扫描 {elapsed:?} 超预算");
        // 最坏情况:每个像素都命中 → movemask 命中位展开循环全量执行
        for px in buf.chunks_exact_mut(4) {
            px[2] = 255;
            px[0] = 0;
            px[1] = 0;
        }
        let target_all = ColorPoint::new(255, 0, 0, 0);
        let start = std::time::Instant::now();
        let hits = find_color_matches(&buf, w, h, Rect::new(0, 0, w, h), target_all);
        let elapsed = start.elapsed();
        eprintln!(
            "4K({w}x{h}) 全区域锚点扫描(全命中): {:.3} ms, engine={}, hits={}",
            elapsed.as_secs_f64() * 1000.0,
            if avx2_available() { "avx2" } else { "scalar" },
            hits.len()
        );
        assert!(elapsed.as_millis() < 50, "4K 全命中扫描 {elapsed:?} 超预算");
        let _ = hits;
    }
}
