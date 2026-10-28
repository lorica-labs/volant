# SPDX-License-Identifier: GPL-3.0-or-later
"""Writes manifest.toml: the Python builds and ansible-core wheels a managed runtime installs.

Run by `just runtime-lock` with any Python 3.12+ and network access. The Python builds come from a
python-build-standalone release, pinned by its SHA256SUMS. The wheels are resolved per platform by
the pip of one of those builds (downloaded and checked first), which records each wheel's URL and
sha256 without installing anything.
"""

import hashlib
import json
import os
import platform
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
from pathlib import Path

RELEASE = "20261013"
PYTHON = "3.13.16"
ANSIBLE_CORE = "2.19.12"
BASE = f"https://github.com/astral-sh/python-build-standalone/releases/download/{RELEASE}"

# The pip platform tags of each triple, lowest system requirement first so pip prefers the wheel
# that runs on the most hosts.
PLATFORMS = {
    "x86_64-unknown-linux-gnu": ["manylinux2014_x86_64", "manylinux_2_17_x86_64", "manylinux_2_28_x86_64"],
    "aarch64-unknown-linux-gnu": ["manylinux2014_aarch64", "manylinux_2_17_aarch64", "manylinux_2_28_aarch64"],
    "x86_64-unknown-linux-musl": ["musllinux_1_1_x86_64", "musllinux_1_2_x86_64"],
    "aarch64-unknown-linux-musl": ["musllinux_1_1_aarch64", "musllinux_1_2_aarch64"],
    "x86_64-apple-darwin": ["macosx_10_13_x86_64"],
    "aarch64-apple-darwin": ["macosx_11_0_arm64"],
}


# The optional sets a runtime can add, each the packages it installs on top of ansible-core:
# `volant runtime install --extras NAME`.
EXTRAS = {
    "windows": ["pywinrm", "pypsrp"],
    "network": ["paramiko", "ansible-pylibssh", "ncclient"],
}
# Left out of a set on a platform it has no wheel for, where the plugin does without it:
# `network_cli` uses paramiko when ansible-pylibssh is missing, as on musl.
OPTIONAL = {"ansible-pylibssh"}


def resolve_set(python, triple, tags, name, packages):
    """The wheels of `packages` on top of ansible-core, less an optional package with no wheel for
    this platform, which is said on stderr."""
    try:
        return resolve(python, tags, packages)
    except subprocess.CalledProcessError:
        kept = []
        for package in packages:
            try:
                resolve(python, tags, [package])
                kept.append(package)
            except subprocess.CalledProcessError:
                if package not in OPTIONAL:
                    sys.exit(f"{package} of the {name} extras has no wheel for {triple}")
                print(f"{name} on {triple}: {package} has no wheel, left out", file=sys.stderr)
        return resolve(python, tags, kept)


def archive(triple):
    return f"cpython-{PYTHON}+{RELEASE}-{triple}-install_only_stripped.tar.gz"


def fetch(url):
    with urllib.request.urlopen(url, timeout=60) as response:
        return response.read()


def own_triple():
    machine = {"x86_64": "x86_64", "amd64": "x86_64", "arm64": "aarch64", "aarch64": "aarch64"}
    arch = machine.get(platform.machine().lower())
    if sys.platform == "darwin":
        return f"{arch}-apple-darwin"
    libc = "gnu" if platform.libc_ver()[0] == "glibc" else "musl"
    return f"{arch}-unknown-linux-{libc}"


def resolve(python, tags, extra=()):
    platform_args = [arg for tag in tags for arg in ("--platform", tag)]
    # No user configuration may reach the pins: a mirror or extra index would change the URLs.
    env = {key: value for key, value in os.environ.items() if not key.startswith("PIP_")}
    with tempfile.TemporaryDirectory() as target:
        report = subprocess.run(
            [python, "-I", "-m", "pip", "--isolated", "install", "--quiet", "--dry-run", "--ignore-installed",
             "--disable-pip-version-check", "--no-input", "--report", "-", "--target", target,
             "--only-binary", ":all:", "--python-version", PYTHON.rsplit(".", 1)[0],
             "--implementation", "cp", "--abi", "cp" + "".join(PYTHON.split(".")[:2]),
             "--abi", "abi3", "--abi", "none", *platform_args, f"ansible-core=={ANSIBLE_CORE}", *extra],
            check=True, capture_output=True, text=True, env=env,
        ).stdout
    wheels = []
    for item in json.loads(report)["install"]:
        info = item["download_info"]
        wheels.append({
            "name": item["metadata"]["name"],
            "version": item["metadata"]["version"],
            "url": info["url"],
            "sha256": info["archive_info"]["hashes"]["sha256"],
        })
    return sorted(wheels, key=lambda wheel: wheel["name"].lower())


def main():
    sums = {}
    for line in fetch(f"{BASE}/SHA256SUMS").decode().splitlines():
        digest, _, name = line.strip().partition(" ")
        sums[name.strip().lstrip("*")] = digest
    pythons = {}
    for triple in PLATFORMS:
        name = archive(triple)
        if name not in sums:
            sys.exit(f"{name} is not in the SHA256SUMS of release {RELEASE}")
        pythons[triple] = {"url": f"{BASE}/{name.replace('+', '%2B')}", "sha256": sums[name]}

    host = own_triple()
    if host not in pythons:
        sys.exit(f"no pinned Python build runs on this machine ({host})")
    with tempfile.TemporaryDirectory() as work:
        data = fetch(pythons[host]["url"])
        actual = hashlib.sha256(data).hexdigest()
        if actual != pythons[host]["sha256"]:
            sys.exit(f"{archive(host)}: expected sha256 {pythons[host]['sha256']}, got {actual}")
        tarball = Path(work, "python.tar.gz")
        tarball.write_bytes(data)
        with tarfile.open(tarball) as tar:
            tar.extractall(work, filter="data")
        python = str(Path(work, "python", "bin", "python3"))
        wheels = {triple: resolve(python, tags) for triple, tags in PLATFORMS.items()}
        extras = {}
        for triple, tags in PLATFORMS.items():
            base = {(wheel["name"].lower(), wheel["version"]) for wheel in wheels[triple]}
            names = {wheel["name"].lower() for wheel in wheels[triple]}
            for name, packages in EXTRAS.items():
                added = []
                for wheel in resolve_set(python, triple, tags, name, packages):
                    key = (wheel["name"].lower(), wheel["version"])
                    if key in base:
                        continue
                    # A set that needs another version of a base wheel cannot go on top of it.
                    if key[0] in names:
                        sys.exit(f"{name} on {triple} needs {wheel['name']} {wheel['version']}, "
                                 "which the base pins at another version")
                    added.append(wheel)
                extras[(triple, name)] = added

    lines = [
        "# Generated by `just runtime-lock` (crates/volant/src/runtime/lock.py). Do not edit by hand.",
        "",
        f'python = "{PYTHON}"',
        f'python-release = "{RELEASE}"',
        f'ansible-core = "{ANSIBLE_CORE}"',
    ]
    for triple in PLATFORMS:
        lines += ["", f"[platform.{triple}.python]",
                  f'url = "{pythons[triple]["url"]}"', f'sha256 = "{pythons[triple]["sha256"]}"']
        for wheel in wheels[triple]:
            lines += ["", f"[[platform.{triple}.wheel]]"]
            lines += [f'{key} = "{wheel[key]}"' for key in ("name", "version", "url", "sha256")]
        for name in EXTRAS:
            for wheel in extras[(triple, name)]:
                lines += ["", f"[[platform.{triple}.extras.{name}]]"]
                lines += [f'{key} = "{wheel[key]}"' for key in ("name", "version", "url", "sha256")]
    Path(__file__).with_name("manifest.toml").write_text("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
