#!/usr/bin/env python3
"""Run oha load tests and produce a compact pass/fail assessment.

Examples:
  python scripts/oha_bench.py --url http://127.0.0.1:8080/
  python scripts/oha_bench.py --url http://127.0.0.1:8080/api/users \
    --requests 20000 --connections 200 --min-rps 5000 --max-p99-ms 120
"""

from __future__ import annotations

import argparse
import json
import math
import shutil
import subprocess
import sys
from datetime import datetime
from pathlib import Path
from typing import Any
from urllib.parse import parse_qsl, urlencode, urlsplit, urlunsplit


def deep_get(data: dict[str, Any], *keys: str) -> Any:
    current: Any = data
    for key in keys:
        if not isinstance(current, dict) or key not in current:
            return None
        current = current[key]
    return current


def sanitize_url(url: str) -> str:
    try:
        parts = urlsplit(url)
    except Exception:
        return "<invalid-url>"

    hostname = parts.hostname or ""
    if parts.port:
        host = f"{hostname}:{parts.port}"
    else:
        host = hostname

    redacted_query = []
    for key, value in parse_qsl(parts.query, keep_blank_values=True):
        lower = key.lower()
        if any(token in lower for token in ("token", "key", "secret", "password", "sig", "signature")):
            redacted_query.append((key, "***"))
        else:
            redacted_query.append((key, value))

    query = urlencode(redacted_query)
    return urlunsplit((parts.scheme, host, parts.path, query, parts.fragment))


def first_number(*values: Any) -> float | None:
    for value in values:
        candidate: float | None = None
        if isinstance(value, (int, float)):
            candidate = float(value)
        elif isinstance(value, str):
            try:
                candidate = float(value)
            except ValueError:
                candidate = None

        if candidate is not None and math.isfinite(candidate):
            return candidate
    return None


def run_oha(url: str, requests: int, connections: int, duration: str | None, timeout_secs: int) -> dict[str, Any]:
    cmd = [
        "oha",
        "--no-tui",
        "--output-format",
        "json",
        "-c",
        str(connections),
        "-n",
        str(requests),
        url,
    ]
    if duration:
        cmd += ["-z", duration]

    redacted_url = sanitize_url(url)

    proc = None
    try:
        proc = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout_secs,
        )
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(
            f"oha timed out for {redacted_url} after {timeout_secs}s\ncommand: oha ... {redacted_url}"
        ) from exc

    if proc.returncode != 0:
        stdout_preview = (proc.stdout or "")[:2000]
        stderr_preview = (proc.stderr or "")[:2000]
        raise RuntimeError(
            f"oha failed for {redacted_url}\ncommand: oha ... {redacted_url}\nstdout:\n{stdout_preview}\nstderr:\n{stderr_preview}"
        )

    try:
        return json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(
            f"oha did not return JSON for {redacted_url}. Raw output:\n{(proc.stdout or '')[:1000]}"
        ) from exc


def summarize(url: str, data: dict[str, Any]) -> dict[str, Any]:
    summary = deep_get(data, "summary") or {}

    rps = first_number(
        deep_get(summary, "requests_per_sec"),
        deep_get(summary, "requestsPerSec"),
    )

    avg_secs = first_number(
        deep_get(summary, "average"),
        deep_get(summary, "latency_average"),
        deep_get(summary, "latencyAvg"),
    )
    p95_secs = first_number(
        deep_get(data, "latencyPercentiles", "p95"),
        deep_get(summary, "p95"),
        deep_get(summary, "latency_p95"),
        deep_get(summary, "latencyP95"),
    )
    p99_secs = first_number(
        deep_get(data, "latencyPercentiles", "p99"),
        deep_get(summary, "p99"),
        deep_get(summary, "latency_p99"),
        deep_get(summary, "latencyP99"),
    )

    status_distribution = data.get("statusCodeDistribution")
    if not isinstance(status_distribution, dict):
        status_distribution = {}

    error_distribution = data.get("errorDistribution")
    if not isinstance(error_distribution, dict):
        error_distribution = {}
    success = sum(
        value
        for key, value in status_distribution.items()
        if isinstance(value, int) and str(key).startswith("2")
    )
    status_count = sum(value for value in status_distribution.values() if isinstance(value, int))
    error_count = sum(value for value in error_distribution.values() if isinstance(value, int))
    total = status_count + error_count

    success_rate = first_number(
        deep_get(summary, "successRate"),
        deep_get(summary, "success_rate"),
    )
    transport_error_rate = None
    if success_rate is not None:
        normalized_success_rate = success_rate / 100.0 if success_rate > 1 else success_rate
        normalized_success_rate = min(1.0, max(0.0, normalized_success_rate))
        transport_error_rate = max(0.0, 1.0 - normalized_success_rate)

    error_rate = None
    status_error_rate = None
    if total > 0:
        status_error_rate = max(0.0, 1.0 - (success / total))

    if status_error_rate is not None and transport_error_rate is not None:
        error_rate = max(status_error_rate, transport_error_rate)
    elif status_error_rate is not None:
        error_rate = status_error_rate
    elif transport_error_rate is not None:
        error_rate = transport_error_rate

    return {
        "url": url,
        "rps": rps,
        "avg_ms": avg_secs * 1000 if avg_secs is not None else None,
        "p95_ms": p95_secs * 1000 if p95_secs is not None else None,
        "p99_ms": p99_secs * 1000 if p99_secs is not None else None,
        "total": float(total),
        "success": float(success),
        "error_rate": error_rate,
        "transport_error_rate": transport_error_rate,
        "raw": data,
    }


def check_thresholds(
    result: dict[str, Any],
    min_rps: float | None,
    max_p99_ms: float | None,
    max_error_rate: float | None,
) -> list[str]:
    failures: list[str] = []
    if min_rps is not None:
        rps = result["rps"]
        if rps is None or rps < min_rps:
            failures.append(f"RPS {rps} < min_rps {min_rps}")

    if max_p99_ms is not None:
        p99_ms = result["p99_ms"]
        if p99_ms is None or p99_ms > max_p99_ms:
            failures.append(f"P99 {p99_ms}ms > max_p99_ms {max_p99_ms}ms")

    if max_error_rate is not None:
        error_rate = result["error_rate"]
        if error_rate is None or error_rate > max_error_rate:
            failures.append(f"error_rate {error_rate} > max_error_rate {max_error_rate}")

    return failures


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be > 0")
    return parsed


def non_negative_int(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be >= 0")
    return parsed


def positive_float(value: str) -> float:
    parsed = float(value)
    if not math.isfinite(parsed) or parsed <= 0:
        raise argparse.ArgumentTypeError("must be a finite number > 0")
    return parsed


def main() -> int:
    parser = argparse.ArgumentParser(description="Run oha load tests and evaluate service thresholds.")
    parser.add_argument("--url", action="append", required=True, help="Target URL. Repeatable.")
    parser.add_argument("--requests", type=positive_int, default=5000, help="Total requests per URL (default: 5000)")
    parser.add_argument("--connections", type=positive_int, default=100, help="Concurrent connections (default: 100)")
    parser.add_argument("--duration", default=None, help="Optional oha duration (e.g. 30s, 2m)")
    parser.add_argument("--warmup", type=non_negative_int, default=200, help="Warmup requests before real run (default: 200)")
    parser.add_argument("--timeout-secs", type=positive_int, default=300, help="Timeout for a single oha run (default: 300)")

    parser.add_argument("--min-rps", type=positive_float, default=None, help="Fail if RPS is below this value")
    parser.add_argument("--max-p99-ms", type=positive_float, default=None, help="Fail if P99 latency exceeds this value")
    parser.add_argument("--max-error-rate", type=float, default=None, help="Fail if error rate exceeds this value (0.01 = 1%)")

    parser.add_argument(
        "--out",
        default="bench/oha_report.json",
        help="Output JSON report path (default: bench/oha_report.json)",
    )

    args = parser.parse_args()

    if args.max_error_rate is not None:
        if not math.isfinite(args.max_error_rate) or args.max_error_rate < 0 or args.max_error_rate > 1:
            parser.error("--max-error-rate must be a finite number in [0, 1]")

    if shutil.which("oha") is None:
        print("ERROR: oha not found in PATH. Install it with `cargo install oha` or package manager.", file=sys.stderr)
        return 2

    all_results: list[dict[str, Any]] = []

    for url in args.url:
        warmup_warning: str | None = None
        try:
            if args.warmup > 0:
                try:
                    run_oha(
                        url=url,
                        requests=args.warmup,
                        connections=min(args.connections, 20),
                        duration=None,
                        timeout_secs=args.timeout_secs,
                    )
                except Exception as exc:  # noqa: BLE001
                    warmup_warning = f"warmup failed: {exc}"

            raw = run_oha(
                url=url,
                requests=args.requests,
                connections=args.connections,
                duration=args.duration,
                timeout_secs=args.timeout_secs,
            )
            summarized = summarize(url, raw)
            failures = check_thresholds(
                summarized,
                min_rps=args.min_rps,
                max_p99_ms=args.max_p99_ms,
                max_error_rate=args.max_error_rate,
            )
            summarized["threshold_failures"] = failures
            if warmup_warning:
                summarized["warnings"] = [warmup_warning]
            all_results.append(summarized)
        except Exception as exc:  # noqa: BLE001
            all_results.append(
                {
                    "url": url,
                    "rps": None,
                    "avg_ms": None,
                    "p95_ms": None,
                    "p99_ms": None,
                    "total": 0.0,
                    "success": 0.0,
                    "error_rate": 1.0,
                    "transport_error_rate": 1.0,
                    "raw": None,
                    "warnings": [warmup_warning] if warmup_warning else [],
                    "threshold_failures": [f"benchmark execution failed: {exc}"],
                }
            )

    report = {
        "generated_at": datetime.utcnow().isoformat() + "Z",
        "config": {
            "requests": args.requests,
            "connections": args.connections,
            "duration": args.duration,
            "warmup": args.warmup,
            "min_rps": args.min_rps,
            "max_p99_ms": args.max_p99_ms,
            "max_error_rate": args.max_error_rate,
        },
        "results": all_results,
    }

    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")

    overall_failures = 0
    print("\n=== oha benchmark summary ===")
    for result in all_results:
        print(
            f"- {result['url']} | "
            f"rps={result['rps']} "
            f"avg={result['avg_ms']}ms "
            f"p95={result['p95_ms']}ms "
            f"p99={result['p99_ms']}ms "
            f"error_rate={result['error_rate']} "
            f"transport_error_rate={result.get('transport_error_rate')}"
        )
        if result["threshold_failures"]:
            overall_failures += 1
            for item in result["threshold_failures"]:
                print(f"  FAIL: {item}")

    print(f"\nreport: {out_path}")
    if overall_failures > 0:
        print(f"benchmark verdict: FAIL ({overall_failures} endpoint(s) exceeded thresholds)")
        return 1

    print("benchmark verdict: PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
