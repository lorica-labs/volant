// SPDX-License-Identifier: GPL-3.0-or-later
//! The `hardware` collector, restricted to the processor and memory facts `LinuxHardware` reads
//! from `/proc/cpuinfo`, `/proc/meminfo` and the scheduler, counted by its rules, quirks
//! included. Mounts, devices, LVM, DMI and uptime are left out: those keys are absent.
//!
//! When one of the reference's reads raises, its fact collector drops every hardware fact and the
//! module goes on; the native does the same and writes none.

use serde_json::{Map, Value};

use crate::natives::runtime::{Root, py_int, py_split, py_strip};

/// The facts, or none where the reference's collector raises.
pub fn collect(root: &Root, architecture: &str) -> Map<String, Value> {
    collect_with(root, architecture, affinity())
}

fn collect_with(root: &Root, architecture: &str, nproc: Option<usize>) -> Map<String, Value> {
    let Some(mut facts) = cpu(root, architecture, nproc) else {
        return Map::new();
    };
    let Some(memory) = memory(root) else {
        return Map::new();
    };
    facts.extend(memory);
    facts
}

/// `len(os.sched_getaffinity(0))`: the processors this process may run on, which the module
/// inherits.
#[cfg(target_os = "linux")]
fn affinity() -> Option<usize> {
    grown_mask(|mask| {
        let ok =
            unsafe { libc::sched_getaffinity(0, size_of_val(mask), mask.as_mut_ptr().cast()) } == 0;
        if ok {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
        }
    })
}

#[cfg(not(target_os = "linux"))]
fn affinity() -> Option<usize> {
    None
}

/// The bits set in the mask `get` fills, the mask doubled for as long as the kernel refuses it
/// as too small (`EINVAL`), as CPython's `sched_getaffinity` doubles it: a kernel built for more
/// than 1024 processors refuses a `cpu_set_t`. `None` for any other error, which Python raises,
/// and past 2^20 processors.
#[cfg(any(target_os = "linux", test))]
fn grown_mask(mut get: impl FnMut(&mut [libc::c_ulong]) -> Result<(), i32>) -> Option<usize> {
    const BITS: usize = libc::c_ulong::BITS as usize;
    let mut words = 1024 / BITS;
    while words * BITS <= 1 << 20 {
        let mut mask = vec![0; words];
        match get(&mut mask) {
            Ok(()) => return Some(mask.iter().map(|word| word.count_ones() as usize).sum()),
            Err(libc::EINVAL) => words *= 2,
            Err(_) => return None,
        }
    }
    None
}

/// `os.access(path, os.R_OK)`.
fn readable(root: &Root, path: &str) -> bool {
    std::fs::File::open(root.path(path)).is_ok()
}

/// A dict keyed by `physical id` or `core id`, in insertion order, where `None` is the integer 0
/// the reference starts from before any id is read.
type Ids = Vec<(Option<String>, i64)>;

fn set(ids: &mut Ids, id: Option<&str>, value: i64) {
    match ids.iter_mut().find(|(key, _)| key.as_deref() == id) {
        Some(entry) => entry.1 = value,
        None => ids.push((id.map(str::to_string), value)),
    }
}

fn has(ids: &Ids, id: Option<&str>) -> bool {
    ids.iter().any(|(key, _)| key.as_deref() == id)
}

/// `round(a / b)`: true division, then round half to even.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "processor counts, far below 2^52, divided in floats as the reference divides them"
)]
fn py_round_div(a: i64, b: i64) -> Option<i64> {
    (b != 0).then(|| (a as f64 / b as f64).round_ties_even() as i64)
}

/// `get_cpu_facts`: `None` where it raises.
#[expect(clippy::too_many_lines)]
fn cpu(root: &Root, architecture: &str, nproc: Option<usize>) -> Option<Map<String, Value>> {
    let mut facts = Map::new();
    let xen = root.exists("/proc/xen")
        || root
            .lines("/sys/hypervisor/type")
            .first()
            .is_some_and(|line| py_strip(line) == "xen");
    if !readable(root, "/proc/cpuinfo") {
        return Some(facts);
    }
    facts.insert("processor".into(), Value::Array(Vec::new()));
    let mut processor = Vec::new();
    let (mut i, mut vendor_ids, mut model_names, mut processors) = (0, 0, 0, 0);
    let (mut physid, mut coreid): (Option<String>, Option<String>) = (None, None);
    let (mut sockets, mut cores): (Ids, Ids) = (Vec::new(), Vec::new());
    let (mut zp, mut zmt) = (0, 0);
    let mut xen_paravirt = false;
    for line in root.lines("/proc/cpuinfo") {
        let (key, val) = match line.split_once(':') {
            Some((key, val)) => (py_strip(key), py_strip(val)),
            None => (py_strip(&line), ""),
        };
        if xen && key == "flags" && !val.contains("vme") {
            xen_paravirt = true;
        }
        if key == "flags" {
            facts.insert("flags".into(), py_split(val).collect::<Vec<_>>().into());
        }
        if [
            "model name",
            "Processor",
            "vendor_id",
            "cpu",
            "Vendor",
            "processor",
        ]
        .contains(&key)
        {
            processor.push(Value::from(val));
            match key {
                "vendor_id" => vendor_ids += 1,
                "model name" => model_names += 1,
                "processor" => processors += 1,
                _ => {}
            }
            i += 1;
        } else if key == "physical id" {
            physid = Some(val.to_string());
            if !has(&sockets, physid.as_deref()) {
                set(&mut sockets, physid.as_deref(), 1);
            }
        } else if key == "core id" {
            coreid = Some(val.to_string());
            // The reference looks the core up among the sockets, not the cores.
            if !has(&sockets, coreid.as_deref()) {
                set(&mut cores, coreid.as_deref(), 1);
            }
        } else if key == "cpu cores" {
            set(&mut sockets, physid.as_deref(), py_int(val, 10)?);
        } else if key == "siblings" {
            set(&mut cores, coreid.as_deref(), py_int(val, 10)?);
        } else if key == "# processors" {
            zp = py_int(val, 10)?;
        } else if key == "max thread id" {
            zmt = py_int(val, 10)? + 1;
        } else if key == "ncpus active" {
            i = py_int(val, 10)?;
        }
    }
    facts.insert("processor".into(), Value::Array(processor));
    if vendor_ids > 0 && vendor_ids == model_names {
        i = vendor_ids;
    }
    if ["armv", "aarch", "ppc"]
        .iter()
        .any(|prefix| architecture.starts_with(prefix))
    {
        i = processors;
    }
    let mut put = |key: &str, value: i64| {
        facts.insert(key.into(), value.into());
    };
    if architecture == "s390x" {
        put("processor_count", 1);
        put("processor_cores", py_round_div(zp, zmt)?);
        put("processor_threads_per_core", zmt);
        put("processor_vcpus", zp);
        put("processor_nproc", zp);
    } else if xen_paravirt {
        for key in [
            "processor_count",
            "processor_cores",
            "processor_vcpus",
            "processor_nproc",
        ] {
            put(key, i);
        }
        put("processor_threads_per_core", 1);
    } else {
        let count = if sockets.is_empty() {
            i
        } else {
            i64::try_from(sockets.len()).ok()?
        };
        let per_socket = sockets
            .first()
            .map(|(_, cores)| *cores)
            .filter(|cores| *cores != 0)
            .unwrap_or(1);
        let threads = py_round_div(
            cores.first().map_or(1, |(_, siblings)| *siblings),
            per_socket,
        )?;
        put("processor_count", count);
        put("processor_cores", per_socket);
        put("processor_threads_per_core", threads);
        put("processor_vcpus", threads * count * per_socket);
        put("processor_nproc", processors);
    }
    // Where the scheduler cannot answer, `os.sched_getaffinity` raises.
    put("processor_nproc", i64::try_from(nproc?).ok()?);
    Some(facts)
}

/// `get_memory_facts`: `None` where it raises.
fn memory(root: &Root) -> Option<Map<String, Value>> {
    const ORIGINAL: [&str; 4] = ["MemTotal", "SwapTotal", "MemFree", "SwapFree"];
    const MORE: [&str; 3] = ["Buffers", "Cached", "SwapCached"];
    let mut facts = Map::new();
    if !readable(root, "/proc/meminfo") {
        return Some(facts);
    }
    let mut stats: Vec<(String, i64)> = Vec::new();
    for line in root.lines("/proc/meminfo") {
        let (key, rest) = match line.split_once(':') {
            Some((key, rest)) => (key, Some(rest)),
            None => (line.as_str(), None),
        };
        if !ORIGINAL.contains(&key) && !MORE.contains(&key) {
            continue;
        }
        let first = py_strip(rest?).split(' ').next().unwrap_or_default();
        let mb = py_int(first, 10)?.div_euclid(1024);
        let lower = key.to_lowercase();
        if ORIGINAL.contains(&key) {
            facts.insert(format!("{lower}_mb"), mb.into());
        }
        match stats.iter_mut().find(|(name, _)| *name == lower) {
            Some(entry) => entry.1 = mb,
            None => stats.push((lower, mb)),
        }
    }
    let get = |name: &str| stats.iter().find(|(key, _)| key == name).map(|(_, v)| *v);
    let both = |a: Option<i64>, b: Option<i64>| a.zip(b);
    let real_used = both(get("memtotal"), get("memfree")).map(|(t, f)| t - f);
    let nocache_free = get("cached")
        .zip(get("memfree"))
        .zip(get("buffers"))
        .map(|((c, f), b)| c + f + b);
    let nocache_used = both(get("memtotal"), nocache_free).map(|(t, f)| t - f);
    let swap_used = both(get("swaptotal"), get("swapfree")).map(|(t, f)| t - f);
    facts.insert(
        "memory_mb".into(),
        serde_json::json!({
            "real": {"total": get("memtotal"), "used": real_used, "free": get("memfree")},
            "nocache": {"free": nocache_free, "used": nocache_used},
            "swap": {
                "total": get("swaptotal"),
                "free": get("swapfree"),
                "used": swap_used,
                "cached": get("swapcached"),
            },
        }),
    );
    Some(facts)
}

#[cfg(test)]
mod tests;
