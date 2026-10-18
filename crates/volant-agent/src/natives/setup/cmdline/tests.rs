// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::json;

use super::super::tests::{FakeRoot, probe};
use super::*;

/// A kernel command line with a repeated key, a bare flag, quotes kept by the non-POSIX split
/// and a quoted word that ends where its quote closes.
#[test]
fn the_kernel_command_line_is_split_like_the_reference() {
    let fake = FakeRoot::new("cmdline");
    let (root, probe) = (fake.root(), probe());
    assert!(
        collect(&fake.host(&root, &probe)).is_empty(),
        "no file, no keys"
    );
    fake.write(
        "/proc/cmdline",
        "BOOT_IMAGE=/vmlinuz-6.8.0 root=/dev/mapper/vg-root ro console=tty0 console=ttyS0,115200 x=\"a b\" \"q r\"s\n",
    );
    let facts = collect(&fake.host(&root, &probe));
    assert_eq!(
        facts["cmdline"],
        json!({"BOOT_IMAGE": "/vmlinuz-6.8.0", "root": "/dev/mapper/vg-root", "ro": true,
               "console": "ttyS0,115200", "x": "\"a", "b\"": true, "\"q r\"": true, "s": true})
    );
    assert_eq!(
        facts["proc_cmdline"]["console"],
        json!(["tty0", "ttyS0,115200"])
    );
    fake.write("/proc/cmdline", "a=1 \"unclosed\n");
    let facts = collect(&fake.host(&root, &probe));
    assert_eq!(facts["cmdline"], json!({}));
    assert_eq!(facts["proc_cmdline"], json!({}));
}
