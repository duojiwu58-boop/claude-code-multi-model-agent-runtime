//! `pixel_match_multi` 工具:多点特征比色(经典外挂式锚点+偏移链匹配)。
//!
//! 参数编排与采集在本模块,扫描/校验引擎在 [`crate::backend::pixelsimd`]。
//! 两种目标:
//! - **hwnd**(客户区坐标):`method=auto` 屏幕 BitBlt / `wgc` DWM 合成帧,
//!   与现有像素工具同源;遮挡标注只在 auto 有语义;
//! - **monitor**(1-based,显示器坐标):DDAPI staging 纹理 Map 后**直接
//!   指针扫描**,免整图连续拷贝;DDAPI 不可用(锁定/静态桌面无帧)时
//!   回退屏幕 BitBlt 并在响应标注。
//!
//! 匹配语义:在 search_rect 内找锚点色的全部命中 → 逐候选校验偏移链
//! (偏移点必须在 search_rect 内)→ 按与期望锚点位置的欧氏距离升序
//! 输出前 max_matches 个。响应带 capture/scan 分段时间供性能验证。

use serde_json::{json, Value};
use std::time::Instant;
use windows::Win32::Foundation::HWND;

use crate::backend::{
    pixelsimd::{
        avx2_available, find_color_matches, verify_anchor_chain, ColorPoint, OffsetCheck,
        PixelReader, Rect,
    },
    window::{client_rect_of, hwnd_str},
};
use crate::error::{ToolError, INVALID_ARGUMENT, LIMIT_EXCEEDED, WINDOW_NOT_FOUND};
use crate::parse::{json_i32, parse_hex_color};

/// 偏移点数量上限:链太长会稀释特征语义,也给校验加无谓负担。
const MAX_OFFSETS: usize = 64;
/// 返回匹配数量上限:模型一次消费不了太多坐标,也给响应体上界。
const MAX_MATCHES: usize = 4096;
const DEFAULT_MAX_MATCHES: usize = 32;

/// 解析 {color: "#RRGGBB", tolerance?: 0..255} 形状的颜色点。
/// `field` 用于错误消息定位(anchor / offsets[i])。
fn parse_color_field(obj: &Value, field: &str) -> Result<ColorPoint, ToolError> {
    let color = obj
        .get("color")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::new(INVALID_ARGUMENT, format!("{field}.color 缺失(#RRGGBB)")))?;
    let (r, g, b) = parse_hex_color(color)?;
    let tolerance = json_i32(obj, "tolerance", false)?
        .unwrap_or(0)
        .clamp(0, 255) as u8;
    Ok(ColorPoint::new(r, g, b, tolerance))
}

/// 解析搜索矩形(可选,坐标相对目标坐标系);缺省返回全区域。
/// 必填校验由调用方按目标坐标系完成(客户区/显示器)。
fn parse_search_rect(
    args: &Value,
    default_width: i32,
    default_height: i32,
) -> Result<Rect, ToolError> {
    match args.get("search_rect") {
        None | Some(Value::Null) => {
            Ok(Rect::new(0, 0, default_width as u32, default_height as u32))
        }
        Some(rect) => {
            let x = json_i32(rect, "x", true)?.expect("required value checked above");
            let y = json_i32(rect, "y", true)?.expect("required value checked above");
            let width = json_i32(rect, "width", true)?.expect("required value checked above");
            let height = json_i32(rect, "height", true)?.expect("required value checked above");
            if width <= 0 || height <= 0 {
                return Err(ToolError::new(
                    INVALID_ARGUMENT,
                    format!("search_rect 尺寸必须为正数,实际为 {width}x{height}"),
                ));
            }
            if x < 0
                || y < 0
                || i64::from(x) + i64::from(width) > i64::from(default_width)
                || i64::from(y) + i64::from(height) > i64::from(default_height)
            {
                return Err(ToolError::new(
                    INVALID_ARGUMENT,
                    format!(
                        "search_rect ({x},{y}) {width}x{height} 超出目标区域 {default_width}x{default_height}"
                    ),
                ));
            }
            Ok(Rect::new(x as u32, y as u32, width as u32, height as u32))
        }
    }
}

/// 解析偏移链。元素缺失 offset_x/offset_y 视为 (0,0) 偏移(冗余校验点)。
fn parse_offsets(args: &Value) -> Result<Vec<OffsetCheck>, ToolError> {
    let Some(list) = args.get("offsets") else {
        return Ok(Vec::new());
    };
    if list.is_null() {
        return Ok(Vec::new());
    }
    let items = list
        .as_array()
        .ok_or_else(|| ToolError::new(INVALID_ARGUMENT, "offsets 必须是数组"))?;
    if items.len() > MAX_OFFSETS {
        return Err(ToolError::new(
            LIMIT_EXCEEDED,
            format!("offsets 数量不能超过 {MAX_OFFSETS},实际为 {}", items.len()),
        ));
    }
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let field = format!("offsets[{index}]");
            let dx = json_i32(item, "offset_x", false)?.unwrap_or(0);
            let dy = json_i32(item, "offset_y", false)?.unwrap_or(0);
            Ok(OffsetCheck {
                dx,
                dy,
                color: parse_color_field(item, &field)?,
            })
        })
        .collect()
}

fn parse_max_matches(args: &Value) -> Result<usize, ToolError> {
    Ok(json_i32(args, "max_matches", false)?
        .unwrap_or(DEFAULT_MAX_MATCHES as i32)
        .clamp(1, MAX_MATCHES as i32) as usize)
}

/// 按与期望锚点欧氏距离升序排序(距离相同按行优先),截断到 max_matches。
/// 返回 (截断前匹配总数, 是否截断)。
fn sort_and_truncate(
    verified: &mut Vec<(u32, u32)>,
    expected: (f64, f64),
    max_matches: usize,
) -> (usize, bool) {
    let total = verified.len();
    verified.sort_by(|a, b| {
        let da = (f64::from(a.0) - expected.0).powi(2) + (f64::from(a.1) - expected.1).powi(2);
        let db = (f64::from(b.0) - expected.0).powi(2) + (f64::from(b.1) - expected.1).powi(2);
        da.partial_cmp(&db)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(b))
    });
    let truncated = verified.len() > max_matches;
    verified.truncate(max_matches);
    (total, truncated)
}

/// `pixel_match_multi` 主入口。`hwnd=None` 表示 monitor 模式
/// (由 tools 层按参数二选一解析)。所有错误走统一错误契约。
pub fn pixel_match_multi(hwnd: Option<HWND>, args: &Value) -> Result<Value, ToolError> {
    let anchor = args
        .get("anchor")
        .ok_or_else(|| ToolError::new(INVALID_ARGUMENT, "anchor 参数缺失"))?;
    let anchor_color = parse_color_field(anchor, "anchor")?;
    let anchor_x = json_i32(anchor, "x", true)?.expect("required value checked above");
    let anchor_y = json_i32(anchor, "y", true)?.expect("required value checked above");
    let offsets = parse_offsets(args)?;
    let max_matches = parse_max_matches(args)?;
    let method = args.get("method").and_then(Value::as_str).unwrap_or("auto");
    if method != "auto" && method != "wgc" {
        return Err(ToolError::new(
            INVALID_ARGUMENT,
            format!("未知采集方式 '{method}',支持 auto / wgc"),
        ));
    }

    match hwnd {
        Some(hwnd) => pixel_match_client(
            hwnd,
            args,
            method,
            anchor_color,
            (anchor_x, anchor_y),
            &offsets,
            max_matches,
        ),
        None => pixel_match_monitor(
            args,
            anchor_color,
            (anchor_x, anchor_y),
            &offsets,
            max_matches,
        ),
    }
}

/// 客户区模式:hwnd + 客户区坐标。采集 → 扫描 → 校验 → 排序输出。
fn pixel_match_client(
    hwnd: HWND,
    args: &Value,
    method: &str,
    anchor: ColorPoint,
    anchor_expected: (i32, i32),
    offsets: &[OffsetCheck],
    max_matches: usize,
) -> Result<Value, ToolError> {
    let client = client_rect_of(hwnd)?;
    let client_w = client.right - client.left;
    let client_h = client.bottom - client.top;
    let roi = parse_search_rect(args, client_w, client_h)?;

    // 采集 search_rect 区域(与现有像素工具同源)
    let capture_started = Instant::now();
    let (bgra, source, occluded) = match method {
        "auto" => {
            let bgra = crate::backend::capture::capture_client_region_bgra(
                hwnd,
                roi.x0 as i32,
                roi.y0 as i32,
                roi.width() as i32,
                roi.height() as i32,
            )?;
            let occluded = crate::backend::window::client_region_occluded(
                hwnd,
                roi.x0 as i32,
                roi.y0 as i32,
                roi.width() as i32,
                roi.height() as i32,
            );
            (bgra, "screen", occluded)
        }
        _ => (
            crate::backend::wgc::capture_client_region_wgc(
                hwnd,
                roi.x0 as i32,
                roi.y0 as i32,
                roi.width() as i32,
                roi.height() as i32,
            )?,
            "wgc",
            false,
        ),
    };
    let capture_ms = capture_started.elapsed().as_secs_f64() * 1000.0;

    let scan_started = Instant::now();
    let (mut verified, candidates, match_total, truncated) = scan_contiguous(
        &bgra,
        roi.width(),
        roi.height(),
        Rect::new(0, 0, roi.width(), roi.height()),
        anchor,
        offsets,
        max_matches,
        anchor_expected,
        roi,
    );
    let scan_ms = scan_started.elapsed().as_secs_f64() * 1000.0;

    let matches: Vec<Value> = verified
        .drain(..)
        .map(|(x, y)| json!({"x": x, "y": y}))
        .collect();
    build_response(json!({
        "hwnd": hwnd_str(hwnd),
        "monitor": Value::Null,
        "source": source,
        "fallback": Value::Null,
        "occluded": occluded,
        "engine": engine_label(),
        "anchor": {"x": anchor_expected.0, "y": anchor_expected.1,
                   "color": hex_of(anchor), "tolerance": anchor.tolerance},
        "offset_count": offsets.len(),
        "search_rect": {"x": roi.x0, "y": roi.y0, "width": roi.width(), "height": roi.height()},
        "candidate_count": candidates,
        "match_count": match_total,
        "truncated": truncated,
        "capture_elapsed_ms": round_ms(capture_ms),
        "scan_elapsed_ms": round_ms(scan_ms),
        "matches": matches
    }))
}

/// 显示器模式:DDAPI 直扫(零整图拷贝)→ 失败回退屏幕 BitBlt。
fn pixel_match_monitor(
    args: &Value,
    anchor: ColorPoint,
    anchor_expected: (i32, i32),
    offsets: &[OffsetCheck],
    max_matches: usize,
) -> Result<Value, ToolError> {
    let monitor_raw = json_i32(args, "monitor", true)?
        .ok_or_else(|| ToolError::new(INVALID_ARGUMENT, "monitor 参数缺失(1-based)"))?;
    if monitor_raw < 1 {
        return Err(ToolError::new(INVALID_ARGUMENT, "monitor 序号从 1 开始"));
    }
    let monitor_rect = crate::backend::capture::monitor_rect_at(monitor_raw as usize - 1)
        .ok_or_else(|| {
            ToolError::new(
                WINDOW_NOT_FOUND,
                format!("显示器序号 {monitor_raw} 不存在(可先 screenshot_all 观察数量)"),
            )
        })?;
    let mon_w = monitor_rect.right - monitor_rect.left;
    let mon_h = monitor_rect.bottom - monitor_rect.top;
    let roi = parse_search_rect(args, mon_w, mon_h)?;

    let capture_started = Instant::now();
    match crate::backend::ddapi::capture_frame_for_rect(monitor_rect) {
        Ok(frame) => {
            let capture_ms = capture_started.elapsed().as_secs_f64() * 1000.0;
            let scan_started = Instant::now();
            let (verified, candidates, match_total, truncated) =
                scan_frame(&frame, roi, anchor, offsets, max_matches, anchor_expected);
            let scan_ms = scan_started.elapsed().as_secs_f64() * 1000.0;
            monitor_response(
                monitor_raw,
                roi,
                anchor,
                anchor_expected,
                offsets,
                "ddapi",
                None,
                verified,
                candidates,
                match_total,
                truncated,
                capture_ms,
                scan_ms,
            )
        }
        Err(ddapi_error) => {
            let capture_ms = capture_started.elapsed().as_secs_f64() * 1000.0;
            // 回退:屏幕 BitBlt 采样 search_rect(与 client auto 同源)
            let fallback = Some(format!("ddapi 不可用: {ddapi_error}"));
            let bgra = crate::backend::capture::capture_screen_rect_bgra(
                monitor_rect.left + roi.x0 as i32,
                monitor_rect.top + roi.y0 as i32,
                roi.width() as i32,
                roi.height() as i32,
            )?;
            let scan_started = Instant::now();
            let (verified, candidates, match_total, truncated) = scan_contiguous(
                &bgra,
                roi.width(),
                roi.height(),
                Rect::new(0, 0, roi.width(), roi.height()),
                anchor,
                offsets,
                max_matches,
                anchor_expected,
                roi,
            );
            let scan_ms = scan_started.elapsed().as_secs_f64() * 1000.0;
            monitor_response(
                monitor_raw,
                roi,
                anchor,
                anchor_expected,
                offsets,
                "screen",
                fallback,
                verified,
                candidates,
                match_total,
                truncated,
                capture_ms,
                scan_ms,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn monitor_response(
    monitor_raw: i32,
    roi: Rect,
    anchor: ColorPoint,
    anchor_expected: (i32, i32),
    offsets: &[OffsetCheck],
    source: &str,
    fallback: Option<String>,
    mut verified: Vec<(u32, u32)>,
    candidates: usize,
    match_total: usize,
    truncated: bool,
    capture_ms: f64,
    scan_ms: f64,
) -> Result<Value, ToolError> {
    let matches: Vec<Value> = verified
        .drain(..)
        .map(|(x, y)| json!({"x": x, "y": y}))
        .collect();
    build_response(json!({
        "hwnd": Value::Null,
        "monitor": monitor_raw,
        "source": source,
        "fallback": fallback,
        "occluded": false,
        "engine": engine_label(),
        "anchor": {"x": anchor_expected.0, "y": anchor_expected.1,
                   "color": hex_of(anchor), "tolerance": anchor.tolerance},
        "offset_count": offsets.len(),
        "search_rect": {"x": roi.x0, "y": roi.y0, "width": roi.width(), "height": roi.height()},
        "candidate_count": candidates,
        "match_count": match_total,
        "truncated": truncated,
        "capture_elapsed_ms": round_ms(capture_ms),
        "scan_elapsed_ms": round_ms(scan_ms),
        "matches": matches
    }))
}

/// DDAPI 帧扫描:ROI 坐标即帧坐标(帧覆盖整个显示器)。零整图拷贝。
fn scan_frame(
    frame: &crate::backend::ddapi::DupFrame,
    roi: Rect,
    anchor: ColorPoint,
    offsets: &[OffsetCheck],
    max_matches: usize,
    anchor_expected: (i32, i32),
) -> (Vec<(u32, u32)>, usize, usize, bool) {
    match frame {
        crate::backend::ddapi::DupFrame::Mapped(mapped) => {
            let reader = mapped.reader();
            let PixelReader::Strided { base, pitch, .. } = &reader else {
                unreachable!("Mapped 帧的 reader 必为 Strided");
            };
            let candidates = unsafe {
                crate::backend::pixelsimd::find_color_matches_strided(
                    *base,
                    *pitch,
                    frame.width(),
                    frame.height(),
                    roi,
                    anchor,
                )
            };
            let mut verified = verify_anchor_chain(&reader, &candidates, offsets, roi);
            let (match_total, truncated) = sort_and_truncate(
                &mut verified,
                (
                    f64::from(anchor_expected.0 - roi.x0 as i32),
                    f64::from(anchor_expected.1 - roi.y0 as i32),
                ),
                max_matches,
            );
            (verified, candidates.len(), match_total, truncated)
        }
        crate::backend::ddapi::DupFrame::Cached {
            bgra,
            width,
            height,
            ..
        } => scan_contiguous(
            bgra,
            *width,
            *height,
            roi,
            anchor,
            offsets,
            max_matches,
            anchor_expected,
            roi,
        ),
    }
}

/// 连续缓冲扫描公共路径:锚点扫描 → 链校验 → 按期望锚点距离排序截断。
/// 返回 (匹配列表[输出坐标], 候选数, 匹配总数, 是否截断);`output_origin`
/// 是缓冲坐标 → 目标坐标系的偏移(roi.x0/y0)。
#[allow(clippy::too_many_arguments)]
fn scan_contiguous(
    bgra: &[u8],
    buf_w: u32,
    buf_h: u32,
    scan_roi: Rect,
    anchor: ColorPoint,
    offsets: &[OffsetCheck],
    max_matches: usize,
    anchor_expected: (i32, i32),
    output_origin: Rect,
) -> (Vec<(u32, u32)>, usize, usize, bool) {
    let candidates = find_color_matches(bgra, buf_w, buf_h, scan_roi, anchor);
    let reader = PixelReader::Contiguous { bgra, buf_w, buf_h };
    let mut verified = verify_anchor_chain(&reader, &candidates, offsets, scan_roi);
    let (match_total, truncated) = sort_and_truncate(
        &mut verified,
        (
            f64::from(anchor_expected.0 - output_origin.x0 as i32),
            f64::from(anchor_expected.1 - output_origin.y0 as i32),
        ),
        max_matches,
    );
    // 缓冲坐标 → 目标坐标系输出坐标
    let mut output = Vec::with_capacity(verified.len());
    for (x, y) in verified {
        output.push((x + output_origin.x0, y + output_origin.y0));
    }
    (output, candidates.len(), match_total, truncated)
}

fn engine_label() -> &'static str {
    if avx2_available() {
        "avx2"
    } else {
        "scalar"
    }
}

fn hex_of(color: ColorPoint) -> String {
    format!("#{:02X}{:02X}{:02X}", color.r, color.g, color.b)
}

fn round_ms(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn build_response(payload: Value) -> Result<Value, ToolError> {
    Ok(json!({
        "content": [{"type": "text", "text": payload.to_string()}]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_anchor_and_offsets_tolerantly() {
        let args = json!({
            "anchor": {"x": 10, "y": "20", "color": "#FF0000", "tolerance": "5"},
            "offsets": [
                {"offset_x": 2, "offset_y": -3, "color": "00FF00"},
                {"color": "#0000FF"}
            ],
            "max_matches": "10"
        });
        let anchor = parse_color_field(args.get("anchor").unwrap(), "anchor").unwrap();
        assert_eq!(anchor, ColorPoint::new(255, 0, 0, 5));
        let offsets = parse_offsets(&args).unwrap();
        assert_eq!(offsets.len(), 2);
        assert_eq!((offsets[0].dx, offsets[0].dy), (2, -3));
        assert_eq!((offsets[1].dx, offsets[1].dy), (0, 0)); // 缺省偏移 = 冗余校验点
        assert_eq!(parse_max_matches(&args).unwrap(), 10);
    }

    #[test]
    fn search_rect_defaults_to_full_region_and_validates_bounds() {
        let full = parse_search_rect(&json!({}), 800, 600).unwrap();
        assert_eq!(full, Rect::new(0, 0, 800, 600));
        let partial = parse_search_rect(
            &json!({"search_rect": {"x": 10, "y": 20, "width": 100, "height": 50}}),
            800,
            600,
        )
        .unwrap();
        assert_eq!(partial, Rect::new(10, 20, 100, 50));
        assert!(parse_search_rect(
            &json!({"search_rect": {"x": 700, "y": 0, "width": 200, "height": 50}}),
            800,
            600
        )
        .is_err());
        assert!(parse_search_rect(
            &json!({"search_rect": {"x": 0, "y": 0, "width": 0, "height": 50}}),
            800,
            600
        )
        .is_err());
    }

    #[test]
    fn rejects_oversized_offset_chains() {
        let items = (0..=MAX_OFFSETS)
            .map(|i| json!({"offset_x": i, "offset_y": 0, "color": "#010203"}))
            .collect::<Vec<_>>();
        let args = json!({"offsets": items});
        let err = parse_offsets(&args).unwrap_err();
        assert_eq!(err.code, LIMIT_EXCEEDED);
    }

    #[test]
    fn sorting_prioritizes_nearest_anchor_and_truncates() {
        let mut hits = vec![(100, 100), (3, 3), (5, 4), (4, 5)];
        let (total, truncated) = sort_and_truncate(&mut hits, (4.0, 4.0), 2);
        assert!(truncated);
        assert_eq!(total, 4, "截断前总数保留");
        // (5,4)/(4,5) 距离 1 最近(同距按行优先),距离 2 的 (3,3) 被截断
        assert_eq!(hits, vec![(4, 5), (5, 4)]);
    }

    #[test]
    fn scan_contiguous_reports_candidates_and_matches_in_output_coords() {
        let mut bgra = vec![0u8; 8 * 6 * 4];
        // 锚点色 (200,200,200) 在两处;偏移点 (2,0) 只在第一处有
        let put = |x: u32, y: u32, r: u8, g: u8, b: u8, buf: &mut Vec<u8>| {
            let off = ((y * 8 + x) * 4) as usize;
            buf[off] = b;
            buf[off + 1] = g;
            buf[off + 2] = r;
            buf[off + 3] = 0xFF;
        };
        put(2, 1, 200, 200, 200, &mut bgra);
        put(4, 1, 10, 20, 30, &mut bgra); // 第一处偏移命中
        put(2, 5, 200, 200, 200, &mut bgra); // 第二处锚点,偏移无命中
        let anchor = ColorPoint::new(200, 200, 200, 0);
        let offsets = [OffsetCheck {
            dx: 2,
            dy: 0,
            color: ColorPoint::new(10, 20, 30, 0),
        }];
        let origin = Rect::new(3, 2, 8, 6); // 输出坐标系原点 (3,2)
        let (matches, candidates, match_total, truncated) = scan_contiguous(
            &bgra,
            8,
            6,
            Rect::new(0, 0, 8, 6),
            anchor,
            &offsets,
            32,
            (3, 3),
            origin,
        );
        assert_eq!(candidates, 2, "两处锚点候选");
        assert_eq!(matches, vec![(5, 3)], "仅第一处链校验通过,已换算输出坐标");
        assert_eq!(match_total, 1);
        assert!(!truncated);
    }
}
