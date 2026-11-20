#!/usr/bin/env python3
"""Adapt real public Wallet observations to lifecycle report schema v2."""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
from urllib.parse import urlsplit


def require(condition, reason):
    if not condition:
        raise ValueError(reason)


def fingerprint(text):
    return hashlib.sha256(text.encode()).hexdigest()


def transaction_fingerprint(tx_id):
    require(isinstance(tx_id, str) and re.fullmatch(r"[0-9a-f]{64}", tx_id),
            "transaction ID must be canonical RPC display hex")
    return hashlib.sha256(bytes.fromhex(tx_id)).hexdigest()


def peer(uri):
    parsed = urlsplit(uri)
    require(parsed.hostname == "127.0.0.1" and parsed.port is not None,
            "this trace adapter requires explicit loopback IPv4 endpoints")
    return parsed.hostname, parsed.port


def trace_endpoint(trace, endpoint):
    expected = peer(endpoint)
    allowed = {expected, peer(os.environ["ZALLY_TEST_NODE_JSON_RPC_ADDR"])}
    peers = set()
    for line in trace.read_text().splitlines():
        if "sa_family=AF_INET" not in line:
            continue
        matched = re.search(r'sin_port=htons\((\d+)\), sin_addr=inet_addr\("([^"]+)"\)', line)
        require(matched is not None, "unrecognized INET connection in process trace")
        peers.add((matched[2], int(matched[1])))
    require(expected in peers, "configured native endpoint absent from process trace")
    require(not peers - allowed, "unexpected network connection in wallet process tree")
    return {"scope": "process-trace", "target_connected": True,
            "unexpected_connections": 0,
            "trace_fingerprint_sha256": hashlib.sha256(trace.read_bytes()).hexdigest(),
            "observed_peers": [{"host": host, "port": port} for host, port in sorted(peers)],
            "trace_scope": "wallet-test process-tree connect syscalls; build tools excluded"}


def qualify(observed, endpoint_evidence):
    require(observed["status"] == "completed" and observed["assertions_enabled"] is True,
            "wallet assertions incomplete")
    transactions = observed["transaction_observations"]
    identities = {transaction_fingerprint(entry["tx_id"]) for entry in transactions.values()}
    require(len(identities) == 1, "submitted, pending, mined, and scanned transactions differ")
    require(transactions["mined"]["height"] == transactions["scanned"]["height"],
            "canonical and scanned mined heights differ")
    restore = observed["restore_observation"]
    require(restore["fresh_database"] is True and restore["balance_matches"] is True,
            "restore must use a fresh independent database with matching balance")
    require(restore["source_database"] != restore["restored_database"], "restore reused database")
    require(restore["source_wallet"] == restore["restored_wallet"], "restored wallet identity differs")
    require(restore["source_activity"] == restore["restored_activity"] and restore["note_count"] > 0,
            "restore did not recover existing activity")
    restart = observed["restart_observation"]
    require(restart["process_exited"] is True, "wallet subprocess did not exit successfully")
    require(restart["baseline"]["pid"] != restart["child"]["pid"], "wallet process identity unchanged")
    require(restart["reopened_height"] > restart["before_height"], "subprocess sync did not advance")
    require(restart["resumed_height"] > restart["reopened_height"], "post-restart block was not scanned")
    require(restart["new_block_mined_after_restart"] is True, "no new block after restart")
    observed["endpoint_evidence"] = endpoint_evidence
    observed["assertion_mode"] = "always-on"
    return observed


def report_for(observed, mode):
    report = {key: observed[key] for key in
              ("status", "endpoint", "assertions_enabled", "assertion_mode", "endpoint_evidence")}
    report.update(fallback_used=False, assertions=observed["assertions"])
    if mode in {"send", "pending", "mined", "scanned"}:
        entry = observed["transaction_observations"][mode]
        report["transaction"] = {
            "fingerprint_sha256": transaction_fingerprint(entry["tx_id"]),
            "observation": {"send": "submitted", "pending": "wallet-pending",
                            "mined": "canonical-mined", "scanned": "wallet-scanned"}[mode],
            "actor": entry["actor"]}
        if mode in {"mined", "scanned"}:
            report["transaction"]["mined_height"] = entry["height"]
    if mode == "restore":
        entry = observed["restore_observation"]
        report["restore"] = {"fresh_database": entry["fresh_database"],
            "source_database_fingerprint_sha256": fingerprint(entry["source_database"]),
            "restored_database_fingerprint_sha256": fingerprint(entry["restored_database"]),
            "source_wallet_fingerprint_sha256": fingerprint(entry["source_wallet"]),
            "restored_wallet_fingerprint_sha256": fingerprint(entry["restored_wallet"]),
            "source_activity_fingerprint_sha256": transaction_fingerprint(entry["source_activity"]),
            "restored_activity_fingerprint_sha256": transaction_fingerprint(entry["restored_activity"]),
            "restored_activity_count": entry["note_count"]}
    if mode == "restart":
        entry = observed["restart_observation"]
        report["restart_scope"] = "process-restart"
        report["restart"] = {"before_process_id": entry["baseline"]["pid"],
            "after_process_id": entry["child"]["pid"], "previous_process_exited": entry["process_exited"],
            "before_height": entry["baseline"]["after_height"], "after_height": entry["reopened_height"]}
    if mode == "resume":
        entry = observed["restart_observation"]
        report["progress"] = {"before_height": entry["reopened_height"],
            "after_height": entry["resumed_height"], "new_block_mined_after_restart": entry["new_block_mined_after_restart"]}
    if mode == "prepare":
        report["assertions"] = dict(report["assertions"], suite_completed=True)
    else:
        require(report["assertions"][mode] is True, "requested wallet assertion is missing")
    return report


def main():
    mode, scratch_arg, endpoint, report_arg = sys.argv[1:]
    scratch, report = Path(scratch_arg), Path(report_arg)
    receipt, qualified = scratch / "zally-lifecycle.json", scratch / "qualified.json"
    if mode == "prepare":
        receipt.unlink(missing_ok=True)
        qualified.unlink(missing_ok=True)
        trace = scratch / "connections.trace"
        strace = shutil.which("strace")
        require(strace is not None, "strace required for observed endpoint evidence")
        env = dict(os.environ, ZINDER_ENDPOINT=endpoint, ZALLY_TEST_LIFECYCLE_REPORT=str(receipt),
                   ZALLY_TEST_LIVE="1", ZALLY_NETWORK="regtest", CARGO_BUILD_JOBS="2")
        root = Path(__file__).resolve().parent.parent
        listing = subprocess.run(["cargo", "nextest", "list", "--locked", "-p", "zally-wallet",
                                  "--features", "zinder", "--message-format", "json"],
                                 cwd=root, env=env, check=True, capture_output=True, text=True)
        client = json.loads(listing.stdout)["rust-suites"]["zally-wallet::acceptance"]["binary-path"]
        subprocess.run([strace, "-f", "-e", "trace=connect", "-o", str(trace), client,
                        "--ignored", "--exact",
                        "live::funded_zinder_round_trip::funded_wallet_syncs_sends_and_submits_pczt_with_zinder",
                        "--nocapture"], cwd=root, env=env, check=True)
        observed = qualify(json.loads(receipt.read_text()), trace_endpoint(trace, endpoint))
        qualified.write_text(json.dumps(observed))
        if archive := os.environ.get("ZALLY_TEST_LIFECYCLE_ARCHIVE"):
            target = Path(archive)
            target.mkdir(mode=0o700, parents=True, exist_ok=False)
            shutil.copy2(trace, target / "connections.trace")
            sanitized = {name: report_for(observed, name) for name in
                         ("send", "pending", "mined", "scanned", "restore", "restart", "resume")}
            (target / "observations.json").write_text(json.dumps(sanitized, indent=2))
    observed = json.loads(qualified.read_text())
    require(observed["endpoint"] == endpoint, "configured endpoint mismatch")
    report.write_text(json.dumps(report_for(observed, mode)))


if __name__ == "__main__":
    main()
