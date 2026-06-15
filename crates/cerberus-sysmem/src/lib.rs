//! Resident-set-size probe behind a tiny cross-platform seam.
//!
//! `mem-gate` (the memory-budget CI gate, PLAN.md §5) needs the process's
//! resident memory, and std has no portable API for it. This adapter wraps the
//! per-OS source: Linux procfs, the Win32 working set via `GetProcessMemoryInfo`,
//! and `None` elsewhere (macOS to come — ADR-0015). It is an **adapter** crate so
//! the single `unsafe` FFI call (Windows) stays isolated and reviewed per the
//! workspace policy (PLAN §7); callers (`cerberus-app`) remain `unsafe`-free.

/// Process resident set size in kilobytes, or `None` when the platform has no
/// implementation yet — callers degrade gracefully (`mem-gate` skips its budget
/// assertion).
#[cfg(target_os = "linux")]
pub fn resident_set_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<u64>().ok());
        }
    }
    None
}

/// Windows: the process working set via `GetProcessMemoryInfo`
/// (`K32GetProcessMemoryInfo`, exported by `kernel32` since Windows 7, so no
/// extra link is needed). See the module note on the isolated `unsafe`.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn resident_set_kb() -> Option<u64> {
    use core::ffi::c_void;

    // Mirrors the Win32 PROCESS_MEMORY_COUNTERS layout; only `working_set_size`
    // is read, but every field must be present for the correct size + offsets.
    #[repr(C)]
    #[allow(dead_code)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn K32GetProcessMemoryInfo(
            process: *mut c_void,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }

    // SAFETY: every field is plain-old-data for which all-zero is valid; we then
    // set `cb` to the struct's byte size, as the API requires.
    let mut counters: ProcessMemoryCounters = unsafe { core::mem::zeroed() };
    counters.cb = core::mem::size_of::<ProcessMemoryCounters>() as u32;

    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no closing;
    // we pass a correctly-sized, fully-initialized out-param and its byte count,
    // exactly as `GetProcessMemoryInfo` specifies.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    if ok == 0 {
        return None;
    }
    Some(counters.working_set_size as u64 / 1024)
}

/// Other platforms (macOS, …) have no probe yet; the gate degrades gracefully.
#[cfg(not(any(target_os = "linux", windows)))]
pub fn resident_set_kb() -> Option<u64> {
    None
}
