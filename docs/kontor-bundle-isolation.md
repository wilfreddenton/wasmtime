# Kontor's retained Wasmtime builds

This fork uses the immutable package version `51.0.0-kontor.1` for its first
retained Kontor build. A later source or feature/configuration identity that needs
native isolation must get a different package version. Do not reuse this version
for a different retained implementation or derive identity from a moving branch.

The versioned-export macro appends the package version to Rust imports/exports.
The Wasmtime C helper and Windows fiber builds obtain `VERSIONED_SUFFIX` from that
same macro. Changing Rust crate metadata alone does not change those native names.
The fork version and its exact lock, features, target, compiler and codegen policy
must all be recorded by the node's eventual bundle build. No lifting prices or
Rust-layout accounting changes are introduced here.

Run the actual dual-VM test with Rust 1.99.1, Python 3.11 or later, and `nm`:

```sh
python3 ci/test-bundle-isolation.py --output target/bundle-isolation
python3 ci/test-bundle-isolation.py --configuration kontor --output target/bundle-isolation-kontor
```

For a fully populated local Cargo cache, add `--offline`. The immutable baseline
commit must be present locally. Each facade receives its own Cargo lock and target
directory, and the final executable links the resulting rlibs directly. The second
graph enables an additional backtrace feature in the minimal case. The `kontor`
case uses the node's default features plus wave, excluding debug-builtins, in both
graphs. The facades have distinct
portable lifting prices, so runtime dispatch is observable. The host exercises
memory and stack traps, fuel yielding on async fibers, alternating VM calls, and
concurrent execution after both signal-handler chains have been installed.

The test never unloads process handlers. Both libraries remain resident for the
executable's lifetime. `report.json` records compiler identity, dependency lock
hashes, features and observed results. `symbols.txt` preserves the final symbol
map; the test checks that both versioned libunwind helpers are present.

The native GDB registration feature `debug-builtins` is forbidden in protocol
graphs. Its weak process-global descriptor would otherwise be mutated under two
independent Rust locks. Kontor leaves `Config::debug_info` false; bundle adapters
must assert that invariant. The harness rejects debugger globals in its final
symbol map. The node build must enforce both the feature and symbol restriction,
and qualify P1's deliberate feature delta with trap, backtrace and error-text
comparisons against the original default-feature build.

The operating system's libunwind registration functions are deliberately shared
services. Each VM owns the code registration it creates, and delegates registration
and deregistration to that process service. The concurrent test repeatedly creates,
traps through, and drops both VMs' code registrations. VM trap handlers remain
resident even after individual engines and code objects are dropped.

This is the Wasmtime prerequisite for Kontor's protocol build, not proof that the
entire node dependency closure is isolated. The node build must separately audit
blst, TLS crypto, secp256k1, compression and other native dependencies. Identical
objects may share only under an explicit ownership/global-state contract. Different
objects need separate native names. Storage remains a node-owned service.
