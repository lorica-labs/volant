// SPDX-License-Identifier: GPL-3.0-or-later
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    watch_head_and_branch_ref();
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let sha = Command::new("git")
        .args(["rev-parse", "--short=9", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map_or_else(
            || "unknown".to_string(),
            |o| String::from_utf8_lossy(&o.stdout).trim().to_string(),
        );

    let epoch = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64)
        });

    println!("cargo:rustc-env=VOLANT_GIT_SHA={sha}");
    println!("cargo:rustc-env=VOLANT_BUILD_DATE={}", civil_date(epoch));
}

/// Tells cargo to rerun this script when the checked-out commit changes.
///
/// `HEAD` alone is not enough: on a fast-forward it still reads `ref: refs/heads/<branch>`
/// and never changes, while the branch ref it names moves. That ref can live as a loose
/// file under `refs/heads/`, or with no file of its own as a line in `packed-refs` (what
/// `git gc` produces). A detached `HEAD` holds the commit hash directly and has no branch
/// ref to watch, which is not an error.
fn watch_head_and_branch_ref() {
    // The crate root is `crates/volant`; the repository root is two levels up. `.git` is a
    // directory in a normal checkout, but a `gitdir: <path>` file in a worktree, whose own
    // `HEAD` and `commondir` live at that path instead.
    let dotgit = Path::new("../../.git");
    let Some(git_dir) = resolve_git_dir(dotgit) else {
        return;
    };

    let head_path = git_dir.join("HEAD");
    if head_path.is_file() {
        println!("cargo:rerun-if-changed={}", head_path.display());
    }

    let Ok(head) = fs::read_to_string(&head_path) else {
        return;
    };
    if !head.trim_end().starts_with("ref: ") {
        return; // Detached HEAD: the commit hash is in HEAD itself, already watched above.
    }

    let common_dir = resolve_common_dir(&git_dir);

    // `refs/heads` itself, never the branch's own subdirectory. Watching the directory rather
    // than the one ref file catches the ref appearing for the first time, e.g. a fast-forward
    // on a branch `git gc` had packed away - but `git pack-refs` deletes the loose files *and*
    // prunes the subdirectories that held them, keeping only `refs/heads`. Watching
    // `refs/heads/<prefix>` for a branch named `<prefix>/<name>` therefore watches a path that
    // packing removed, and this repository's branches all carry a prefix. Cargo walks the
    // subtree, so a nested ref still counts.
    let refs_heads = common_dir.join("refs/heads");
    if refs_heads.is_dir() {
        println!("cargo:rerun-if-changed={}", refs_heads.display());
    }

    let packed_refs = common_dir.join("packed-refs");
    if packed_refs.is_file() {
        println!("cargo:rerun-if-changed={}", packed_refs.display());
    }
}

/// Resolves `dotgit` (a repo's `.git`) to the directory that actually holds `HEAD`.
fn resolve_git_dir(dotgit: &Path) -> Option<PathBuf> {
    if dotgit.is_dir() {
        return Some(dotgit.to_path_buf());
    }
    let contents = fs::read_to_string(dotgit).ok()?;
    let raw = contents.trim_end().strip_prefix("gitdir: ")?;
    let path = PathBuf::from(raw);
    Some(if path.is_absolute() {
        path
    } else {
        dotgit.parent().unwrap_or(Path::new(".")).join(path)
    })
}

/// Resolves the directory refs are shared from: `git_dir` itself outside a worktree, or
/// wherever `git_dir/commondir` points to inside one.
fn resolve_common_dir(git_dir: &Path) -> PathBuf {
    match fs::read_to_string(git_dir.join("commondir")) {
        Ok(contents) => git_dir.join(contents.trim_end()),
        Err(_) => git_dir.to_path_buf(),
    }
}

/// Converts Unix seconds to a proleptic Gregorian `YYYY-MM-DD` (Howard Hinnant's algorithm).
fn civil_date(epoch: i64) -> String {
    let z = epoch.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}
