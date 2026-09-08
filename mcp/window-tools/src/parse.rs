//! 纯参数解析原语:不依赖 Win32、不产生副作用,便于单元测试与复用。
//! JSON 整数必须显式验证范围,避免坐标/尺寸/PID 被静默截断。
//! 本模块错误全部为 [`INVALID_ARGUMENT`](crate::error::INVALID_ARGUMENT)。

use serde_json::Value;

use crate::error::{ToolError, INVALID_ARGUMENT};

/// JSON 整数不能直接 `as i32`：先验证范围，避免窗口坐标/尺寸被静默截断。
/// 多模型接入容错：数字与整数字符串("120")等价;可选字段传 null 视为缺省。
/// 浮点数显式拒绝——模型选错字段/类型的信号,静默截断是语义 bug。
pub fn json_i32(args: &Value, key: &str, required: bool) -> Result<Option<i32>, ToolError> {
    let Some(value) = args.get(key) else {
        return if required {
            Err(ToolError::new(INVALID_ARGUMENT, format!("{key} 参数缺失")))
        } else {
            Ok(None)
        };
    };
    if value.is_null() && !required {
        return Ok(None);
    }
    let invalid = || {
        ToolError::new(
            INVALID_ARGUMENT,
            format!("{key} 必须是 32 位整数(或整数字符串)"),
        )
    };
    let raw = match value {
        Value::Number(n) => n.as_i64().ok_or_else(invalid)?,
        Value::String(s) => s.trim().parse::<i64>().map_err(|_| invalid())?,
        _ => return Err(invalid()),
    };
    i32::try_from(raw)
        .map(Some)
        .map_err(|_| ToolError::new(INVALID_ARGUMENT, format!("{key} 超出 32 位整数范围")))
}

/// Windows PID 是 u32；拒绝超范围 JSON 数字，而不是截断后指向其他进程。
/// 容错语义与 [`json_i32`] 一致：整数字符串等价，null 视为缺省。
pub fn json_u32(args: &Value, key: &str) -> Result<Option<u32>, ToolError> {
    let Some(value) = args.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let invalid = || {
        ToolError::new(
            INVALID_ARGUMENT,
            format!("{key} 必须是非负整数(或整数字符串)"),
        )
    };
    let raw = match value {
        Value::Number(n) => n.as_u64().ok_or_else(invalid)?,
        Value::String(s) => s.trim().parse::<u64>().map_err(|_| invalid())?,
        _ => return Err(invalid()),
    };
    u32::try_from(raw)
        .map(Some)
        .map_err(|_| ToolError::new(INVALID_ARGUMENT, format!("{key} 超出 Windows PID 范围")))
}

/// 解析 hwnd 字符串:带 0x/0X 前缀按十六进制,否则按十进制。
/// 之前的实现只是剥掉前缀后仍按十进制 parse,含 A-F 的十六进制句柄必然失败。
pub fn parse_hwnd_str(raw: &str, display: &str) -> Result<i64, ToolError> {
    let invalid = || {
        ToolError::new(
            INVALID_ARGUMENT,
            format!("hwnd '{display}' 不是有效的十进制/十六进制句柄"),
        )
    };
    let trimmed = raw.trim();
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        let value = u64::from_str_radix(hex, 16).map_err(|_| invalid())?;
        i64::try_from(value).map_err(|_| {
            ToolError::new(
                INVALID_ARGUMENT,
                format!("hwnd '{display}' 超出 64 位句柄范围"),
            )
        })
    } else {
        trimmed.parse::<i64>().map_err(|_| invalid())
    }
}

/// 解析 "#RRGGBB" / "RRGGBB" 颜色，返回 (r, g, b)。
pub fn parse_hex_color(raw: &str) -> Result<(u8, u8, u8), ToolError> {
    let s = raw.trim().trim_start_matches('#');
    if s.len() != 6 || !s.is_ascii() {
        return Err(ToolError::new(
            INVALID_ARGUMENT,
            format!("颜色必须是 6 位十六进制 RRGGBB，实际为 '{raw}'"),
        ));
    }
    let hex_byte = |range: std::ops::Range<usize>| {
        u8::from_str_radix(&s[range], 16).map_err(|_| {
            ToolError::new(INVALID_ARGUMENT, format!("颜色 '{raw}' 含非法十六进制字符"))
        })
    };
    Ok((hex_byte(0..2)?, hex_byte(2..4)?, hex_byte(4..6)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_out_of_range_json_integers() {
        assert!(json_i32(&json!({"x": 2_147_483_648_i64}), "x", true).is_err());
        assert!(json_u32(&json!({"pid": 4_294_967_296_u64}), "pid").is_err());
    }

    #[test]
    fn accepts_numeric_strings_across_models() {
        assert_eq!(
            json_i32(&json!({"x": "120"}), "x", true).unwrap(),
            Some(120)
        );
        assert_eq!(
            json_i32(&json!({"x": " 120 "}), "x", true).unwrap(),
            Some(120)
        );
        assert_eq!(json_i32(&json!({"x": 120}), "x", true).unwrap(), Some(120));
        assert_eq!(json_u32(&json!({"pid": "42"}), "pid").unwrap(), Some(42));
        // 负数 PID 字符串仍拒绝
        assert!(json_u32(&json!({"pid": "-1"}), "pid").is_err());
    }

    #[test]
    fn treats_null_optional_as_absent() {
        assert_eq!(
            json_i32(&json!({"steps": serde_json::Value::Null}), "steps", false).unwrap(),
            None
        );
        assert_eq!(
            json_u32(&json!({"pid": serde_json::Value::Null}), "pid").unwrap(),
            None
        );
        // 必填字段的 null 仍是错误
        assert!(json_i32(&json!({"x": serde_json::Value::Null}), "x", true).is_err());
    }

    #[test]
    fn rejects_floats_and_non_numeric_strings() {
        // 浮点是模型选错类型的信号,拒绝而非截断
        assert!(json_i32(&json!({"x": 120.5}), "x", true).is_err());
        assert!(json_i32(&json!({"x": "12.5"}), "x", true).is_err());
        assert!(json_u32(&json!({"pid": "abc"}), "pid").is_err());
        assert!(json_u32(&json!({"pid": 1.5}), "pid").is_err());
    }

    #[test]
    fn errors_carry_stable_codes() {
        let err = json_i32(&json!({"x": "nope"}), "x", true).unwrap_err();
        assert_eq!(err.code, crate::error::INVALID_ARGUMENT);
        assert!(!err.retryable);
    }

    #[test]
    fn parses_decimal_and_hex_hwnds() {
        assert_eq!(parse_hwnd_str("19793406", "19793406").unwrap(), 19793406);
        assert_eq!(parse_hwnd_str("0x12E05FE", "0x12E05FE").unwrap(), 19793406);
        assert_eq!(
            parse_hwnd_str(" 0X12e05fe ", " 0X12e05fe ").unwrap(),
            19793406
        );
        // 十进制负值保持旧行为兼容
        assert_eq!(parse_hwnd_str("-123", "-123").unwrap(), -123);
    }

    #[test]
    fn rejects_invalid_hwnd_strings() {
        assert!(parse_hwnd_str("0xGGGG", "0xGGGG").is_err());
        assert!(parse_hwnd_str("123abc", "123abc").is_err());
        assert!(parse_hwnd_str("", "").is_err());
        // 超出 64 位句柄范围
        assert!(parse_hwnd_str("0xFFFFFFFFFFFFFFFFF", "x").is_err());
    }

    #[test]
    fn parses_hex_colors_with_and_without_hash() {
        assert_eq!(parse_hex_color("#FAFAFA").unwrap(), (250, 250, 250));
        assert_eq!(parse_hex_color("F5F5F5").unwrap(), (245, 245, 245));
        assert_eq!(parse_hex_color(" #ff00AA ").unwrap(), (255, 0, 170));
        assert!(parse_hex_color("#FFF").is_err());
        assert!(parse_hex_color("GGGGGG").is_err());
        assert!(parse_hex_color("FAFAF").is_err());
        assert!(parse_hex_color("中文中文").is_err());
    }
}
