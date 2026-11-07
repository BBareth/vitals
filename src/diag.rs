//! A small append-only log at `%APPDATA%\Vitals\vitals.log`.
//!
//! Release builds have no console, so this is the only record of the GPU
//! helper being restarted. It is capped so it can never grow without bound.

use std::io::Write;

const MAX_BYTES: u64 = 64 * 1024;

pub fn path() -> std::path::PathBuf {
    crate::config::config_dir().join("vitals.log")
}

pub fn log(msg: &str) {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {msg}\r\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    );
    #[cfg(debug_assertions)]
    eprint!("[vitals] {line}");

    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Keep the newer half once the cap is reached.
    if std::fs::metadata(&p).map(|m| m.len() > MAX_BYTES).unwrap_or(false)
        && let Ok(old) = std::fs::read(&p)
    {
        let cut = old.len() / 2;
        let start = old[cut..].iter().position(|&b| b == b'\n').map(|i| cut + i + 1).unwrap_or(cut);
        let _ = std::fs::write(&p, &old[start..]);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = f.write_all(line.as_bytes());
    }
}
