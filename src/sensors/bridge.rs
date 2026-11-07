//! Optional source for sensors that need kernel-level access, chiefly CPU
//! package temperature.
//!
//! Vitals ships no driver of its own. If HWiNFO or LibreHardwareMonitor is
//! already running, this reads what it has already collected: HWiNFO through
//! its shared-memory block, LibreHardwareMonitor through its local web server.
//! Both are polled on a background thread so a stalled reader never blocks a
//! frame.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::BridgeCfg;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BridgeSource {
    #[default]
    None,
    HwInfo,
    Lhm,
}

impl BridgeSource {
    pub fn label(self) -> &'static str {
        match self {
            BridgeSource::None => "no sensor bridge",
            BridgeSource::HwInfo => "HWiNFO",
            BridgeSource::Lhm => "LibreHardwareMonitor",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct BridgeData {
    pub source: BridgeSource,
    pub cpu_temp_c: Option<f32>,
    pub cpu_temp_label: Option<String>,
    pub mobo_temp_c: Option<f32>,
}

pub struct Bridge {
    shared: Arc<Mutex<BridgeData>>,
}

impl Bridge {
    pub fn start(cfg: &BridgeCfg) -> Self {
        let shared = Arc::new(Mutex::new(BridgeData::default()));
        if cfg.source == "none" {
            return Self { shared };
        }

        let want_hwinfo = matches!(cfg.source.as_str(), "auto" | "hwinfo");
        let want_lhm = matches!(cfg.source.as_str(), "auto" | "lhm");
        let url = cfg.lhm_url.clone();
        let out = Arc::clone(&shared);

        std::thread::Builder::new()
            .name("vitals-bridge".into())
            .spawn(move || {
                loop {
                    let mut data = BridgeData::default();
                    if want_hwinfo && let Some(d) = hwinfo::read() {
                        data = d;
                    }
                    if data.cpu_temp_c.is_none()
                        && want_lhm
                        && let Some(d) = lhm::read(&url)
                    {
                        data = d;
                    }
                    if let Ok(mut g) = out.lock() {
                        *g = data;
                    }
                    // Bridge data moves slowly and both paths cost more than a
                    // native read, so it runs at half the UI rate.
                    std::thread::sleep(Duration::from_millis(2000));
                }
            })
            .ok();

        Self { shared }
    }

    pub fn snapshot(&self) -> BridgeData {
        self.shared.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

/// Scores a temperature label so the most representative CPU sensor wins,
/// regardless of which of the two backends produced it.
fn cpu_temp_score(label: &str) -> i32 {
    let l = label.to_ascii_lowercase();
    if l.contains("cpu package") || l.contains("package id") {
        100
    } else if l.contains("tctl") || l.contains("tdie") {
        95
    } else if l.contains("core max") || l.contains("cpu (max)") {
        80
    } else if l.contains("core average") || l.contains("cpu average") {
        70
    } else if l.starts_with("cpu core") || l.contains("core #") {
        40
    } else if l.contains("cpu") {
        30
    } else {
        0
    }
}

/// Picks the probe that actually represents board temperature, rather than one
/// that happens to sit on the motherboard.
fn board_temp_score(label: &str) -> i32 {
    let l = label.to_ascii_lowercase();
    if l.contains("motherboard") || l.contains("mainboard") {
        100
    } else if l == "system" || l.contains("system temp") {
        90
    } else if l.contains("cpu") || l.contains("vrm") || l.contains("mos") || l.contains("pch") {
        // Component-specific probes, not an ambient board reading.
        0
    } else {
        10
    }
}

mod hwinfo {
    //! Reads the HWiNFO_SENS_SM2 shared-memory block.
    //!
    //! Layout is fixed and published by HWiNFO. Element strides come from the
    //! header rather than being hardcoded, so a future revision that grows an
    //! element still walks correctly.

    use super::{BridgeData, BridgeSource, cpu_temp_score};
    use std::ffi::c_void;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Memory::{FILE_MAP_READ, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile};
    use windows::core::PCWSTR;

    /// 'SiWH' as a little-endian DWORD; the first four bytes read "HWiS".
    const SIGNATURE: u32 = 0x5369_5748;
    const READING_TYPE_TEMP: u32 = 1;

    // The published structures are packed -- there is no alignment padding
    // before the 64-bit fields, so every offset here is the plain running sum
    // of the preceding members. Assuming natural C alignment shifts the whole
    // header by four bytes and lands the value read on the wrong field.
    //
    // HWiNFO_SENSORS_SHARED_MEM2:
    //   0 signature, 4 version, 8 revision, 12 poll_time (i64, unaligned),
    //   20 sensor offset, 24 sensor element size, 28 sensor count,
    //   32 reading offset, 36 reading element size, 40 reading count.
    const H_SENSOR_OFF: usize = 20;
    const H_SENSOR_SIZE: usize = 24;
    const H_SENSOR_COUNT: usize = 28;
    const H_READING_OFF: usize = 32;
    const H_READING_SIZE: usize = 36;
    const H_READING_COUNT: usize = 40;

    // HWiNFO_SENSORS_READING_ELEMENT: 0 type, 4 sensor index, 8 reading id,
    // 12 szLabelOrig[128], 140 szLabelUser[128], 268 szUnit[16], then the four
    // doubles. Current builds make the element larger than these 316 bytes, so
    // the stride comes from the header while the field offsets stay fixed.
    const R_LABEL_OFF: usize = 12;
    const R_VALUE_OFF: usize = 284;
    const R_MIN_SIZE: usize = 316;
    const STRING_LEN: usize = 128;
    /// Sensor element: 0 id, 4 instance, 8 szSensorNameOrig[128].
    const S_NAME_OFF: usize = 8;
    const S_MIN_SIZE: usize = 264;

    fn u32_at(b: &[u8], off: usize) -> Option<u32> {
        b.get(off..off + 4)?.try_into().ok().map(u32::from_le_bytes)
    }
    fn f64_at(b: &[u8], off: usize) -> Option<f64> {
        b.get(off..off + 8)?.try_into().ok().map(f64::from_le_bytes)
    }
    fn cstr_at(b: &[u8], off: usize, max: usize) -> String {
        let Some(slice) = b.get(off..off + max) else {
            return String::new();
        };
        let end = slice.iter().position(|&c| c == 0).unwrap_or(max);
        String::from_utf8_lossy(&slice[..end]).trim().to_string()
    }

    pub fn read() -> Option<BridgeData> {
        // The mapping is reopened each poll: it costs microseconds and means a
        // HWiNFO restart is picked up automatically instead of going stale.
        let handle = open_mapping()?;
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ, 0, 0, 0) };
        if view.Value.is_null() {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return None;
        }

        let result = (|| {
            let header = unsafe { std::slice::from_raw_parts(view.Value as *const u8, 48) };
            if u32_at(header, 0)? != SIGNATURE {
                return None;
            }
            let sensor_off = u32_at(header, H_SENSOR_OFF)? as usize;
            let sensor_size = u32_at(header, H_SENSOR_SIZE)? as usize;
            let sensor_count = u32_at(header, H_SENSOR_COUNT)? as usize;
            let reading_off = u32_at(header, H_READING_OFF)? as usize;
            let reading_size = u32_at(header, H_READING_SIZE)? as usize;
            let reading_count = u32_at(header, H_READING_COUNT)? as usize;

            if reading_size < R_MIN_SIZE || reading_count == 0 || reading_count > 100_000 {
                return None;
            }
            // The reading section starts immediately after the sensor section.
            // If that does not hold, the layout is not what we think it is and
            // reading on would produce confident nonsense.
            if sensor_off + sensor_size * sensor_count != reading_off {
                return None;
            }

            let total = reading_off + reading_size * reading_count;
            let all = unsafe { std::slice::from_raw_parts(view.Value as *const u8, total) };

            // Sensor names give context, e.g. distinguishing a CPU package
            // reading from a drive or chipset reading with a similar label.
            let sensor_name = |idx: usize| -> String {
                if idx >= sensor_count || sensor_size < S_MIN_SIZE {
                    return String::new();
                }
                cstr_at(all, sensor_off + idx * sensor_size + S_NAME_OFF, STRING_LEN)
            };

            let mut best: Option<(i32, f32, String)> = None;
            let mut mobo: Option<f32> = None;

            for i in 0..reading_count {
                let base = reading_off + i * reading_size;
                if u32_at(all, base)? != READING_TYPE_TEMP {
                    continue;
                }
                let sensor_idx = u32_at(all, base + 4)? as usize;
                let label = cstr_at(all, base + R_LABEL_OFF, STRING_LEN);
                let value = f64_at(all, base + R_VALUE_OFF)? as f32;
                if !value.is_finite() || value <= 0.0 || value > 150.0 {
                    continue;
                }

                let sname = sensor_name(sensor_idx);
                let mut score = cpu_temp_score(&label);
                if score > 0 && !sname.to_ascii_lowercase().contains("cpu") {
                    // A label mentioning "CPU" under a non-CPU sensor (a
                    // motherboard "CPU socket" probe, say) is less trustworthy.
                    score -= 25;
                }
                if score > 0 && best.as_ref().is_none_or(|(s, _, _)| score > *s) {
                    best = Some((score, value, label.clone()));
                }

                let ll = label.to_ascii_lowercase();
                if mobo.is_none() && (ll.contains("motherboard") || ll.contains("system")) {
                    mobo = Some(value);
                }
            }

            best.map(|(_, v, label)| BridgeData {
                source: BridgeSource::HwInfo,
                cpu_temp_c: Some(v),
                cpu_temp_label: Some(label),
                mobo_temp_c: mobo,
            })
        })();

        unsafe {
            let _ = UnmapViewOfFile(view);
            let _ = CloseHandle(handle);
        }
        result
    }

    fn open_mapping() -> Option<windows::Win32::Foundation::HANDLE> {
        for name in ["Global\\HWiNFO_SENS_SM2", "HWiNFO_SENS_SM2"] {
            let w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            if let Ok(h) = unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(w.as_ptr())) }
                && !h.is_invalid()
            {
                return Some(h);
            }
        }
        None
    }

    // Silences an unused-import warning on the c_void alias used by the view type.
    #[allow(dead_code)]
    fn _keep(_: *const c_void) {}
}

mod lhm {
    //! Reads LibreHardwareMonitor's Remote Web Server JSON.
    //!
    //! A hand-rolled request keeps the dependency list to serde_json; the
    //! endpoint is always loopback, so there is no TLS or redirect handling to
    //! get wrong.

    use super::{BridgeData, BridgeSource, board_temp_score, cpu_temp_score};
    use serde_json::Value;
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;

    pub fn read(url: &str) -> Option<BridgeData> {
        let body = http_get(url)?;
        let json: Value = serde_json::from_str(&body).ok()?;

        let mut best: Option<(i32, f32, String)> = None;
        let mut mobo: Option<(i32, f32)> = None;
        walk(&json, "", &mut best, &mut mobo);
        let mobo = mobo.map(|(_, v)| v);

        best.map(|(_, v, label)| BridgeData {
            source: BridgeSource::Lhm,
            cpu_temp_c: Some(v),
            cpu_temp_label: Some(label),
            mobo_temp_c: mobo,
        })
    }

    fn walk(node: &Value, hardware: &str, best: &mut Option<(i32, f32, String)>, mobo: &mut Option<(i32, f32)>) {
        let text = node.get("Text").and_then(Value::as_str).unwrap_or("");
        let sensor_id = node.get("SensorId").and_then(Value::as_str).unwrap_or("");
        let ty = node.get("Type").and_then(Value::as_str).unwrap_or("");

        if ty == "Temperature"
            && let Some(v) = parse_value(node.get("Value").and_then(Value::as_str).unwrap_or(""))
            && v > 0.0
            && v < 150.0
        {
            let is_cpu = sensor_id.contains("/intelcpu/")
                || sensor_id.contains("/amdcpu/")
                || hardware.to_ascii_lowercase().contains("cpu");
            if is_cpu {
                let score = cpu_temp_score(text).max(20);
                if best.as_ref().is_none_or(|(s, _, _)| score > *s) {
                    *best = Some((score, v, text.to_string()));
                }
            } else if sensor_id.contains("/lpc/") {
                // The super-I/O chip exposes a whole row of probes. Its
                // "CPU" and "CPU Socket" ones sit next to the socket and
                // track the CPU, so presenting either as a board
                // temperature would be quietly misleading; the ambient
                // "System" probe is the one that means what it says.
                let score = board_temp_score(text);
                if score > 0 && mobo.is_none_or(|(s, _)| score > s) {
                    *mobo = Some((score, v));
                }
            }
        }

        // Hardware nodes carry no Type; their Text names the device that owns
        // every sensor beneath them. The category nodes LibreHardwareMonitor
        // inserts between hardware and sensors have no Type either, so they are
        // skipped explicitly or they would shadow the device name.
        const GROUPS: [&str; 10] = [
            "Temperatures",
            "Clocks",
            "Load",
            "Powers",
            "Voltages",
            "Fans",
            "Controls",
            "Data",
            "Throughput",
            "Levels",
        ];
        let next_hardware = if ty.is_empty() && !text.is_empty() && !GROUPS.contains(&text) { text } else { hardware };
        if let Some(children) = node.get("Children").and_then(Value::as_array) {
            for c in children {
                walk(c, next_hardware, best, mobo);
            }
        }
    }

    /// Values arrive as display strings such as "45,0 °C" or "45.0 °C" -- the
    /// decimal separator follows the host's locale.
    fn parse_value(s: &str) -> Option<f32> {
        let cleaned: String = s
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',' || *c == '-')
            .map(|c| if c == ',' { '.' } else { c })
            .collect();
        cleaned.parse::<f32>().ok()
    }

    fn http_get(url: &str) -> Option<String> {
        let rest = url.strip_prefix("http://")?;
        let (host_port, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let addr = host_port.to_socket_addrs().ok()?.next()?;

        // Short timeouts: if LibreHardwareMonitor is not running we want to
        // fail immediately and retry on the next poll, not hold the thread.
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(300)).ok()?;
        stream.set_read_timeout(Some(Duration::from_millis(1500))).ok()?;
        stream.set_write_timeout(Some(Duration::from_millis(500))).ok()?;

        let req = format!(
            "GET {path} HTTP/1.0\r\nHost: {host_port}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).ok()?;

        let mut raw = Vec::new();
        // HTTP/1.0 with Connection: close means the body ends at EOF, so a
        // plain read-to-end is the whole response. Cap it defensively.
        let mut limited = stream.take(4 * 1024 * 1024);
        limited.read_to_end(&mut raw).ok()?;

        let text = String::from_utf8_lossy(&raw);
        let body_start = text.find("\r\n\r\n").map(|i| i + 4).or_else(|| text.find("\n\n").map(|i| i + 2))?;
        Some(text[body_start..].to_string())
    }
}
