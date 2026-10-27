#!/usr/bin/env python3
"""Build independently resolved Wasmtime graphs and exercise them in one process."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tomllib


BASELINE = "bfdee9ba9a483915dc95432708cf7db8281adbc1"
TOOLCHAIN = "1.99.1"


def native_suffix(version):
    return "".join(c if c.isalnum() and c.isascii() else "_" for c in version)


def run(command, cwd, env, log):
    print("Running:", " ".join(map(str, command)), flush=True)
    with log.open("w") as output:
        result = subprocess.run(command, cwd=cwd, env=env, stdout=output, stderr=subprocess.STDOUT)
    if result.returncode:
        print(log.read_text()[-12000:], flush=True)
        raise RuntimeError(f"command failed ({result.returncode}); full log: {log}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--configuration", choices=["minimal", "kontor"], default="minimal")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN)
    env.pop("RUSTC_WRAPPER", None)
    env.pop("CARGO_ENCODED_RUSTFLAGS", None)
    baseline = output / f"baseline-{BASELINE}"
    if not baseline.exists():
        archive = output / "baseline.tar"
        with archive.open("wb") as target:
            subprocess.run(["git", "archive", BASELINE], cwd=root, stdout=target, check=True)
        baseline.mkdir()
        with tarfile.open(archive) as source:
            source.extractall(baseline, filter="data")
        archive.unlink()

    versions = [tomllib.loads((source / "Cargo.toml").read_text())["workspace"]["package"]["version"]
                for source in (baseline, root)]
    if len({native_suffix(version) for version in versions}) != 2:
        raise RuntimeError("bundle versions must produce different native symbol suffixes")

    artifacts = []
    reports = []
    for name, source, rate in (("first", baseline, 1), ("second", root, 7)):
        project = output / name
        project.mkdir(exist_ok=True)
        features = ["std", "runtime", "cranelift", "wat", "async", "component-model"]
        if args.configuration == "kontor":
            package = tomllib.loads((source / "crates/wasmtime/Cargo.toml").read_text())
            features = sorted(set(package["features"]["default"] + ["wave", "component-model-async"]) - {"debug-builtins"})
        elif name == "second":
            features.append("backtrace")
        manifest = (
            f'[package]\nname = "bundle_{name}"\nversion = "0.0.0"\nedition = "2024"\n'
            '[workspace]\n[lib]\npath = "bundle.rs"\n'
            '[profile.release]\ndebug = 0\n'
            '[dependencies.wasmtime]\n'
            f'path = {json.dumps(str(source / "crates/wasmtime"))}\n'
            'default-features = false\n'
            f'features = {json.dumps(features)}\n'
        )
        (project / "Cargo.toml").write_text(manifest)
        shutil.copyfile(root / "tests/bundle-isolation/bundle.rs", project / "bundle.rs")
        if not (project / "Cargo.lock").exists():
            shutil.copyfile(source / "Cargo.lock", project / "Cargo.lock")
        target = project / "target"
        build_env = dict(env, CARGO_TARGET_DIR=str(target),
                         RUSTFLAGS=f"-Cmetadata=kontor_bundle_{name}", BUNDLE_LIFT_RATE=str(rate))
        # Preserve the source lock's versions while removing unused workspace
        # packages; the actual compile consumes this independent lock unchanged.
        offline = ["--offline"] if args.offline else []
        run(["cargo", "metadata", *offline, "--format-version", "1"], project,
            build_env, project / "metadata.log")
        run(["cargo", "build", "--locked", *offline, "--release", "-j2", "--message-format=json"],
            project, build_env, project / "build.log")
        found = []
        for line in (project / "build.log").read_text().splitlines():
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            if message.get("reason") == "compiler-artifact" and message["target"]["name"] == f"bundle_{name}":
                found.extend(Path(path) for path in message["filenames"] if path.endswith(".rlib"))
        if len(found) != 1:
            raise RuntimeError(f"expected one current bundle rlib, got {found}")
        artifacts.append((name, found[0], target / "release/deps"))
        version = tomllib.loads((source / "Cargo.toml").read_text())["workspace"]["package"]["version"]
        reports.append({"bundle": name, "wasmtime_version": version, "features": features,
                        "lock_sha256": hashlib.sha256((project / "Cargo.lock").read_bytes()).hexdigest()})

    executable = output / "dual-vm"
    command = ["rustc", "--edition=2024", str(root / "tests/bundle-isolation/host.rs"),
               "-Cdebuginfo=0", "-o", str(executable)]
    for name, artifact, deps in artifacts:
        command.extend(["--extern", f"bundle_{name}={artifact}", "-L", f"dependency={deps}"])
    run(command, root, env, output / "link.log")
    run([str(executable)], root, env, output / "run.log")
    symbols = subprocess.check_output(["nm", "-g", str(executable)], text=True)
    (output / "symbols.txt").write_text(symbols)
    defined = {}
    for line in symbols.splitlines():
        parts = line.split()
        if len(parts) == 3 and parts[1].upper() != "U":
            defined[parts[2].removeprefix("_")] = parts[:2]
    for report in reports:
        suffix = native_suffix(report["wasmtime_version"])
        helper = f"wasmtime_libunwind_{suffix}"
        # Darwin's nm spells C symbols with an extra leading underscore.
        match = defined.get(helper)
        if match is None:
            raise RuntimeError(f"missing independently owned native helper for {suffix}")
        report["native_helper"] = {"name": helper, "address": match[0], "kind": match[1]}
    for symbol in ("_jit_debug_descriptor", "_jit_debug_register_code"):
        if symbol in defined or "_" + symbol in defined:
            raise RuntimeError(f"unsafe shared debugger global: {symbol}")
    report = {"compiler": subprocess.check_output(["rustc", "-Vv"], env=env, text=True),
              "baseline": BASELINE, "configuration": args.configuration,
              "bundles": reports, "result": (output / "run.log").read_text()}
    (output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2), flush=True)


if __name__ == "__main__":
    main()
