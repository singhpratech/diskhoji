//! WinDirStat-style elevation: Diskhoji can relaunch itself with
//! administrator rights so every folder on the disk becomes readable.
//!
//! Nothing here touches the network — elevation is the local OS prompt
//! (polkit on Linux, the macOS authorization dialog, UAC on Windows).

#[cfg(unix)]
pub fn is_elevated() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(windows)]
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut ret = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elev as *mut TOKEN_ELEVATION as *mut std::ffi::c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        );
        CloseHandle(token);
        ok != 0 && elev.TokenIsElevated != 0
    }
}

/// The freshly elevated instance calls this with the old instance's pid so
/// exactly one window survives the hand-off.
#[cfg(unix)]
pub fn takeover(pid: u32) {
    if pid > 1 {
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }
}

/// On Windows the pre-elevation instance exits itself right after UAC
/// consent (ShellExecuteW is synchronous through the prompt), so there is
/// nothing to kill.
#[cfg(windows)]
pub fn takeover(_pid: u32) {}

/// Relaunch through polkit. The returned child is pkexec itself: it exits
/// quickly (126/127) when authorization is dismissed, and otherwise lives
/// as long as the elevated instance — which SIGTERMs us via --takeover once
/// it is up, so on success this process normally dies before the child does.
#[cfg(target_os = "linux")]
pub fn relaunch_elevated() -> Result<std::process::Child, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new("pkexec");
    cmd.arg("env");
    // pkexec scrubs the environment; hand back just the display bits.
    // X11 (or XWayland) is the battle-tested path for root GUI clients, so
    // Wayland vars are only passed when there is no X display at all.
    let mut have_x = false;
    for k in ["DISPLAY", "XAUTHORITY"] {
        if let Ok(v) = std::env::var(k) {
            have_x |= k == "DISPLAY";
            cmd.arg(format!("{k}={v}"));
        }
    }
    if !have_x {
        for k in ["WAYLAND_DISPLAY", "XDG_RUNTIME_DIR"] {
            if let Ok(v) = std::env::var(k) {
                cmd.arg(format!("{k}={v}"));
            }
        }
    }
    cmd.arg(exe).arg("--takeover").arg(std::process::id().to_string());
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn()
        .map_err(|_| "pkexec not found — install polkit to elevate".to_string())
}

/// AppleScript shows the standard macOS authorization dialog; the inner
/// shell command backgrounds the app so osascript exits right after auth
/// (0 = authorized and launched, non-zero = the user cancelled).
#[cfg(target_os = "macos")]
pub fn relaunch_elevated() -> Result<std::process::Child, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let quoted = format!("'{}'", exe.to_string_lossy().replace('\'', r"'\''"));
    let script = format!(
        "do shell script \"{} --takeover {} >/dev/null 2>&1 &\" with administrator privileges",
        quoted.replace('\\', "\\\\").replace('"', "\\\""),
        std::process::id()
    );
    std::process::Command::new("osascript")
        .args(["-e", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())
}

/// Blocks while the UAC prompt is up, then returns; Ok means the elevated
/// instance was created and the caller should exit this one.
#[cfg(windows)]
pub fn relaunch_elevated() -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let file: Vec<u16> = exe.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let verb: Vec<u16> = "runas".encode_utf16().chain(std::iter::once(0)).collect();
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if r as isize > 32 {
        Ok(())
    } else {
        Err("Windows declined the elevation request".to_string())
    }
}
