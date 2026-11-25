//! Official Antigravity 2.35.0 attached-mode discovery, process/port ownership
//! checks, and pinned TLS certificate extraction from `app.asar`.

use crate::{InstanceSummary, TARGET_LS_VERSION, TransportErrorCategory, WireError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    env, fmt, fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Verified attached endpoint including the private CSRF token.
///
/// `Debug` intentionally redacts `csrf_token`; this struct is never serialized.
#[derive(Clone, PartialEq, Eq)]
pub struct DiscoveredEndpoint {
    pub pid: u32,
    pub https_port: u16,
    pub ls_version: String,
    csrf_token: String,
}

impl DiscoveredEndpoint {
    pub fn new(pid: u32, https_port: u16, ls_version: String, csrf_token: String) -> Self {
        Self {
            pid,
            https_port,
            ls_version,
            csrf_token,
        }
    }

    pub fn summary(&self) -> InstanceSummary {
        InstanceSummary {
            pid: self.pid,
            https_port: self.https_port,
            ls_version: self.ls_version.clone(),
        }
    }

    pub(crate) fn csrf_token(&self) -> &str {
        &self.csrf_token
    }
}

impl fmt::Debug for DiscoveredEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiscoveredEndpoint")
            .field("pid", &self.pid)
            .field("https_port", &self.https_port)
            .field("ls_version", &self.ls_version)
            .field("csrf_token", &"[REDACTED]")
            .finish()
    }
}

/// Diagnostic record for a discovery file skipped during instance verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedInstance {
    pub file_name: String,
    pub pid: Option<u32>,
    pub https_port: Option<u16>,
    pub ls_version: Option<String>,
    pub reason_code: String,
    pub detail: String,
}

/// Full result of scanning official `hub_*.json` discovery records.
#[derive(Debug, Clone)]
pub struct DiscoveryReport {
    pub valid: Vec<DiscoveredEndpoint>,
    pub skipped: Vec<SkippedInstance>,
    pub installed_package_version: Option<String>,
    pub install_dir: PathBuf,
    pub discovery_dir: PathBuf,
}

impl DiscoveryReport {
    pub fn valid_summaries(&self) -> Vec<InstanceSummary> {
        self.valid.iter().map(DiscoveredEndpoint::summary).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoverySelectError {
    NoLiveInstance,
    UnsupportedVersion {
        found_versions: Vec<String>,
        target: String,
    },
    RequestedPidNotFound {
        pid: u32,
        available: Vec<InstanceSummary>,
    },
    MultipleInstances {
        candidates: Vec<InstanceSummary>,
    },
}

impl fmt::Display for DiscoverySelectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoLiveInstance => {
                write!(
                    f,
                    "no verified live Antigravity {TARGET_LS_VERSION} language_server instance found"
                )
            }
            Self::UnsupportedVersion {
                found_versions,
                target,
            } => write!(
                f,
                "unsupported official language_server version(s) {:?}; only {target} is verified",
                found_versions
            ),
            Self::RequestedPidNotFound { pid, .. } => {
                write!(
                    f,
                    "requested language_server PID {pid} is not an active verified instance"
                )
            }
            Self::MultipleInstances { candidates } => {
                write!(
                    f,
                    "found {} verified language_server instances; explicit PID selection required",
                    candidates.len()
                )
            }
        }
    }
}

impl std::error::Error for DiscoverySelectError {}

/// Process & TCP listener verification hook (mockable for offline unit tests).
pub trait ProcessPortInspector {
    /// Returns the executable path of `pid` if and only if `pid` is alive and
    /// currently listening on TCP `port`.
    fn listening_process_path(&self, pid: u32, port: u16) -> Result<Option<PathBuf>, WireError>;

    /// Batch-inspects candidate `(pid, port)` pairs. Default implementation
    /// delegates to `listening_process_path`; Windows overrides this to run a
    /// single process/listener query instead of spawning one process per file.
    fn listening_process_paths(
        &self,
        candidates: &[(u32, u16)],
    ) -> Result<std::collections::HashMap<(u32, u16), PathBuf>, WireError> {
        let mut out = std::collections::HashMap::new();
        for &(pid, port) in candidates {
            if let Some(path) = self.listening_process_path(pid, port)? {
                out.insert((pid, port), path);
            }
        }
        Ok(out)
    }
}

/// Windows process and listener inspector backed by PowerShell `Get-Process` +
/// `Get-NetTCPConnection` with `CREATE_NO_WINDOW`.
#[derive(Default, Debug, Clone, Copy)]
pub struct WindowsProcessPortInspector;

impl ProcessPortInspector for WindowsProcessPortInspector {
    fn listening_process_path(&self, pid: u32, port: u16) -> Result<Option<PathBuf>, WireError> {
        let mut map = self.listening_process_paths(&[(pid, port)])?;
        Ok(map.remove(&(pid, port)))
    }

    fn listening_process_paths(
        &self,
        candidates: &[(u32, u16)],
    ) -> Result<std::collections::HashMap<(u32, u16), PathBuf>, WireError> {
        let mut out = std::collections::HashMap::new();
        if candidates.is_empty() {
            return Ok(out);
        }
        // Only validated numeric (u32, u16) literals appear on the command line;
        // credentials never leave Rust memory.
        let pairs_literal = candidates
            .iter()
            .map(|(pid, port)| format!("[pscustomobject]@{{Id={pid};Port={port}}}"))
            .collect::<Vec<_>>()
            .join(",");
        let script = format!(
            "$pairs=@({pairs_literal}); $res=@{{}}; \
             foreach ($pair in $pairs) {{ \
               $id=$pair.Id; $port=$pair.Port; \
               $p=Get-Process -Id $id -ErrorAction SilentlyContinue; \
               if ($p -and $p.Path) {{ \
                 $n=Get-NetTCPConnection -State Listen -LocalPort $port -ErrorAction SilentlyContinue | Where-Object OwningProcess -eq $id; \
                 if ($n) {{ $res[\"$id`:$port\"]=$p.Path }} \
               }} \
             }}; \
             $res | ConvertTo-Json -Compress"
        );
        // Windows PowerShell ships with Windows; PowerShell 7 is not a runtime prerequisite.
        let shell = env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        let mut cmd = Command::new(shell);
        let script = format!("[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); {script}");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let output = cmd
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .map_err(|err| {
                WireError::not_sent(
                    "PROCESS_INSPECT_FAILED",
                    format!("failed to inspect live process/listener via Windows PowerShell: {err}"),
                )
            })?;
        if !output.status.success() {
            return Err(WireError::not_sent("PROCESS_INSPECT_FAILED",
                format!("process/listener inspection exited with {}", output.status)));
        }
        let value = serde_json::from_slice::<Value>(&output.stdout).map_err(|err|
            WireError::not_sent("PROCESS_INSPECT_FAILED", format!("invalid inspection response: {err}")))?;
        if let Some(obj) = value.as_object() {
            for &(pid, port) in candidates {
                let key = format!("{pid}:{port}");
                if let Some(path) = obj.get(&key).and_then(Value::as_str) {
                    out.insert((pid, port), PathBuf::from(path));
                }
            }
        }
        Ok(out)
    }
}

/// Default official resources directory: `%LOCALAPPDATA%\Programs\antigravity\resources`.
pub fn default_install_resources_dir() -> Result<PathBuf, WireError> {
    let local = env::var_os("LOCALAPPDATA").ok_or_else(|| {
        WireError::not_sent(
            "MISSING_ENV_LOCALAPPDATA",
            "LOCALAPPDATA environment variable is not set",
        )
    })?;
    Ok(PathBuf::from(local).join("Programs/antigravity/resources"))
}

/// Default official discovery directory: `%USERPROFILE%\.gemini\antigravity\daemon`.
pub fn default_discovery_dir() -> Result<PathBuf, WireError> {
    let profile = env::var_os("USERPROFILE").ok_or_else(|| {
        WireError::not_sent(
            "MISSING_ENV_USERPROFILE",
            "USERPROFILE environment variable is not set",
        )
    })?;
    Ok(PathBuf::from(profile).join(".gemini/antigravity/daemon"))
}

/// Reads a single file entry from an Electron `app.asar` archive buffer.
/// Only used to read `package.json` and the public pinned TLS certificate module.
pub fn extract_asar_entry(bytes: &[u8], entry_path: &str) -> Result<Vec<u8>, WireError> {
    if bytes.len() < 16 {
        return Err(WireError::not_sent(
            "INVALID_ASAR",
            "app.asar is too short to contain a valid header",
        ));
    }
    let read_u32 = |at: usize| -> usize {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
    };
    let header_size = read_u32(4);
    let json_len = read_u32(12);
    let header_end = 16usize
        .checked_add(json_len)
        .ok_or_else(|| WireError::not_sent("INVALID_ASAR", "app.asar header length overflow"))?;
    let header_slice = bytes.get(16..header_end).ok_or_else(|| {
        WireError::not_sent("INVALID_ASAR", "app.asar header truncated beyond file end")
    })?;
    let header: Value = serde_json::from_slice(header_slice).map_err(|err| {
        WireError::not_sent(
            "INVALID_ASAR",
            format!("failed to parse app.asar JSON header: {err}"),
        )
    })?;

    let mut node = &header;
    for part in entry_path.split('/') {
        node = &node["files"][part];
    }
    let offset_str = node.get("offset").and_then(Value::as_str).ok_or_else(|| {
        WireError::not_sent(
            "ASAR_ENTRY_MISSING",
            format!("missing app.asar entry offset for {entry_path}"),
        )
    })?;
    let offset: usize = offset_str.parse().map_err(|err| {
        WireError::not_sent(
            "INVALID_ASAR",
            format!("invalid app.asar entry offset for {entry_path}: {err}"),
        )
    })?;
    let size = node.get("size").and_then(Value::as_u64).ok_or_else(|| {
        WireError::not_sent(
            "ASAR_ENTRY_MISSING",
            format!("missing app.asar entry size for {entry_path}"),
        )
    })? as usize;

    let data_base = 8usize
        .checked_add(header_size)
        .and_then(|v| v.checked_add(offset))
        .ok_or_else(|| WireError::not_sent("INVALID_ASAR", "app.asar data offset overflow"))?;
    let data_end = data_base
        .checked_add(size)
        .ok_or_else(|| WireError::not_sent("INVALID_ASAR", "app.asar data extent overflow"))?;
    bytes
        .get(data_base..data_end)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| {
            WireError::not_sent(
                "INVALID_ASAR",
                format!("app.asar entry {entry_path} extends past archive end"),
            )
        })
}

/// Extracts the public pinned PEM certificate from `app.asar`'s
/// `dist/actuator/pinnedLsCert.js`. Never accesses private keys or credentials.
pub fn extract_pinned_cert_pem(asar_bytes: &[u8]) -> Result<Vec<u8>, WireError> {
    let module_bytes = extract_asar_entry(asar_bytes, "dist/actuator/pinnedLsCert.js")?;
    let source = std::str::from_utf8(&module_bytes).map_err(|err| {
        WireError::not_sent(
            "INVALID_PINNED_CERT",
            format!("pinnedLsCert.js is not valid UTF-8: {err}"),
        )
    })?;
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let start = source.find(BEGIN).ok_or_else(|| {
        WireError::not_sent(
            "INVALID_PINNED_CERT",
            "pinnedLsCert.js does not contain a PEM certificate header",
        )
    })?;
    let rel_end = source[start..].find(END).ok_or_else(|| {
        WireError::not_sent(
            "INVALID_PINNED_CERT",
            "pinnedLsCert.js certificate is missing PEM footer",
        )
    })?;
    let end = start + rel_end + END.len();
    Ok(source.as_bytes()[start..end].to_vec())
}

/// Extracts `version` from `package.json` inside `app.asar`.
pub fn extract_installed_package_version(asar_bytes: &[u8]) -> Result<String, WireError> {
    let pkg_bytes = extract_asar_entry(asar_bytes, "package.json")?;
    let pkg: Value = serde_json::from_slice(&pkg_bytes).map_err(|err| {
        WireError::not_sent(
            "INVALID_INSTALL_METADATA",
            format!("failed to parse app.asar package.json: {err}"),
        )
    })?;
    pkg.get("version")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            WireError::not_sent(
                "INVALID_INSTALL_METADATA",
                "app.asar package.json is missing a non-empty version field",
            )
        })
}

/// Loads the pinned PEM certificate from `<install_resources_dir>/app.asar`.
pub fn load_pinned_cert_pem(install_resources_dir: &Path) -> Result<Vec<u8>, WireError> {
    let asar_path = install_resources_dir.join("app.asar");
    let bytes = fs::read(&asar_path).map_err(|err| {
        WireError::not_sent(
            "INSTALL_ASAR_UNREADABLE",
            format!("unable to read {}: {err}", asar_path.display()),
        )
    })?;
    extract_pinned_cert_pem(&bytes)
}

/// Discovers and verifies live official Antigravity `2.35.0` instances using
/// default Windows installation and daemon paths.
pub fn discover_instances() -> Result<DiscoveryReport, WireError> {
    let discovery_dir = default_discovery_dir()?;
    let install_dir = default_install_resources_dir()?;
    let inspector = WindowsProcessPortInspector;
    discover_instances_with(&discovery_dir, &install_dir, &inspector)
}

/// Discovers and verifies live official instances with injectable paths and
/// process/listener inspection (used by both production code and offline tests).
pub fn discover_instances_with(
    discovery_dir: &Path,
    install_dir: &Path,
    inspector: &dyn ProcessPortInspector,
) -> Result<DiscoveryReport, WireError> {
    let asar_path = install_dir.join("app.asar");
    let installed_package_version = if asar_path.exists() {
        let asar_bytes = fs::read(&asar_path).map_err(|err| {
            WireError::not_sent(
                "INSTALL_ASAR_UNREADABLE",
                format!("unable to read {}: {err}", asar_path.display()),
            )
        })?;
        Some(extract_installed_package_version(&asar_bytes)?)
    } else {
        None
    };

    let expected_binary_raw = install_dir.join("bin/language_server.exe");
    let expected_binary = expected_binary_raw.canonicalize().ok();

    let mut report = DiscoveryReport {
        valid: Vec::new(),
        skipped: Vec::new(),
        installed_package_version: installed_package_version.clone(),
        install_dir: install_dir.to_path_buf(),
        discovery_dir: discovery_dir.to_path_buf(),
    };

    if !discovery_dir.exists() {
        return Ok(report);
    }

    let mut entries: Vec<PathBuf> = fs::read_dir(discovery_dir)
        .map_err(|err| {
            WireError::not_sent(
                "DISCOVERY_DIR_UNREADABLE",
                format!("unable to read {}: {err}", discovery_dir.display()),
            )
        })?
        .filter_map(|e| e.ok().map(|entry| entry.path()))
        .collect();
    entries.sort();

    struct CandidateFile {
        file_name: String,
        pid: u32,
        port: u16,
        csrf: String,
        version: Option<String>,
    }

    let mut parsed_candidates = Vec::new();

    for path in entries {
        let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !file_name.starts_with("hub_")
            || path.extension().and_then(|s| s.to_str()) != Some("json")
        {
            continue;
        }

        let raw = match fs::read(&path) {
            Ok(b) => b,
            Err(err) => {
                report.skipped.push(SkippedInstance {
                    file_name: file_name.to_owned(),
                    pid: None,
                    https_port: None,
                    ls_version: None,
                    reason_code: "DISCOVERY_FILE_UNREADABLE".to_owned(),
                    detail: err.to_string(),
                });
                continue;
            }
        };

        let value: Value = match serde_json::from_slice(&raw) {
            Ok(v) => v,
            Err(err) => {
                report.skipped.push(SkippedInstance {
                    file_name: file_name.to_owned(),
                    pid: None,
                    https_port: None,
                    ls_version: None,
                    reason_code: "INVALID_DISCOVERY_JSON".to_owned(),
                    detail: err.to_string(),
                });
                continue;
            }
        };

        let pid_u64 = value.get("pid").and_then(Value::as_u64);
        let port_u64 = value.get("httpsPort").and_then(Value::as_u64);
        let token = value
            .get("csrfToken")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let version = value
            .get("lsVersion")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);

        let (Some(pid_raw), Some(port_raw), Some(csrf)) = (pid_u64, port_u64, token) else {
            report.skipped.push(SkippedInstance {
                file_name: file_name.to_owned(),
                pid: pid_u64.and_then(|p| u32::try_from(p).ok()),
                https_port: port_u64.and_then(|p| u16::try_from(p).ok()),
                ls_version: version,
                reason_code: "MISSING_REQUIRED_FIELDS".to_owned(),
                detail: "discovery JSON missing valid pid, httpsPort, or csrfToken".to_owned(),
            });
            continue;
        };

        if pid_raw == 0 || pid_raw > u32::MAX as u64 || port_raw == 0 || port_raw > 65535 {
            report.skipped.push(SkippedInstance {
                file_name: file_name.to_owned(),
                pid: u32::try_from(pid_raw).ok(),
                https_port: u16::try_from(port_raw).ok(),
                ls_version: version,
                reason_code: "OUT_OF_RANGE_FIELDS".to_owned(),
                detail: "pid or httpsPort outside allowed numeric range".to_owned(),
            });
            continue;
        }

        parsed_candidates.push(CandidateFile {
            file_name: file_name.to_owned(),
            pid: pid_raw as u32,
            port: port_raw as u16,
            csrf: csrf.to_owned(),
            version,
        });
    }

    let pairs: Vec<(u32, u16)> = parsed_candidates.iter().map(|c| (c.pid, c.port)).collect();
    let live_paths = inspector.listening_process_paths(&pairs)?;

    for cand in parsed_candidates {
        let CandidateFile {
            file_name,
            pid,
            port,
            csrf,
            version,
        } = cand;

        let Some(actual_image) = live_paths.get(&(pid, port)) else {
            report.skipped.push(SkippedInstance {
                file_name,
                pid: Some(pid),
                https_port: Some(port),
                ls_version: version,
                reason_code: "STALE_OR_NOT_LISTENING".to_owned(),
                detail: "process is not alive or not listening on httpsPort".to_owned(),
            });
            continue;
        };

        let Some(expected_bin) = &expected_binary else {
            report.skipped.push(SkippedInstance {
                file_name,
                pid: Some(pid),
                https_port: Some(port),
                ls_version: version,
                reason_code: "OFFICIAL_BINARY_MISSING".to_owned(),
                detail: format!("expected binary missing: {}", expected_binary_raw.display()),
            });
            continue;
        };

        let actual_canonical = match actual_image.canonicalize() {
            Ok(p) => p,
            Err(_) => actual_image.clone(),
        };
        if &actual_canonical != expected_bin {
            report.skipped.push(SkippedInstance {
                file_name,
                pid: Some(pid),
                https_port: Some(port),
                ls_version: version,
                reason_code: "IMAGE_PATH_MISMATCH".to_owned(),
                detail:
                    "listening process executable path does not match official language_server.exe"
                        .to_owned(),
            });
            continue;
        }

        let Some(ls_version) = version else {
            report.skipped.push(SkippedInstance {
                file_name,
                pid: Some(pid),
                https_port: Some(port),
                ls_version: None,
                reason_code: "UNSUPPORTED_VERSION".to_owned(),
                detail: "discovery JSON missing lsVersion".to_owned(),
            });
            continue;
        };

        if ls_version != TARGET_LS_VERSION {
            report.skipped.push(SkippedInstance {
                file_name,
                pid: Some(pid),
                https_port: Some(port),
                ls_version: Some(ls_version.clone()),
                reason_code: "UNSUPPORTED_VERSION".to_owned(),
                detail: format!(
                    "discovered lsVersion={ls_version}, expected pinned {TARGET_LS_VERSION}"
                ),
            });
            continue;
        }

        match &installed_package_version {
            Some(pkg_ver) if pkg_ver == &ls_version => {}
            other => {
                report.skipped.push(SkippedInstance {
                    file_name,
                    pid: Some(pid),
                    https_port: Some(port),
                    ls_version: Some(ls_version),
                    reason_code: "PACKAGE_VERSION_MISMATCH".to_owned(),
                    detail: format!(
                        "installed app.asar package.json version ({other:?}) does not match {TARGET_LS_VERSION}"
                    ),
                });
                continue;
            }
        }

        report.valid.push(DiscoveredEndpoint {
            pid,
            https_port: port,
            ls_version,
            csrf_token: csrf,
        });
    }

    Ok(report)
}

/// Selects a single verified endpoint without ever guessing between multiple live instances.
pub fn select_endpoint(
    report: &DiscoveryReport,
    requested_pid: Option<u32>,
) -> Result<DiscoveredEndpoint, DiscoverySelectError> {
    if let Some(pid) = requested_pid {
        if let Some(found) = report.valid.iter().find(|ep| ep.pid == pid) {
            return Ok(found.clone());
        }
        if let Some(skipped) = report.skipped.iter().find(|s| {
            s.pid == Some(pid)
                && (s.reason_code == "UNSUPPORTED_VERSION"
                    || s.reason_code == "PACKAGE_VERSION_MISMATCH")
        }) {
            return Err(DiscoverySelectError::UnsupportedVersion {
                found_versions: skipped.ls_version.iter().cloned().collect(),
                target: TARGET_LS_VERSION.to_owned(),
            });
        }
        return Err(DiscoverySelectError::RequestedPidNotFound {
            pid,
            available: report.valid_summaries(),
        });
    }

    match report.valid.len() {
        1 => Ok(report.valid[0].clone()),
        0 => {
            let unsupported: Vec<String> = report
                .skipped
                .iter()
                .filter(|s| {
                    s.reason_code == "UNSUPPORTED_VERSION"
                        || s.reason_code == "PACKAGE_VERSION_MISMATCH"
                })
                .filter_map(|s| s.ls_version.clone())
                .collect();
            if !unsupported.is_empty() {
                Err(DiscoverySelectError::UnsupportedVersion {
                    found_versions: unsupported,
                    target: TARGET_LS_VERSION.to_owned(),
                })
            } else {
                Err(DiscoverySelectError::NoLiveInstance)
            }
        }
        _ => Err(DiscoverySelectError::MultipleInstances {
            candidates: report.valid_summaries(),
        }),
    }
}

impl From<DiscoverySelectError> for WireError {
    fn from(err: DiscoverySelectError) -> Self {
        let code = match &err {
            DiscoverySelectError::NoLiveInstance => "NO_LIVE_INSTANCE",
            DiscoverySelectError::UnsupportedVersion { .. } => "UNSUPPORTED_LS_VERSION",
            DiscoverySelectError::RequestedPidNotFound { .. } => "REQUESTED_PID_NOT_FOUND",
            DiscoverySelectError::MultipleInstances { .. } => "MULTIPLE_INSTANCES",
        };
        Self {
            category: TransportErrorCategory::NotSent,
            code: code.to_owned(),
            http_status: None,
            message: err.to_string(),
            detail: None,
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    #[test]
    fn system_powershell_inspects_local_listener_without_pwsh() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let found = super::ProcessPortInspector::listening_process_path(
            &super::WindowsProcessPortInspector, std::process::id(),
            listener.local_addr().unwrap().port(),
        ).unwrap().expect("current process owns the listener");
        assert_eq!(found.canonicalize().unwrap(), std::env::current_exe().unwrap().canonicalize().unwrap());
    }
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use uuid::Uuid;

    struct MockInspector {
        map: HashMap<(u32, u16), PathBuf>,
    }

    impl ProcessPortInspector for MockInspector {
        fn listening_process_path(
            &self,
            pid: u32,
            port: u16,
        ) -> Result<Option<PathBuf>, WireError> {
            Ok(self.map.get(&(pid, port)).cloned())
        }
    }

    fn build_minimal_asar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut files = serde_json::Map::new();
        let mut body = Vec::new();
        for (path, data) in entries {
            let offset = body.len().to_string();
            let size = data.len();
            body.extend_from_slice(data);
            insert_asar_node(
                &mut files,
                &path.split('/').collect::<Vec<_>>(),
                offset,
                size,
            );
        }
        let header_json = serde_json::to_vec(&json!({ "files": files })).unwrap();
        let json_len = header_json.len() as u32;
        let aligned_len = (json_len + 3) & !3;
        let header_size = aligned_len + 8;
        let mut out = Vec::new();
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&header_size.to_le_bytes());
        out.extend_from_slice(&(aligned_len + 4).to_le_bytes());
        out.extend_from_slice(&json_len.to_le_bytes());
        out.extend_from_slice(&header_json);
        while (out.len() - 16) < aligned_len as usize {
            out.push(0);
        }
        out.extend_from_slice(&body);
        out
    }

    fn insert_asar_node(
        files: &mut serde_json::Map<String, Value>,
        parts: &[&str],
        offset: String,
        size: usize,
    ) {
        if parts.len() == 1 {
            files.insert(
                parts[0].to_owned(),
                json!({ "offset": offset, "size": size }),
            );
            return;
        }
        let entry = files
            .entry(parts[0].to_owned())
            .or_insert_with(|| json!({ "files": {} }));
        let nested = entry
            .get_mut("files")
            .and_then(Value::as_object_mut)
            .unwrap();
        insert_asar_node(nested, &parts[1..], offset, size);
    }

    #[test]
    fn extracts_package_version_and_pinned_cert_from_asar() {
        let cert_js = b"export const CERT = `-----BEGIN CERTIFICATE-----\nMIIBTEST\n-----END CERTIFICATE-----`;";
        let asar = build_minimal_asar(&[
            ("package.json", br#"{"version":"2.35.0"}"#),
            ("dist/actuator/pinnedLsCert.js", cert_js),
        ]);
        assert_eq!(extract_installed_package_version(&asar).unwrap(), "2.35.0");
        let pem = extract_pinned_cert_pem(&asar).unwrap();
        assert_eq!(
            std::str::from_utf8(&pem).unwrap(),
            "-----BEGIN CERTIFICATE-----\nMIIBTEST\n-----END CERTIFICATE-----"
        );
    }

    #[test]
    fn verifies_process_port_version_and_multi_instance_selection() {
        let root = env::temp_dir().join(format!("agcn-disc-test-{}", Uuid::new_v4()));
        let disc_dir = root.join("daemon");
        let install_dir = root.join("resources");
        let bin_dir = install_dir.join("bin");
        fs::create_dir_all(&disc_dir).unwrap();
        fs::create_dir_all(&bin_dir).unwrap();

        let ls_exe = bin_dir.join("language_server.exe");
        let other_exe = root.join("other.exe");
        fs::write(&ls_exe, b"stub").unwrap();
        fs::write(&other_exe, b"other").unwrap();

        let asar = build_minimal_asar(&[("package.json", br#"{"version":"2.35.0"}"#)]);
        fs::write(install_dir.join("app.asar"), asar).unwrap();

        fs::write(
            disc_dir.join("hub_1.json"),
            br#"{"pid":101,"httpsPort":9001,"csrfToken":"secret-a","lsVersion":"2.35.0"}"#,
        )
        .unwrap();
        fs::write(
            disc_dir.join("hub_2.json"),
            br#"{"pid":102,"httpsPort":9002,"csrfToken":"secret-b","lsVersion":"2.35.0"}"#,
        )
        .unwrap();
        fs::write(
            disc_dir.join("hub_3_wrong_ver.json"),
            br#"{"pid":103,"httpsPort":9003,"csrfToken":"secret-c","lsVersion":"2.36.0"}"#,
        )
        .unwrap();
        fs::write(
            disc_dir.join("hub_4_wrong_bin.json"),
            br#"{"pid":104,"httpsPort":9004,"csrfToken":"secret-d","lsVersion":"2.35.0"}"#,
        )
        .unwrap();
        fs::write(
            disc_dir.join("hub_5_stale.json"),
            br#"{"pid":105,"httpsPort":9005,"csrfToken":"secret-e","lsVersion":"2.35.0"}"#,
        )
        .unwrap();

        let mut map = HashMap::new();
        map.insert((101, 9001), ls_exe.clone());
        map.insert((102, 9002), ls_exe.clone());
        map.insert((103, 9003), ls_exe.clone());
        map.insert((104, 9004), other_exe);
        let inspector = MockInspector { map };

        let report = discover_instances_with(&disc_dir, &install_dir, &inspector).unwrap();
        assert_eq!(report.valid.len(), 2);
        assert_eq!(report.skipped.len(), 3);
        assert!(format!("{:?}", report.valid[0]).contains("[REDACTED]"));
        assert!(!format!("{:?}", report.valid[0]).contains("secret-a"));

        // Never guesses between multiple valid instances.
        assert!(matches!(
            select_endpoint(&report, None),
            Err(DiscoverySelectError::MultipleInstances { .. })
        ));
        assert_eq!(select_endpoint(&report, Some(102)).unwrap().pid, 102);
        assert!(matches!(
            select_endpoint(&report, Some(103)),
            Err(DiscoverySelectError::UnsupportedVersion { .. })
        ));

        let _ = fs::remove_dir_all(&root);
    }
}
