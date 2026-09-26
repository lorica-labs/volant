// SPDX-License-Identifier: GPL-3.0-or-later
//! The `hardware` collector, restricted to the processor and memory facts `LinuxHardware` reads
//! from `/proc/cpuinfo`, `/proc/meminfo` and the scheduler, counted by its rules, quirks
//! included. Mounts, devices, LVM, DMI and uptime are left out: those keys are absent.
//!
//! When one of the reference's reads raises, its fact collector drops every hardware fact and the
//! module goes on; the native does the same and writes none.

use serde_json::{Map, Value};

use super::{Root, py_int, py_split, py_strip};

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

fn set(ids: &mut Ids, id: &Option<String>, value: i64) {
    match ids.iter_mut().find(|(key, _)| key == id) {
        Some(entry) => entry.1 = value,
        None => ids.push((id.clone(), value)),
    }
}

fn has(ids: &Ids, id: &Option<String>) -> bool {
    ids.iter().any(|(key, _)| key == id)
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
            if !has(&sockets, &physid) {
                set(&mut sockets, &physid, 1);
            }
        } else if key == "core id" {
            coreid = Some(val.to_string());
            // The reference looks the core up among the sockets, not the cores.
            if !has(&sockets, &coreid) {
                set(&mut cores, &coreid, 1);
            }
        } else if key == "cpu cores" {
            set(&mut sockets, &physid, py_int(val, 10)?);
        } else if key == "siblings" {
            set(&mut cores, &coreid, py_int(val, 10)?);
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
mod tests {
    use serde_json::json;

    use super::super::tests::FakeRoot;
    use super::*;

    /// One processor block of `/proc/cpuinfo`, the fields the counting reads and a few it skips,
    /// shaped like the x86 guests measured (no serial numbers, flags cut short).
    fn block(processor: u32, physical: u32, siblings: u32, core: u32, cores: u32) -> String {
        format!(
            "processor\t: {processor}\nvendor_id\t: GenuineIntel\ncpu family\t: 6\nmodel\t\t: 63\n\
             model name\t: Intel(R) Xeon(R) CPU E5-2683 v3 @ 2.00GHz\nstepping\t: 2\n\
             cpu MHz\t\t: 1999.997\ncache size\t: 16384 KB\nphysical id\t: {physical}\n\
             siblings\t: {siblings}\ncore id\t\t: {core}\ncpu cores\t: {cores}\napicid\t\t: {processor}\n\
             fpu\t\t: yes\nflags\t\t: fpu vme de pse tsc msr pae\nbogomips\t: 3999.99\n\
             power management:\n\n"
        )
    }

    fn counts(facts: &Map<String, Value>) -> Value {
        json!([
            facts["processor_count"],
            facts["processor_cores"],
            facts["processor_threads_per_core"],
            facts["processor_vcpus"],
            facts["processor_nproc"],
        ])
    }

    fn cpuinfo(name: &str, blocks: &[String]) -> Map<String, Value> {
        let fake = FakeRoot::new(name);
        fake.write("/proc/cpuinfo", &blocks.concat())
            .write("/proc/meminfo", "MemTotal: 2048 kB\n");
        collect_with(&fake.root(), "x86_64", Some(4))
    }

    /// Four vCPUs as `target` shows them: one socket, four cores, one thread each. The same four
    /// as four one-core sockets, as a hypervisor may present them. Two sockets of two
    /// hyperthreaded cores.
    ///
    /// What would make this red: `processor_vcpus` counted from `siblings` (1 on the four
    /// sockets, 4 on the hyperthreaded pair of sockets, where the reference says 4 and 8), the
    /// socket count taken from the processor count, or `processor_nproc` from the processor
    /// lines rather than the scheduler.
    #[test]
    fn processors_are_counted_by_the_reference_s_rules() {
        let target: Vec<String> = (0..4).map(|n| block(n, 0, 4, n, 4)).collect();
        let facts = cpuinfo("cpu-target", &target);
        assert_eq!(counts(&facts), json!([1, 4, 1, 4, 4]));
        assert_eq!(
            facts["processor"],
            json!([
                "0",
                "GenuineIntel",
                "Intel(R) Xeon(R) CPU E5-2683 v3 @ 2.00GHz",
                "1",
                "GenuineIntel",
                "Intel(R) Xeon(R) CPU E5-2683 v3 @ 2.00GHz",
                "2",
                "GenuineIntel",
                "Intel(R) Xeon(R) CPU E5-2683 v3 @ 2.00GHz",
                "3",
                "GenuineIntel",
                "Intel(R) Xeon(R) CPU E5-2683 v3 @ 2.00GHz",
            ])
        );
        assert_eq!(
            facts["flags"],
            json!(["fpu", "vme", "de", "pse", "tsc", "msr", "pae"])
        );

        let sockets: Vec<String> = (0..4).map(|n| block(n, n, 1, 0, 1)).collect();
        assert_eq!(
            counts(&cpuinfo("cpu-sockets", &sockets)),
            json!([4, 1, 1, 4, 4])
        );

        let hyperthreaded: Vec<String> =
            (0..8).map(|n| block(n, n / 4, 4, (n / 2) % 2, 2)).collect();
        assert_eq!(
            counts(&cpuinfo("cpu-ht", &hyperthreaded)),
            json!([2, 2, 2, 8, 4])
        );
    }

    /// The reference's quirks, kept: a core id looked up among the sockets, `cpu cores` read
    /// before any `physical id` filed under the integer 0, half rounded to even, ARM counted by
    /// its `processor` lines, s390x from its own fields, Xen paravirt without `vme` counted flat.
    #[test]
    fn the_reference_s_quirks_are_kept() {
        let fake = FakeRoot::new("cpu-quirks");
        let root = fake.root();
        // No `physical id`: `cpu cores` goes to the integer key; `siblings` 1 over 2 cores
        // rounds half to even, to 0.
        fake.write(
            "/proc/cpuinfo",
            "processor : 0\ncpu cores : 2\ncore id : 0\nsiblings : 1\n",
        );
        let facts = collect_with(&root, "x86_64", Some(1));
        assert_eq!(counts(&facts), json!([1, 2, 0, 0, 1]));

        // ARM: `processor` and `Processor` both listed, only `processor` lines counted.
        fake.write(
            "/proc/cpuinfo",
            "Processor : ARMv7\nprocessor : 0\nprocessor : 1\nFeatures : half thumb\n",
        );
        let facts = collect_with(&root, "armv7l", Some(2));
        assert_eq!(facts["processor"], json!(["ARMv7", "0", "1"]));
        assert_eq!(counts(&facts), json!([2, 1, 1, 2, 2]));
        assert!(!facts.contains_key("flags"));

        fake.write(
            "/proc/cpuinfo",
            "vendor_id : IBM/S390\n# processors : 4\nmax thread id : 1\n",
        );
        let facts = collect_with(&root, "s390x", Some(4));
        assert_eq!(counts(&facts), json!([1, 2, 2, 4, 4]));
        fake.write("/proc/cpuinfo", "vendor_id : IBM/S390\n# processors : 4\n");
        assert_eq!(
            collect_with(&root, "s390x", Some(4)),
            Map::new(),
            "no max thread id: the division raises"
        );

        fake.mkdir("/proc/xen").write(
            "/proc/cpuinfo",
            "processor : 0\nvendor_id : x\nmodel name : y\nflags : fpu de\n\
             processor : 1\nvendor_id : x\nmodel name : y\nflags : fpu de\n",
        );
        let facts = collect_with(&root, "x86_64", Some(2));
        assert_eq!(counts(&facts), json!([2, 2, 1, 2, 2]));
    }

    /// Anything the reference's reads would raise on drops every hardware fact, memory
    /// included; an unreadable file only leaves its own facts out.
    #[test]
    fn a_read_the_reference_raises_on_drops_the_collector() {
        let fake = FakeRoot::new("cpu-raise");
        let root = fake.root();
        fake.write("/proc/meminfo", "MemTotal: 2048 kB\n");
        assert_eq!(
            collect_with(&root, "x86_64", Some(1))["memtotal_mb"],
            2,
            "no cpuinfo: memory still written"
        );
        assert!(!collect_with(&root, "x86_64", Some(1)).contains_key("processor_nproc"));
        fake.write("/proc/cpuinfo", "processor : 0\ncpu cores : many\n");
        assert_eq!(collect_with(&root, "x86_64", Some(1)), Map::new());
        fake.write("/proc/cpuinfo", "processor : 0\n");
        assert_eq!(
            collect_with(&root, "x86_64", None),
            Map::new(),
            "no affinity"
        );
        fake.write("/proc/meminfo", "MemTotal: lots kB\n");
        assert_eq!(collect_with(&root, "x86_64", Some(1)), Map::new());
    }

    /// Memory in MiB, floored; the dict's missing members as `null`.
    #[test]
    fn memory_is_read_in_mebibytes() {
        let fake = FakeRoot::new("memory");
        fake.write(
            "/proc/meminfo",
            "MemTotal:        4015200 kB\nMemFree:          812344 kB\nMemAvailable:    3100000 kB\n\
             Buffers:          102400 kB\nCached:          2048000 kB\nSwapCached:            0 kB\n\
             SwapTotal:       2097148 kB\nSwapFree:        2097148 kB\n",
        );
        let facts = memory(&fake.root()).unwrap();
        assert_eq!(facts["memtotal_mb"], 3921);
        assert_eq!(facts["memfree_mb"], 793);
        assert_eq!(facts["swaptotal_mb"], 2047);
        assert_eq!(facts["swapfree_mb"], 2047);
        assert_eq!(
            facts["memory_mb"],
            json!({
                "real": {"total": 3921, "used": 3128, "free": 793},
                "nocache": {"free": 2893, "used": 1028},
                "swap": {"total": 2047, "free": 2047, "used": 0, "cached": 0},
            })
        );
        fake.write("/proc/meminfo", "MemTotal: 1024 kB\n");
        assert_eq!(
            memory(&fake.root()).unwrap()["memory_mb"],
            json!({
                "real": {"total": 1, "used": null, "free": null},
                "nocache": {"free": null, "used": null},
                "swap": {"total": null, "free": null, "used": null, "cached": null},
            })
        );
    }

    /// A kernel that refuses a mask narrower than 4096 processors is asked again with a wider
    /// one, and every set bit is counted, the high ones included.
    ///
    /// What would make this red: the mask kept at one `cpu_set_t`, which drops every hardware
    /// key on such a host where the reference reports them; or any other error retried.
    #[test]
    fn the_affinity_mask_grows_until_the_kernel_takes_it() {
        const BITS: usize = libc::c_ulong::BITS as usize;
        let mut asked = Vec::new();
        let count = grown_mask(|mask| {
            asked.push(mask.len() * BITS);
            if mask.len() * BITS < 4096 {
                return Err(libc::EINVAL);
            }
            mask[0] = 0b1011;
            mask[3000 / BITS] |= 1 << (3000 % BITS);
            Ok(())
        });
        assert_eq!(count, Some(4));
        assert_eq!(asked, [1024, 2048, 4096]);
        assert_eq!(grown_mask(|_| Err(libc::EPERM)), None, "EPERM raises");
        assert_eq!(grown_mask(|_| Err(libc::EINVAL)), None, "bounded");
    }

    #[test]
    fn the_scheduler_answers_on_this_machine() {
        assert!(affinity().is_some_and(|n| n > 0));
    }
}
