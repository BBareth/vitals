//! Network throughput, summed over real hardware interfaces.
//!
//! GetIfTable2 is used rather than PDH here: it is a single call with no
//! counter priming, and it exposes the hardware/loopback flags needed to skip
//! virtual adapters that would otherwise double-count traffic.

use std::time::Instant;

use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;

const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
const IF_TYPE_TUNNEL: u32 = 131;

#[derive(Clone, Debug, Default)]
pub struct NetStats {
    pub down_bps: f64,
    pub up_bps: f64,
}

pub struct NetReader {
    prev_in: u64,
    prev_out: u64,
    last: Instant,
    primed: bool,
}

impl NetReader {
    pub fn new() -> Self {
        let mut r = Self { prev_in: 0, prev_out: 0, last: Instant::now(), primed: false };
        if let Some((i, o)) = totals() {
            r.prev_in = i;
            r.prev_out = o;
            r.primed = true;
        }
        r
    }

    pub fn read(&mut self) -> NetStats {
        let Some((cur_in, cur_out)) = totals() else {
            return NetStats::default();
        };
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f64();
        let mut s = NetStats::default();

        if self.primed && dt > 0.0 {
            // Counters can reset when an adapter is re-enabled; saturating_sub
            // turns that into a single zero sample instead of a huge spike.
            s.down_bps = cur_in.saturating_sub(self.prev_in) as f64 / dt;
            s.up_bps = cur_out.saturating_sub(self.prev_out) as f64 / dt;
        }

        self.prev_in = cur_in;
        self.prev_out = cur_out;
        self.last = now;
        self.primed = true;
        s
    }
}

fn totals() -> Option<(u64, u64)> {
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    if unsafe { GetIfTable2(&mut table) }.is_err() || table.is_null() {
        return None;
    }
    let (mut sum_in, mut sum_out) = (0u64, 0u64);
    unsafe {
        let n = (*table).NumEntries as usize;
        let rows = std::ptr::addr_of!((*table).Table) as *const _;
        let rows: &[windows::Win32::NetworkManagement::IpHelper::MIB_IF_ROW2] = std::slice::from_raw_parts(rows, n);
        for row in rows {
            if row.Type == IF_TYPE_SOFTWARE_LOOPBACK || row.Type == IF_TYPE_TUNNEL {
                continue;
            }
            if row.OperStatus != IfOperStatusUp {
                continue;
            }
            // Bitfield: bit 0 is HardwareInterface. Virtual adapters (VPN,
            // Hyper-V switches) mirror physical traffic and would double-count.
            if row.InterfaceAndOperStatusFlags._bitfield & 0x01 == 0 {
                continue;
            }
            sum_in += row.InOctets;
            sum_out += row.OutOctets;
        }
        FreeMibTable(table as *const _);
    }
    Some((sum_in, sum_out))
}
