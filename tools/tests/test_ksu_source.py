import contextlib
import io
from pathlib import Path
import re
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "third_party/kernelsu/scripts"))
import apply_patches
import gen_patches
import ksu_common as C


class KsuSourceTest(unittest.TestCase):
    def test_published_series_matches_generator(self):
        self.assertEqual(C.read_series(C.SERIES_PATH),
                         [spec["name"] for spec in gen_patches.PATCHES])

    def test_published_source_matches_manifest(self):
        manifest = C.read_json(C.MANIFEST_PATH)
        self.assertEqual(C.tree_sha256(C.APPLIED_DIR), manifest["applied"]["treeSha256"])

    def test_declared_transaction_sources_are_exported(self):
        source = C.APPLIED_DIR / "userspace/ksud/src"
        module = (source / "module.rs").read_text(encoding="utf-8")
        paths = re.findall(r'#\[path = "(module_\w+_transaction\.rs)"\]', module)
        self.assertEqual(len(paths), 2)
        for name in paths:
            with self.subTest(source=name):
                self.assertTrue((source / name).is_file(), name)

    def assert_rejected_before_clean(self, series):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "upstream").mkdir()
            sentinel = root / "applied/existing-source.rs"
            C.write_bytes(sentinel, b"existing source\n")
            C.write_text(root / "patches/7k/series", "\n".join(series) + "\n")
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as error:
                apply_patches.build(root, clean=True, check_only=False)
            self.assertEqual(error.exception.code, apply_patches.EXIT_INPUT)
            self.assertEqual(sentinel.read_bytes(), b"existing source\n")

    def test_missing_transaction_patch_is_rejected_before_clean(self):
        series = [spec["name"] for spec in gen_patches.PATCHES]
        self.assert_rejected_before_clean(series[:-2])

    def test_reordered_patch_is_rejected_before_clean(self):
        series = [spec["name"] for spec in gen_patches.PATCHES]
        series[-2:] = reversed(series[-2:])
        self.assert_rejected_before_clean(series)

    def test_extra_patch_is_rejected_before_clean(self):
        series = [spec["name"] for spec in gen_patches.PATCHES]
        self.assert_rejected_before_clean(series + ["0028-unselected.patch"])


if __name__ == "__main__":
    unittest.main()
