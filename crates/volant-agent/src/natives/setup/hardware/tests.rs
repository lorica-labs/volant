// SPDX-License-Identifier: GPL-3.0-or-later

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

/// Processors, quirks, dropped reads and memory follow the reference's rules.
#[test]
#[expect(clippy::too_many_lines)]
fn processors_memory_and_quirks_follow_the_reference() {
    {
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
    {
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
    {
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
    {
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
}

/// A kernel that refuses a mask narrower than 4096 processors is asked again with a wider one,
/// and every set bit is counted, the high ones included.
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
#[cfg(target_os = "linux")]
fn the_scheduler_answers_on_this_machine() {
    assert!(affinity().is_some_and(|n| n > 0));
}
