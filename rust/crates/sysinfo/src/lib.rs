use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default)]
pub struct Memory {
    pub rss: u64,
    pub heap_alloc: u64,
    pub heap_sys: u64,
    pub sys: u64,
    pub tasks: usize,
}

pub fn read_memory() -> Memory {
    Memory {
        rss: resident_bytes(),
        heap_alloc: 0,
        heap_sys: 0,
        sys: 0,
        tasks: 0,
    }
}

fn resident_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(raw) = fs::read_to_string("/proc/self/statm")
            && let Some(res_pages) = raw.split_whitespace().nth(1)
            && let Ok(pages) = res_pages.parse::<u64>()
        {
            // SAFETY: sysconf(_SC_PAGESIZE) is a standard POSIX query with no pointers.
            let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            if page_size > 0 {
                return pages.saturating_mul(page_size as u64);
            }
        }
        0
    }
    #[cfg(not(target_os = "linux"))]
    {
        let pid = std::process::id().to_string();
        if let Ok(out) = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid])
            .output()
            && out.status.success()
            && let Ok(kb) = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>()
        {
            return kb.saturating_mul(1024);
        }
        0
    }
}

pub fn megabytes(value: u64) -> f64 {
    value as f64 / (1u64 << 20) as f64
}

pub fn format_uptime_secs(total_secs: i64) -> String {
    if total_secs < 0 {
        return "--".to_owned();
    }
    let clock = format!(
        "{:02}:{:02}:{:02}",
        (total_secs % 86400) / 3600,
        (total_secs % 3600) / 60,
        total_secs % 60
    );
    let days = total_secs / 86400;
    if days > 0 {
        format!("{days}天 {clock}")
    } else {
        clock
    }
}

pub fn format_uptime(value: Duration) -> String {
    format_uptime_secs(value.as_secs() as i64)
}

pub fn format_bytes(size: u64) -> String {
    let value = size as f64;
    if size >= (1u64 << 30) {
        format!("{:.2} GB", value / (1u64 << 30) as f64)
    } else if size >= (1u64 << 20) {
        format!("{:.1} MB", value / (1u64 << 20) as f64)
    } else if size >= (1u64 << 10) {
        format!("{:.1} KB", value / (1u64 << 10) as f64)
    } else {
        format!("{size} B")
    }
}

#[derive(Debug, Clone, Default)]
pub struct Host {
    pub hostname: String,
    pub platform: String,
    pub kernel_release: String,
    pub uptime: Duration,
    pub load_average: [f64; 3],
    pub cpus: usize,
    pub total_memory: u64,
    pub free_memory: u64,
}

pub fn read_host() -> Host {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let mut host = Host {
        platform: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        cpus,
        ..Default::default()
    };
    if let Ok(name) = hostname() {
        host.hostname = name;
    }
    if let Some(rel) = kernel_release() {
        host.kernel_release = rel;
    }
    if let Some(secs) = read_uptime("/proc/uptime") {
        host.uptime = Duration::from_secs_f64(secs.max(0.0));
    }
    if let Some(load) = read_load_average("/proc/loadavg") {
        host.load_average = load;
    }
    if let Some((total, free)) = read_mem_info("/proc/meminfo") {
        host.total_memory = total;
        host.free_memory = free;
    }
    host
}

fn hostname() -> std::io::Result<String> {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: gethostname writes at most buf.len() bytes into buf.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        Ok(String::from_utf8_lossy(&buf[..end]).into_owned())
    }
    #[cfg(not(unix))]
    {
        Ok(String::new())
    }
}

fn kernel_release() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: uname initializes the utsname struct when returning 0.
        let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
        // SAFETY: uts is a valid mutable utsname pointer.
        if unsafe { libc::uname(&mut uts) } != 0 {
            return None;
        }
        let bytes: Vec<u8> = uts
            .release
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn read_uptime(path: &str) -> Option<f64> {
    let content = fs::read_to_string(path).ok()?;
    content.split_whitespace().next()?.parse::<f64>().ok()
}

fn read_load_average(path: &str) -> Option<[f64; 3]> {
    let content = fs::read_to_string(path).ok()?;
    let mut parts = content.split_whitespace();
    let a = parts.next()?.parse::<f64>().ok()?;
    let b = parts.next()?.parse::<f64>().ok()?;
    let c = parts.next()?.parse::<f64>().ok()?;
    Some([a, b, c])
}

fn read_mem_info(path: &str) -> Option<(u64, u64)> {
    let content = fs::read_to_string(path).ok()?;
    let mut total = 0u64;
    let mut free = 0u64;
    for line in content.lines() {
        let mut fields = line.split_whitespace();
        let Some(key) = fields.next() else { continue };
        let Some(kb_str) = fields.next() else { continue };
        let Ok(kb) = kb_str.parse::<u64>() else { continue };
        match key {
            "MemTotal:" => total = kb.saturating_mul(1024),
            "MemAvailable:" => free = kb.saturating_mul(1024),
            _ => {}
        }
    }
    Some((total, free))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Capacity {
    pub used: u64,
    pub total: u64,
    pub known: bool,
}

impl Capacity {
    pub fn percent(self) -> Option<f64> {
        if !self.known {
            return None;
        }
        if self.total == 0 {
            return Some(0.0);
        }
        Some((self.used as f64 / self.total as f64 * 100.0).min(100.0))
    }
}

#[derive(Debug, Clone, Default)]
pub struct Resources {
    pub system_cpu: f64,
    pub process_cpu: f64,
    pub has_cpu: bool,
    pub memory: Capacity,
    pub swap: Capacity,
    pub disk: Capacity,
    pub os: String,
    pub sampled: Duration,
}

fn cpu_ticks() -> Option<(u64, u64)> {
    let content = fs::read_to_string("/proc/stat").ok()?;
    let first = content.lines().next()?;
    let mut fields = first.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let mut total = 0u64;
    let mut idle = 0u64;
    let mut count = 0usize;
    for (idx, field) in fields.enumerate() {
        let val = field.parse::<u64>().ok()?;
        total = total.saturating_add(val);
        if idx == 3 || idx == 4 {
            idle = idle.saturating_add(val);
        }
        count += 1;
    }
    if count < 4 {
        return None;
    }
    Some((idle, total))
}

fn process_cpu() -> Duration {
    #[cfg(unix)]
    {
        // SAFETY: getrusage initializes rusage when returning 0.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: usage is a valid mutable rusage pointer.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
            return Duration::ZERO;
        }
        let user = Duration::new(
            usage.ru_utime.tv_sec.max(0) as u64,
            usage.ru_utime.tv_usec.max(0) as u32 * 1000,
        );
        let sys = Duration::new(
            usage.ru_stime.tv_sec.max(0) as u64,
            usage.ru_stime.tv_usec.max(0) as u32 * 1000,
        );
        user + sys
    }
    #[cfg(not(unix))]
    {
        Duration::ZERO
    }
}

pub async fn sample_resources(root: &Path, window: Duration) -> Resources {
    let started = Instant::now();
    let before_cpu = cpu_ticks();
    let before_proc = process_cpu();
    tokio::time::sleep(window).await;
    let after_cpu = cpu_ticks();
    let elapsed = started.elapsed();

    let (memory, swap) = read_memory_and_swap("/proc/meminfo");
    let mut res = Resources {
        os: os_release(),
        disk: disk_usage(root),
        memory,
        swap,
        ..Default::default()
    };

    if let (Some((idle_b, total_b)), Some((idle_a, total_a))) = (before_cpu, after_cpu)
        && total_a > total_b
    {
        let total_delta = (total_a - total_b) as f64;
        let idle_delta = (idle_a.saturating_sub(idle_b)) as f64;
        let busy = (total_delta - idle_delta) / total_delta;
        res.system_cpu = (busy * 100.0).clamp(0.0, 100.0);
        res.has_cpu = true;
    }

    if !elapsed.is_zero() {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .max(1) as f64;
        let proc_delta = process_cpu().saturating_sub(before_proc);
        res.process_cpu = (proc_delta.as_secs_f64() / elapsed.as_secs_f64() / cores * 100.0)
            .clamp(0.0, 100.0);
    }

    res.sampled = started.elapsed();
    res
}

fn read_memory_and_swap(path: &str) -> (Capacity, Capacity) {
    let Ok(content) = fs::read_to_string(path) else {
        return (Capacity::default(), Capacity::default());
    };
    let mut mem_total = None;
    let mut mem_avail = None;
    let mut mem_free = 0u64;
    let mut swap_total = None;
    let mut swap_free = 0u64;

    for line in content.lines() {
        let mut fields = line.split_whitespace();
        let Some(key) = fields.next() else { continue };
        let Some(val_str) = fields.next() else { continue };
        let Ok(kb) = val_str.parse::<u64>() else { continue };
        let bytes = kb.saturating_mul(1024);
        match key.trim_end_matches(':') {
            "MemTotal" => mem_total = Some(bytes),
            "MemAvailable" => mem_avail = Some(bytes),
            "MemFree" => mem_free = bytes,
            "SwapTotal" => swap_total = Some(bytes),
            "SwapFree" => swap_free = bytes,
            _ => {}
        }
    }

    let memory = match mem_total {
        Some(total) => {
            let avail = mem_avail.unwrap_or(mem_free);
            Capacity {
                used: total.saturating_sub(avail.min(total)),
                total,
                known: true,
            }
        }
        None => Capacity::default(),
    };

    let swap = match swap_total {
        Some(total) => Capacity {
            used: total.saturating_sub(swap_free.min(total)),
            total,
            known: true,
        },
        None => Capacity::default(),
    };

    (memory, swap)
}

fn disk_usage(path: &Path) -> Capacity {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let Ok(cpath) = CString::new(path.as_os_str().as_bytes()) else {
            return Capacity::default();
        };
        // SAFETY: statvfs initializes stat when returning 0.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: cpath is nul-terminated and stat is valid mutable storage.
        if unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) } != 0 {
            return Capacity::default();
        }
        let block_size = if stat.f_frsize > 0 {
            stat.f_frsize
        } else {
            stat.f_bsize
        };
        let total = (stat.f_blocks as u64).saturating_mul(block_size);
        let free = (stat.f_bfree as u64).saturating_mul(block_size);
        if total == 0 {
            return Capacity::default();
        }
        Capacity {
            used: total.saturating_sub(free.min(total)),
            total,
            known: true,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Capacity::default()
    }
}

fn os_release() -> String {
    if let Ok(content) = fs::read_to_string("/etc/os-release") {
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("PRETTY_NAME=") {
                let trimmed = rest.trim().trim_matches(|c| c == '"' || c == '\'');
                if !trimmed.is_empty() {
                    return trimmed.to_owned();
                }
            }
        }
    }
    std::env::consts::OS.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_uptime_and_bytes() {
        assert_eq!(format_uptime_secs(31 * 60 + 9), "00:31:09");
        assert_eq!(
            format_uptime_secs(2 * 86400 + 3 * 3600 + 4 * 60 + 5),
            "2天 03:04:05"
        );
        assert_eq!(format_uptime_secs(-1), "--");

        assert_eq!(format_bytes(12), "12 B");
        assert_eq!(format_bytes(5 << 10), "5.0 KB");
        assert_eq!(format_bytes(298_520_000), "284.7 MB");
        assert_eq!(format_bytes(5 << 30), "5.00 GB");
    }
}
