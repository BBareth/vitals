//! CPU load, topology and clock speed. All of this is available to a normal
//! user process -- no driver, no elevation.

use std::ffi::c_void;

use super::pdh::Pdh;

/// SystemProcessorPerformanceInformation
const SPPI_CLASS: u32 = 8;

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(class: u32, buf: *mut c_void, len: u32, ret_len: *mut u32) -> i32;
}

/// Mirrors SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION (48 bytes on x64).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Sppi {
    idle: i64,
    kernel: i64, // includes idle time
    user: i64,
    dpc: i64,
    interrupt: i64,
    interrupt_count: u32,
    _pad: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CoreKind {
    #[default]
    Performance,
    Efficiency,
}

#[derive(Clone, Debug, Default)]
pub struct CpuStats {
    pub total: f32,
    /// One entry per logical processor, in OS order.
    pub per_thread: Vec<f32>,
    /// Same length as `per_thread`; which physical core kind each thread sits on.
    pub thread_kind: Vec<CoreKind>,
    /// Current effective clock in MHz, derived from % Processor Performance.
    pub clock_mhz: f32,
    /// Filled in by the optional HWiNFO / LibreHardwareMonitor bridge.
    pub temp_c: Option<f32>,
}

pub struct CpuReader {
    prev: Vec<Sppi>,
    kinds: Vec<CoreKind>,
    base_mhz: f32,
    /// PDH query plus the index of the "% Processor Performance" counter.
    perf: Option<(Pdh, usize)>,
}

impl CpuReader {
    pub fn new() -> Self {
        let logical = num_logical();
        let (kinds, _cores, _hybrid) = topology(logical);
        let mut r =
            Self { prev: vec![Sppi::default(); logical], kinds, base_mhz: base_mhz(), perf: open_perf_counter() };
        // Prime the delta baseline so the first real sample is meaningful.
        r.prev = r.sample_raw();
        r
    }

    fn sample_raw(&self) -> Vec<Sppi> {
        let n = self.prev.len();
        let mut buf = vec![Sppi::default(); n];
        let bytes = (n * std::mem::size_of::<Sppi>()) as u32;
        let mut ret = 0u32;
        let status = unsafe { NtQuerySystemInformation(SPPI_CLASS, buf.as_mut_ptr() as *mut c_void, bytes, &mut ret) };
        if status < 0 {
            return vec![Sppi::default(); n];
        }
        buf
    }

    pub fn read(&mut self) -> CpuStats {
        let now = self.sample_raw();
        let mut per_thread = Vec::with_capacity(now.len());
        let (mut busy_sum, mut total_sum) = (0i64, 0i64);

        for (i, cur) in now.iter().enumerate() {
            let p = &self.prev[i];
            let d_idle = cur.idle - p.idle;
            let d_kernel = cur.kernel - p.kernel;
            let d_user = cur.user - p.user;
            let total = d_kernel + d_user;
            let busy = total - d_idle;
            let pct = if total > 0 { (busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0) as f32 } else { 0.0 };
            per_thread.push(pct);
            busy_sum += busy.max(0);
            total_sum += total.max(0);
        }
        self.prev = now;

        let total =
            if total_sum > 0 { (busy_sum as f64 / total_sum as f64 * 100.0).clamp(0.0, 100.0) as f32 } else { 0.0 };

        let clock_mhz = self
            .perf
            .as_mut()
            .and_then(|(pdh, idx)| {
                pdh.collect();
                pdh.value(*idx)
            })
            .map(|pct| self.base_mhz * pct as f32 / 100.0)
            .unwrap_or(self.base_mhz);

        CpuStats { total, thread_kind: self.kinds.clone(), per_thread, clock_mhz, temp_c: None }
    }
}

fn num_logical() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// Walks GetLogicalProcessorInformationEx(RelationProcessorCore) by raw bytes.
/// The variable-length GroupMask array makes the generated union types awkward,
/// and the layout is stable and documented, so we parse offsets directly.
fn topology(logical: usize) -> (Vec<CoreKind>, usize, bool) {
    use windows::Win32::System::SystemInformation::{GetLogicalProcessorInformationEx, RelationProcessorCore};

    let mut kinds = vec![CoreKind::Performance; logical];
    let mut len: u32 = 0;
    unsafe {
        let _ = GetLogicalProcessorInformationEx(RelationProcessorCore, None, &mut len);
    }
    if len == 0 {
        return (kinds, logical, false);
    }
    let mut buf = vec![0u8; len as usize];
    let ok =
        unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, Some(buf.as_mut_ptr() as *mut _), &mut len) };
    if ok.is_err() {
        return (kinds, logical, false);
    }

    let mut off = 0usize;
    let mut cores = 0usize;
    let mut classes: Vec<u8> = Vec::new();
    let mut assign: Vec<(usize, u8)> = Vec::new();

    while off + 32 <= buf.len() {
        let size = u32::from_le_bytes(buf[off + 4..off + 8].try_into().unwrap()) as usize;
        if size == 0 || off + size > buf.len() {
            break;
        }
        // PROCESSOR_RELATIONSHIP: Flags @8, EfficiencyClass @9, GroupCount @30,
        // GROUP_AFFINITY GroupMask[] @32 (16 bytes each: KAFFINITY mask, WORD group).
        let eff = buf[off + 9];
        let group_count = u16::from_le_bytes(buf[off + 30..off + 32].try_into().unwrap()) as usize;
        cores += 1;
        classes.push(eff);
        for g in 0..group_count {
            let m = off + 32 + g * 16;
            if m + 8 > buf.len() {
                break;
            }
            let mask = u64::from_le_bytes(buf[m..m + 8].try_into().unwrap());
            for bit in 0..64usize {
                // Single-group machines only; a second processor group would
                // need the group number folded in, which no consumer CPU reaches.
                if mask & (1u64 << bit) != 0 && bit < logical {
                    assign.push((bit, eff));
                }
            }
        }
        off += size;
    }

    let min_class = classes.iter().copied().min().unwrap_or(0);
    let max_class = classes.iter().copied().max().unwrap_or(0);
    let hybrid = max_class > min_class;

    for (idx, eff) in assign {
        // Higher EfficiencyClass means higher performance, per the Win32 docs.
        kinds[idx] = if hybrid && eff == min_class { CoreKind::Efficiency } else { CoreKind::Performance };
    }

    (kinds, cores.max(1), hybrid)
}

fn base_mhz() -> f32 {
    read_reg_dword("HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0", "~MHz").unwrap_or(0) as f32
}

fn read_reg_dword(subkey: &str, value: &str) -> Option<u32> {
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};
    let sub: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
    let val: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let mut out: u32 = 0;
    let mut size: u32 = 4;
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            windows::core::PCWSTR(sub.as_ptr()),
            windows::core::PCWSTR(val.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut out as *mut u32 as *mut c_void),
            Some(&mut size),
        )
    };
    if r.is_err() { None } else { Some(out) }
}

/// "% Processor Performance" is the ratio of actual to nominal clock, so it
/// reflects real turbo and downclock behaviour, where Win32_Processor just
/// parrots the nominal frequency back.
fn open_perf_counter() -> Option<(Pdh, usize)> {
    let mut pdh = Pdh::new()?;
    let idx = pdh.add("\\Processor Information(_Total)\\% Processor Performance")?;
    // The first collection only establishes a baseline for the rate counter.
    pdh.collect();
    Some((pdh, idx))
}
