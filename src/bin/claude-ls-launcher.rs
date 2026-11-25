//! Console-free user-mode entry point; waits and propagates the child exit code.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[path = "../ls_user_environment.rs"]
mod user_environment;

fn managed(path: std::path::PathBuf) -> Result<(), String> {
    use std::{fs, process::{Command, Stdio}, time::Duration};
    let request: serde_json::Value = serde_json::from_slice(&fs::read(&path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let field = |key: &str| request[key].as_str().ok_or_else(|| format!("Missing {key}"));
    let log = fs::OpenOptions::new().create(true).append(true)
        .open(std::path::Path::new(field("data_root")?).join("bridge-host.log")).map_err(|e| e.to_string())?;
    let environment = user_environment::current_user_environment()?;
    let proxy = environment.iter().find(|(k,_)| k.to_string_lossy().eq_ignore_ascii_case("HTTPS_PROXY"))
        .or_else(|| environment.iter().find(|(k,_)| k.to_string_lossy().eq_ignore_ascii_case("HTTP_PROXY")));
    if proxy.is_none_or(|(_,v)| v.is_empty()) {
        use std::io::Write;
        let mut log = log;
        let _ = writeln!(log,"LS launch refused: interactive user has no HTTP_PROXY/HTTPS_PROXY environment variable");
        return Err("Interactive user proxy environment is missing".into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?.with_file_name("claude-ls-host.exe");
    let mut cmd = Command::new(exe);
    cmd.env_clear().envs(environment).arg("--install-root").arg(field("install_root")?)
        .arg("--data-root").arg(field("data_root")?).arg("--token-file").arg(field("token_file")?)
        .arg("--port").arg(field("port")?).arg("--launch-id").arg(field("launch_id")?)
        .arg("--exit-on-stdin-close").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::from(log));
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000);
    let lease = std::path::PathBuf::from(format!("{}.lease",path.display()));
    let lease_alive = || fs::metadata(&lease).and_then(|m| m.modified()).ok()
        .and_then(|t| t.elapsed().ok()).is_some_and(|d| d < Duration::from_secs(10));
    if !lease_alive() { return Err("Bridge launch lease expired".into()); }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return if status.success() { Ok(()) } else { Err(format!("Host exited: {status}")) };
        }
        if !lease_alive() {
            drop(child.stdin.take());
            let _ = child.wait();
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(program) = args.next() else { std::process::exit(2) };
    if program == "--managed-request" {
        let result = args.next().ok_or_else(|| "Missing launch request".to_owned()).and_then(|p| managed(p.into()));
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    }
    let mut command = std::process::Command::new(program);
    command.args(args).stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let code = command.status().ok().and_then(|status| status.code()).unwrap_or(1);
    std::process::exit(code);
}
