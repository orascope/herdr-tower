// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)

//! How busy the machine is: the processor's time counters and how much of the
//! memory is in use, read the way each system's own monitor reads them.
//!
//! The processor gives no share by itself, only counters that grow: a share
//! is the busy part of the time that passed between two readings
//! ([`CpuTicks::busy_pct_since`]). Memory is a reading of its own.
//!
//! Every call answers `None` rather than failing: a header that cannot show
//! the machine's load shows nothing, and that is no reason for an error.

/// The processor's counters, summed over every core, in the system's own
/// unit (clock ticks on Unix, 100 ns on Windows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTicks {
    /// Time spent doing work, of any kind.
    pub busy: u64,
    /// All the time counted: `busy` and idle together.
    pub total: u64,
}

impl CpuTicks {
    /// The share of the time since `earlier` that was busy, 0–100. `None`
    /// when no time is counted between them — two readings within one tick,
    /// or counters that went backwards.
    pub fn busy_pct_since(&self, earlier: &CpuTicks) -> Option<f64> {
        let total = self.total.checked_sub(earlier.total)?;
        let busy = self.busy.checked_sub(earlier.busy)?;
        if total == 0 {
            return None;
        }
        Some((busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0))
    }
}

/// Physical memory, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    /// What the system's monitor calls used: on macOS Activity Monitor's
    /// "Memory Used" (app, wired and compressed), on Linux everything but
    /// `MemAvailable`, on Windows everything but what is available.
    pub used: u64,
    pub total: u64,
}

impl Memory {
    /// The share in use, 0–100.
    pub fn used_pct(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        (self.used as f64 / self.total as f64 * 100.0).clamp(0.0, 100.0)
    }
}

/// What a machine runs: its system and that system's version, the
/// processor's architecture, and the herdr-tower answering for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct System {
    /// `Ubuntu 26.04`, `macOS 27.0`, `Windows 11 24H2`; empty when this
    /// system could not be told.
    pub os: String,
    /// `x86_64`, `aarch64`.
    pub arch: String,
    /// The herdr-tower's version, `v0.57.3`.
    pub tower: String,
}

impl System {
    /// As a machine's band shows it: `Ubuntu 26.04 amd64 (herdr-tower
    /// v0.57.3)`, leaving out what is not known.
    pub fn label(&self) -> String {
        let mut out = [self.os.as_str(), arch_name(&self.arch)]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if !self.tower.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&format!("(herdr-tower {})", self.tower));
        }
        out
    }
}

/// The architecture as the release files and the systems' own tools name
/// it — `amd64`, `arm64` — rather than as Rust does; any other is kept.
pub fn arch_name(arch: &str) -> &str {
    match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// This machine, answered by a herdr-tower of version `tower`.
pub fn system(tower: &str) -> System {
    System {
        os: os_name().unwrap_or_default(),
        arch: std::env::consts::ARCH.to_string(),
        tower: tower.to_string(),
    }
}

/// The system's name and version, read once: it does not change while a
/// tower runs.
pub fn os_name() -> Option<String> {
    static NAME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        #[cfg(target_os = "linux")]
        {
            parse_os_release(&std::fs::read_to_string("/etc/os-release").ok()?)
        }
        #[cfg(target_os = "macos")]
        {
            parse_system_version(
                &std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist")
                    .ok()?,
            )
        }
        #[cfg(windows)]
        {
            windows::os_name()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            None
        }
    })
    .clone()
}

/// `NAME` and `VERSION_ID` of `/etc/os-release` — `Ubuntu 26.04`, `Rocky
/// Linux 8.10` — or its `PRETTY_NAME` when those are missing.
pub fn parse_os_release(text: &str) -> Option<String> {
    let field = |key: &str| -> Option<String> {
        let line = text.lines().find(|l| l.split('=').next() == Some(key))?;
        let value = line.split_once('=')?.1.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value)
            .trim();
        (!value.is_empty()).then(|| value.to_string())
    };
    match (field("NAME"), field("VERSION_ID")) {
        (Some(name), Some(version)) => Some(format!("{name} {version}")),
        _ => field("PRETTY_NAME").or_else(|| field("NAME")),
    }
}

/// `ProductName` and `ProductVersion` of macOS's `SystemVersion.plist`:
/// `macOS 27.0`.
pub fn parse_system_version(plist: &str) -> Option<String> {
    let string_after = |key: &str| -> Option<String> {
        let at = plist.find(&format!("<key>{key}</key>"))?;
        let rest = &plist[at..];
        let open = rest.find("<string>")? + "<string>".len();
        let close = rest[open..].find("</string>")?;
        let value = rest[open..open + close].trim();
        (!value.is_empty()).then(|| value.to_string())
    };
    Some(format!(
        "{} {}",
        string_after("ProductName")?,
        string_after("ProductVersion")?
    ))
}

/// The `sys` line of a [`report`]. The system's name goes last, as the
/// rest of the line, since it has spaces in it.
pub fn format_system(s: &System) -> String {
    let word = |v: &str| v.split_whitespace().collect::<Vec<_>>().join("_");
    format!(
        "sys arch={} tower={} os={}\n",
        word(&s.arch),
        word(&s.tower),
        s.os.lines().next().unwrap_or("").trim()
    )
}

/// The [`System`] in a [`report`], or `None` from a herdr-tower too old to
/// send one.
pub fn parse_system(text: &str) -> Option<System> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix("sys ")?;
        let (arch, rest) = rest.strip_prefix("arch=")?.split_once(' ')?;
        let (tower, rest) = rest.strip_prefix("tower=")?.split_once(' ')?;
        let os = rest.strip_prefix("os=")?.trim();
        Some(System {
            os: os.to_string(),
            arch: arch.to_string(),
            tower: tower.to_string(),
        })
    })
}

/// The processor's counters now.
pub fn cpu_ticks() -> Option<CpuTicks> {
    #[cfg(target_os = "linux")]
    {
        parse_proc_stat(&std::fs::read_to_string("/proc/stat").ok()?)
    }
    #[cfg(target_os = "macos")]
    {
        macos::cpu_ticks()
    }
    #[cfg(windows)]
    {
        windows::cpu_ticks()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        None
    }
}

/// The memory in use now.
pub fn memory() -> Option<Memory> {
    #[cfg(target_os = "linux")]
    {
        parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
    }
    #[cfg(target_os = "macos")]
    {
        macos::memory()
    }
    #[cfg(windows)]
    {
        windows::memory()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        None
    }
}

/// This machine's counters and memory as `herdr-tower host` prints them, for a
/// tower on another machine to read with [`parse_report`]:
///
/// ```text
/// cpu busy=569212486 total=1221423382
/// mem used=72598118400 total=103079215104
/// sys arch=aarch64 tower=v0.57.3 os=macOS 27.0
/// ```
///
/// Counters, not a share: the share is the busy part of the time between two
/// reports, and the reader is the one that has two. A part this system cannot
/// read is left out. The `sys` line ([`parse_system`]) is what the machine
/// runs, `tower` being the version of the herdr-tower answering.
pub fn report(tower: &str) -> String {
    format_report(cpu_ticks(), memory()) + &format_system(&system(tower))
}

/// [`report`] of the given readings.
pub fn format_report(cpu: Option<CpuTicks>, memory: Option<Memory>) -> String {
    let mut out = String::new();
    if let Some(c) = cpu {
        out.push_str(&format!("cpu busy={} total={}\n", c.busy, c.total));
    }
    if let Some(m) = memory {
        out.push_str(&format!("mem used={} total={}\n", m.used, m.total));
    }
    out
}

/// The readings in a [`report`]. Anything else in the text — an older
/// herdr-tower's "unknown command", a shell's banner — is not a line of it
/// and reads as nothing.
pub fn parse_report(text: &str) -> (Option<CpuTicks>, Option<Memory>) {
    let pair = |line: &str, kind: &str, a: &str, b: &str| -> Option<(u64, u64)> {
        let mut words = line.split_whitespace();
        if words.next() != Some(kind) {
            return None;
        }
        let x = words
            .next()?
            .strip_prefix(a)?
            .strip_prefix('=')?
            .parse()
            .ok()?;
        let y = words
            .next()?
            .strip_prefix(b)?
            .strip_prefix('=')?
            .parse()
            .ok()?;
        words.next().is_none().then_some((x, y))
    };
    let mut cpu = None;
    let mut memory = None;
    for line in text.lines() {
        if let Some((busy, total)) = pair(line, "cpu", "busy", "total") {
            cpu = (busy <= total).then_some(CpuTicks { busy, total });
        } else if let Some((used, total)) = pair(line, "mem", "used", "total") {
            memory = (used <= total && total > 0).then_some(Memory { used, total });
        }
    }
    (cpu, memory)
}

/// The `cpu` line of `/proc/stat`: user, nice, system, idle, iowait, irq,
/// softirq, steal. Idle and iowait are the idle part. The guest columns after
/// them are already counted in user and nice, so they are left out.
pub fn parse_proc_stat(text: &str) -> Option<CpuTicks> {
    let line = text
        .lines()
        .find(|l| l.split_whitespace().next() == Some("cpu"))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(|f| f.parse().ok())
        .collect::<Option<_>>()?;
    if fields.len() < 4 {
        return None;
    }
    let total: u64 = fields.iter().sum();
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
    Some(CpuTicks {
        busy: total - idle,
        total,
    })
}

/// `MemTotal` and `MemAvailable` from `/proc/meminfo`, in kB there. Used is
/// what is not available, as `free` and btop count it: the page cache the
/// kernel would give back on demand is not in use.
pub fn parse_meminfo(text: &str) -> Option<Memory> {
    let kb = |key: &str| -> Option<u64> {
        let line = text.lines().find(|l| l.split(':').next() == Some(key))?;
        line.split(':')
            .nth(1)?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    let total = kb("MemTotal")?.checked_mul(1024)?;
    let available = kb("MemAvailable")?.checked_mul(1024)?;
    Some(Memory {
        used: total.saturating_sub(available),
        total,
    })
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{CpuTicks, Memory};
    use std::sync::OnceLock;

    /// The host port, asked for once: every `mach_host_self` adds a reference
    /// to the port that nothing here would give back.
    #[allow(deprecated)]
    fn host() -> libc::mach_port_t {
        static HOST: OnceLock<libc::mach_port_t> = OnceLock::new();
        // SAFETY: takes no arguments and returns this task's host port.
        *HOST.get_or_init(|| unsafe { libc::mach_host_self() })
    }

    pub fn cpu_ticks() -> Option<CpuTicks> {
        // SAFETY: the struct is plain data the call fills, and the count says
        // how much room it has.
        let ticks = unsafe {
            let mut info: libc::host_cpu_load_info = std::mem::zeroed();
            let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
            let r = libc::host_statistics(
                host(),
                libc::HOST_CPU_LOAD_INFO,
                &mut info as *mut _ as libc::host_info_t,
                &mut count,
            );
            if r != libc::KERN_SUCCESS {
                return None;
            }
            info.cpu_ticks
        };
        let tick = |state: libc::c_int| u64::from(ticks[state as usize]);
        let idle = tick(libc::CPU_STATE_IDLE);
        let busy =
            tick(libc::CPU_STATE_USER) + tick(libc::CPU_STATE_SYSTEM) + tick(libc::CPU_STATE_NICE);
        Some(CpuTicks {
            busy,
            total: busy + idle,
        })
    }

    pub fn memory() -> Option<Memory> {
        let mut total: u64 = 0;
        let mut size = std::mem::size_of::<u64>();
        // SAFETY: `hw.memsize` is a 64-bit integer and `size` says so.
        let r = unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                &mut total as *mut u64 as *mut libc::c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if r != 0 || total == 0 {
            return None;
        }
        // SAFETY: as for the processor's counters.
        let vm = unsafe {
            let mut vm: libc::vm_statistics64 = std::mem::zeroed();
            let mut count = libc::HOST_VM_INFO64_COUNT;
            let r = libc::host_statistics64(
                host(),
                libc::HOST_VM_INFO64,
                &mut vm as *mut _ as libc::host_info64_t,
                &mut count,
            );
            if r != libc::KERN_SUCCESS {
                return None;
            }
            vm
        };
        // SAFETY: sysconf(3) takes a name and cannot fail on this one.
        let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
        // Activity Monitor's "Memory Used": app memory (anonymous pages that
        // cannot simply be dropped), wired, and what the compressor holds.
        let app = u64::from(vm.internal_page_count).saturating_sub(u64::from(vm.purgeable_count));
        let pages = app + u64::from(vm.wire_count) + u64::from(vm.compressor_page_count);
        Some(Memory {
            used: (pages * page).min(total),
            total,
        })
    }
}

#[cfg(windows)]
mod windows {
    use super::{CpuTicks, Memory};
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::GetSystemTimes;

    fn ticks(t: FILETIME) -> u64 {
        (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
    }

    pub fn cpu_ticks() -> Option<CpuTicks> {
        // SAFETY: the three structs are plain data the call fills.
        let (idle, kernel, user) = unsafe {
            let mut idle: FILETIME = std::mem::zeroed();
            let mut kernel: FILETIME = std::mem::zeroed();
            let mut user: FILETIME = std::mem::zeroed();
            if GetSystemTimes(&mut idle, &mut kernel, &mut user) == 0 {
                return None;
            }
            (ticks(idle), ticks(kernel), ticks(user))
        };
        // Kernel time includes the idle time.
        let total = kernel + user;
        Some(CpuTicks {
            busy: total.saturating_sub(idle),
            total,
        })
    }

    /// `Windows 11 24H2`: the registry's build number tells 11 from 10 —
    /// its `ProductName` still says 10 on 11 — and `DisplayVersion` is the
    /// release.
    pub fn os_name() -> Option<String> {
        use windows_sys::Win32::System::Registry::{
            RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ,
        };
        let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
        let key = wide(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
        let read = |name: &str| -> Option<String> {
            let name = wide(name);
            let mut buf = [0u16; 128];
            let mut size = std::mem::size_of_val(&buf) as u32;
            // SAFETY: the buffer and its size in bytes go together, and the
            // key and value names are NUL-terminated.
            let r = unsafe {
                RegGetValueW(
                    HKEY_LOCAL_MACHINE,
                    key.as_ptr(),
                    name.as_ptr(),
                    RRF_RT_REG_SZ,
                    std::ptr::null_mut(),
                    buf.as_mut_ptr().cast(),
                    &mut size,
                )
            };
            if r != 0 {
                return None;
            }
            let n = (size as usize / 2).min(buf.len());
            let s = String::from_utf16_lossy(&buf[..n]);
            let s = s.trim_end_matches('\0').trim();
            (!s.is_empty()).then(|| s.to_string())
        };
        let build: u32 = read("CurrentBuild")?.parse().ok()?;
        let name = if build >= 22000 {
            "Windows 11"
        } else {
            "Windows 10"
        };
        Some(match read("DisplayVersion") {
            Some(release) => format!("{name} {release}"),
            None => format!("{name} ({build})"),
        })
    }

    pub fn memory() -> Option<Memory> {
        // SAFETY: the struct is plain data, its length set as the call asks.
        unsafe {
            let mut status: MEMORYSTATUSEX = std::mem::zeroed();
            status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(&mut status) == 0 || status.ullTotalPhys == 0 {
                return None;
            }
            Some(Memory {
                used: status.ullTotalPhys.saturating_sub(status.ullAvailPhys),
                total: status.ullTotalPhys,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cpu_line_counts_idle_and_iowait_as_idle_and_leaves_out_guests() {
        let text = "cpu  100 20 30 400 50 6 4 0 99 99\ncpu0 1 2 3 4 5 6 7 8 9 10\nintr 1\n";
        let t = parse_proc_stat(text).unwrap();
        assert_eq!(t.total, 610);
        assert_eq!(t.busy, 160);
    }

    #[test]
    fn an_old_kernel_with_four_columns_still_reads() {
        let t = parse_proc_stat("cpu 1 2 3 4\n").unwrap();
        assert_eq!((t.busy, t.total), (6, 10));
    }

    #[test]
    fn a_stat_file_without_its_cpu_line_or_with_junk_reads_as_nothing() {
        assert_eq!(parse_proc_stat("cpu0 1 2 3 4\n"), None);
        assert_eq!(parse_proc_stat("cpu 1 2 x 4\n"), None);
        assert_eq!(parse_proc_stat("cpu 1 2\n"), None);
        assert_eq!(parse_proc_stat(""), None);
    }

    #[test]
    fn meminfo_counts_what_is_not_available_as_used() {
        let text = "MemTotal:       32000000 kB\nMemFree:  100 kB\nMemAvailable:   8000000 kB\n";
        let m = parse_meminfo(text).unwrap();
        assert_eq!(m.total, 32_000_000 * 1024);
        assert_eq!(m.used, 24_000_000 * 1024);
        assert_eq!(m.used_pct(), 75.0);
    }

    #[test]
    fn meminfo_without_mem_available_reads_as_nothing() {
        // Kernels before 3.14 have no MemAvailable; guessing it from MemFree
        // would call the page cache used and show every machine nearly full.
        assert_eq!(parse_meminfo("MemTotal: 100 kB\nMemFree: 50 kB\n"), None);
    }

    #[test]
    fn the_busy_share_is_taken_between_two_readings() {
        let a = CpuTicks {
            busy: 100,
            total: 400,
        };
        let b = CpuTicks {
            busy: 150,
            total: 600,
        };
        assert_eq!(b.busy_pct_since(&a), Some(25.0));
        assert_eq!(a.busy_pct_since(&a), None, "no time passed");
        assert_eq!(a.busy_pct_since(&b), None, "counters went backwards");
    }

    /// Edge: counters at the top of `u64`, a busy count that ran ahead of
    /// the total, and a total that did not move all stay inside 0–100 or
    /// read as nothing.
    #[test]
    fn the_busy_share_never_leaves_its_range() {
        let t = |busy, total| CpuTicks { busy, total };
        let cases = [
            ("all busy", t(0, 0), t(10, 10), Some(100.0)),
            ("all idle", t(0, 0), t(0, 10), Some(0.0)),
            ("busy ran ahead", t(0, 0), t(20, 10), Some(100.0)),
            (
                "at the top of u64",
                t(0, u64::MAX - 1),
                t(1, u64::MAX),
                Some(100.0),
            ),
            ("busy went back", t(5, 0), t(4, 10), None),
            ("total stood still", t(1, 7), t(2, 7), None),
        ];
        for (name, before, now, want) in cases {
            assert_eq!(now.busy_pct_since(&before), want, "{name}");
        }
        assert_eq!(
            Memory { used: 9, total: 4 }.used_pct(),
            100.0,
            "used over total"
        );
        assert_eq!(
            Memory {
                used: 0,
                total: u64::MAX
            }
            .used_pct(),
            0.0
        );
    }

    /// Property and cross-check: over generated pairs of readings, the share
    /// is the plain `Δbusy / Δtotal` worked out on its own, and it is `None`
    /// exactly when no time passed or a counter went back.
    #[test]
    fn the_busy_share_is_the_plain_ratio_of_the_deltas() {
        let mut seed: u64 = 20_261_021;
        let mut next = |m: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % m
        };
        for _ in 0..2000 {
            let before = CpuTicks {
                busy: next(1000),
                total: next(5000),
            };
            let now = CpuTicks {
                busy: next(1000),
                total: next(5000),
            };
            let got = now.busy_pct_since(&before);
            let (db, dt) = (
                now.busy as i64 - before.busy as i64,
                now.total as i64 - before.total as i64,
            );
            if db < 0 || dt <= 0 {
                assert_eq!(got, None, "{before:?} {now:?}");
            } else {
                let want = (db as f64 * 100.0 / dt as f64).min(100.0);
                let got = got.expect("a share");
                assert!(
                    (got - want).abs() < 1e-9,
                    "{before:?} {now:?}: {got} vs {want}"
                );
            }
        }
    }

    #[test]
    fn an_empty_memory_reads_as_none_used() {
        assert_eq!(Memory { used: 5, total: 0 }.used_pct(), 0.0);
    }

    #[test]
    fn a_report_reads_back_as_what_was_written() {
        let c = CpuTicks { busy: 7, total: 9 };
        let m = Memory { used: 3, total: 8 };
        let text = format_report(Some(c), Some(m));
        assert_eq!(text, "cpu busy=7 total=9\nmem used=3 total=8\n");
        assert_eq!(parse_report(&text), (Some(c), Some(m)));
        assert_eq!(parse_report(&format_report(None, Some(m))), (None, Some(m)));
        assert_eq!(format_report(None, None), "");
    }

    /// Property: whatever the counters, the report reads back exactly.
    #[test]
    fn any_report_round_trips() {
        let mut seed: u64 = 20_261_020;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            seed
        };
        for _ in 0..500 {
            let (a, b) = (next(), next());
            let c = CpuTicks {
                busy: a.min(b),
                total: a.max(b),
            };
            let m = Memory {
                used: a.min(b),
                total: a.max(b).max(1),
            };
            assert_eq!(
                parse_report(&format_report(Some(c), Some(m))),
                (Some(c), Some(m))
            );
        }
    }

    #[test]
    fn anything_that_is_not_a_report_line_reads_as_nothing() {
        for text in [
            "",
            "unknown command: host\n'herdr-tower help' lists them all\n",
            "cpu 1 2\n",
            "cpu busy=1\n",
            "cpu busy=x total=2\n",
            "cpu total=2 busy=1\n",
            "cpu busy=1 total=2 extra\n",
            "cpu busy=3 total=2\n",
            "mem used=1 total=0\n",
            "mem used=-1 total=4\n",
            "cpuz busy=1 total=2\n",
        ] {
            assert_eq!(parse_report(text), (None, None), "{text:?}");
        }
        // Lines around a report do not spoil it.
        let (cpu, memory) = parse_report("Welcome!\ncpu busy=1 total=2\n\nmem used=1 total=4\nbye");
        assert_eq!(cpu, Some(CpuTicks { busy: 1, total: 2 }));
        assert_eq!(memory, Some(Memory { used: 1, total: 4 }));
    }

    #[test]
    fn os_release_names_the_system_and_its_version() {
        let cases = [
            (
                "ubuntu",
                "PRETTY_NAME=\"Ubuntu 26.04 LTS\"\nNAME=\"Ubuntu\"\nVERSION_ID=\"26.04\"\nID=ubuntu\n",
                Some("Ubuntu 26.04"),
            ),
            (
                "rocky",
                "NAME=\"Rocky Linux\"\nVERSION=\"8.10 (Green Obsidian)\"\nVERSION_ID=\"8.10\"\n",
                Some("Rocky Linux 8.10"),
            ),
            ("single quotes", "NAME='Arch'\nVERSION_ID='2026'\n", Some("Arch 2026")),
            ("unquoted", "NAME=Alpine\nVERSION_ID=3.22.1\n", Some("Alpine 3.22.1")),
            (
                "rolling, no version",
                "NAME=\"Arch Linux\"\nPRETTY_NAME=\"Arch Linux\"\n",
                Some("Arch Linux"),
            ),
            ("name only", "NAME=Gentoo\n", Some("Gentoo")),
            ("empty values", "NAME=\"\"\nVERSION_ID=\"\"\n", None),
            ("nothing", "", None),
            ("a key that only starts like NAME", "NAMESPACE=x\nVERSION_ID=1\n", None),
        ];
        for (name, text, want) in cases {
            assert_eq!(parse_os_release(text).as_deref(), want, "{name}");
        }
    }

    #[test]
    fn macos_names_itself_in_its_system_version_plist() {
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
	<key>BuildID</key>
	<string>7A1B2C</string>
	<key>ProductBuildVersion</key>
	<string>27A123</string>
	<key>ProductName</key>
	<string>macOS</string>
	<key>ProductVersion</key>
	<string>27.0</string>
</dict>
</plist>"#;
        assert_eq!(parse_system_version(plist).as_deref(), Some("macOS 27.0"));
        assert_eq!(parse_system_version("<dict></dict>"), None);
        assert_eq!(
            parse_system_version("<key>ProductName</key><string>macOS</string>"),
            None,
            "no version, no answer"
        );
    }

    #[test]
    fn a_systems_label_leaves_out_what_is_not_known() {
        let s = |os: &str, arch: &str, tower: &str| System {
            os: os.into(),
            arch: arch.into(),
            tower: tower.into(),
        };
        let cases = [
            (
                s("Ubuntu 26.04", "x86_64", "v0.57.3"),
                "Ubuntu 26.04 amd64 (herdr-tower v0.57.3)",
            ),
            (s("", "aarch64", "v0.57.3"), "arm64 (herdr-tower v0.57.3)"),
            (s("macOS 27.0", "aarch64", ""), "macOS 27.0 arm64"),
            (
                s("Rocky Linux 8.10", "riscv64", ""),
                "Rocky Linux 8.10 riscv64",
            ),
            (s("", "", "dev"), "(herdr-tower dev)"),
            (s("", "", ""), ""),
        ];
        for (system, want) in cases {
            assert_eq!(system.label(), want, "{system:?}");
        }
    }

    #[test]
    fn an_architecture_takes_the_name_its_release_file_has() {
        let cases = [
            ("x86_64", "amd64"),
            ("aarch64", "arm64"),
            ("amd64", "amd64"),
            ("arm64", "arm64"),
            ("riscv64", "riscv64"),
            ("", ""),
        ];
        for (arch, want) in cases {
            assert_eq!(arch_name(arch), want, "{arch}");
            // Naming twice names the same.
            assert_eq!(arch_name(arch_name(arch)), want, "{arch}");
        }
    }

    /// Round trip: whatever the system is called — spaces, an empty part —
    /// its `sys` line reads back as it was.
    #[test]
    fn a_sys_line_reads_back_as_written() {
        for (os, arch, tower) in [
            ("Ubuntu 26.04", "x86_64", "v0.57.3"),
            ("Windows 11 24H2", "x86_64", "v0.57.3"),
            ("Rocky Linux 8.10", "aarch64", "dev"),
            ("", "aarch64", "v1.0.0"),
            ("macOS 27.0", "", ""),
        ] {
            let system = System {
                os: os.into(),
                arch: arch.into(),
                tower: tower.into(),
            };
            let text =
                format_report(Some(CpuTicks { busy: 1, total: 2 }), None) + &format_system(&system);
            assert_eq!(parse_system(&text), Some(system.clone()), "{text:?}");
            assert_eq!(parse_report(&text).0, Some(CpuTicks { busy: 1, total: 2 }));
        }
        // A herdr-tower too old to send one, or junk, says nothing.
        for text in [
            "cpu busy=1 total=2\n",
            "",
            "sys os=x\n",
            "sys arch=x\n",
            "system arch=a tower=b os=c\n",
        ] {
            assert_eq!(parse_system(text), None, "{text:?}");
        }
        // And an older reader skips the new line.
        let text = format_system(&System::default()) + "mem used=1 total=4\n";
        assert_eq!(
            parse_report(&text),
            (None, Some(Memory { used: 1, total: 4 }))
        );
    }

    #[test]
    fn this_machine_says_what_it_runs() {
        let s = system("v9.9.9");
        assert_eq!(s.arch, std::env::consts::ARCH);
        assert_eq!(s.tower, "v9.9.9");
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(!s.os.is_empty(), "{s:?}");
        }
        assert_eq!(parse_system(&report("v9.9.9")), Some(s));
    }

    #[test]
    fn this_machine_answers_on_the_systems_that_are_built() {
        if cfg!(any(target_os = "linux", target_os = "macos", windows)) {
            let t = cpu_ticks().expect("cpu counters");
            assert!(t.busy <= t.total && t.total > 0, "{t:?}");
            let m = memory().expect("memory");
            assert!(m.used <= m.total && m.total > 0, "{m:?}");
        }
    }
}
