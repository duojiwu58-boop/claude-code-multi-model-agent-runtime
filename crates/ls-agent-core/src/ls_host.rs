//! Owns only the LS child we create. No GUI, credential copying or shared daemon discovery.
use crate::EngineError;
use ls_wire::{
    DiscoveredEndpoint, ProcessPortInspector, TARGET_LS_VERSION, WindowsProcessPortInspector,
    extract_installed_package_version, extract_pinned_cert_pem,
};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Read,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

fn resolve_install_root_from(
    username: Option<OsString>,
    local_app_data: Option<OsString>,
    user_profile: Option<OsString>,
) -> Result<PathBuf, EngineError> {
    if let Some(user) = username.filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(r"C:\Users")
            .join(user)
            .join(r"AppData\Local\Programs\antigravity"));
    }
    if let Some(local) = local_app_data.filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(local).join(r"Programs\antigravity"));
    }
    if let Some(profile) = user_profile.filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(profile).join(r"AppData\Local\Programs\antigravity"));
    }
    Err(error("MISSING_INSTALL_ROOT_ENV"))
}

#[derive(Clone, Debug)]
pub struct LsHostConfig {
    pub install_root: PathBuf,
    pub data_root: PathBuf,
    pub startup_timeout: Duration,
}

impl LsHostConfig {
    pub fn default_install_root() -> Result<PathBuf, EngineError> {
        resolve_install_root_from(
            std::env::var_os("USERNAME"),
            std::env::var_os("LOCALAPPDATA"),
            std::env::var_os("USERPROFILE"),
        )
    }
    pub fn for_data_root(data_root: PathBuf) -> Result<Self, EngineError> {
        Ok(Self {
            install_root: Self::default_install_root()?,
            data_root,
            startup_timeout: Duration::from_secs(40),
        })
    }
    pub fn default_local() -> Result<Self, EngineError> {
        let local =
            std::env::var_os("LOCALAPPDATA").ok_or_else(|| error("MISSING_LOCALAPPDATA"))?;
        Self::for_data_root(PathBuf::from(local).join("ClaudeCodeBridge/ls-engine"))
    }
}

fn error(code: &str) -> EngineError {
    EngineError::not_sent(
        code,
        match code {
            "MISSING_INSTALL_ROOT_ENV" => {
                "无法从系统环境变量（USERNAME / LOCALAPPDATA / USERPROFILE）确定 Antigravity 安装目录。"
            }
            "ENGINE_DATA_BUSY" => "引擎数据目录已被占用，不能同时启动第二个实例。",
            "UNSUPPORTED_LS_VERSION" => "指定安装的引擎版本不受支持。",
            "LS_START_TIMEOUT" => "自有引擎启动超时，已回收本次进程。",
            "LS_EXITED" => "自有引擎已退出，需要重新连接；未重发任何任务。",
            _ => "自有引擎启动或回收失败，请检查安装、数据目录和错误码。",
        },
    )
}

/// Holding this object keeps the exclusive data lock, child stdin and Windows Job alive.
pub struct LsHost {
    child: Child,
    job: Job,
    _lock: File,
    readers: Vec<thread::JoinHandle<()>>,
    endpoint: DiscoveredEndpoint,
    binary: PathBuf,
    cert: Vec<u8>,
    stopped: bool,
}

impl LsHost {
    pub fn start(config: &LsHostConfig) -> Result<Self, EngineError> {
        let resources = config.install_root.join("resources");
        let binary = resources
            .join("bin/language_server.exe")
            .canonicalize()
            .map_err(|_| error("LS_BINARY_MISSING"))?;
        let asar =
            fs::read(resources.join("app.asar")).map_err(|_| error("INSTALL_ASAR_UNREADABLE"))?;
        let version = extract_installed_package_version(&asar)
            .map_err(|e| EngineError::from_wire(e, "无法读取引擎版本"))?;
        if ![TARGET_LS_VERSION, "2.35.1"].contains(&version.as_str()) {
            return Err(error("UNSUPPORTED_LS_VERSION"));
        }
        let cert = extract_pinned_cert_pem(&asar)
            .map_err(|e| EngineError::from_wire(e, "无法读取引擎公钥证书"))?;
        let data_root = crate::data_root::validate(&config.data_root)?;
        fs::create_dir_all(&data_root).map_err(|_| error("ENGINE_DATA_UNWRITABLE"))?;
        let data_root = crate::data_root::validate(&data_root)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(data_root.join("agcn-engine.lock"))
            .map_err(|_| error("ENGINE_DATA_UNWRITABLE"))?;
        lock.try_lock().map_err(|_| error("ENGINE_DATA_BUSY"))?;
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let job = Job::new()?;
        let mut cmd = Command::new(&binary);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // Suspended until assigned to the kill-on-close job: no descendant can escape.
            cmd.creation_flags(0x0800_0004);
        }
        let mut child = cmd
            .args(["--standalone", "--gemini_dir"])
            .arg(&data_root)
            .args([
                "--app_data_dir",
                "antigravity",
                "--config_dir",
                "config",
                "--https_server_port",
                "0",
                "--override_ide_name",
                "antigravity",
                "--override_ide_version",
                &version,
                "--override_user_agent_name",
                "antigravity",
                "--subclient_type",
                "hub",
                "--api_server_url",
                "https://generativelanguage.googleapis.com",
                "--cloud_code_endpoint",
                "https://daily-cloudcode-pa.googleapis.com",
                "--exit_on_stdin_close",
                "--disable_telemetry",
            ])
            .env("ANTIGRAVITY_SERVER_CSRF_TOKEN", &token)
            .env("AGY_REQUIRE_AUTHENTICATED_UI_BOOTSTRAP", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| error("LS_SPAWN_FAILED"))?;
        if let Err(err) = job.assign(&child).and_then(|()| job.resume(&child)) {
            drop(child.stdin.take());
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let readers = vec![
            drain(child.stdout.take().unwrap(), tx.clone()),
            drain(child.stderr.take().unwrap(), tx),
        ];
        let endpoint = DiscoveredEndpoint::new(child.id(), 0, version, token);
        let mut host = Self {
            child,
            job,
            _lock: lock,
            readers,
            endpoint,
            binary,
            cert,
            stopped: false,
        };
        let deadline = Instant::now() + config.startup_timeout;
        while Instant::now() < deadline {
            host.check_alive()?;
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(port) => { host.endpoint.https_port = port; },
                Err(mpsc::RecvTimeoutError::Disconnected) if host.endpoint.https_port == 0 => {
                    return Err(error("LS_OUTPUT_CLOSED"));
                },
                Err(_) => {},
            }
            if host.endpoint.https_port != 0 {
                if host.verify().is_ok() { return Ok(host); }
                thread::sleep(Duration::from_millis(50));
            }
        }
        Err(error("LS_START_TIMEOUT"))
    }

    pub fn endpoint(&self) -> DiscoveredEndpoint {
        self.endpoint.clone()
    }
    pub fn cert(&self) -> &[u8] {
        &self.cert
    }
    pub fn check_alive(&mut self) -> Result<(), EngineError> {
        match self.child.try_wait().map_err(|_| error("LS_WAIT_FAILED"))? {
            None if !self.stopped => Ok(()),
            Some(status) => Err(EngineError::not_sent(
                if status.code() == Some(42) {
                    "LS_RESTART_REQUESTED"
                } else {
                    "LS_EXITED"
                },
                format!(
                    "自有引擎已退出（代码 {:?}），请重新连接；不会自动重发任务。",
                    status.code()
                ),
            )),
            _ => Err(error("LS_EXITED")),
        }
    }
    pub fn verify(&mut self) -> Result<(), EngineError> {
        self.check_alive()?;
        let found = WindowsProcessPortInspector
            .listening_process_path(self.child.id(), self.endpoint.https_port)
            .map_err(|e| EngineError::from_wire(e, "无法核验自有引擎端口"))?;
        if found.and_then(|p| p.canonicalize().ok()).as_ref() != Some(&self.binary) {
            return Err(error("INSTANCE_IDENTITY_CHANGED"));
        }
        Ok(())
    }
    pub fn stop(&mut self) -> Result<(), EngineError> {
        if self.stopped {
            return Ok(());
        }
        drop(self.child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        while self
            .child
            .try_wait()
            .map_err(|_| error("LS_WAIT_FAILED"))?
            .is_none()
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(25));
        }
        // Always terminate the owned job, including surviving grandchildren after LS exits.
        self.job.terminate()?;
        let _ = self.child.wait().map_err(|_| error("LS_WAIT_FAILED"))?;
        self.job.wait_empty()?;
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
        self.stopped = true;
        Ok(())
    }
}
impl Drop for LsHost {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn parse_port(line: &[u8]) -> Option<u16> {
    let s = std::str::from_utf8(line).ok()?;
    let s = s.split("port at ").nth(1)?;
    let (port, suffix) = s.split_once(' ')?;
    if !suffix.starts_with("for HTTPS") {
        return None;
    }
    port.parse::<u16>().ok().filter(|p| *p > 0)
}

// Keep only bounded lines for port extraction; raw logs/credentials never leave the pipe.
fn drain(
    mut pipe: impl Read + Send + 'static,
    tx: mpsc::SyncSender<u16>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        let mut line = Vec::new();
        let mut overflow = false;
        while let Ok(n) = pipe.read(&mut buffer) {
            if n == 0 {
                break;
            }
            for &b in &buffer[..n] {
                if b == b'\n' {
                    if !overflow && let Some(port) = parse_port(&line) {
                        let _ = tx.try_send(port);
                    }
                    line.clear();
                    overflow = false;
                } else if line.len() < 8192 {
                    line.push(b);
                } else {
                    overflow = true;
                }
            }
        }
    })
}

#[cfg(windows)]
struct Job(windows_sys::Win32::Foundation::HANDLE);
// The handle is owned, never exposed, and accessed only through LsHost's mutex.
#[cfg(windows)]
unsafe impl Send for Job {}
#[cfg(windows)]
impl Job {
    fn resume(&self, child: &Child) -> Result<(), EngineError> {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
            System::{Diagnostics::ToolHelp::*, Threading::*},
        };
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return Err(error("THREAD_SNAPSHOT_FAILED"));
            }
            let mut entry: THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = std::mem::size_of_val(&entry) as u32;
            let mut found = false;
            let mut more = Thread32First(snapshot, &mut entry);
            while more != 0 {
                if entry.th32OwnerProcessID == child.id() {
                    let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                    if !thread.is_null() {
                        if ResumeThread(thread) != u32::MAX {
                            found = true;
                        }
                        CloseHandle(thread);
                    }
                }
                more = Thread32Next(snapshot, &mut entry);
            }
            CloseHandle(snapshot);
            if found {
                Ok(())
            } else {
                Err(error("LS_RESUME_FAILED"))
            }
        }
    }
    fn new() -> Result<Self, EngineError> {
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let job = Self(CreateJobObjectW(std::ptr::null(), std::ptr::null()));
            if job.0.is_null() {
                return Err(error("JOB_CREATE_FAILED"));
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of_val(&info) as u32,
            ) == 0
            {
                return Err(error("JOB_CONFIG_FAILED"));
            }
            Ok(job)
        }
    }
    fn assign(&self, child: &Child) -> Result<(), EngineError> {
        use std::os::windows::io::AsRawHandle;
        if unsafe {
            windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(
                self.0,
                child.as_raw_handle(),
            )
        } == 0
        {
            Err(error("JOB_ASSIGN_FAILED"))
        } else {
            Ok(())
        }
    }
    fn terminate(&self) -> Result<(), EngineError> {
        if unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 1) } == 0 {
            Err(error("JOB_STOP_FAILED"))
        } else {
            Ok(())
        }
    }
    fn wait_empty(&self) -> Result<(), EngineError> {
        use windows_sys::Win32::System::JobObjects::*;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe {
                QueryInformationJobObject(
                    self.0,
                    JobObjectBasicAccountingInformation,
                    &mut info as *mut _ as *mut _,
                    std::mem::size_of_val(&info) as u32,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(error("JOB_QUERY_FAILED"));
            }
            if info.ActiveProcesses == 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(error("JOB_STOP_TIMEOUT"));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}
#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_accepts_https_port_and_rejects_zero_or_bad_input() {
        assert_eq!(
            parse_port(b"Language server listening on random port at 12345 for HTTPS (gRPC)"),
            Some(12345)
        );
        for s in [
            "port at 0 for HTTPS",
            "port at 70000 for HTTPS",
            "port at 123 for HTTP",
            "port at secret for HTTPS",
        ] {
            assert_eq!(parse_port(s.as_bytes()), None);
        }
    }
    #[test]
    fn data_lock_is_exclusive_and_released_without_deleting_file() {
        let root = std::env::temp_dir().join(format!("agcn-lock-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let path = root.join("lock");
        let a = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        a.try_lock().unwrap();
        let b = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(b.try_lock().is_err());
        drop(a);
        b.try_lock().unwrap();
        drop(b);
        fs::remove_file(path).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn resolves_install_root_from_user_env_vars() {
        assert_eq!(
            resolve_install_root_from(
                Some(OsString::from("carol")),
                Some(OsString::from(r"C:\Users\alice\AppData\Local")),
                Some(OsString::from(r"D:\Users\bob")),
            )
            .unwrap(),
            PathBuf::from(r"C:\Users\carol\AppData\Local\Programs\antigravity")
        );
        assert_eq!(
            resolve_install_root_from(
                Some(OsString::from("")),
                Some(OsString::from(r"C:\Users\alice\AppData\Local")),
                Some(OsString::from(r"D:\Users\bob")),
            )
            .unwrap(),
            PathBuf::from(r"C:\Users\alice\AppData\Local\Programs\antigravity")
        );
        assert_eq!(
            resolve_install_root_from(None, None, Some(OsString::from(r"D:\Users\bob"))).unwrap(),
            PathBuf::from(r"D:\Users\bob\AppData\Local\Programs\antigravity")
        );
        assert_eq!(
            resolve_install_root_from(None, None, None)
                .unwrap_err()
                .reason_code,
            "MISSING_INSTALL_ROOT_ENV"
        );
    }

    #[test]
    fn missing_binary_and_unsupported_version_fail_before_spawn() {
        let root = std::env::temp_dir().join(format!("agcn-install-{}", Uuid::new_v4()));
        let mut cfg = LsHostConfig::for_data_root(root.join("data")).unwrap();
        cfg.install_root = root.join("install");
        assert_eq!(
            LsHost::start(&cfg).err().unwrap().reason_code,
            "LS_BINARY_MISSING"
        );
        let resources = cfg.install_root.join("resources");
        fs::create_dir_all(resources.join("bin")).unwrap();
        fs::write(resources.join("bin/language_server.exe"), b"not executable").unwrap();
        let package = br#"{"version":"0.0.0-unsupported"}"#;
        let header = serde_json::to_vec(
            &serde_json::json!({"files":{"package.json":{"offset":"0","size":package.len()}}}),
        )
        .unwrap();
        let mut asar = Vec::new();
        for value in [
            4u32,
            (8 + header.len()) as u32,
            (4 + header.len()) as u32,
            header.len() as u32,
        ] {
            asar.extend_from_slice(&value.to_le_bytes());
        }
        asar.extend_from_slice(&header);
        asar.extend_from_slice(package);
        fs::write(resources.join("app.asar"), asar).unwrap();
        assert_eq!(
            LsHost::start(&cfg).err().unwrap().reason_code,
            "UNSUPPORTED_LS_VERSION"
        );
        assert!(
            !cfg.data_root.exists(),
            "invalid install must not initialize engine data"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "uses installed official LS, no model task"]
    fn live_startup_timeout_reclaims_child_and_releases_data_lock() {
        let root = std::env::temp_dir().join(format!("agcn-timeout-{}", Uuid::new_v4()));
        let mut cfg = LsHostConfig::for_data_root(root).unwrap();
        cfg.startup_timeout = Duration::ZERO;
        assert_eq!(
            LsHost::start(&cfg).err().unwrap().reason_code,
            "LS_START_TIMEOUT"
        );
        cfg.startup_timeout = Duration::from_secs(40);
        let mut host = LsHost::start(&cfg).unwrap();
        host.verify().unwrap();
        host.stop().unwrap();
        assert!(host.check_alive().is_err());
    }
}
