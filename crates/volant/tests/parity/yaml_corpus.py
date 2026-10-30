# SPDX-License-Identifier: GPL-3.0-or-later
"""Loads every YAML file of a pinned corpus with Volant and with ansible-core, and prints the
files whose values differ, grouped by cause. Exits 1 when a difference is not in
yaml_corpus_known.txt, which names the issue that tracks each known one.

Run by `just yaml-corpus` under the reference ansible-core's Python:
    yaml_corpus.py <corpus dir> <yaml_dump binary> <ansible package dir>
"""
import datetime
import json
import os
import subprocess
import sys

# Both loaders read a key written twice the same way; the reference's warnings would only fill
# the output.
os.environ.setdefault("ANSIBLE_DUPLICATE_YAML_DICT_KEY", "ignore")

import yaml as pyyaml  # noqa: E402
from ansible.parsing.utils.yaml import from_yaml  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
PINS = os.path.join(HERE, "yaml_corpus.yml")
KNOWN = os.path.join(HERE, "yaml_corpus_known.txt")
SUFFIXES = (".yml", ".yaml")
# Not YAML for either loader to read, or a test of ansible-core's own refusals.
SKIP_PARTS = ("/.git/", "/node_modules/")


def fetch(root, pin):
    """A shallow checkout of the pin, made once and reused."""
    dest = os.path.join(root, pin["name"])
    stamp = os.path.join(dest, ".volant-ref")
    if os.path.exists(stamp) and open(stamp).read() == pin["ref"]:
        return dest
    os.makedirs(dest, exist_ok=True)
    git = ["git", "-C", dest]
    subprocess.run(git + ["init", "-q"], check=True)
    subprocess.run(git + ["fetch", "-q", "--depth", "1", pin["git"], pin["ref"]], check=True)
    subprocess.run(git + ["checkout", "-q", "--force", "FETCH_HEAD"], check=True)
    with open(stamp, "w") as f:
        f.write(pin["ref"])
    return dest


def files(base, paths):
    for top in [os.path.join(base, p) for p in paths] if paths else [base]:
        for directory, _, names in os.walk(top):
            for name in names:
                path = os.path.join(directory, name)
                if name.endswith(SUFFIXES) and not any(p in path for p in SKIP_PARTS):
                    yield path


def key_text(key):
    """A key as Volant's JSON holds it, as ansible-core prints it."""
    if isinstance(key, bool):
        return "true" if key else "false"
    if key is None:
        return "None"
    if isinstance(key, float):
        return repr(key)
    return str(key)


def plain(value):
    """ansible-core's value as JSON, or a marker for what Volant knowingly loads otherwise."""
    if isinstance(value, dict):
        return {key_text(k): plain(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [plain(v) for v in value]
    if isinstance(value, (datetime.date, datetime.datetime)):
        return {"__date__": str(value)}
    if type(value).__name__ == "EncryptedString":
        return "VAULT:" + value._ciphertext.strip()
    if isinstance(value, (bytes, set)):
        return {"__unsupported__": type(value).__name__}
    # ansible-core tags what it loads with subclasses of the plain types.
    if isinstance(value, bool):
        return bool(value)
    if isinstance(value, int):
        return int(value)
    if isinstance(value, float):
        return float(value)
    if isinstance(value, str):
        return str(value)
    return value


def first_difference(ours, theirs, where="$"):
    """Where the two values first differ, or None. A date Volant reads as text is no difference:
    the documented gap."""
    if isinstance(theirs, dict) and "__date__" in theirs and isinstance(ours, str):
        return None
    if isinstance(theirs, dict) and "__unsupported__" in theirs:
        return None
    if isinstance(ours, dict) and isinstance(theirs, dict):
        if list(ours) != list(theirs):
            return f"{where} keys {sorted(set(ours) ^ set(theirs)) or 'order'}"
        for k in ours:
            found = first_difference(ours[k], theirs[k], f"{where}.{k}")
            if found:
                return found
        return None
    if isinstance(ours, list) and isinstance(theirs, list):
        if len(ours) != len(theirs):
            return f"{where} length {len(ours)} != {len(theirs)}"
        for i, (a, b) in enumerate(zip(ours, theirs)):
            found = first_difference(a, b, f"{where}[{i}]")
            if found:
                return found
        return None
    if ours != theirs or type(ours) is not type(theirs):
        return f"{where}: {json.dumps(ours)[:80]} != {json.dumps(theirs, default=str)[:80]}"
    return None


def reference(path):
    try:
        with open(path, encoding="utf-8") as f:
            text = f.read()
    except (UnicodeDecodeError, OSError) as e:
        return {"error": type(e).__name__}
    try:
        return {"value": plain(from_yaml(text, file_name=path, json_only=False))}
    except Exception as e:  # noqa: BLE001 - any refusal is the reference's answer
        return {"error": str(e).splitlines()[0] if str(e) else type(e).__name__}


def cause(ours, theirs):
    if "error" in ours and "error" in theirs:
        return None
    if "error" in ours:
        return "volant refuses: " + ours["error"].split(": ")[-1][:60]
    if "error" in theirs:
        return "ansible-core refuses: " + theirs["error"][:60]
    docs = ours["docs"]
    value = docs[0] if docs else None
    found = first_difference(value, theirs["value"])
    return found and "value differs"


def main(corpus, dump, package):
    # ansible-core's loader takes an absolute file name only.
    corpus, dump, package = map(os.path.abspath, (corpus, dump, package))
    pins = pyyaml.safe_load(open(PINS, encoding="utf-8"))
    sources = [("ansible", package, ["ansible_collections"])]
    sources += [(p["name"], fetch(corpus, p), p.get("paths")) for p in pins]
    paths = sorted(p for _, base, sub in sources for p in files(base, sub))
    run = subprocess.run([dump], input="\n".join(paths), capture_output=True, text=True, check=True)
    ours = {}
    for line in run.stdout.splitlines():
        entry = json.loads(line)
        ours[entry["path"]] = entry
    roots = {base: name for name, base, _ in sources}
    known = {}
    if os.path.exists(KNOWN):
        for line in open(KNOWN, encoding="utf-8"):
            line = line.split("#", 1)[0].strip()
            if line:
                known[line.split()[0]] = line
    groups, new = {}, []
    for path in paths:
        theirs = reference(path)
        why = cause(ours[path], theirs)
        if not why:
            continue
        base = next(b for b in roots if path.startswith(b))
        name = roots[base] + path[len(base):]
        detail = why if why != "value differs" else first_difference(
            (ours[path]["docs"] or [None])[0], theirs["value"])
        groups.setdefault(why, []).append(f"{name}  {detail}")
        if name not in known:
            new.append(name)
    for why in sorted(groups):
        print(f"\n{why} ({len(groups[why])})")
        for entry in groups[why]:
            print(f"  {entry}")
    print(f"\n{len(paths)} files, {sum(len(g) for g in groups.values())} differ, {len(new)} not known")
    for name in new:
        print(f"  new: {name}")
    return 1 if new else 0


if __name__ == "__main__":
    sys.exit(main(*sys.argv[1:4]))
