"""Keep `units.json` and the build in step. Reads `ledgerfio layout --json` on stdin.

    write  -- replace the cache, stamping the commit it was taken at
    check  -- refuse a difference, which is what `make verify` runs

Its own file rather than a line inside the Makefile: a rule the build enforces should be readable, and
the difference between `write` and `check` is what stops a cache from becoming the stale number it was
supposed to prevent.
"""

import json
import subprocess
import sys

from model import UNITS_PATH


def commit():
    done = subprocess.run(
        ["git", "rev-parse", "HEAD"], capture_output=True, text=True, check=False
    )
    return done.stdout.strip() or "unknown"


def main(mode):
    fresh = json.load(sys.stdin)
    if mode == "write":
        stamped = {"commit": commit(), "source": "ledgerfio layout --json", **fresh}
        with open(UNITS_PATH, "w") as handle:
            handle.write(json.dumps(stamped, indent=2, sort_keys=True) + "\n")
        return 0

    with open(UNITS_PATH) as handle:
        cached = json.load(handle)
    # Not the commit: it says where the numbers came from, and demanding it match HEAD would mean
    # refreshing on every unrelated commit -- a check that fires for something it is not about.
    cached.pop("commit", None)
    cached.pop("source", None)
    if cached == fresh:
        return 0

    cached_parts = {part["name"]: part for part in cached.get("parts", [])}
    fresh_parts = {part["name"]: part for part in fresh["parts"]}
    for name in sorted(set(fresh_parts) - set(cached_parts)):
        print(f"  new: {name}", file=sys.stderr)
    for name in sorted(set(cached_parts) - set(fresh_parts)):
        print(f"  gone: {name}", file=sys.stderr)
    for name in sorted(set(cached_parts) & set(fresh_parts)):
        if cached_parts[name] != fresh_parts[name]:
            print(
                f"  changed: {name} {cached_parts[name]['bytes']}B -> {fresh_parts[name]['bytes']}B",
                file=sys.stderr,
            )
    print("sizing/units.json is stale -- run `make sizing-units`", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1] if len(sys.argv) > 1 else "check"))
