//! Rebuild the actual launch user's environment; never inherit the service's or
//! an agent shell's overridden proxy. No username or proxy endpoint is embedded.
use std::{ffi::OsString, os::windows::ffi::OsStringExt};

pub fn current_user_environment() -> Result<Vec<(OsString, OsString)>, String> {
    use windows_sys::Win32::{Foundation::CloseHandle, Security::{TOKEN_DUPLICATE, TOKEN_QUERY},
        System::{Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock}, Threading::{GetCurrentProcess, OpenProcessToken}}};
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE, &mut token) == 0 {
            return Err(format!("OpenProcessToken: {}", std::io::Error::last_os_error()));
        }
        let mut block = std::ptr::null_mut();
        let ok = CreateEnvironmentBlock(&mut block, token, 0);
        CloseHandle(token);
        if ok == 0 { return Err(format!("CreateEnvironmentBlock: {}", std::io::Error::last_os_error())); }
        let mut values = Vec::new();
        let mut p = block.cast::<u16>();
        while *p != 0 {
            let mut len = 0;
            while *p.add(len) != 0 { len += 1; }
            let row = std::slice::from_raw_parts(p, len);
            if let Some(eq) = row.iter().position(|c| *c == b'=' as u16).filter(|i| *i > 0) {
                values.push((OsString::from_wide(&row[..eq]), OsString::from_wide(&row[eq+1..])));
            }
            p = p.add(len+1);
        }
        DestroyEnvironmentBlock(block);
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn user_environment_does_not_inherit_process_proxy_overrides() {
        let lookup = |rows: Vec<(OsString, OsString)>| rows.into_iter()
            .find(|(k,_)| k.to_string_lossy().eq_ignore_ascii_case("HTTPS_PROXY")).map(|(_,v)| v);
        let expected = lookup(current_user_environment().unwrap());
        let saved = std::env::var_os("HTTPS_PROXY");
        std::env::set_var("HTTPS_PROXY", "http://process-override.invalid:9");
        let actual = lookup(current_user_environment().unwrap());
        if let Some(saved) = saved { std::env::set_var("HTTPS_PROXY", saved); }
        else { std::env::remove_var("HTTPS_PROXY"); }
        assert_eq!(actual, expected);
        assert_ne!(actual, Some("http://process-override.invalid:9".into()));
    }
}
