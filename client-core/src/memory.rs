//! Small process-local RSS reading for runtime diagnostics.
//!
//! This reports resident bytes for this process only. It is not an allocator
//! budget and cannot account for shared pages or memory in child processes.

#[cfg(target_os = "linux")]
use std::fs;
use std::io;

#[derive(Debug)]
pub enum RssError {
    Unsupported,
    Io(io::Error),
    Invalid,
}

impl std::fmt::Display for RssError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => {
                formatter.write_str("RSS reporting is unsupported on this platform")
            }
            Self::Io(error) => write!(formatter, "could not read process RSS: {error}"),
            Self::Invalid => formatter.write_str("process RSS data was invalid"),
        }
    }
}

impl std::error::Error for RssError {}

/// Return this process's current resident-set size in bytes where supported.
///
/// On Linux this reads `VmRSS` from procfs. On macOS it calls `proc_pidinfo`
/// with `PROC_PIDTASKINFO` and uses the SDK's `pti_resident_size` field. The
/// value covers only the current process and includes shared pages according
/// to the operating-system RSS definition.
pub fn current_process_rss_bytes() -> Result<u64, RssError> {
    #[cfg(target_os = "linux")]
    {
        let status = fs::read_to_string("/proc/self/status").map_err(RssError::Io)?;
        let value = status
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .ok_or(RssError::Invalid)?;
        let kib = value
            .split_whitespace()
            .next()
            .ok_or(RssError::Invalid)?
            .parse::<u64>()
            .map_err(|_| RssError::Invalid)?;
        return kib.checked_mul(1024).ok_or(RssError::Invalid);
    }
    #[cfg(target_os = "macos")]
    {
        use std::mem::{size_of, MaybeUninit};

        // SAFETY: proc_pidinfo writes a `proc_taskinfo` when its return value
        // equals the requested buffer size. The buffer has that exact SDK/
        // libc layout and remains live for the call.
        let mut task = MaybeUninit::<libc::proc_taskinfo>::uninit();
        let size = size_of::<libc::proc_taskinfo>() as libc::c_int;
        let written = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDTASKINFO,
                0,
                task.as_mut_ptr().cast(),
                size,
            )
        };
        if written < 0 {
            return Err(RssError::Io(io::Error::last_os_error()));
        }
        if written != size {
            return Err(RssError::Invalid);
        }
        // SAFETY: the exact return size above confirms every byte was filled.
        let task = unsafe { task.assume_init() };
        if task.pti_resident_size == 0 {
            return Err(RssError::Invalid);
        }
        return Ok(task.pti_resident_size);
    }
    #[cfg(windows)]
    {
        Ok(windows_memory_counters()?.WorkingSetSize as u64)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        Err(RssError::Unsupported)
    }
}

/// Kernel high-water RSS includes peaks between application samples.
pub fn peak_process_rss_bytes() -> Result<u64, RssError> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: getrusage initializes the exact libc layout on success.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
            return Err(RssError::Io(io::Error::last_os_error()));
        }
        let value = unsafe { usage.assume_init() }.ru_maxrss;
        let value = u64::try_from(value).map_err(|_| RssError::Invalid)?;
        #[cfg(target_os = "linux")]
        {
            return value.checked_mul(1024).ok_or(RssError::Invalid);
        }
        #[cfg(target_os = "macos")]
        {
            return Ok(value);
        }
    }
    #[cfg(windows)]
    {
        Ok(windows_memory_counters()?.PeakWorkingSetSize as u64)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        Err(RssError::Unsupported)
    }
}

#[cfg(windows)]
fn windows_memory_counters(
) -> Result<windows_sys::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS, RssError> {
    use windows_sys::Win32::System::{
        ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::GetCurrentProcess,
    };
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size,
        ..Default::default()
    };
    // The SDK struct and size match the Windows API; the pseudo-handle refers
    // only to this process. Working set is resident memory, not private commit.
    if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) } == 0 {
        return Err(RssError::Io(io::Error::last_os_error()));
    }
    if counters.WorkingSetSize == 0 || counters.PeakWorkingSetSize < counters.WorkingSetSize {
        return Err(RssError::Invalid);
    }
    Ok(counters)
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux", windows)))]
mod tests {
    use super::current_process_rss_bytes;

    #[test]
    fn current_process_rss_is_nonzero_and_stable_across_adjacent_samples() {
        let first = current_process_rss_bytes().expect("first current-process RSS sample");
        let second = current_process_rss_bytes().expect("second current-process RSS sample");
        assert!(first > 0, "RSS should be reported in bytes");
        assert!(second > 0, "RSS should be reported in bytes");
        assert!(super::peak_process_rss_bytes().unwrap() >= first);
        assert!(
            first.abs_diff(second) < 256 * 1024 * 1024,
            "adjacent current-process RSS samples should be plausible"
        );
    }
}
