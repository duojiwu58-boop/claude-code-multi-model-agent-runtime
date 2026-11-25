use serde_json::{Value, json};

/// Engine mode and local approval automation are independent settings.
pub fn mode_tool_config(mode: &str, auto_approve: bool) -> Option<Value> {
    let preset = match mode {
        "request_review" => "AGENT_PERMISSION_PRESET_REQUEST_REVIEW",
        "default" => "AGENT_PERMISSION_PRESET_DEFAULT",
        "auto" => "AGENT_PERMISSION_PRESET_AUTO",
        "turbo" => "AGENT_PERMISSION_PRESET_TURBO",
        _ => return None,
    };
    Some(json!({"localAutoApprove": auto_approve, "permissionConfig": {
        "permissionsV2": true, "effectivePermissionPreset": preset
    }}))
}

/// Automatic mode is explicit; the reusable bridge core defaults to review.
/// The local marker is removed before serializing any LS request.
pub fn tool_config(choice: Option<&str>) -> Option<Value> {
    let choice = choice.unwrap_or("review");
    let preset = match choice {
        "review" => "AGENT_PERMISSION_PRESET_REQUEST_REVIEW",
        "default" => "AGENT_PERMISSION_PRESET_DEFAULT",
        "auto" => "AGENT_PERMISSION_PRESET_REQUEST_REVIEW",
        "turbo" => "AGENT_PERMISSION_PRESET_TURBO",
        _ => return None,
    };
    Some(json!({"localAutoApprove": choice == "auto", "permissionConfig": {
        "permissionsV2": true,
        "effectivePermissionPreset": preset
    }}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn engine_modes_are_independent_of_local_approval() {
        for (mode, suffix) in [("request_review", "REQUEST_REVIEW"), ("default", "DEFAULT"), ("auto", "AUTO"), ("turbo", "TURBO")] {
            for auto in [false, true] {
                let config = mode_tool_config(mode, auto).unwrap();
                assert_eq!(config["localAutoApprove"], auto);
                assert_eq!(config["permissionConfig"]["effectivePermissionPreset"], format!("AGENT_PERMISSION_PRESET_{suffix}"));
            }
        }
        assert!(mode_tool_config("unknown", true).is_none());
    }
    #[test]
    fn defaults_to_review_and_rejects_unknown_instead_of_relaxing() {
        assert_eq!(tool_config(None).unwrap()["localAutoApprove"], false);
        assert_eq!(
            tool_config(None).unwrap()["permissionConfig"]["effectivePermissionPreset"],
            "AGENT_PERMISSION_PRESET_REQUEST_REVIEW"
        );
        assert!(tool_config(Some("unrestricted")).is_none());
        for choice in ["review", "default", "auto", "turbo"] {
            let config = tool_config(Some(choice)).unwrap();
            assert_eq!(config["permissionConfig"]["permissionsV2"], true);
            assert!(config.get("runCommand").is_none());
        }
    }
}
