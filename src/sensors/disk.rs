//! Storage: NVMe drive temperatures plus aggregate disk throughput.
//!
//! Drive temperature comes from the NVMe SMART / Health log (page 0x02) via
//! IOCTL_STORAGE_QUERY_PROPERTY. Opening the device with a desired access of
//! zero grants only the right to issue query IOCTLs, which is what lets this
//! work without elevation.

use std::ffi::c_void;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::PCWSTR;

use super::pdh::Pdh;

const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;

const PROPERTY_STANDARD_QUERY: u32 = 0;
const STORAGE_DEVICE_PROPERTY: u32 = 0;
const STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY: u32 = 50;

const PROTOCOL_TYPE_NVME: u32 = 3;
const NVME_DATA_TYPE_LOG_PAGE: u32 = 2;
const NVME_LOG_PAGE_HEALTH_INFO: u32 = 0x02;

const BUS_TYPE_NVME: u32 = 17;

#[derive(Clone, Debug, Default)]
pub struct DriveInfo {
    pub model: String,
    pub temp_c: Option<f32>,
    /// NVMe "Percentage Used" endurance estimate; 100 means the rated write
    /// endurance has been consumed.
    pub wear_percent: Option<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct DiskStats {
    pub drives: Vec<DriveInfo>,
    pub read_bps: f64,
    pub write_bps: f64,
    pub busy_percent: f32,
}

pub struct DiskReader {
    drives: Vec<u32>,
    /// Model strings never change, so they are read once at startup.
    models: Vec<(u32, String, bool)>,
    pdh: Option<Pdh>,
    idx_read: Option<usize>,
    idx_write: Option<usize>,
    idx_idle: Option<usize>,
}

impl DiskReader {
    pub fn new() -> Self {
        let mut drives = Vec::new();
        let mut models = Vec::new();
        for i in 0..16u32 {
            if let Some(h) = open_drive(i) {
                if let Some((model, bus)) = device_descriptor(h) {
                    let is_nvme = bus == BUS_TYPE_NVME;
                    models.push((i, model, is_nvme));
                    drives.push(i);
                }
                unsafe {
                    let _ = CloseHandle(h);
                }
            }
        }

        let mut pdh = Pdh::new();
        let (mut r, mut w, mut idle) = (None, None, None);
        if let Some(p) = pdh.as_mut() {
            r = p.add("\\PhysicalDisk(_Total)\\Disk Read Bytes/sec");
            w = p.add("\\PhysicalDisk(_Total)\\Disk Write Bytes/sec");
            // Per-instance rather than _Total: averaging idle time across three
            // mostly-idle NVMe drives hides the one drive that is actually busy.
            // % Idle Time is used over % Disk Time because the latter is
            // unbounded on multi-queue devices.
            idle = p.add("\\PhysicalDisk(*)\\% Idle Time");
            p.collect();
        }

        Self { drives, models, pdh, idx_read: r, idx_write: w, idx_idle: idle }
    }

    pub fn read(&mut self) -> DiskStats {
        let mut out = DiskStats::default();

        for &i in &self.drives {
            let (model, is_nvme) =
                self.models.iter().find(|(idx, _, _)| *idx == i).map(|(_, m, n)| (m.clone(), *n)).unwrap_or_default();

            let mut info = DriveInfo { model, ..Default::default() };
            if is_nvme && let Some(h) = open_drive(i) {
                if let Some((temp, wear)) = nvme_health(h) {
                    info.temp_c = temp;
                    info.wear_percent = wear;
                }
                unsafe {
                    let _ = CloseHandle(h);
                }
            }
            out.drives.push(info);
        }

        self.fill_throughput(&mut out);
        out
    }

    /// Throughput without the per-drive SMART round trip, for ticks where the
    /// slower health data is being reused.
    pub fn read_throughput_only(&mut self) -> DiskStats {
        let mut out = DiskStats::default();
        self.fill_throughput(&mut out);
        out
    }

    fn fill_throughput(&mut self, out: &mut DiskStats) {
        if let Some(p) = self.pdh.as_mut() {
            p.collect();
            out.read_bps = self.idx_read.and_then(|i| p.value(i)).unwrap_or(0.0);
            out.write_bps = self.idx_write.and_then(|i| p.value(i)).unwrap_or(0.0);
            out.busy_percent = self
                .idx_idle
                .map(|i| {
                    p.values_by_instance(i)
                        .into_iter()
                        .filter(|(name, _)| !name.eq_ignore_ascii_case("_total"))
                        .map(|(_, idle)| (100.0 - idle).clamp(0.0, 100.0) as f32)
                        .fold(0.0f32, f32::max)
                })
                .unwrap_or(0.0);
        }
    }
}

fn open_drive(index: u32) -> Option<HANDLE> {
    let path: Vec<u16> = format!("\\\\.\\PhysicalDrive{index}").encode_utf16().chain(std::iter::once(0)).collect();
    let h = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            0, // query-only: no read or write access requested
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };
    match h {
        Ok(h) if !h.is_invalid() => Some(h),
        _ => None,
    }
}

/// STORAGE_DEVICE_DESCRIPTOR: ProductIdOffset @16, BusType @28. String offsets
/// are relative to the start of the descriptor and NUL-terminated ASCII.
fn device_descriptor(h: HANDLE) -> Option<(String, u32)> {
    let mut query = [0u8; 12];
    query[0..4].copy_from_slice(&STORAGE_DEVICE_PROPERTY.to_le_bytes());
    query[4..8].copy_from_slice(&PROPERTY_STANDARD_QUERY.to_le_bytes());

    let mut out = vec![0u8; 1024];
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(query.as_ptr() as *const c_void),
            query.len() as u32,
            Some(out.as_mut_ptr() as *mut c_void),
            out.len() as u32,
            Some(&mut returned),
            None,
        )
    };
    if ok.is_err() || returned < 32 {
        return None;
    }

    let product_off = u32::from_le_bytes(out[16..20].try_into().ok()?) as usize;
    let bus_type = u32::from_le_bytes(out[28..32].try_into().ok()?);
    let model = if product_off > 0 && product_off < out.len() {
        let end = out[product_off..].iter().position(|&b| b == 0).map(|p| product_off + p).unwrap_or(out.len());
        String::from_utf8_lossy(&out[product_off..end]).trim().to_string()
    } else {
        String::new()
    };
    Some((model, bus_type))
}

/// Reads NVMe log page 0x02 and pulls out composite temperature and wear.
///
/// The storage stack expects one buffer used for both request and reply, sized
/// to hold the full descriptor plus the log, and it validates ProtocolDataOffset
/// on the way in -- leaving that field zero makes the call fail on most drivers.
fn nvme_health(h: HANDLE) -> Option<(Option<f32>, Option<u8>)> {
    const SPSD_SIZE: u32 = 40; // sizeof(STORAGE_PROTOCOL_SPECIFIC_DATA)
    const LOG_SIZE: u32 = 512; // sizeof(NVME_HEALTH_INFO_LOG)

    // STORAGE_PROPERTY_QUERY header (8) + STORAGE_PROTOCOL_SPECIFIC_DATA (40) + log.
    let mut buf = vec![0u8; (8 + SPSD_SIZE + LOG_SIZE) as usize];
    buf[0..4].copy_from_slice(&STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY.to_le_bytes());
    buf[4..8].copy_from_slice(&PROPERTY_STANDARD_QUERY.to_le_bytes());
    buf[8..12].copy_from_slice(&PROTOCOL_TYPE_NVME.to_le_bytes());
    buf[12..16].copy_from_slice(&NVME_DATA_TYPE_LOG_PAGE.to_le_bytes());
    buf[16..20].copy_from_slice(&NVME_LOG_PAGE_HEALTH_INFO.to_le_bytes());
    buf[24..28].copy_from_slice(&SPSD_SIZE.to_le_bytes()); // ProtocolDataOffset
    buf[28..32].copy_from_slice(&LOG_SIZE.to_le_bytes()); // ProtocolDataLength

    let mut out = buf.clone();
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(buf.as_ptr() as *const c_void),
            buf.len() as u32,
            Some(out.as_mut_ptr() as *mut c_void),
            out.len() as u32,
            Some(&mut returned),
            None,
        )
    };
    if ok.is_err() || returned < 48 {
        return None;
    }

    // ProtocolDataOffset is relative to the STORAGE_PROTOCOL_SPECIFIC_DATA that
    // starts at descriptor offset 8.
    let data_off = 8 + u32::from_le_bytes(out[24..28].try_into().ok()?) as usize;
    let data_len = u32::from_le_bytes(out[28..32].try_into().ok()?) as usize;
    if data_len < 6 || data_off + data_len > out.len() {
        return None;
    }
    let log = &out[data_off..data_off + data_len];

    let kelvin = u16::from_le_bytes(log[1..3].try_into().ok()?);
    let temp = if kelvin == 0 { None } else { Some(kelvin as f32 - 273.15) };
    let wear = Some(log[5]);
    Some((temp, wear))
}
