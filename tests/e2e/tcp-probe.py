#!/usr/bin/env python3
import argparse
import socket
import time


def exchange(host: str, port: int, payload: bytes) -> bool:
    try:
        with socket.create_connection((host, port), timeout=3) as stream:
            stream.settimeout(5)
            stream.sendall(payload + b"\n")
            response = b""
            while not response.endswith(b"\n"):
                chunk = stream.recv(4096)
                if not chunk:
                    return False
                response += chunk
            return response.endswith(b":" + payload + b"\n")
    except OSError:
        return False


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--payload", required=True)
    parser.add_argument("--wait-secs", type=int, default=60)
    parser.add_argument("--expect-unavailable", action="store_true")
    args = parser.parse_args()

    deadline = time.monotonic() + args.wait_secs
    while True:
        available = exchange(args.host, args.port, args.payload.encode())
        if available != args.expect_unavailable:
            return 0
        if time.monotonic() >= deadline:
            expectation = "unavailable" if args.expect_unavailable else "available"
            raise RuntimeError(f"TCP service did not become {expectation}")
        time.sleep(1)


if __name__ == "__main__":
    raise SystemExit(main())
