//! 工具目录与调度:选择器解析、工具参数校验、MCP 工具 schema 与分发。
//! 具体 Win32 执行在 [`crate::backend`];JSON-RPC 协议在 [`crate::protocol`]。

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::{json, Value};
use windows::Win32::Foundation::HWND;

use crate::backend::{
    capture::{capture_all_monitors, capture_client_region_bgra, capture_window},
    input::send_input,
    window::{collect_windows, entry_for_hwnd, foreground_hwnd, hwnd_num, hwnd_str, WindowEntry},
};
use crate::parse::{json_i32, json_u32, parse_hex_color, parse_hwnd_str};

pub const TOOL_LIST: &str = "list_windows";
pub const TOOL_GET_RECT: &str = "get_window_rect";
pub const TOOL_SCREENSHOT_WINDOW: &str = "screenshot_window";
pub const TOOL_SCREENSHOT_ALL: &str = "screenshot_all";
pub const TOOL_RESIZE_WINDOW: &str = "resize_window";
pub const TOOL_ACTIVATE: &str = "window_activate";
pub const TOOL_PIXEL_SCAN_LINE: &str = "pixel_scan_line";
pub const TOOL_PIXEL_FIND_COLOR: &str = "pixel_find_color";
pub const TOOL_SEND_INPUT: &str = "send_input";

const MAX_WINDOWS_LIMIT: usize = 500;
const MAX_SCAN_BANDS: usize = 256;

// ---------------------------------------------------------------------------
// 工具实现
// ---------------------------------------------------------------------------

pub fn list_windows(args: &Value) -> Result<Value, String> {
    let title = args.get("title").and_then(Value::as_str).unwrap_or("");
    let class = args.get("class").and_then(Value::as_str).unwrap_or("");
    let pid = json_u32(args, "pid")?;
    let visible_only = args
        .get("visible_only")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(100)
        .clamp(1, MAX_WINDOWS_LIMIT);

    let mut entries = collect_windows();
    if !title.is_empty() {
        let needle = title.to_lowercase();
        entries.retain(|e| e.title.to_lowercase().contains(&needle));
    }
    if !class.is_empty() {
        let needle = class.to_lowercase();
        entries.retain(|e| e.class.to_lowercase().contains(&needle));
    }
    if let Some(p) = pid {
        entries.retain(|e| e.pid == p);
    }
    if visible_only {
        entries.retain(|e| e.visible);
    }
    entries.sort_by(|a, b| {
        // 可见优先,不可标题为空,面积从大到小
        (b.visible as u8)
            .cmp(&(a.visible as u8))
            .then((a.title.is_empty() as u8).cmp(&(b.title.is_empty() as u8)))
            .then((b.width as i64 * b.height as i64).cmp(&(a.width as i64 * a.height as i64)))
    });
    entries.truncate(limit);

    let rows: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "hwnd": e.hwnd.to_string(),
                "pid": e.pid,
                "title": e.title,
                "class": e.class,
                "visible": e.visible,
                "minimized": e.minimized,
                "rect": {
                    "left": e.left, "top": e.top,
                    "width": e.width, "height": e.height
                }
            })
        })
        .collect();
    let text = serde_json::to_string_pretty(&json!({
        "count": rows.len(),
        "windows": rows
    }))
    .map_err(|e| format!("JSON 序列化失败: {e}"))?;
    Ok(json!({"content": [{"type": "text", "text": text}]}))
}

// ---------------------------------------------------------------------------
// 目标窗口解析: hwnd / title / pid 三选一
// ---------------------------------------------------------------------------

fn resolve_target(args: &Value) -> Result<HWND, String> {
    let has_hwnd = args.get("hwnd").is_some();
    let has_title = args
        .get("title")
        .and_then(Value::as_str)
        .is_some_and(|title| !title.trim().is_empty());
    let has_pid = args.get("pid").is_some();
    let selector_count = usize::from(has_hwnd) + usize::from(has_title) + usize::from(has_pid);
    // 模糊标题、PID 与 HWND 叠加会掩盖调用者意图；强制唯一定位方式。
    if selector_count != 1 {
        return Err("必须且只能提供一种定位方式：hwnd / title / pid".into());
    }

    if let Some(hs) = args.get("hwnd") {
        let raw: i64 = if let Some(s) = hs.as_str() {
            // `@active` 是显式快捷方式，不与任意标题模糊匹配混用；便于截图前后对比。
            if s.trim().eq_ignore_ascii_case("@active") {
                let hwnd = foreground_hwnd();
                if hwnd.0.is_null() {
                    return Err("当前没有可用的前台窗口".into());
                }
                let entry = entry_for_hwnd(hwnd)?;
                if !entry.visible {
                    return Err("当前前台窗口不可见，无法作为 @active 目标".into());
                }
                return Ok(hwnd);
            }
            parse_hwnd_str(s, s)?
        } else if let Some(n) = hs.as_i64() {
            n
        } else {
            return Err("hwnd 必须是十进制字符串或整数".into());
        };
        let hwnd = HWND(raw as isize as *mut core::ffi::c_void);
        entry_for_hwnd(hwnd)?;
        return Ok(hwnd);
    }

    let title = args.get("title").and_then(Value::as_str).unwrap_or("");
    let pid = json_u32(args, "pid")?;

    let mut entries = collect_windows();
    if !title.is_empty() {
        let needle = title.to_lowercase();
        entries.retain(|e| e.title.to_lowercase().contains(&needle));
    }
    if let Some(p) = pid {
        entries.retain(|e| e.pid == p);
    }
    // 优先可见窗口;若确实没有可见匹配,则全部候选都可能不可见
    let visible_entries: Vec<WindowEntry> = entries.iter().filter(|e| e.visible).cloned().collect();
    let pool = if visible_entries.is_empty() {
        entries
    } else {
        visible_entries
    };
    if pool.is_empty() {
        return Err(format!(
            "未找到匹配窗口 (title='{title}', pid={})。可先运行 list_windows 查句柄",
            pid.map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string())
        ));
    }
    // 前台窗口优先,否则取面积最大的
    let foreground = foreground_hwnd();
    let mut pool = pool;
    pool.sort_by(|a, b| {
        let fa = a.hwnd == hwnd_num(foreground);
        let fb = b.hwnd == hwnd_num(foreground);
        fb.cmp(&fa)
            .then((b.width as i64 * b.height as i64).cmp(&(a.width as i64 * a.height as i64)))
    });
    let best = &pool[0];
    if !best.visible {
        return Err(format!(
            "匹配到窗口但不可见: hwnd={} title='{}' 。请改用 hwnd 精确指定",
            best.hwnd, best.title
        ));
    }
    Ok(HWND(best.hwnd as *mut core::ffi::c_void))
}

/// 带副作用或单查工具不接受模糊标题/PID，避免调用方在多窗口场景误操作。
fn resolve_explicit_hwnd(args: &Value) -> Result<HWND, String> {
    if args.get("hwnd").is_none() || args.get("title").is_some() || args.get("pid").is_some() {
        return Err("此工具必须只提供 hwnd（可为 @active），不接受 title 或 pid".into());
    }
    resolve_target(args)
}

// ---------------------------------------------------------------------------
// 像素扫描 (布局验证基元)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
struct ColorBand {
    /// 相对扫描起点（0 基）的包含式行区间。
    y_from: u32,
    y_to: u32,
    /// 带内像素的通道平均值。
    color: (u8, u8, u8),
}

fn similar_color(a: (u8, u8, u8), b: (u8, u8, u8), tolerance: u8) -> bool {
    let t = i32::from(tolerance);
    (i32::from(a.0) - i32::from(b.0)).abs() <= t
        && (i32::from(a.1) - i32::from(b.1)).abs() <= t
        && (i32::from(a.2) - i32::from(b.2)).abs() <= t
}

/// 将一条像素列的逐行颜色合并为连续颜色带：相邻行各通道差 ≤ tolerance 归入同一带。
/// 返回的带只保留长度 ≥ min_band 的段，每带颜色取带内平均值（四舍五入）。
fn merge_color_bands(colors: &[(u8, u8, u8)], tolerance: u8, min_band: usize) -> Vec<ColorBand> {
    struct BandAcc {
        y_from: u32,
        y_to: u32,
        sum: (i64, i64, i64),
        count: i64,
        last: (u8, u8, u8),
    }
    let mut accs: Vec<BandAcc> = Vec::new();
    for (index, color) in colors.iter().enumerate() {
        let fits = accs
            .last()
            .is_some_and(|acc| similar_color(acc.last, *color, tolerance));
        if fits {
            let acc = accs.last_mut().expect("checked above");
            acc.y_to = index as u32;
            acc.sum.0 += i64::from(color.0);
            acc.sum.1 += i64::from(color.1);
            acc.sum.2 += i64::from(color.2);
            acc.count += 1;
            acc.last = *color;
        } else {
            accs.push(BandAcc {
                y_from: index as u32,
                y_to: index as u32,
                sum: (i64::from(color.0), i64::from(color.1), i64::from(color.2)),
                count: 1,
                last: *color,
            });
        }
    }
    accs.into_iter()
        .filter(|acc| (acc.y_to - acc.y_from + 1) as usize >= min_band)
        .map(|acc| {
            let round =
                |sum: i64| ((sum as f64 / acc.count as f64).round() as i32).clamp(0, 255) as u8;
            ColorBand {
                y_from: acc.y_from,
                y_to: acc.y_to,
                color: (round(acc.sum.0), round(acc.sum.1), round(acc.sum.2)),
            }
        })
        .collect()
}

/// 在窗口客户区内沿 x 指定的垂直列扫描屏幕像素，输出颜色带列表。
/// 用于布局验证：面板边界、控件高度、占位空隙一类的断言。
fn pixel_scan_line(hwnd: HWND, args: &Value) -> Result<Value, String> {
    let x = json_i32(args, "x", true)?.expect("required value checked above");
    let y_from = json_i32(args, "y_from", true)?.expect("required value checked above");
    let y_to = json_i32(args, "y_to", true)?.expect("required value checked above");
    if y_to <= y_from {
        return Err("y_to 必须大于 y_from".into());
    }
    let scan_height = y_to
        .checked_sub(y_from)
        .ok_or("y_to - y_from 超出 32 位整数范围")?;
    let tolerance = json_i32(args, "tolerance", false)?
        .unwrap_or(2)
        .clamp(0, 255) as u8;
    let min_band = json_i32(args, "min_band", false)?
        .unwrap_or(2)
        .clamp(1, 65535) as usize;
    let max_bands = json_i32(args, "max_bands", false)?
        .unwrap_or(MAX_SCAN_BANDS as i32)
        .clamp(1, MAX_SCAN_BANDS as i32) as usize;

    let bgra = capture_client_region_bgra(hwnd, x, y_from, 1, scan_height)?;
    // BGRA → (r,g,b)
    let colors: Vec<(u8, u8, u8)> = bgra
        .chunks_exact(4)
        .map(|px| (px[2], px[1], px[0]))
        .collect();
    let mut bands = merge_color_bands(&colors, tolerance, min_band);
    let truncated = bands.len() > max_bands;
    bands.truncate(max_bands);

    let rows: Vec<Value> = bands
        .iter()
        .map(|band| {
            let (y_lo, y_hi) = (y_from + band.y_from as i32, y_from + band.y_to as i32);
            json!({
                "y_from": y_lo,
                "y_to": y_hi,
                "height": y_hi - y_lo + 1,
                "color": format!("#{:02X}{:02X}{:02X}", band.color.0, band.color.1, band.color.2)
            })
        })
        .collect();

    Ok(json!({
        "content": [{"type": "text", "text": json!({
            "hwnd": hwnd_str(hwnd),
            "x": x,
            "y_from": y_from,
            "y_to": y_to,
            "tolerance": tolerance,
            "min_band": min_band,
            "band_count": rows.len(),
            "truncated": truncated,
            "bands": rows
        }).to_string()}]
    }))
}

/// 在窗口客户区的矩形区域内查找与目标颜色匹配的像素，返回计数与最小包围盒。
fn pixel_find_color(hwnd: HWND, args: &Value) -> Result<Value, String> {
    let color_arg = args
        .get("color")
        .and_then(Value::as_str)
        .ok_or("color 参数缺失（#RRGGBB 格式）")?;
    let (r, g, b) = parse_hex_color(color_arg)?;
    let tolerance = json_i32(args, "tolerance", false)?
        .unwrap_or(0)
        .clamp(0, 255) as u8;
    let x = json_i32(args, "x", true)?.expect("required value checked above");
    let y = json_i32(args, "y", true)?.expect("required value checked above");
    let width = json_i32(args, "width", true)?.expect("required value checked above");
    let height = json_i32(args, "height", true)?.expect("required value checked above");

    let bgra = capture_client_region_bgra(hwnd, x, y, width, height)?;
    let mut count: i64 = 0;
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    for (index, px) in bgra.chunks_exact(4).enumerate() {
        let (px_r, px_g, px_b) = (px[2], px[1], px[0]);
        if (i32::from(px_r) - i32::from(r)).abs() <= i32::from(tolerance)
            && (i32::from(px_g) - i32::from(g)).abs() <= i32::from(tolerance)
            && (i32::from(px_b) - i32::from(b)).abs() <= i32::from(tolerance)
        {
            let (cx, cy) = (index % width as usize, index / width as usize);
            count += 1;
            min_x = min_x.min(cx as i32);
            min_y = min_y.min(cy as i32);
            max_x = max_x.max(cx as i32);
            max_y = max_y.max(cy as i32);
        }
    }

    let bbox = if count > 0 {
        Some(json!({
            "left": x + min_x,
            "top": y + min_y,
            "width": max_x - min_x + 1,
            "height": max_y - min_y + 1
        }))
    } else {
        None
    };

    Ok(json!({
        "content": [{"type": "text", "text": json!({
            "hwnd": hwnd_str(hwnd),
            "color": format!("#{:02X}{:02X}{:02X}", r, g, b),
            "tolerance": tolerance,
            "match_count": count,
            "bbox": bbox
        }).to_string()}]
    }))
}

// ---------------------------------------------------------------------------
// 工具目录与调度
// ---------------------------------------------------------------------------

pub fn tools_list() -> Value {
    json!({"tools": [
        {
            "name": TOOL_LIST,
            "description": "枚举当前桌面的顶层窗口,支持按标题子串、窗口类名或进程 PID 过滤。返回每个窗口的句柄(hwnd,十进制字符串)、pid、标题、类名、可见性、最小化状态和屏幕矩形。",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "标题子串过滤(忽略大小写)"},
                    "class": {"type": "string", "description": "窗口类名子串过滤(忽略大小写)"},
                    "pid": {"type": "integer", "description": "按进程 PID 过滤"},
                    "visible_only": {"type": "boolean", "description": "只返回可见窗口,默认 true"},
                    "limit": {"type": "integer", "description": "最多返回条数,默认 100,上限 500"}
                },
                "additionalProperties": false
            }
        },
        {
            "name": TOOL_GET_RECT,
            "description": "查询一个窗口的外框与客户区屏幕矩形。hwnd 可使用 list_windows 返回的十进制句柄，或使用 @active 查询当前前台窗口。",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hwnd": {"type": "string", "description": "精确窗口句柄，或 @active"}
                },
                "required": ["hwnd"],
                "additionalProperties": false
            }
        },
        {
            "name": TOOL_SCREENSHOT_WINDOW,
            "description": "截取指定窗口的渲染内容。client_only=false（默认）包含标题栏和边框；true 仅截客户区。hwnd 可为 @active；其余定位必须且只能用 hwnd / title / pid 之一。单窗口最高 1200 万像素，超限请先 resize_window。部分 GPU 加速、受保护或无响应窗口可能无法截图。",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hwnd": {"type": "string", "description": "窗口句柄(十进制字符串)"},
                    "title": {"type": "string", "description": "窗口标题子串"},
                    "pid": {"type": "integer", "description": "窗口所属进程 PID"},
                    "client_only": {"type": "boolean", "description": "仅截客户区，默认 false"}
                },
                "oneOf": [{"required": ["hwnd"]}, {"required": ["title"]}, {"required": ["pid"]}],
                "additionalProperties": false
            }
        },
        {
            "name": TOOL_SCREENSHOT_ALL,
            "description": "截取所有显示器（每台一张 PNG）。最高保留三台 UHD 4K（3840x2160）显示器的总像素；超过时按相同比例缩放，并返回原始与实际尺寸。",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        },
        {
            "name": TOOL_RESIZE_WINDOW,
            "description": "调整一个由 hwnd 精确指定的窗口尺寸（可选移动到 x,y）。这是有副作用的窗口布局操作；受目标窗口自身最小尺寸约束。",
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hwnd": {"type": "string", "description": "窗口句柄(十进制字符串，必须由 list_windows 返回)"},
                    "width": {"type": "integer", "minimum": 40, "description": "目标宽度"},
                    "height": {"type": "integer", "minimum": 40, "description": "目标高度"},
                    "x": {"type": "integer", "description": "可选,目标 X 坐标"},
                    "y": {"type": "integer", "description": "可选,目标 Y 坐标"}
                },
                "required": ["hwnd", "width", "height"],
                "additionalProperties": false
            }
        },
        {
            "name": TOOL_ACTIVATE,
            "description": "将一个由 hwnd 精确指定的窗口请求置为前台，用于截图前的人工对比或布局检查。Windows 焦点保护拒绝时返回错误；不发送鼠标或键盘输入。",
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hwnd": {"type": "string", "description": "精确窗口句柄，或 @active"}
                },
                "required": ["hwnd"],
                "additionalProperties": false
            }
        },
        {
            "name": TOOL_PIXEL_SCAN_LINE,
            "description": "在窗口客户区内沿指定垂直列扫描屏幕像素，将颜色相近的连续行合并为颜色带（每个带含起止行、高度和平均颜色）。用于布局验证：测量面板边界、控件高度、空白占位。坐标均为相对客户区的物理像素；窗口被遮挡的区域按屏幕当前显示内容采样。",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hwnd": {"type": "string", "description": "精确窗口句柄，或 @active"},
                    "x": {"type": "integer", "description": "扫描列（客户区 X）"},
                    "y_from": {"type": "integer", "description": "起始行（客户区 Y，含）"},
                    "y_to": {"type": "integer", "description": "结束行（客户区 Y，不含）"},
                    "tolerance": {"type": "integer", "minimum": 0, "maximum": 255, "description": "相邻行颜色差阈值，默认 2"},
                    "min_band": {"type": "integer", "minimum": 1, "description": "小于该高度的颜色带被忽略，默认 2"},
                    "max_bands": {"type": "integer", "minimum": 1, "maximum": 256, "description": "最多返回颜色带数，默认 256"}
                },
                "required": ["hwnd", "x", "y_from", "y_to"],
                "additionalProperties": false
            }
        },
        {
            "name": TOOL_PIXEL_FIND_COLOR,
            "description": "在窗口客户区矩形区域内查找与指定颜色匹配的像素，返回匹配数量和最小包围盒。可与 pixel_scan_line 配合做布局断言；color 为 #RRGGBB，坐标均为相对客户区的物理像素。",
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hwnd": {"type": "string", "description": "精确窗口句柄，或 @active"},
                    "x": {"type": "integer", "description": "区域左上角（客户区 X）"},
                    "y": {"type": "integer", "description": "区域左上角（客户区 Y）"},
                    "width": {"type": "integer", "minimum": 1, "description": "区域宽度"},
                    "height": {"type": "integer", "minimum": 1, "description": "区域高度"},
                    "color": {"type": "string", "description": "目标颜色 #RRGGBB"},
                    "tolerance": {"type": "integer", "minimum": 0, "maximum": 255, "description": "每通道颜色差阈值，默认 0"}
                },
                "required": ["hwnd", "x", "y", "width", "height", "color"],
                "additionalProperties": false
            }
        },
        {
            "name": TOOL_SEND_INPUT,
            "description": "向指定窗口发送鼠标/键盘输入。目标必须保持为前台窗口，每个鼠标坐标必须位于客户区；activate:true 可先请求置前。动作先整体校验再执行，down/key_down 必须在同一批次内由 up/key_up 配平，异常路径会自动释放。每批最多 128 个动作且计划延迟不超过 60 秒。",
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "hwnd": {"type": "string", "description": "精确窗口句柄，或 @active"},
                    "activate": {"type": "boolean", "description": "先请求激活目标窗口再注入，默认 false"},
                    "delay_ms": {"type": "integer", "minimum": 0, "maximum": 5000, "description": "动作间延迟，默认 50"},
                    "actions": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 128,
                        "items": {
                            "type": "object",
                            "properties": {
                                "kind": {"type": "string", "enum": ["move", "click", "down", "up", "scroll", "key_tap", "key_down", "key_up"]},
                                "x": {"type": "integer", "description": "客户区 X（鼠标动作必填）"},
                                "y": {"type": "integer", "description": "客户区 Y（鼠标动作必填）"},
                                "button": {"type": "string", "enum": ["left", "right", "middle"], "description": "鼠标键，默认 left"},
                                "delta": {"type": "integer", "description": "scroll：滚轮增量（正=上滚）"},
                                "vkey": {"type": ["integer", "string"], "description": "虚拟键码或按键名（key_tap/key_down/key_up 必填）"},
                                "steps": {"type": "integer", "minimum": 1, "maximum": 200, "description": "move 插值步数，默认 1"}
                            },
                            "required": ["kind"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["hwnd", "actions"],
                "additionalProperties": false
            }
        }
    ]})
}

pub fn handle_tool_call(name: &str, args: &Value) -> Result<Value, String> {
    match name {
        TOOL_LIST => list_windows(args),
        TOOL_GET_RECT => crate::backend::window::get_window_rect(resolve_explicit_hwnd(args)?),
        TOOL_SCREENSHOT_WINDOW => {
            let hwnd = resolve_target(args)?;
            let client_only = args
                .get("client_only")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let (png, rect) = capture_window(hwnd, client_only)?;
            Ok(json!({
                "content": [
                    {"type": "text", "text": format!(
                        "窗口截图: hwnd={} client_only={} 矩形=({},{}) {}x{} 图像={} bytes",
                        hwnd_str(hwnd), client_only, rect.left, rect.top,
                        rect.right - rect.left, rect.bottom - rect.top, png.len()
                    )},
                    {"type": "image", "data": B64.encode(&png), "mimeType": "image/png"}
                ]
            }))
        }
        TOOL_SCREENSHOT_ALL => {
            let shots = capture_all_monitors()?;
            let mut content = Vec::new();
            for shot in shots {
                let dimensions = format!(
                    "原始 {}x{}，输出 {}x{}",
                    shot.source_width, shot.source_height, shot.output_width, shot.output_height
                );
                if let Some(error) = shot.error {
                    content.push(json!({"type": "text", "text": format!("{}: {dimensions}; 错误: {error}", shot.label)}));
                } else {
                    content.push(json!({"type": "text", "text": format!("{}: {dimensions}; {} bytes", shot.label, shot.png.len())}));
                    content.push(json!({"type": "image", "data": B64.encode(&shot.png), "mimeType": "image/png"}));
                }
            }
            Ok(json!({"content": content}))
        }
        TOOL_RESIZE_WINDOW => {
            let hwnd = resolve_explicit_hwnd(args)?;
            crate::backend::window::resize_window(hwnd, args)
        }
        TOOL_ACTIVATE => crate::backend::window::window_activate(resolve_explicit_hwnd(args)?),
        TOOL_PIXEL_SCAN_LINE => pixel_scan_line(resolve_explicit_hwnd(args)?, args),
        TOOL_PIXEL_FIND_COLOR => pixel_find_color(resolve_explicit_hwnd(args)?, args),
        TOOL_SEND_INPUT => send_input(resolve_explicit_hwnd(args)?, args),
        _ => Err(format!("未知工具 '{name}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::input::MAX_INPUT_ACTIONS;

    #[test]
    fn target_requires_exactly_one_selector() {
        assert!(resolve_target(&json!({})).is_err());
        assert!(resolve_target(&json!({"hwnd": "1", "pid": 1})).is_err());
    }

    #[test]
    fn explicit_hwnd_tools_reject_ambiguous_selectors() {
        assert!(resolve_explicit_hwnd(&json!({"title": "Editor"})).is_err());
        assert!(resolve_explicit_hwnd(&json!({"hwnd": "1", "pid": 1})).is_err());
    }

    #[test]
    fn publishes_window_observation_and_layout_tools() {
        let catalog = tools_list();
        let tools = catalog["tools"].as_array().unwrap();
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        for expected in [
            TOOL_LIST,
            TOOL_GET_RECT,
            TOOL_SCREENSHOT_WINDOW,
            TOOL_SCREENSHOT_ALL,
            TOOL_RESIZE_WINDOW,
            TOOL_ACTIVATE,
            TOOL_PIXEL_SCAN_LINE,
            TOOL_PIXEL_FIND_COLOR,
            TOOL_SEND_INPUT,
        ] {
            assert!(names.contains(&expected), "缺少工具 {expected}");
        }
        let screenshot = tools
            .iter()
            .find(|tool| tool["name"] == TOOL_SCREENSHOT_WINDOW)
            .unwrap();
        assert_eq!(
            screenshot["inputSchema"]["properties"]["client_only"]["type"],
            "boolean"
        );
        let list = tools.iter().find(|tool| tool["name"] == TOOL_LIST).unwrap();
        assert_eq!(list["inputSchema"]["properties"]["class"]["type"], "string");
        // 新扫描工具要求 hwnd 精确指定
        for name in [TOOL_PIXEL_SCAN_LINE, TOOL_PIXEL_FIND_COLOR, TOOL_SEND_INPUT] {
            let scan = tools.iter().find(|tool| tool["name"] == name).unwrap();
            assert!(scan["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "hwnd"));
            assert!(scan["annotations"]["destructiveHint"] == json!(name == TOOL_SEND_INPUT));
        }
        let input = tools
            .iter()
            .find(|tool| tool["name"] == TOOL_SEND_INPUT)
            .unwrap();
        assert_eq!(
            input["inputSchema"]["properties"]["actions"]["maxItems"],
            MAX_INPUT_ACTIONS
        );
    }

    #[test]
    fn merges_adjacent_similar_rows_into_bands() {
        let colors: Vec<(u8, u8, u8)> = vec![
            (250, 250, 250),
            (250, 250, 250),
            (252, 252, 252),
            (245, 245, 245),
            (245, 245, 245),
        ];
        let bands = merge_color_bands(&colors, 2, 1);
        assert_eq!(bands.len(), 2);
        assert_eq!(bands[0].y_from, 0);
        assert_eq!(bands[0].y_to, 2);
        assert_eq!(bands[0].color, (251, 251, 251)); // 250+250+252 平均
        assert_eq!(bands[1].y_from, 3);
        assert_eq!(bands[1].y_to, 4);
        // min_band 过滤过短的颜色带
        let filtered = merge_color_bands(&colors, 2, 3);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].y_from, 0);
        // 相邻行各通道差超过 tolerance 时不合并
        let split: Vec<(u8, u8, u8)> = vec![(0, 0, 0), (10, 0, 0)];
        assert_eq!(merge_color_bands(&split, 0, 1).len(), 2);
        assert_eq!(merge_color_bands(&split, 11, 1).len(), 1);
    }
}
