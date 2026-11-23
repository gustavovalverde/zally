#!/usr/bin/env python3
"""Run dependency policy with the one-release SQLite exception."""

import json
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import tomllib


def main():
    check = ["cargo", "deny", "--locked", "--all-features"]
    status = subprocess.run([*check, "check"], check=False).returncode
    if status != 0:
        return check_release_exception(check, status)
    return 0


def check_release_exception(check, status):
    if subprocess.check_output(
        ["git", "rev-parse", "--is-shallow-repository"], text=True
    ).strip() != "false":
        return status
    tag = subprocess.run(
        ["git", "show-ref", "--verify", "--quiet", "refs/tags/v0.2.3"],
        check=False,
    )
    if tag.returncode != 1:
        return status
    workspace = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"]
        )
    )
    if {crate["version"] for crate in workspace["packages"]} != {"0.2.3"}:
        return status
    lockfile = tomllib.loads(Path("Cargo.lock").read_text())
    sqlite = [crate for crate in lockfile["package"] if crate["name"] == "libsqlite3-sys"]
    if len(sqlite) != 1 or (
        sqlite[0]["version"],
        sqlite[0].get("source"),
    ) != ("0.35.0", "registry+https://github.com/rust-lang/crates.io-index"):
        return status

    config = Path("deny.toml").read_text()
    ignore = "ignore = [\n"
    if config.count(ignore) != 1:
        return status
    exception = (
        '    { id = "RUSTSEC-2026-0459", reason = "Owner-approved for v0.2.3 release work only" },\n'
    )
    print("Rechecking all dependency policy with the v0.2.3 SQLite exception.", flush=True)
    with TemporaryDirectory() as directory:
        path = Path(directory) / "deny.toml"
        path.write_text(config.replace(ignore, ignore + exception, 1))
        return subprocess.run([*check, "--config", str(path), "check"], check=False).returncode


if __name__ == "__main__":
    raise SystemExit(main())
