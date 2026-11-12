#!/usr/bin/env python3
"""Check that the embedded syrtis-agent and the app share one reviewed engine.

Three rules, all read from files, none inferred:

1. The vendor/tokscale-core gitlink equals the tokscale-core rev the agent's
   own Cargo.toml pins at the rev this repository depends on. The workspace
   `[patch]` swaps the agent's engine for the vendored one; the agent's
   security review was done against its pinned engine, so the two SHAs must
   be the same commit.
2. Cargo.lock holds exactly one tokscale-core, and it is the path (vendored)
   one.
3. Every package in the lock closure of the agent's non-engine dependencies
   (snow, hmac, sha2, serde, serde_json, unicode-normalization, libc) equals,
   by (name, version, source, checksum), an entry of the agent's
   tests/fixtures/v1/runtime/storage/cargo-lock-baseline.json at that rev, so
   the crypto dependencies the agent reviewed are the ones this binary ships.

The agent's files are found with `cargo metadata --locked` (cargo's own git
checkout of the pinned rev). `--self-test` runs the rules against the
fixtures in scripts/fixtures/agent-engine-pin/ and needs neither git nor cargo.
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tomllib
from pathlib import Path

AGENT = "syrtis-agent"
ENGINE = "tokscale-core"
AGENT_ROOTS = ("snow", "hmac", "sha2", "serde", "serde_json", "unicode-normalization", "libc")
BASELINE = Path("tests/fixtures/v1/runtime/storage/cargo-lock-baseline.json")


def agent_engine_rev(agent_manifest: str) -> str | None:
    """The rev of tokscale-core in the agent's Cargo.toml, in any dependency table."""
    manifest = tomllib.loads(agent_manifest)
    tables = [manifest.get("dependencies", {})]
    tables += [t.get("dependencies", {}) for t in manifest.get("target", {}).values()]
    for table in tables:
        spec = table.get(ENGINE)
        if isinstance(spec, dict) and "rev" in spec:
            return spec["rev"]
    return None


def lock_packages(lock_text: str) -> list[dict]:
    return tomllib.loads(lock_text).get("package", [])


def resolve(packages: list[dict], dep: str) -> dict:
    """A lock dependency string (`name`, `name version` or `name version (source)`)."""
    parts = dep.split(" ", 2)
    matches = [p for p in packages if p["name"] == parts[0]]
    if len(parts) > 1:
        matches = [p for p in matches if p["version"] == parts[1]]
    if len(parts) > 2:
        matches = [p for p in matches if p.get("source") == parts[2].strip("()")]
    if len(matches) != 1:
        raise ValueError(f"cannot resolve lock dependency {dep!r}")
    return matches[0]


def check(vendor_commit: str, agent_manifest: str, baseline: list[dict], lock_text: str) -> list[str]:
    errors = []
    rev = agent_engine_rev(agent_manifest)
    if rev is None:
        errors.append("the agent's Cargo.toml pins no tokscale-core rev")
    elif rev != vendor_commit:
        errors.append(f"vendor/tokscale-core is {vendor_commit}, the agent pins {rev}")

    packages = lock_packages(lock_text)
    engines = [p for p in packages if p["name"] == ENGINE]
    if len(engines) != 1 or "source" in engines[0]:
        errors.append(f"Cargo.lock must hold exactly one path tokscale-core, found {len(engines)}")

    agents = [p for p in packages if p["name"] == AGENT]
    if len(agents) != 1:
        errors.append(f"Cargo.lock must hold exactly one {AGENT}, found {len(agents)}")
        return errors
    known = {(b["name"], b["version"], b["source"], b["checksum"]) for b in baseline}
    try:
        direct = [resolve(packages, d) for d in agents[0].get("dependencies", [])]
        seen: dict[tuple[str, str], dict] = {}
        stack = [p for p in direct if p["name"] in AGENT_ROOTS]
        if sorted({p["name"] for p in stack}) != sorted(AGENT_ROOTS):
            errors.append(f"{AGENT}'s lock entry does not list every reviewed root {AGENT_ROOTS}")
        while stack:
            package = stack.pop()
            key = (package["name"], package["version"])
            if key in seen or package["name"] == ENGINE:
                continue
            seen[key] = package
            stack.extend(resolve(packages, d) for d in package.get("dependencies", []))
    except ValueError as error:
        return errors + [str(error)]
    for package in sorted(seen.values(), key=lambda p: (p["name"], p["version"])):
        entry = (package["name"], package["version"], package.get("source"), package.get("checksum"))
        if entry not in known:
            errors.append(f"{package['name']} {package['version']} is not in the agent's lock baseline")
    return errors


def gather(root: Path) -> tuple[str, str, list[dict], str]:
    staged = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-s", "vendor/tokscale-core"],
        check=True, capture_output=True, text=True,
    ).stdout.split()
    if len(staged) < 2 or staged[0] != "160000":
        raise SystemExit("vendor/tokscale-core is not a gitlink in the index")
    metadata = json.loads(subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1"],
        cwd=root, check=True, capture_output=True, text=True,
    ).stdout)
    agents = [p for p in metadata["packages"] if p["name"] == AGENT]
    if len(agents) != 1:
        raise SystemExit(f"cargo metadata lists {len(agents)} {AGENT} packages")
    agent_dir = Path(agents[0]["manifest_path"]).parent
    return (
        staged[1],
        (agent_dir / "Cargo.toml").read_text(),
        json.loads((agent_dir / BASELINE).read_text()),
        (root / "Cargo.lock").read_text(),
    )


def self_test() -> int:
    fixtures = Path(__file__).resolve().parent / "fixtures" / "agent-engine-pin"
    manifest = (fixtures / "agent-Cargo.toml").read_text()
    baseline = json.loads((fixtures / "baseline.json").read_text())
    pinned = agent_engine_rev(manifest)
    cases = [
        ("same engine passes", pinned, "ok.lock", False),
        ("another vendor commit fails", "0" * 40, "ok.lock", True),
        ("a second tokscale-core fails", pinned, "two-engines.lock", True),
        ("a dependency outside the baseline fails", pinned, "checksum-differs.lock", True),
    ]
    failed = 0
    for name, vendor, lock, should_fail in cases:
        errors = check(vendor, manifest, baseline, (fixtures / lock).read_text())
        if bool(errors) != should_fail:
            failed += 1
            print(f"FAIL {name}: {errors}")
        else:
            print(f"ok   {name}")
    return 1 if failed else 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()
    errors = check(*gather(args.root))
    for error in errors:
        print(f"error: {error}", file=sys.stderr)
    if not errors:
        print("agent engine pin and lock closure match")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
