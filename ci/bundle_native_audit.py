"""Fail closed on native definitions that have no declared bundle owner."""

import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys


# These are compiler/linker support, not a shared native-library exemption.
RUNTIME_SYMBOLS = {
    "DW.ref.rust_eh_personality", "rust_eh_personality", "main", "_start",
    "__TMC_END__", "__bss_end__", "__bss_start", "__bss_start__", "__dso_handle",
    "__end__", "_bss_end__", "_edata", "_end", "_fini", "_init",
    "__aarch64_cpu_features", "__aarch64_have_lse_atomics", "__init_cpu_features",
    "__pthread_atfork", "__udivti3", "__umodti3",
}
INPUT_RUNTIME_SYMBOLS = {"DW.ref.rust_eh_personality"}


def definitions(path):
    result = subprocess.run(["nm", "-g", str(path)], text=True, capture_output=True, check=True)
    symbols = {}
    for line in result.stdout.splitlines():
        fields = line.split()
        if len(fields) != 3 or not re.fullmatch(r"[0-9a-fA-F]+", fields[0]):
            continue
        address, kind, name = fields
        if kind.upper() == "U":
            continue
        if sys.platform == "darwin":
            name = name.removeprefix("_")
        # C++ also uses _ZN. Only legacy Rust's hash-bearing form is exempt.
        if re.fullmatch(r"_ZN.+17h[0-9a-f]{16}E(?:\.llvm\.\d+)?", name) or re.match(r"_R[A-Z]", name):
            continue
        # LLVM promotes anonymous constants to globals during optimization.
        if re.fullmatch(r"anon\.[0-9a-f]+\.\d+(?:\.llvm\.\d+)?", name):
            continue
        symbols[name] = {"address": address, "kind": kind}
    return symbols


def inputs(messages):
    paths = set()
    for message in messages:
        if message.get("reason") == "compiler-artifact":
            paths.update(Path(p) for p in message["filenames"] if p.endswith(".rlib"))
        elif message.get("reason") == "build-script-executed":
            # Include native archives even if dead stripping would hide their
            # symbols. Only current build-script outputs are considered.
            paths.update(Path(message["out_dir"]).rglob("*.a"))
    if not paths:
        raise RuntimeError("no current native/Rust archive inputs to audit")
    return [{"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
             "definitions": definitions(path)} for path in sorted(paths)]


def native_owner(name, suffix):
    if name in RUNTIME_SYMBOLS:
        return "compiler/linker runtime"
    if name.startswith("wasmtime_") and name.endswith("_" + suffix):
        return "bundle " + suffix
    return None


def audit_inputs(inventories, suffixes, output):
    (output / "native-inputs.json").write_text(json.dumps(inventories, indent=2) + "\n")
    symbol_sets = [{name for item in inventory for name in item["definitions"]}
                   for inventory in inventories]
    collisions = (symbol_sets[0] & symbol_sets[1]) - INPUT_RUNTIME_SYMBOLS
    unknown = {suffix: sorted(name for name in names
                             if name not in INPUT_RUNTIME_SYMBOLS
                             and native_owner(name, suffix) != "bundle " + suffix)
               for suffix, names in zip(suffixes, symbol_sets)}
    unknown = {suffix: names for suffix, names in unknown.items() if names}
    if collisions or unknown:
        raise RuntimeError(f"unowned native inputs: collisions={sorted(collisions)}, unknown={unknown}")


def audit_final(executable, suffixes, output):
    symbols = definitions(executable)
    owners = {name: next((owner for suffix in suffixes
                         if (owner := native_owner(name, suffix))), None) for name in symbols}
    (output / "native-final.json").write_text(json.dumps(
        {name: dict(symbol, owner=owners[name]) for name, symbol in symbols.items()}, indent=2) + "\n")
    unknown = [name for name, owner in owners.items() if owner is None]
    if unknown:
        raise RuntimeError(f"unowned final native definitions: {unknown}")
    return symbols
