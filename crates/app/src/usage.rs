//! What the machine and Viewfinder are using of the CPU, RAM and NVIDIA GPU, read once a second
//! while the window is on screen. The GPU readings are the counters Task Manager reads.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::FILETIME;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW,
};
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, GetSystemTimes};
use windows::core::{PCWSTR, w};

use crate::worker::{Hub, lock, log};

pub(crate) static LATEST: Mutex<Option<Usage>> = Mutex::new(None);

const GIB: f64 = (1u64 << 30) as f64;
const NVIDIA: u32 = 0x10de;

pub(crate) struct Usage {
    /// Percent of all cores, for the machine and for Viewfinder.
    pub(crate) cpu: f32,
    pub(crate) cpu_own: f32,
    /// Bytes in use of the total, and Viewfinder's own.
    pub(crate) ram: u64,
    pub(crate) ram_total: u64,
    pub(crate) ram_own: u64,
    pub(crate) gpu: Option<Gpu>,
}

pub(crate) struct Gpu {
    /// The busiest engine's percent, for the whole GPU and for Viewfinder.
    pub(crate) busy: f32,
    pub(crate) own: f32,
    /// Viewfinder's dedicated video memory in bytes.
    pub(crate) vram: u64,
}

/// One meter's words and how full it is, as shares of the whole.
pub(crate) struct Meter {
    pub(crate) total: String,
    pub(crate) own: String,
    pub(crate) fill: f32,
    pub(crate) own_fill: f32,
}

impl Usage {
    pub(crate) fn meters(&self) -> Vec<Meter> {
        let share = |n: u64| n as f32 / self.ram_total.max(1) as f32;
        let mut meters = vec![
            Meter {
                total: format!("CPU {:.0}%", self.cpu),
                own: format!("Viewfinder {:.0}%", self.cpu_own),
                fill: self.cpu / 100.0,
                own_fill: self.cpu_own / 100.0,
            },
            Meter {
                total: format!(
                    "RAM {:.1} of {:.0} GB",
                    self.ram as f64 / GIB,
                    self.ram_total as f64 / GIB
                ),
                own: format!("Viewfinder {}", size(self.ram_own)),
                fill: share(self.ram),
                own_fill: share(self.ram_own),
            },
        ];
        meters.extend(self.gpu.as_ref().map(|g| Meter {
            total: format!("GPU {:.0}%", g.busy),
            own: format!("Viewfinder {:.0}%, {}", g.own, size(g.vram)),
            fill: g.busy / 100.0,
            own_fill: g.own / 100.0,
        }));
        meters
    }
}

fn size(bytes: u64) -> String {
    let gb = bytes as f64 / GIB;
    if gb < 1.0 {
        format!("{:.0} MB", gb * 1024.0)
    } else {
        format!("{gb:.1} GB")
    }
}

/// The NVIDIA adapter's name, and its LUID as the GPU counters spell it.
pub(crate) struct Card {
    pub(crate) name: String,
    luid: String,
}

/// The GPU the CUDA effects run on.
#[expect(unsafe_code, reason = "listing adapters is a DXGI call")]
pub(crate) fn nvidia() -> Option<&'static Card> {
    static CARD: OnceLock<Option<Card>> = OnceLock::new();
    CARD.get_or_init(|| {
        // SAFETY: takes no arguments.
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
        // SAFETY: EnumAdapters1 fails past the last adapter, which ends the list.
        (0..)
            .map_while(|i| unsafe { factory.EnumAdapters1(i) }.ok())
            .find_map(|adapter| {
                // SAFETY: the adapter came from the factory just above.
                let d = unsafe { adapter.GetDesc1() }.ok()?;
                let name = d.Description.split(|&c| c == 0).next().unwrap_or_default();
                (d.VendorId == NVIDIA).then(|| Card {
                    name: String::from_utf16_lossy(name),
                    luid: format!(
                        "luid_0x{:08x}_0x{:08x}",
                        d.AdapterLuid.HighPart, d.AdapterLuid.LowPart
                    ),
                })
            })
    })
    .as_ref()
}

/// Reads once a second while the window is on screen.
pub(crate) fn spawn(hub: Arc<Hub>) {
    thread::Builder::new()
        .name("usage".into())
        .spawn(move || run(&hub))
        .ok();
}

#[expect(
    clippy::disallowed_methods,
    reason = "a thread of its own that only reads once a second"
)]
fn run(hub: &Hub) {
    let luid = nvidia().map(|c| c.luid.as_str());
    let pdh = Pdh::open();
    let pid = format!("pid_{}_", std::process::id());
    let mut last = times();
    let mut logged = false;
    loop {
        thread::sleep(Duration::from_secs(1));
        if !hub.preview_wanted.load(Ordering::Relaxed) {
            continue;
        }
        let start = Instant::now();
        let now = times();
        let (cpu, cpu_own) = cpu_percent(last, now);
        last = now;
        let (ram, ram_total) = memory();
        let gpu = pdh.as_ref().map(|q| q.read(luid, &pid));
        *lock(&LATEST) = Some(Usage {
            cpu,
            cpu_own,
            ram,
            ram_total,
            ram_own: own_memory(),
            gpu,
        });
        if !logged {
            logged = true;
            log(&format!(
                "usage: a reading takes {:.1} ms, GPU counters {}",
                start.elapsed().as_secs_f64() * 1e3,
                if pdh.is_some() { "on" } else { "off" }
            ));
        }
    }
}

/// The machine's idle, kernel and user time, and this process's, in 100 ns ticks.
#[derive(Clone, Copy, Default)]
struct Times {
    idle: u64,
    kernel: u64,
    user: u64,
    own: u64,
}

#[expect(unsafe_code, reason = "CPU times are a Win32 call")]
fn times() -> Times {
    let ticks = |t: FILETIME| u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime);
    let [
        mut idle,
        mut kernel,
        mut user,
        mut created,
        mut ended,
        mut own_kernel,
        mut own_user,
    ] = [FILETIME::default(); 7];
    // SAFETY: each pointer is to a local FILETIME.
    let ok = unsafe {
        GetSystemTimes(
            Some(&raw mut idle),
            Some(&raw mut kernel),
            Some(&raw mut user),
        )
        .is_ok()
            && GetProcessTimes(
                GetCurrentProcess(),
                &raw mut created,
                &raw mut ended,
                &raw mut own_kernel,
                &raw mut own_user,
            )
            .is_ok()
    };
    if !ok {
        return Times::default();
    }
    Times {
        idle: ticks(idle),
        kernel: ticks(kernel),
        user: ticks(user),
        own: ticks(own_kernel) + ticks(own_user),
    }
}

/// The machine's and this process's share of all cores. Kernel time includes idle time.
fn cpu_percent(before: Times, after: Times) -> (f32, f32) {
    let all = (after.kernel + after.user).saturating_sub(before.kernel + before.user);
    if all == 0 {
        return (0.0, 0.0);
    }
    let busy = all.saturating_sub(after.idle.saturating_sub(before.idle));
    let own = after.own.saturating_sub(before.own);
    let percent = |n: u64| (n as f64 * 100.0 / all as f64).min(100.0) as f32;
    (percent(busy), percent(own))
}

/// Physical memory in use and in all, in bytes.
#[expect(unsafe_code, reason = "memory status is a Win32 call")]
fn memory() -> (u64, u64) {
    let mut m = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: dwLength is set, as the call requires.
    if unsafe { GlobalMemoryStatusEx(&raw mut m) }.is_err() {
        return (0, 0);
    }
    (m.ullTotalPhys - m.ullAvailPhys, m.ullTotalPhys)
}

/// The private working set, which is Task Manager's Memory column: RAM only this process
/// holds, leaving out shared DLL pages and anything paged out.
#[expect(unsafe_code, reason = "process memory is a Win32 call")]
fn own_memory() -> u64 {
    let mut c = PROCESS_MEMORY_COUNTERS_EX2::default();
    let cb = size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32;
    let p = std::ptr::from_mut(&mut c).cast::<PROCESS_MEMORY_COUNTERS>();
    // SAFETY: the pointer is to a local struct `cb` bytes long, which the call fills.
    if unsafe { GetProcessMemoryInfo(GetCurrentProcess(), p, cb) }.is_err() {
        return 0;
    }
    c.PrivateWorkingSetSize as u64
}

/// The GPU counters, kept open since the engine use is a rate between two collections.
struct Pdh {
    query: PDH_HQUERY,
    engines: PDH_HCOUNTER,
    vram: PDH_HCOUNTER,
}

#[expect(unsafe_code, reason = "performance counters are a Win32 call")]
impl Pdh {
    fn open() -> Option<Self> {
        let mut q = Self {
            query: PDH_HQUERY::default(),
            engines: PDH_HCOUNTER::default(),
            vram: PDH_HCOUNTER::default(),
        };
        // SAFETY: a null source means live data, and the handle goes into `q`.
        if unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &raw mut q.query) } != 0 {
            return None;
        }
        // SAFETY: the query is open, and the paths are static wide strings. `q` closes the
        // query when dropped, on every way out from here.
        let ok = unsafe {
            PdhAddEnglishCounterW(
                q.query,
                w!("\\GPU Engine(*)\\Utilization Percentage"),
                0,
                &raw mut q.engines,
            ) == 0
                && PdhAddEnglishCounterW(
                    q.query,
                    w!("\\GPU Process Memory(*)\\Dedicated Usage"),
                    0,
                    &raw mut q.vram,
                ) == 0
                && PdhCollectQueryData(q.query) == 0
        };
        ok.then_some(q)
    }

    fn read(&self, luid: Option<&str>, pid: &str) -> Gpu {
        // SAFETY: the query is open. A failed collection leaves the arrays empty below.
        unsafe { PdhCollectQueryData(self.query) };
        let (busy, own) = busiest(&items(self.engines), luid, pid);
        let vram = items(self.vram)
            .iter()
            .filter(|(name, _)| name.starts_with(pid) && luid.is_none_or(|l| name.contains(l)))
            .map(|(_, bytes)| *bytes)
            .sum::<f64>();
        Gpu {
            busy,
            own,
            vram: vram as u64,
        }
    }
}

impl Drop for Pdh {
    #[expect(unsafe_code, reason = "performance counters are a Win32 call")]
    fn drop(&mut self) {
        // SAFETY: the query was opened in `open` and is closed only here.
        unsafe { PdhCloseQuery(self.query) };
    }
}

/// Each instance of a wildcard counter, its name in lower case, with its value.
#[expect(unsafe_code, reason = "performance counters are a Win32 call")]
fn items(counter: PDH_HCOUNTER) -> Vec<(String, f64)> {
    let (mut bytes, mut count) = (0u32, 0u32);
    // SAFETY: with no buffer the call only reports the size it needs.
    let sized = unsafe {
        PdhGetFormattedCounterArrayW(
            counter,
            PDH_FMT_DOUBLE,
            &raw mut bytes,
            &raw mut count,
            None,
        )
    };
    if sized != PDH_MORE_DATA {
        return Vec::new();
    }
    let n = (bytes as usize).div_ceil(size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>());
    let mut buf = vec![PDH_FMT_COUNTERVALUE_ITEM_W::default(); n];
    // SAFETY: the buffer holds at least `bytes` bytes, aligned for the items, and the names
    // the items point to live in it too.
    let read = unsafe {
        PdhGetFormattedCounterArrayW(
            counter,
            PDH_FMT_DOUBLE,
            &raw mut bytes,
            &raw mut count,
            Some(buf.as_mut_ptr()),
        )
    };
    if read != 0 {
        return Vec::new();
    }
    buf.iter()
        .take(count as usize)
        // Valid or new data. Anything else is an instance with no reading yet.
        .filter(|item| item.FmtValue.CStatus <= 1)
        .map(|item| {
            // SAFETY: the name is a null terminated string inside `buf`, and a double was
            // asked for.
            let (name, value) =
                unsafe { (item.szName.to_string(), item.FmtValue.Anonymous.doubleValue) };
            (name.unwrap_or_default().to_ascii_lowercase(), value)
        })
        .collect()
}

/// Task Manager's sums: each engine's use added up over the processes using it, and the busiest
/// engine is the GPU's, for the whole GPU and for this process alone.
fn busiest(engines: &[(String, f64)], luid: Option<&str>, pid: &str) -> (f32, f32) {
    let mut sums: HashMap<&str, (f64, f64)> = HashMap::new();
    for (name, value) in engines {
        if luid.is_some_and(|l| !name.contains(l)) {
            continue;
        }
        let Some((_, engine)) = name.split_once("_luid_") else {
            continue;
        };
        let sum = sums.entry(engine).or_default();
        sum.0 += value;
        if name.starts_with(pid) {
            sum.1 += value;
        }
    }
    let most = |pick: fn(&(f64, f64)) -> f64| {
        sums.values().map(pick).fold(0.0, f64::max).min(100.0) as f32
    };
    (most(|s| s.0), most(|s| s.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_is_a_share_of_all_cores() {
        let before = Times::default();
        // 4 s of core time went by, 3 s of it idle, and this process used 0.2 s.
        let after = Times {
            idle: 30_000_000,
            kernel: 32_000_000,
            user: 8_000_000,
            own: 2_000_000,
        };
        assert_eq!(cpu_percent(before, after), (25.0, 5.0));
        assert_eq!(cpu_percent(after, after), (0.0, 0.0));
    }

    #[test]
    fn meters_read_like_task_manager() {
        let usage = Usage {
            cpu: 23.4,
            cpu_own: 4.2,
            ram: 9_771_000_000,
            ram_total: 17_179_869_184,
            ram_own: 325_058_560,
            gpu: Some(Gpu {
                busy: 31.0,
                own: 12.0,
                vram: 1_288_490_189,
            }),
        };
        let words: Vec<String> = usage
            .meters()
            .iter()
            .map(|m| format!("{}, {}", m.total, m.own))
            .collect();
        assert_eq!(
            words,
            [
                "CPU 23%, Viewfinder 4%",
                "RAM 9.1 of 16 GB, Viewfinder 310 MB",
                "GPU 31%, Viewfinder 12%, 1.2 GB",
            ]
        );
    }

    #[test]
    fn gpu_is_the_busiest_engine_on_the_card() {
        let me = "pid_7_";
        let nv = "luid_0x00000000_0x0000c2e1";
        let engine = |pid: u32, luid: &str, eng: &str, v: f64| {
            (format!("pid_{pid}_{luid}_phys_0_eng_{eng}"), v)
        };
        let engines = [
            engine(7, nv, "0_engtype_3d", 10.0),
            engine(9, nv, "0_engtype_3d", 25.0),
            engine(7, nv, "3_engtype_compute_0", 30.0),
            engine(9, "luid_0x00000000_0x0000aaaa", "0_engtype_3d", 90.0),
        ];
        assert_eq!(busiest(&engines, Some(nv), me), (35.0, 30.0));
        assert_eq!(busiest(&engines, None, me), (90.0, 30.0));
    }
}
