//! Validate before creating directories; resolve existing ancestors and Windows aliases.
use std::path::{Component, Path, PathBuf};
use crate::EngineError;

fn resolve(path: &Path) -> Result<PathBuf, EngineError> {
    if !path.is_absolute() {
        return Err(EngineError::not_sent("DATA_ROOT_NOT_ABSOLUTE", "引擎数据目录必须是绝对路径"));
    }
    let mut out = PathBuf::new();
    for part in path.components() {
        if let Component::Prefix(_) = part { out.push(part.as_os_str()); continue; }
        match part {
            Component::CurDir => {},
            Component::ParentDir => { out.pop(); },
            other => out.push(other.as_os_str()),
        }
        if !out.is_absolute() { continue; }
        match out.canonicalize() {
            Ok(real) => out = real,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
            Err(e) => return Err(EngineError::not_sent("DATA_ROOT_UNRESOLVED", format!("无法核验数据目录的实际路径 {}: {e}", out.display()))),
        }
    }
    Ok(out)
}

fn key(path: &Path) -> String {
    path.to_string_lossy().trim_start_matches(r"\\?\").replace('\\', "/").trim_end_matches('/').to_lowercase()
}

fn validate_against(path: &Path, official: &Path) -> Result<PathBuf, EngineError> {
    let resolved = resolve(path)?;
    let a = key(&resolved);
    let b = key(&resolve(official)?);
    if a == b || a.starts_with(&(b.clone() + "/")) || b.starts_with(&(a + "/")) {
        return Err(EngineError::not_sent("SHARED_DATA_ROOT_FORBIDDEN", "禁止使用官方共享数据目录及其父子目录"));
    }
    Ok(resolved)
}

pub(crate) fn validate(path: &Path) -> Result<PathBuf, EngineError> {
    let home = std::env::var_os("USERPROFILE")
        .ok_or_else(|| EngineError::not_sent("MISSING_USERPROFILE", "无法确定官方数据目录"))?;
    let resolved = validate_against(path, &PathBuf::from(home).join(".gemini"))?;
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        validate_against(&resolved, &PathBuf::from(local).join("AntigravityCN/engine"))?;
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_missing_shared_root_and_parent_without_creating_them() {
        let base = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let official = base.join(".gemini");
        for target in [official.clone(), official.join("child"), base.clone(), base.join(".GEMINI/child")] {
            assert!(validate_against(&target, &official).is_err());
        }
        assert!(validate_against(&base.join("private/../engine"), &official).is_ok());
        assert!(!base.exists());
        assert!(validate_against(Path::new("relative"), &official).is_err());
    }
}
