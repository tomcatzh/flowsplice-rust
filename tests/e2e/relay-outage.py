#!/usr/bin/env python3
"""Take one Relay of this E2E topology down for a few seconds, one outage at a time.

iPhone, iPad and macOS share a topology (tests/e2e/run.sh) and each stops a Relay once to test
failover. A lock in tests/e2e/generated keeps the outages apart, so the other Relay stays up for
every platform, and it is released only after the Relay is back in the Server's directory.

Example:
  tests/e2e/relay-outage.py relay1 --seconds 5
"""
import argparse
import datetime
import fcntl
import subprocess
import sys
import time
from pathlib import Path

E2E = Path(__file__).resolve().parent
COMPOSE = ["docker", "compose", "-f", str(E2E / "compose.yaml")]
RELAY_IDS = {"relay1": "relay-1", "relay2": "relay-2"}

parser = argparse.ArgumentParser()
parser.add_argument("service", choices=sorted(RELAY_IDS))
parser.add_argument("--seconds", type=float, default=5, help="how long the Relay stays down")
parser.add_argument("--recover-seconds", type=float, default=60)
args = parser.parse_args()


def logs_since(since, *services):
    return subprocess.run(COMPOSE + ["logs", "--no-color", "--since", since, *services],
                          capture_output=True, text=True, check=True).stdout


def back(since):
    # A published port accepts connections even while its container is down, so wait for the
    # Relay's own listeners and for the Server to put it back into the Relay directory.
    relay = logs_since(since, args.service)
    server = logs_since(since, "server")
    return ("relay management listener ready" in relay and "relay data listener ready" in relay
            and any("installed authenticated Relay session in directory" in line
                    and f"relay_id={RELAY_IDS[args.service]}" in line for line in server.splitlines()))


with (E2E / "generated" / "relay-outage.lock").open("w") as lock:
    waiting = time.monotonic()
    fcntl.flock(lock, fcntl.LOCK_EX)
    waited = time.monotonic() - waiting
    if waited >= 1:
        print(f"{args.service} outage waited {waited:.0f} s for another platform's outage", file=sys.stderr)
    # The binaries run as PID 1 without a SIGTERM handler, so `stop` would only add its 10 s grace
    # period before the same kill.
    subprocess.run(COMPOSE + ["kill", args.service], check=True, stdout=subprocess.DEVNULL)
    time.sleep(args.seconds)
    since = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    subprocess.run(COMPOSE + ["start", args.service], check=True, stdout=subprocess.DEVNULL)
    deadline = time.monotonic() + args.recover_seconds
    while not back(since):
        if time.monotonic() >= deadline:
            raise SystemExit(f"{args.service} did not recover within {args.recover_seconds:.0f} s after restart")
        time.sleep(1)
