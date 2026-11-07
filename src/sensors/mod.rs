//! Sensor aggregation.
//!
//! Everything here works from an unprivileged process. The one exception is
//! CPU package temperature, which needs MSR access and therefore arrives via
//! the optional `bridge` module if the user runs HWiNFO or LibreHardwareMonitor.

pub mod bridge;
pub mod cpu;
pub mod disk;
pub mod gpu;
pub mod mem;
pub mod net;
pub mod pdh;

use crate::config::BridgeCfg;
use bridge::{Bridge, BridgeSource};
use cpu::{CpuReader, CpuStats};
use disk::{DiskReader, DiskStats};
use gpu::{GpuReader, GpuStats};
use mem::MemStats;
use net::{NetReader, NetStats};

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub cpu: CpuStats,
    pub mem: MemStats,
    pub gpu: Option<GpuStats>,
    pub disk: DiskStats,
    pub net: NetStats,
    pub bridge_source: BridgeSource,
    /// Which sensor the CPU temperature came from, e.g. "CPU Package".
    pub cpu_temp_label: Option<String>,
    pub board_temp_c: Option<f32>,
    /// Why the GPU panel is empty, when it is.
    pub gpu_error: Option<String>,
}

pub struct Sensors {
    cpu: CpuReader,
    pub gpu: GpuReader,
    disk: DiskReader,
    net: NetReader,
    bridge: Bridge,
    /// Drive health is slow to change and the IOCTL costs more than the other
    /// reads, so it is refreshed every Nth tick rather than every tick.
    disk_divider: u32,
    tick: u32,
    last_disk: DiskStats,
}

impl Sensors {
    pub fn new(bridge_cfg: &BridgeCfg, interval_ms: u64) -> Self {
        Self {
            cpu: CpuReader::new(),
            gpu: GpuReader::new(interval_ms),
            disk: DiskReader::new(),
            net: NetReader::new(),
            bridge: Bridge::start(bridge_cfg),
            disk_divider: 5,
            tick: 0,
            last_disk: DiskStats::default(),
        }
    }

    pub fn sample(&mut self) -> Snapshot {
        let mut cpu = self.cpu.read();
        let b = self.bridge.snapshot();
        cpu.temp_c = b.cpu_temp_c;

        let disk = if self.tick.is_multiple_of(self.disk_divider) {
            self.last_disk = self.disk.read();
            self.last_disk.clone()
        } else {
            // Throughput still needs a fresh PDH collection every tick; only
            // the per-drive SMART reads are throttled.
            let mut d = self.disk.read_throughput_only();
            d.drives = self.last_disk.drives.clone();
            d
        };
        self.tick = self.tick.wrapping_add(1);

        let gpu = self.gpu.read();
        Snapshot {
            cpu,
            mem: mem::read(),
            gpu_error: if gpu.is_none() { self.gpu.status() } else { None },
            gpu,
            disk,
            net: self.net.read(),
            bridge_source: b.source,
            cpu_temp_label: b.cpu_temp_label.clone(),
            board_temp_c: b.mobo_temp_c,
        }
    }
}

/// Formats a byte count with a binary-prefix unit, e.g. "12.4 GB".
pub fn fmt_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KB * KB * KB {
        format!("{:.1} GB", b / (KB * KB * KB))
    } else if b >= KB * KB {
        format!("{:.0} MB", b / (KB * KB))
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Formats a per-second rate. Network speeds read more naturally in decimal
/// units, matching how link speeds are quoted.
pub fn fmt_rate(bytes_per_sec: f64) -> String {
    let b = bytes_per_sec.max(0.0);
    if b >= 1_000_000_000.0 {
        format!("{:.2} GB/s", b / 1_000_000_000.0)
    } else if b >= 1_000_000.0 {
        format!("{:.1} MB/s", b / 1_000_000.0)
    } else if b >= 1_000.0 {
        format!("{:.0} KB/s", b / 1_000.0)
    } else {
        format!("{b:.0} B/s")
    }
}
