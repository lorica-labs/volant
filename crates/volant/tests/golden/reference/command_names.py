# SPDX-License-Identifier: GPL-3.0-or-later
"""The `command-names` kind: the scripts of tests/command-names run with the reference's own
`ansible-*` programs, each output file kept as the expected file the CI job compares with."""
import os
import subprocess
import tempfile

from . import GOLDEN
from .run import DEADLINE, Failed

SCRIPTS = os.path.normpath(os.path.join(GOLDEN, "..", "..", "..", "..", "tests", "command-names"))


def record():
    """Runs each script with RECORD set, in an environment with no `ANSIBLE_*` setting and a
    home directory of its own, so no configuration but the reference's defaults is read."""
    with tempfile.TemporaryDirectory() as home:
        env = {k: v for k, v in os.environ.items() if not k.startswith("ANSIBLE_")}
        env.update(HOME=home, RECORD="1", OUT_DIR=os.path.join(home, "out"))
        for script in sorted(f for f in os.listdir(SCRIPTS) if f.endswith(".sh") and f != "lib.sh"):
            try:
                done = subprocess.run(
                    ["sh", os.path.join(SCRIPTS, script)],
                    env=env,
                    stdin=subprocess.DEVNULL,
                    capture_output=True,
                    text=True,
                    timeout=DEADLINE,
                )
            except subprocess.TimeoutExpired:
                raise Failed(f"{script} did not finish within {DEADLINE} s") from None
            if done.returncode:
                raise Failed(f"{script} exited {done.returncode} while recording\n{done.stdout}\n{done.stderr}")
