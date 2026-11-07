//! Thin wrapper over the Performance Data Helper API.
//!
//! Rate counters need two collections before they produce a value, so every
//! query keeps its own priming state and simply returns None until ready.

use windows::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PdhAddEnglishCounterW,
    PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhGetFormattedCounterValue, PdhOpenQueryW,
};
use windows::core::PCWSTR;

pub struct Pdh {
    query: PDH_HQUERY,
    counters: Vec<PDH_HCOUNTER>,
    collections: u32,
}

// The handles are opaque kernel-side objects; the raw pointers they wrap are
// never dereferenced here, and PDH itself is thread-safe per query.
unsafe impl Send for Pdh {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

impl Pdh {
    pub fn new() -> Option<Self> {
        let mut query = PDH_HQUERY::default();
        if unsafe { PdhOpenQueryW(None, 0, &mut query) } != 0 {
            return None;
        }
        Some(Self { query, counters: Vec::new(), collections: 0 })
    }

    /// Adds a counter by its English path so the app works on localised Windows.
    /// Returns the handle index used with `value`, or None if the counter is
    /// absent on this machine.
    pub fn add(&mut self, path: &str) -> Option<usize> {
        let w = wide(path);
        let mut counter = PDH_HCOUNTER::default();
        if unsafe { PdhAddEnglishCounterW(self.query, PCWSTR(w.as_ptr()), 0, &mut counter) } != 0 {
            return None;
        }
        self.counters.push(counter);
        Some(self.counters.len() - 1)
    }

    pub fn collect(&mut self) {
        if unsafe { PdhCollectQueryData(self.query) } == 0 {
            self.collections = self.collections.saturating_add(1);
        }
    }

    pub fn value(&self, idx: usize) -> Option<f64> {
        if self.collections < 2 {
            return None;
        }
        let handle = *self.counters.get(idx)?;
        let mut v = PDH_FMT_COUNTERVALUE::default();
        if unsafe { PdhGetFormattedCounterValue(handle, PDH_FMT_DOUBLE, None, &mut v) } != 0 {
            return None;
        }
        Some(unsafe { v.Anonymous.doubleValue })
    }

    /// Expands a counter added with a `(*)` instance wildcard into one value
    /// per instance. Returns `(instance name, value)` pairs.
    pub fn values_by_instance(&self, idx: usize) -> Vec<(String, f64)> {
        if self.collections < 2 {
            return Vec::new();
        }
        let Some(&handle) = self.counters.get(idx) else {
            return Vec::new();
        };

        // First call sizes the buffer; PDH returns the byte count needed for
        // the item array plus the instance-name strings it points at.
        let mut size = 0u32;
        let mut count = 0u32;
        unsafe {
            PdhGetFormattedCounterArrayW(handle, PDH_FMT_DOUBLE, &mut size, &mut count, None);
        }
        if size == 0 {
            return Vec::new();
        }

        let mut raw = vec![0u8; size as usize];
        let rc = unsafe {
            PdhGetFormattedCounterArrayW(
                handle,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                Some(raw.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W),
            )
        };
        if rc != 0 {
            return Vec::new();
        }

        let items = raw.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W;
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            unsafe {
                let item = &*items.add(i);
                let name = item.szName.to_string().unwrap_or_default();
                out.push((name, item.FmtValue.Anonymous.doubleValue));
            }
        }
        out
    }
}

impl Drop for Pdh {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}
