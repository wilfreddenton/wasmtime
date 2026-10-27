"""Regression checks for linkers silently coalescing independently owned globals."""

from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from bundle_native_audit import audit_final, audit_inputs, definitions


class NativeAuditTests(unittest.TestCase):
    def test_cpp_weak_global_is_not_mistaken_for_rust(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            inventories = []
            for index in (1, 2):
                source = root / f"bundle{index}.cpp"
                obj = source.with_suffix(".o")
                source.write_text("namespace runtime { "
                                  f"__attribute__((weak)) int shared = {index}; }}\n")
                subprocess.run(["c++", "-c", str(source), "-o", str(obj)], check=True)
                symbols = definitions(obj)
                self.assertIn("_ZN7runtime6sharedE", symbols)
                inventories.append([{"path": str(obj), "definitions": symbols}])
            with self.assertRaisesRegex(RuntimeError, "collisions=.*_ZN7runtime6sharedE"):
                audit_inputs(inventories, ["first", "second"], root)

    def test_weak_collision_survives_successful_link(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            inventories = []
            objects = []
            for index in (1, 2):
                source = root / f"bundle{index}.c"
                obj = source.with_suffix(".o")
                source.write_text(
                    f"__attribute__((weak)) int shared_global = {index};\n"
                    f"int read{index}(void) {{ return shared_global; }}\n")
                subprocess.run(["cc", "-c", str(source), "-o", str(obj)], check=True)
                inventories.append([{"path": str(obj), "definitions": definitions(obj)}])
                objects.append(str(obj))
            main = root / "main.c"
            main.write_text("int read1(void); int read2(void);\n"
                            "int main(void) { return read1() != read2(); }\n")
            executable = root / "coalesced"
            subprocess.run(["cc", str(main), *objects, "-o", str(executable)], check=True)
            # Both reads now return one value, despite different initializers.
            subprocess.run([str(executable)], check=True)
            with self.assertRaisesRegex(RuntimeError, "collisions=.*shared_global"):
                audit_inputs(inventories, ["first", "second"], root)

    def test_unshared_native_definition_needs_owner(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(RuntimeError, "unknown.*unowned"):
                audit_inputs([[{"definitions": {"unowned": {}}}], []],
                             ["first", "second"], Path(directory))

    def test_runtime_allowance_does_not_allow_library_main(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(RuntimeError, "unknown.*main"):
                audit_inputs([[{"definitions": {"main": {}}}], []],
                             ["first", "second"], Path(directory))

    def test_final_unknown_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch("bundle_native_audit.definitions", return_value={"unowned": {}}):
                with self.assertRaisesRegex(RuntimeError, "unowned final"):
                    audit_final(Path("unused"), ["first", "second"], Path(directory))

    def test_macho_header_is_only_allowed_in_final_executable(self):
        with tempfile.TemporaryDirectory() as directory:
            symbols = {"_mh_execute_header": {"address": "100000000", "kind": "T"}}
            with patch("bundle_native_audit.sys.platform", "darwin"):
                with patch("bundle_native_audit.definitions", return_value=symbols):
                    audit_final(Path("unused"), ["first", "second"], Path(directory))
                with self.assertRaisesRegex(RuntimeError, "unknown.*_mh_execute_header"):
                    audit_inputs([[{"definitions": symbols}], []],
                                 ["first", "second"], Path(directory))

    def test_darwin_normalization_preserves_rust_mangling(self):
        result = subprocess.CompletedProcess([], 0, stdout=(
            "00000000 T _wasmtime_helper_first\n"
            "00000001 T __RNv_example\n"
            "00000002 T __jit_debug_register_code\n"))
        with patch("bundle_native_audit.sys.platform", "darwin"):
            with patch("bundle_native_audit.subprocess.run", return_value=result):
                self.assertEqual(set(definitions(Path("unused"))),
                                 {"wasmtime_helper_first", "_jit_debug_register_code"})


if __name__ == "__main__":
    unittest.main()
