//! NVIDIA GPU telemetry via NVML (nvml.dll ships with the driver).
//!
//! NVML is the reason this whole app needs no kernel driver: it exposes
//! temperature, power and clocks to an unprivileged process, which is exactly
//! the data that requires MSR access on the CPU side.
//!
//! NVML is also the one thing that has taken the overlay down. When the NVIDIA
//! driver is installed or updated, PnP stops nvlddmkm and swaps the DriverStore
//! folder while the old nvml.dll is still mapped with live device state. The
//! next call (nvmlDeviceGetUtilizationRates in every captured dump) then reads
//! freed memory inside nvml.dll and faults with c0000005 instead of returning
//! an error. A fault inside a foreign DLL cannot be recovered in process, so
//! NVML never loads into the overlay at all: `vitals.exe --gpu-helper` owns it
//! and streams one text line per sample over a pipe. If the helper dies, hangs
//! or loses the device, the supervisor here shows the GPU as restarting and
//! starts a fresh helper, which loads whatever driver is installed by then.

use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::diag;

#[derive(Clone, Debug, Default)]
pub struct GpuStats {
    pub util_percent: f32,
    pub temp_c: Option<f32>,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    pub mem_percent: f32,
    pub power_w: Option<f32>,
    pub power_limit_w: Option<f32>,
    pub core_clock_mhz: Option<u32>,
    pub fan_percent: Option<f32>,
}

/// Command-line switch that turns vitals.exe into the NVML helper.
pub const HELPER_ARG: &str = "--gpu-helper";

/// Helper exit codes, so the supervisor can tell a missing GPU from a crash.
const EXIT_INIT_FAILED: i32 = 2;
const EXIT_DEVICE_LOST: i32 = 3;

/// Consecutive failed device reads before the helper gives up and exits, so the
/// next one re-initialises NVML from scratch.
const MAX_DEVICE_FAILURES: u32 = 3;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ---------------------------------------------------------------------------
// Overlay side
// ---------------------------------------------------------------------------

struct Shared {
    stats: Option<GpuStats>,
    /// When the last sample arrived.
    sample_at: Option<Instant>,
    /// Last sign of life from the current helper (spawn or any line).
    alive_at: Instant,
    /// Why there are no stats, shown in the panel.
    status: Option<String>,
    child: Option<Child>,
    /// Earliest time the supervisor may start the next helper.
    next_spawn: Instant,
    backoff: Duration,
    interval_ms: u64,
    /// Bumped by `restart`. A helper started under an older generation is
    /// killed, and its exit is logged as requested rather than as a failure.
    restart_gen: u64,
}

pub struct GpuReader {
    shared: Arc<Mutex<Shared>>,
}

impl GpuReader {
    pub fn new(interval_ms: u64) -> Self {
        let now = Instant::now();
        let shared = Arc::new(Mutex::new(Shared {
            stats: None,
            sample_at: None,
            alive_at: now,
            status: Some("starting".into()),
            child: None,
            next_spawn: now,
            backoff: Duration::from_secs(1),
            interval_ms: interval_ms.max(200),
            restart_gen: 0,
        }));
        let job = KillOnCloseJob::new();
        let sup = Arc::clone(&shared);
        let spawned = std::thread::Builder::new().name("vitals-gpu".into()).spawn(move || supervise(sup, job));
        if let Err(e) = spawned
            && let Ok(mut g) = shared.lock()
        {
            g.status = Some(format!("no GPU thread: {e}"));
        }

        // Wait briefly for the first sample, so the first frame shows real GPU
        // numbers instead of a "starting" placeholder.
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline {
            let ready =
                shared.lock().map(|g| g.sample_at.is_some() || g.status.as_deref() != Some("starting")).unwrap_or(true);
            if ready {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        Self { shared }
    }

    pub fn read(&self) -> Option<GpuStats> {
        let mut g = self.shared.lock().ok()?;
        let interval = Duration::from_millis(g.interval_ms);
        let stale_after = (interval * 3).max(Duration::from_secs(3));
        let hung_after = (interval * 5).max(Duration::from_secs(10));

        // An NVML call that blocks forever (seen when the driver is half
        // installed) would leave the panel frozen; kill the helper instead.
        if g.child.is_some() && g.alive_at.elapsed() > hung_after {
            diag::log(&format!("GPU helper silent for {}s; killing it", g.alive_at.elapsed().as_secs()));
            if let Some(c) = g.child.as_mut() {
                let _ = c.kill();
            }
            g.alive_at = Instant::now();
        }

        match g.sample_at {
            Some(t) if t.elapsed() <= stale_after => g.stats.clone(),
            _ => None,
        }
    }

    /// Why the GPU panel is empty, when it is.
    pub fn status(&self) -> Option<String> {
        let g = self.shared.lock().ok()?;
        if g.sample_at.is_some_and(|t| t.elapsed() <= Duration::from_secs(3)) {
            return None;
        }
        Some(g.status.clone().unwrap_or_else(|| "restarting".into()))
    }

    /// Called when a display adapter appears or disappears, which is what a
    /// driver install looks like from user mode. The running helper still has
    /// the old nvml.dll mapped, so it is stopped before its next poll and a new
    /// one starts once the new driver has had a moment to come up.
    pub fn restart(&self, reason: &str) {
        if let Ok(mut g) = self.shared.lock() {
            diag::log(&format!("GPU helper restart: {reason}"));
            g.restart_gen += 1;
            if let Some(c) = g.child.as_mut() {
                let _ = c.kill();
            }
            g.stats = None;
            g.sample_at = None;
            g.status = Some("restarting".into());
            g.backoff = Duration::from_secs(1);
            g.next_spawn = Instant::now() + Duration::from_secs(3);
        }
    }

    pub fn set_interval(&self, interval_ms: u64) {
        let changed = self
            .shared
            .lock()
            .map(|mut g| {
                let new = interval_ms.max(200);
                let changed = g.interval_ms != new;
                g.interval_ms = new;
                changed
            })
            .unwrap_or(false);
        if changed {
            self.restart("update interval changed");
            if let Ok(mut g) = self.shared.lock() {
                g.next_spawn = Instant::now();
            }
        }
    }
}

impl Drop for GpuReader {
    fn drop(&mut self) {
        if let Ok(mut g) = self.shared.lock()
            && let Some(c) = g.child.as_mut()
        {
            let _ = c.kill();
        }
    }
}

fn supervise(shared: Arc<Mutex<Shared>>, job: KillOnCloseJob) {
    let exe = std::env::current_exe().ok();
    let mut last_logged: Option<String> = None;

    loop {
        // Wait until the next helper is due. `restart` can move this later,
        // so it is re-read each step.
        let (interval, spawn_gen) = loop {
            let Ok(g) = shared.lock() else { return };
            let now = Instant::now();
            if now >= g.next_spawn {
                break (g.interval_ms, g.restart_gen);
            }
            let wait = (g.next_spawn - now).min(Duration::from_millis(250));
            drop(g);
            std::thread::sleep(wait);
        };

        let Some(exe) = exe.as_ref() else {
            set_status(&shared, "cannot locate vitals.exe");
            return;
        };

        let spawned = Command::new(exe)
            .arg(HELPER_ARG)
            .arg(interval.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("cannot start GPU helper: {e}");
                if last_logged.as_deref() != Some(msg.as_str()) {
                    diag::log(&msg);
                    last_logged = Some(msg.clone());
                }
                if let Ok(mut g) = shared.lock() {
                    g.status = Some("helper failed".into());
                    g.next_spawn = Instant::now() + g.backoff;
                    g.backoff = (g.backoff * 2).min(Duration::from_secs(60));
                }
                continue;
            }
        };
        job.assign(&child);
        let pid = child.id();
        let stdout = child.stdout.take();
        let started = Instant::now();
        if let Ok(mut g) = shared.lock() {
            g.alive_at = started;
            // A restart that landed while this one was starting.
            if g.restart_gen != spawn_gen {
                let _ = child.kill();
            }
            g.child = Some(child);
        }

        // The last error line the helper printed explains a non-crash exit.
        let mut last_error: Option<String> = None;
        if let Some(out) = stdout {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                let Ok(mut g) = shared.lock() else { break };
                g.alive_at = Instant::now();
                if let Some(rest) = line.strip_prefix("G ") {
                    if let Some(s) = parse_sample(rest) {
                        g.stats = Some(s);
                        g.sample_at = Some(Instant::now());
                        g.status = None;
                    }
                } else if let Some(msg) = line.strip_prefix("E ") {
                    g.status = Some(msg.to_string());
                    last_error = Some(msg.to_string());
                }
            }
        }

        // Pipe closed: the helper exited, crashed or was killed.
        let child = shared.lock().ok().and_then(|mut g| g.child.take());
        let code = child.and_then(|mut c| c.wait().ok()).and_then(|s| s.code());
        let ran = started.elapsed();

        let Ok(mut g) = shared.lock() else { return };
        let expected = g.restart_gen != spawn_gen;
        if ran > Duration::from_secs(60) {
            g.backoff = Duration::from_secs(1);
        }
        g.stats = None;
        g.sample_at = None;
        g.status = Some(match code {
            Some(EXIT_INIT_FAILED) => last_error.clone().unwrap_or_else(|| "NVML unavailable".into()),
            _ => "restarting".into(),
        });
        // `restart` already set the time the new driver gets to come up.
        if !expected {
            g.next_spawn = Instant::now() + g.backoff;
        }
        let wait = g.next_spawn.saturating_duration_since(Instant::now());
        g.backoff = (g.backoff * 2).min(Duration::from_secs(60));
        drop(g);

        let what = match code {
            Some(EXIT_INIT_FAILED) => {
                format!("could not start NVML ({})", last_error.as_deref().unwrap_or("no reason given"))
            }
            Some(EXIT_DEVICE_LOST) => format!("lost the GPU ({})", last_error.as_deref().unwrap_or("no reason given")),
            Some(c) => format!("exited with {}", describe_code(c)),
            None => "exited".into(),
        };
        let msg = format!(
            "GPU helper pid {pid} {what} after {}{}; next start in {}s",
            fmt_duration(ran),
            if expected { " (requested)" } else { "" },
            (wait.as_millis() as u64).div_ceil(1000)
        );
        // A PC without an NVIDIA GPU retries every minute; log that once, not
        // 1440 times a day.
        let key = format!("{what}{expected}");
        if code != Some(EXIT_INIT_FAILED) || last_logged.as_deref() != Some(key.as_str()) {
            diag::log(&msg);
        }
        last_logged = Some(key);
    }
}

fn set_status(shared: &Arc<Mutex<Shared>>, s: &str) {
    if let Ok(mut g) = shared.lock() {
        g.status = Some(s.into());
    }
}

fn describe_code(code: i32) -> String {
    let u = code as u32;
    let name = match u {
        0xC000_0005 => " access violation",
        0xC000_0409 => " stack buffer overrun",
        0xC000_00FD => " stack overflow",
        0xC000_001D => " illegal instruction",
        0xC000_0374 => " heap corruption",
        0xC000_0135 => " DLL not found",
        0xFFFF_FFFF | 1 => " killed",
        _ => "",
    };
    if u >= 0x8000_0000 { format!("0x{u:08X}{name}") } else { format!("{code}{name}") }
}

fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 86_400 {
        format!("{}d{}h", s / 86_400, s % 86_400 / 3600)
    } else if s >= 3600 {
        format!("{}h{}m", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// Sample line: `util temp used total power limit core fan`, with `-` for a
/// value the GPU does not report.
fn parse_sample(rest: &str) -> Option<GpuStats> {
    let f: Vec<&str> = rest.split(' ').collect();
    if f.len() != 8 {
        return None;
    }
    fn opt<T: std::str::FromStr>(s: &str) -> Option<T> {
        if s == "-" { None } else { s.parse().ok() }
    }
    let mut g = GpuStats {
        util_percent: f[0].parse().ok()?,
        temp_c: opt(f[1]),
        mem_used_bytes: f[2].parse().ok()?,
        mem_total_bytes: f[3].parse().ok()?,
        mem_percent: 0.0,
        power_w: opt(f[4]),
        power_limit_w: opt(f[5]),
        core_clock_mhz: opt(f[6]),
        fan_percent: opt(f[7]),
    };
    if g.mem_total_bytes > 0 {
        g.mem_percent = (g.mem_used_bytes as f64 / g.mem_total_bytes as f64 * 100.0) as f32;
    }
    Some(g)
}

/// A job object that kills the helper when the overlay exits for any reason,
/// including its own crash, so no orphan keeps nvml.dll loaded.
struct KillOnCloseJob(Option<isize>);

impl KillOnCloseJob {
    fn new() -> Self {
        use windows::Win32::System::JobObjects::*;
        unsafe {
            let Ok(job) = CreateJobObjectW(None, windows::core::PCWSTR::null()) else {
                return Self(None);
            };
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok.is_err() {
                let _ = windows::Win32::Foundation::CloseHandle(job);
                return Self(None);
            }
            // Deliberately never closed: the handle dies with the process,
            // which is what triggers the kill.
            Self(Some(job.0 as isize))
        }
    }

    fn assign(&self, child: &Child) {
        let Some(job) = self.0 else { return };
        let job = windows::Win32::Foundation::HANDLE(job as *mut core::ffi::c_void);
        let proc = windows::Win32::Foundation::HANDLE(child.as_raw_handle());
        unsafe {
            let _ = windows::Win32::System::JobObjects::AssignProcessToJobObject(job, proc);
        }
    }
}

// ---------------------------------------------------------------------------
// Helper side: the only code in vitals that touches NVML
// ---------------------------------------------------------------------------

/// Entry point for `vitals.exe --gpu-helper <interval_ms>`. Never returns.
pub fn helper_main(args: &[String]) -> ! {
    use nvml_wrapper::Nvml;
    use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
    use nvml_wrapper::error::NvmlError;

    // A crash here is expected to be rare but survivable: keep the Event Log
    // entry and any LocalDumps dump as evidence, but never show a dialog.
    unsafe {
        let _ = windows::Win32::System::ErrorReporting::WerSetFlags(
            windows::Win32::System::ErrorReporting::WER_FAULT_REPORTING(
                windows::Win32::System::ErrorReporting::WER_FAULT_REPORTING_NO_UI,
            ),
        );
    }

    let interval = args.first().and_then(|s| s.parse::<u64>().ok()).unwrap_or(1000).max(200);
    let interval = Duration::from_millis(interval);

    let mut out = std::io::stdout().lock();
    // A failed write means the overlay is gone, so the helper should be too.
    macro_rules! emit {
        ($($t:tt)*) => {
            if writeln!(out, $($t)*).and_then(|_| out.flush()).is_err() {
                std::process::exit(0);
            }
        };
    }

    fn short(e: &NvmlError) -> String {
        match e {
            NvmlError::LibloadingError(_) | NvmlError::LibraryNotFound => "NVML not found".into(),
            NvmlError::DriverNotLoaded => "driver not loaded".into(),
            NvmlError::NoPermission => "no permission".into(),
            NvmlError::GpuLost => "GPU lost".into(),
            NvmlError::NotFound | NvmlError::InvalidArg => "no NVIDIA GPU".into(),
            other => other.to_string(),
        }
    }

    let nvml = match Nvml::init() {
        Ok(n) => n,
        Err(e) => {
            emit!("E {}", short(&e));
            std::process::exit(EXIT_INIT_FAILED);
        }
    };

    #[cfg(feature = "crash-test")]
    let crash_after: Option<u32> =
        Some(std::env::var("VITALS_GPU_CRASH_AFTER").ok().and_then(|s| s.parse().ok()).unwrap_or(5));
    #[cfg(feature = "crash-test")]
    let mut samples: u32 = 0;

    let mut failures = 0u32;
    let mut ticks = 0u64;
    loop {
        let t0 = Instant::now();

        // NVML touches far more memory while initialising than it needs per
        // sample; give it back once, then occasionally, as the panel does.
        ticks += 1;
        if ticks == 3 || ticks.is_multiple_of(600) {
            unsafe {
                let _ = windows::Win32::System::ProcessStatus::EmptyWorkingSet(
                    windows::Win32::System::Threading::GetCurrentProcess(),
                );
            }
        }

        #[cfg(feature = "crash-test")]
        {
            samples += 1;
            if crash_after.is_some_and(|n| samples > n) {
                // A genuine access violation, the same kind nvml.dll raises.
                unsafe { std::ptr::read_volatile(0x10usize as *const u64) };
            }
        }

        match nvml.device_by_index(0) {
            Ok(dev) => match dev.utilization_rates() {
                Ok(u) => {
                    failures = 0;
                    let temp = dev.temperature(TemperatureSensor::Gpu).ok();
                    let (used, total) = dev.memory_info().map(|m| (m.used, m.total)).unwrap_or((0, 0));
                    let power = dev.power_usage().ok().map(|mw| mw as f32 / 1000.0);
                    // enforced_power_limit reflects any active cap; fall back to
                    // the management limit when the driver does not report one.
                    let limit = dev
                        .enforced_power_limit()
                        .or_else(|_| dev.power_management_limit())
                        .ok()
                        .map(|mw| mw as f32 / 1000.0);
                    let core = dev.clock_info(Clock::Graphics).ok();
                    let fan = dev.fan_speed(0).ok();
                    emit!(
                        "G {} {} {} {} {} {} {} {}",
                        u.gpu,
                        o(temp),
                        used,
                        total,
                        o(power.map(|w| format!("{w:.1}"))),
                        o(limit.map(|w| format!("{w:.1}"))),
                        o(core),
                        o(fan)
                    );
                }
                Err(e) => {
                    failures += 1;
                    emit!("E {}", short(&e));
                }
            },
            Err(e) => {
                failures += 1;
                emit!("E {}", short(&e));
            }
        }
        if failures >= MAX_DEVICE_FAILURES {
            // A stale handle after a GPU reset stays stale; a new process
            // re-initialises NVML against whatever driver is loaded now.
            std::process::exit(EXIT_DEVICE_LOST);
        }

        std::thread::sleep(interval.saturating_sub(t0.elapsed()));
    }
}

fn o<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_sample() {
        let g = parse_sample("40 49 2684354560 17094934528 69.5 300.0 2865 30").unwrap();
        assert_eq!(g.util_percent, 40.0);
        assert_eq!(g.temp_c, Some(49.0));
        assert_eq!(g.mem_used_bytes, 2_684_354_560);
        assert!((g.mem_percent - 15.7).abs() < 0.1);
        assert_eq!(g.power_w, Some(69.5));
        assert_eq!(g.power_limit_w, Some(300.0));
        assert_eq!(g.core_clock_mhz, Some(2865));
        assert_eq!(g.fan_percent, Some(30.0));
    }

    #[test]
    fn dashes_are_missing_values() {
        let g = parse_sample("5 - 100 200 - - - -").unwrap();
        assert_eq!(g.temp_c, None);
        assert_eq!(g.power_w, None);
        assert_eq!(g.core_clock_mhz, None);
        assert_eq!(g.fan_percent, None);
        assert_eq!(g.mem_percent, 50.0);
    }

    #[test]
    fn rejects_malformed_samples() {
        assert!(parse_sample("").is_none());
        assert!(parse_sample("1 2 3").is_none());
        assert!(parse_sample("x 49 1 2 - - - -").is_none());
        // The old 11-field format from before the unused readings were dropped.
        assert!(parse_sample("40 49 1 2 - - - - - - -").is_none());
    }

    #[test]
    fn names_crash_codes() {
        assert_eq!(describe_code(0xC000_0005u32 as i32), "0xC0000005 access violation");
        assert_eq!(describe_code(-1), "0xFFFFFFFF killed");
        assert_eq!(describe_code(EXIT_DEVICE_LOST), "3");
    }
}
