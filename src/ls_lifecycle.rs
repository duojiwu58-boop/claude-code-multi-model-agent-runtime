/// The bridge owns its Host, which in turn owns the LS process tree.
#[derive(Default)]
struct LsLifecycle {
    child: Option<tokio::process::Child>,
    config: Option<LsProviderConfig>,
    request_path: Option<PathBuf>,
    launch_id: Option<String>,
}

impl LsLifecycle {
    async fn stop(&mut self) -> Result<(), String> {
        if self.child.as_mut().is_some_and(|child| matches!(child.try_wait(), Ok(None))) {
            if let Some(config) = &self.config {
                let _ = tokio::time::timeout(Duration::from_secs(12), ls_host_post(config, "/shutdown", &json!({}))).await;
            }
        }
        if let Some(child) = self.child.as_mut() {
            // Host watches EOF even when its HTTP listener has not started yet.
            drop(child.stdin.take());
            if tokio::time::timeout(Duration::from_secs(15), child.wait()).await.is_err() {
                child.kill().await.map_err(|e| format!("Cannot stop owned LS Host: {e}"))?;
            }
        }
        self.child = None;
        self.config = None;
        self.launch_id = None;
        if let Some(path) = self.request_path.take() {
            let _ = fs::remove_file(format!("{}.lease",path.display()));
            let _ = fs::remove_file(path);
        }
        Ok(())
    }

    async fn require_idle(&self) -> Result<(), String> {
        if let Some(config) = &self.config {
            let response = tokio::time::timeout(Duration::from_secs(5), ls_host_post(config, "/interactions", &json!({})))
                .await.map_err(|_| "Cannot confirm LS is idle")??;
            if !response.status().is_success() { return Err("Cannot confirm LS is idle".into()); }
            let value: Value = response.json().await.map_err(|_| "Invalid LS interactions response")?;
            if value["sessions"].as_array().ok_or("Missing LS sessions")?.iter()
                .any(|s| s["busy"] == true || s["uncertain"] == true) {
                return Err("LS has an active or uncertain task; cancel/resolve it before switching providers".into());
            }
        }
        Ok(())
    }

    async fn select(&mut self, config: Option<&LsProviderConfig>) -> Result<(), String> {
        let alive = self.child.as_mut().map(|c| c.try_wait()).transpose()
            .map_err(|e| format!("Cannot inspect LS Host: {e}"))?.flatten().is_none()
            && self.child.is_some();
        if alive && self.config.as_ref().map(|c| &c.launch) == config.map(|c| &c.launch)
            && self.config.as_ref().map(|c| (&c.host_url, &c.token_file)) == config.map(|c| (&c.host_url, &c.token_file)) {
            let config = config.unwrap();
            let response = tokio::time::timeout(Duration::from_secs(3), ls_host_post(config, "/health", &json!({})))
                .await.map_err(|_| "LS health check timed out")??;
            if response.status().is_success() {
                    let body: Value = response.json().await.map_err(|_| "Invalid LS health response")?;
                    if body.get("launch_id").and_then(Value::as_str) != self.launch_id.as_deref() {
                        return Err("LS Host launch identity mismatch".into());
                    }
                    return Ok(());
                }
            if response.status() != StatusCode::SERVICE_UNAVAILABLE {
                return Err(format!("LS health check failed: {}", response.status()));
            }
            // The owned Host confirms that its LS exited. Recover the process,
            // never any in-flight request or conversation turn.
            self.stop().await?;
        }
        if alive && self.child.is_some() { self.require_idle().await?; }
        self.stop().await?;
        let Some(config) = config else { return Ok(()) };
        let launch = config.launch.as_ref().ok_or("LS requires ls.launch with absolute install_root and data_root; login startup is no longer used")?;
        // Never attach to, adopt, or kill an externally started Host.
        let url = url::Url::parse(&config.host_url).map_err(|e| e.to_string())?;
        let port = url.port().ok_or("Missing LS Host port")?;
        let probe = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await
            .map_err(|_| "LS Host port is already occupied; stop the old standalone Host first")?;
        drop(probe);
        let install = std::env::current_exe().map_err(|e| e.to_string())?
            .parent().ok_or("Missing installation directory")?.to_owned();
        let control = PathBuf::from(std::env::var_os("PROGRAMDATA").ok_or("Missing PROGRAMDATA")?)
            .join("ClaudeCodeBridge/ls-control");
        let launch_id = Uuid::new_v4().to_string();
        let path = control.join(format!("{launch_id}.json"));
        let request = json!({"launch_id":launch_id,"install_root":launch.install_root,
            "data_root":launch.data_root,"token_file":config.token_file,"port":port.to_string()});
        fs::write(&path, serde_json::to_vec(&request).unwrap()).map_err(|e| format!("Cannot write LS launch request (install the interactive task): {e}"))?;
        self.request_path = Some(path.clone());
        self.launch_id = Some(launch_id);
        let pwsh = PathBuf::from(std::env::var_os("ProgramFiles").ok_or("Missing ProgramFiles")?)
            .join("PowerShell/7/pwsh.exe");
        let mut cmd = tokio::process::Command::new(pwsh);
        let log = fs::OpenOptions::new().create(true).append(true)
            .open(control.join("controller.log")).map_err(|e| format!("Cannot open LS controller log: {e}"))?;
        cmd.args(["-NoProfile", "-NonInteractive", "-File"])
            .arg(install.join("scripts/invoke-ls-session.ps1")).arg("-RequestFile").arg(path)
            .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(log)).kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        self.child = Some(cmd.spawn().map_err(|e| format!("Cannot start interactive LS controller: {e}"))?);
        self.config = Some(config.clone());
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().map_err(|e| e.to_string())? {
                self.stop().await?;
                return Err(format!("Owned LS Host exited during startup: {status}"));
            }
            if let Ok(Ok(response)) = tokio::time::timeout(Duration::from_secs(2), ls_host_post(config, "/health", &json!({}))).await {
                if response.status().is_success() {
                    let body: Value = response.json().await.map_err(|_| "Invalid LS health response")?;
                    if body.get("launch_id").and_then(Value::as_str) != self.launch_id.as_deref() {
                        return Err("LS Host launch identity mismatch".into());
                    }
                    return Ok(());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                self.stop().await?;
                return Err("Owned LS Host startup timed out".into());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

async fn maintain_ls(state: Arc<AppState>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() { break; }
        // Serialize with route persistence; never restore a stale selection.
        if let Ok(_transition) = state.admin_state_lock.try_lock() {
            if let Ok(mut host) = state.ls_lifecycle.try_write() {
                let profile = active_provider_profile(&state);
                let desired = profile.as_ref().and_then(|p| p.ls.as_ref());
                if let Err(e) = host.select(desired).await { tracing::warn!("LS lifecycle: {e}"); }
            }
        }
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
    }
}
