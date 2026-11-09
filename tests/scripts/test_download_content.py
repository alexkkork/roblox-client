"""Offline tests for the built-in content downloader (no network)."""
import io
from pathlib import Path
import sys
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'scripts'))
import download_content


def zip_bytes(entries):
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, 'w') as archive:
        for name, data in entries.items():
            archive.writestr(zipfile.ZipInfo(name), data)
    return buffer.getvalue()


class DownloadContentTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='rove content ')
        self.addCleanup(temporary.cleanup)
        self.dest = Path(temporary.name) / 'out'

    def test_manifest_pairs_packages_with_md5(self):
        text = ('v0\r\nRobloxApp.zip\r\n' + 'a' * 32 + '\r\n123\r\n456\r\n'
                'content-fonts.zip\r\n' + 'B' * 32 + '\r\n7\r\n8\r\n')
        self.assertEqual(download_content.parse_manifest(text),
                         {'RobloxApp.zip': 'a' * 32, 'content-fonts.zip': 'b' * 32})

    def test_windows_backslash_paths_are_normalised(self):
        # Roblox's packages are built on Windows; unzip rejects these paths.
        data = zip_bytes({'\\families\\Arial.json': b'{}', 'Arial.ttf': b'font', 'dir\\': b''})
        self.assertEqual(download_content.safe_extract(data, self.dest), 2)
        self.assertEqual((self.dest / 'families' / 'Arial.json').read_bytes(), b'{}')
        self.assertEqual((self.dest / 'Arial.ttf').read_bytes(), b'font')

    def test_path_escapes_are_refused(self):
        for name in ('..\\evil.txt', '../evil.txt', 'a/../../evil.txt', 'a\\..\\..\\evil.txt'):
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, 'unsafe path'):
                download_content.safe_extract(zip_bytes({name: b'x'}), self.dest)
        self.assertFalse((self.dest.parent / 'evil.txt').exists())

    def test_package_map_matches_render_asset_layout(self):
        folders = set(download_content.PACKAGES.values())
        self.assertIn('content/textures', folders)
        self.assertIn('content/fonts', folders)
        self.assertIn('content/avatar', folders)


class RenderContentDirTests(unittest.TestCase):
    def test_downloaded_content_is_preferred_over_mac_app(self):
        import render_assets
        with tempfile.TemporaryDirectory() as temporary:
            downloaded = Path(temporary) / 'content'
            original = render_assets.DOWNLOADED_CONTENT_DIR
            try:
                render_assets.DOWNLOADED_CONTENT_DIR = downloaded
                self.assertEqual(render_assets.default_content_dir(), render_assets.MAC_CONTENT_DIR)
                downloaded.mkdir()
                self.assertEqual(render_assets.default_content_dir(), downloaded)
            finally:
                render_assets.DOWNLOADED_CONTENT_DIR = original


if __name__ == '__main__':
    unittest.main()
