//! Durable session binding and transcript, never an instruction source.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session: String,
    pub cascade: String,
    pub profile: String,
    pub model: String,
    pub workspace: String,
    pub history: Vec<Value>,
    /// Persisted BEFORE dispatch. A crash cannot turn an uncertain write into a retry.
    pub uncertain: bool,
}

pub fn save(directory: &Path, record: &SessionRecord) -> Result<(), String> {
    use std::io::Write;
    let filename = format!("{}.json", record.cascade);
    let path = directory.join(filename);
    let temporary = directory.join(format!("{}.tmp", record.cascade));
    let bytes = serde_json::to_vec(record).map_err(|e| e.to_string())?;
    let mut file = std::fs::File::create(&temporary).map_err(|e| e.to_string())?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    drop(file);
    // Windows rename cannot replace an existing file; MoveFileExW does atomically.
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let source: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING
                    | windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    #[cfg(not(windows))]
    std::fs::rename(temporary, path).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn load(directory: &Path) -> Result<Vec<SessionRecord>, String> {
    std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let mut records = vec![];
    for entry in std::fs::read_dir(directory).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        let record: SessionRecord = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if uuid::Uuid::parse_str(&record.cascade).is_err() {
            return Err("Invalid registered Cascade".into());
        }
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uncertain_registration_survives_interrupted_replacement() {
        let directory =
            std::env::temp_dir().join(format!("ls-registry-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let mut record = SessionRecord {
            session: "client".into(),
            cascade: uuid::Uuid::new_v4().to_string(),
            profile: "ls.json".into(),
            model: "model".into(),
            workspace: directory.to_string_lossy().into_owned(),
            history: vec![],
            uncertain: true,
        };
        save(&directory, &record).unwrap();
        let temporary = directory.join(format!("{}.tmp", record.cascade));
        std::fs::write(&temporary, b"interrupted write").unwrap();
        assert!(load(&directory).unwrap()[0].uncertain);
        record.uncertain = false;
        record
            .history
            .push(serde_json::json!({"role":"user","content":"exact"}));
        save(&directory, &record).unwrap();
        let restored = load(&directory).unwrap();
        assert!(!restored[0].uncertain);
        assert_eq!(restored[0].history, record.history);
        std::fs::remove_file(directory.join(format!("{}.json", record.cascade))).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
