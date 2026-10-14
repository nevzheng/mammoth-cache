"""Stores every entry of a traffic model once, so a following run finds the cache warm.

    uv run python load/seed.py --target targets/compose.toml --keys 100000 --model warm

Run as its own process (the locustfile calls it for `--run warm`), because Locust's gevent
patching does not mix with a thread pool.
"""

from __future__ import annotations

import argparse
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import httpx

from mammoth_e2e.client import basic
from mammoth_e2e.target import Target
from mammoth_e2e.traffic import MODELS, Traffic

GRADLE_CONTENT_TYPE = "application/vnd.gradle.build-cache-artifact.v2"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--target", default="targets/compose.toml")
    parser.add_argument("--model", choices=sorted(MODELS), default="warm")
    parser.add_argument("--keys", type=int, default=0, help="0 keeps the model's default")
    parser.add_argument("--concurrency", type=int, default=32)
    args = parser.parse_args()

    target = Target.load(Path(args.target))
    model = MODELS[args.model]
    if args.keys:
        model = model.with_keys(args.keys)
    traffic = Traffic(model)
    headers = {"Content-Type": GRADLE_CONTENT_TYPE}
    if target.credentials:
        headers["Authorization"] = basic(target.credentials)
    limits = httpx.Limits(max_connections=args.concurrency)
    started = time.monotonic()
    with (
        httpx.Client(base_url=str(target.base_url), headers=headers, limits=limits) as http,
        ThreadPoolExecutor(max_workers=args.concurrency) as pool,
    ):

        def store(rank: int) -> int:
            path = f"/cache/{traffic.gradle_key(rank)}"
            return http.put(path, content=traffic.body(rank), timeout=60.0).status_code

        codes = list(pool.map(store, range(model.keys), chunksize=256))
    failed = sum(code not in {200, 201} for code in codes)
    size = sum(traffic.size(rank) for rank in range(model.keys))
    print(  # noqa: T201 - the seeder's report
        f"seeded {model.keys} entries, {size / 2**20:.0f} MiB, "
        f"in {time.monotonic() - started:.0f}s; {failed} failed"
    )
    if failed:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
