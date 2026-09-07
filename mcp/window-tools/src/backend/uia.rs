//! UIA (UI Automation) 无障碍树查询:命中测试与聚焦元素属性读取。
//!
//! 这是库内唯一的 COM 使用点。COM 初始化限定在 worker 线程内按调用完成
//! (库内其他路径无 COM 需求,不在主线程全局初始化);查询走跨进程 COM,
//! 目标进程挂起时调用会阻塞,因此复用通用限时执行原语,服务绝不被拖死。

use serde_json::{json, Value};
use windows::{
    core::{BSTR, HRESULT},
    Win32::{
        Foundation::{HWND, POINT},
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
            COINIT_APARTMENTTHREADED,
        },
        UI::Accessibility::{
            CUIAutomation, IUIAutomation, IUIAutomationElement, UIA_ButtonControlTypeId,
            UIA_CalendarControlTypeId, UIA_CheckBoxControlTypeId, UIA_ComboBoxControlTypeId,
            UIA_CustomControlTypeId, UIA_DataGridControlTypeId, UIA_DataItemControlTypeId,
            UIA_DocumentControlTypeId, UIA_EditControlTypeId, UIA_GroupControlTypeId,
            UIA_HeaderControlTypeId, UIA_HeaderItemControlTypeId, UIA_HyperlinkControlTypeId,
            UIA_ImageControlTypeId, UIA_ListControlTypeId, UIA_ListItemControlTypeId,
            UIA_MenuBarControlTypeId, UIA_MenuControlTypeId, UIA_MenuItemControlTypeId,
            UIA_PaneControlTypeId, UIA_ProgressBarControlTypeId, UIA_RadioButtonControlTypeId,
            UIA_ScrollBarControlTypeId, UIA_SeparatorControlTypeId, UIA_SliderControlTypeId,
            UIA_SpinnerControlTypeId, UIA_SplitButtonControlTypeId, UIA_StatusBarControlTypeId,
            UIA_TabControlTypeId, UIA_TabItemControlTypeId, UIA_TableControlTypeId,
            UIA_TextControlTypeId, UIA_ThumbControlTypeId, UIA_TitleBarControlTypeId,
            UIA_ToolBarControlTypeId, UIA_ToolTipControlTypeId, UIA_TreeControlTypeId,
            UIA_TreeItemControlTypeId, UIA_WindowControlTypeId, UIA_CONTROLTYPE_ID,
        },
    },
};

use crate::backend::{capture::run_with_timeout, window::client_rect_of, window::hwnd_str};
use crate::error::{ToolError, INVALID_ARGUMENT, UIA_UNAVAILABLE, WINDOW_HUNG};
use crate::parse::json_i32;

/// UIA 查询硬超时:跨进程 COM 可能被目标进程挂起拖住,超过即返回结构化错误。
pub const UIA_TIMEOUT_MS: u64 = 5_000;

/// 在窗口客户区指定坐标做 UIA 命中测试,返回该点的无障碍元素属性。
/// 坐标为客户区物理像素,内部转屏幕坐标执行 ElementFromPoint;
/// 该点被其他窗口遮挡时按屏幕现状返回遮挡方元素。
pub fn inspect_element_at(hwnd: HWND, args: &Value) -> Result<Value, ToolError> {
    let x = json_i32(args, "x", true)?.expect("required value checked above");
    let y = json_i32(args, "y", true)?.expect("required value checked above");
    // 与像素扫描工具一致的客户区边界校验(物理像素)。
    // 注意 client_rect_of 返回的是屏幕坐标矩形(角点已 ClientToScreen),
    // 因此局部尺寸取差值,屏幕点 = 屏幕原点 + 客户区局部坐标。
    let client = client_rect_of(hwnd)?;
    let cw = client.right - client.left;
    let ch = client.bottom - client.top;
    if x < 0 || y < 0 || x >= cw || y >= ch {
        return Err(ToolError::new(
            INVALID_ARGUMENT,
            format!("检查点 ({x},{y}) 超出窗口客户区 {cw}x{ch}"),
        ));
    }
    let pt = POINT {
        x: client.left + x,
        y: client.top + y,
    };
    let element = run_with_timeout(
        UIA_TIMEOUT_MS,
        move || query_element_at(pt),
        |ms| {
            ToolError::new(
                WINDOW_HUNG,
                format!("UIA 查询超过 {ms}ms 未完成，目标进程可能无响应"),
            )
        },
    )?;
    Ok(json!({
        "content": [{"type": "text", "text": json!({
            "hwnd": hwnd_str(hwnd),
            "client_point": {"x": x, "y": y},
            "screen_point": {"x": pt.x, "y": pt.y},
            "element": element
        }).to_string()}]
    }))
}

/// 读取系统当前键盘聚焦元素的 UIA 属性,无需指定窗口。
pub fn inspect_focused_element() -> Result<Value, ToolError> {
    let element = run_with_timeout(UIA_TIMEOUT_MS, query_focused_element, |ms| {
        ToolError::new(
            WINDOW_HUNG,
            format!("UIA 查询超过 {ms}ms 未完成，目标进程可能无响应"),
        )
    })?;
    Ok(json!({
        "content": [{"type": "text", "text": json!({
            "element": element
        }).to_string()}]
    }))
}

fn query_element_at(screen_pt: POINT) -> Result<Value, ToolError> {
    with_com(|| {
        let automation = new_automation()?;
        let element: IUIAutomationElement = unsafe { automation.ElementFromPoint(screen_pt) }
            .map_err(|e| ToolError::new(UIA_UNAVAILABLE, format!("ElementFromPoint 失败: {e}")))?;
        element_info(&element)
    })
}

fn query_focused_element() -> Result<Value, ToolError> {
    with_com(|| {
        let automation = new_automation()?;
        let element: IUIAutomationElement = unsafe { automation.GetFocusedElement() }
            .map_err(|e| ToolError::new(UIA_UNAVAILABLE, format!("GetFocusedElement 失败: {e}")))?;
        element_info(&element)
    })
}

fn new_automation() -> Result<IUIAutomation, ToolError> {
    unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| ToolError::new(UIA_UNAVAILABLE, format!("创建 UIA 客户端失败: {e}")))
}

/// COM 初始化限定在本函数:worker 线程每次查询都是新建线程,CoInitializeEx
/// 应返回 S_OK;RPC_E_CHANGED_MODE 表示线程已被其他模式初始化,COM 仍可用。
/// 接口指针随闭包返回释放,之后才反初始化 COM。
fn with_com<T>(f: impl FnOnce() -> Result<T, ToolError>) -> Result<T, ToolError> {
    const RPC_E_CHANGED_MODE: HRESULT = HRESULT(-2147417850); // 0x80010106
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_err() && hr != RPC_E_CHANGED_MODE {
        return Err(ToolError::new(
            UIA_UNAVAILABLE,
            format!("COM 初始化失败: 0x{:08X}", hr.0 as u32),
        ));
    }
    let result = f();
    unsafe { CoUninitialize() };
    result
}

/// 元素属性快照:ControlType 读取失败整体报错(它是调用方分支的核心字段),
/// 其余可选属性降级为空值——部分提供程序不实现 Name/ClassName 等。
fn element_info(element: &IUIAutomationElement) -> Result<Value, ToolError> {
    let control_type_id = unsafe { element.CurrentControlType() }
        .map_err(|e| ToolError::new(UIA_UNAVAILABLE, format!("读取 ControlType 失败: {e}")))?
        .0;
    let bounding_rect = unsafe { element.CurrentBoundingRectangle() }.ok().map(|r| {
        json!({
            "left": r.left,
            "top": r.top,
            "width": r.right - r.left,
            "height": r.bottom - r.top
        })
    });
    Ok(json!({
        "control_type": control_type_name(UIA_CONTROLTYPE_ID(control_type_id)),
        "control_type_id": control_type_id,
        "name": bstr_to_string(unsafe { element.CurrentName() }),
        "class_name": bstr_to_string(unsafe { element.CurrentClassName() }),
        "automation_id": bstr_to_string(unsafe { element.CurrentAutomationId() }),
        "bounding_rect": bounding_rect,
        "keyboard_focusable": unsafe { element.CurrentIsKeyboardFocusable() }
            .map(|b| b.as_bool())
            .unwrap_or(false)
    }))
}

/// 可选 BSTR 属性读取失败 → 空字符串,不让单属性缺失拖垮整个查询。
fn bstr_to_string(result: windows::core::Result<BSTR>) -> String {
    result.map(|b| b.to_string()).unwrap_or_default()
}

/// UIA 控件类型码 → 人类可读名称(标准类型集,未知类型返回 "Unknown")。
// UIA 官方常量是驼峰命名,用作 match pattern 会触发 non_upper_case_globals,
// 此处保留官方命名以自文档化。
#[allow(non_upper_case_globals)]
fn control_type_name(id: UIA_CONTROLTYPE_ID) -> &'static str {
    match id {
        UIA_ButtonControlTypeId => "Button",
        UIA_CalendarControlTypeId => "Calendar",
        UIA_CheckBoxControlTypeId => "CheckBox",
        UIA_ComboBoxControlTypeId => "ComboBox",
        UIA_EditControlTypeId => "Edit",
        UIA_HyperlinkControlTypeId => "Hyperlink",
        UIA_ImageControlTypeId => "Image",
        UIA_ListItemControlTypeId => "ListItem",
        UIA_ListControlTypeId => "List",
        UIA_MenuControlTypeId => "Menu",
        UIA_MenuBarControlTypeId => "MenuBar",
        UIA_MenuItemControlTypeId => "MenuItem",
        UIA_ProgressBarControlTypeId => "ProgressBar",
        UIA_RadioButtonControlTypeId => "RadioButton",
        UIA_ScrollBarControlTypeId => "ScrollBar",
        UIA_SliderControlTypeId => "Slider",
        UIA_SpinnerControlTypeId => "Spinner",
        UIA_StatusBarControlTypeId => "StatusBar",
        UIA_TabControlTypeId => "Tab",
        UIA_TabItemControlTypeId => "TabItem",
        UIA_TextControlTypeId => "Text",
        UIA_ToolBarControlTypeId => "ToolBar",
        UIA_ToolTipControlTypeId => "ToolTip",
        UIA_TreeControlTypeId => "Tree",
        UIA_TreeItemControlTypeId => "TreeItem",
        UIA_CustomControlTypeId => "Custom",
        UIA_GroupControlTypeId => "Group",
        UIA_ThumbControlTypeId => "Thumb",
        UIA_DataGridControlTypeId => "DataGrid",
        UIA_DataItemControlTypeId => "DataItem",
        UIA_DocumentControlTypeId => "Document",
        UIA_SplitButtonControlTypeId => "SplitButton",
        UIA_WindowControlTypeId => "Window",
        UIA_PaneControlTypeId => "Pane",
        UIA_HeaderControlTypeId => "Header",
        UIA_HeaderItemControlTypeId => "HeaderItem",
        UIA_TableControlTypeId => "Table",
        UIA_TitleBarControlTypeId => "TitleBar",
        UIA_SeparatorControlTypeId => "Separator",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_standard_control_types() {
        assert_eq!(control_type_name(UIA_CONTROLTYPE_ID(50000)), "Button");
        assert_eq!(control_type_name(UIA_CONTROLTYPE_ID(50004)), "Edit");
        assert_eq!(control_type_name(UIA_CONTROLTYPE_ID(50020)), "Text");
        assert_eq!(control_type_name(UIA_CONTROLTYPE_ID(50032)), "Window");
        assert_eq!(control_type_name(UIA_CONTROLTYPE_ID(50038)), "Separator");
        assert_eq!(control_type_name(UIA_CONTROLTYPE_ID(49999)), "Unknown");
    }

    #[test]
    fn optional_bstr_failure_degrades_to_empty() {
        let err: windows::core::Result<BSTR> = Err(windows::core::Error::from_hresult(
            HRESULT(-2147467259), // E_FAIL
        ));
        assert_eq!(bstr_to_string(err), "");
    }
}
