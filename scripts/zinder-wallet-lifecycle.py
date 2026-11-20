#!/usr/bin/env python3
"""Adapt the funded public Wallet conformance suite to lifecycle report schema v1."""
import json
import os
from pathlib import Path
import subprocess
import sys

mode, scratch_arg, endpoint, report_arg = sys.argv[1:]
scratch = Path(scratch_arg)
report = Path(report_arg)
receipt = scratch / "zally-lifecycle.json"
if mode == "prepare":
    env = dict(os.environ, ZINDER_ENDPOINT=endpoint,
               ZALLY_TEST_LIFECYCLE_REPORT=str(receipt))
    # Fresh output is mandatory, including when a previous invocation failed.
    receipt.unlink(missing_ok=True)
    subprocess.run([str(Path(__file__).with_name("verify-zinder-wallet.sh"))],
                   cwd=Path(__file__).resolve().parent.parent, env=env, check=True)
observed = json.loads(receipt.read_text())
assert observed["endpoint"] == endpoint
assert observed["fallback_used"] is False
if mode != "prepare":
    assert observed["assertions"][mode] is True
else:
    observed["assertions"]["suite_completed"] = True
report.write_text(json.dumps(observed))
