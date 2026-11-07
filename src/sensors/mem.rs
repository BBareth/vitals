//! Physical memory and commit charge.

use windows::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

#[derive(Clone, Debug, Default)]
pub struct MemStats {
    pub used_bytes: u64,
    pub total_bytes: u64,
    pub percent: f32,
    pub commit_bytes: u64,
    pub commit_limit_bytes: u64,
}

pub fn read() -> MemStats {
    let mut ms = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    let mut out = MemStats::default();
    if unsafe { GlobalMemoryStatusEx(&mut ms) }.is_ok() {
        out.total_bytes = ms.ullTotalPhys;
        out.used_bytes = ms.ullTotalPhys.saturating_sub(ms.ullAvailPhys);
        out.percent = ms.dwMemoryLoad as f32;
    }

    let mut pi =
        PERFORMANCE_INFORMATION { cb: std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32, ..Default::default() };
    if unsafe { GetPerformanceInfo(&mut pi, pi.cb) }.is_ok() {
        let page = pi.PageSize as u64;
        out.commit_bytes = pi.CommitTotal as u64 * page;
        out.commit_limit_bytes = pi.CommitLimit as u64 * page;
    }
    out
}
