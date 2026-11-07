import subprocess
import sys
import tempfile
from pathlib import Path
import unittest
from appimage_layout import PROJECT, prepare, verify, png_size, desktop_values
from repack_appimage import prepare_permissions

APP_ID = 'com.kalzi.videotranscoder'


class AppImageLayoutTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'Video Transcoder.AppDir'
        (self.root / 'usr/bin').mkdir(parents=True)
        (self.root / 'usr/bin/video-transcoder').write_bytes(b'binary fixture')
        (self.root / 'AppRun').write_bytes(b'launcher fixture')
        (self.root / 'Video Transcoder.desktop').write_text('[Desktop Entry]\nIcon=video-transcoder\n')
        (self.root / 'Video Transcoder.png').write_bytes(b'old alias')
        (self.root / '.DirIcon').symlink_to('/nonexistent/build-host/icon.png')

    def test_canonical_identity_and_real_icon_sizes(self):
        cache = self.root / 'usr/share/icons/hicolor/icon-theme.cache'
        cache.parent.mkdir(parents=True)
        cache.write_bytes(b'obsolete icon names')
        prepare(self.root)
        self.assertFalse(cache.exists())
        values = desktop_values(self.root / f'{APP_ID}.desktop')
        self.assertEqual(values['Name'], 'Video Transcoder')
        self.assertEqual(values['Icon'], APP_ID)
        self.assertEqual(values['StartupWMClass'], APP_ID)
        self.assertEqual(values['Exec'], 'video-transcoder')
        self.assertEqual(png_size(self.root / f'{APP_ID}.png'), (512, 512))
        self.assertFalse((self.root / 'Video Transcoder.png').exists())
        for size in [32, 128, 256, 512]:
            self.assertEqual(png_size(self.root / f'usr/share/icons/hicolor/{size}x{size}/apps/{APP_ID}.png'), (size, size))
        self.assertFalse((self.root / 'usr/share/icons/hicolor/index.theme').exists())

    def test_restrictive_build_permissions_do_not_make_bundle_private(self):
        self.root.chmod(0o700)
        binary = self.root / 'usr/bin/video-transcoder'
        binary.chmod(0o700)
        data = self.root / 'Video Transcoder.png'
        data.chmod(0o600)
        external = Path(self.temp.name) / 'external-file'
        external.write_bytes(b'not bundled')
        external.chmod(0o600)
        (self.root / 'external-link').symlink_to(external)
        prepare_permissions(self.root)
        self.assertEqual(self.root.stat().st_mode & 0o777, 0o755)
        self.assertEqual(binary.stat().st_mode & 0o777, 0o755)
        self.assertEqual(data.stat().st_mode & 0o777, 0o644)
        self.assertEqual(external.stat().st_mode & 0o777, 0o600)

    def test_idempotent_and_relocatable(self):
        prepare(self.root)
        prepare(self.root)
        moved = self.root.with_name('renamed folder with spaces')
        self.root.rename(moved)
        verify(moved)

    def test_unknown_desktop_entry_is_not_deleted(self):
        foreign = self.root / 'another-app.desktop'
        foreign.write_text('do not touch')
        with self.assertRaises(ValueError):
            prepare(self.root)
        self.assertEqual(foreign.read_text(), 'do not touch')

    def test_detects_missing_or_mismatched_theme_icon(self):
        prepare(self.root)
        icon = self.root / f'usr/share/icons/hicolor/32x32/apps/{APP_ID}.png'
        icon.write_bytes((PROJECT / 'src-tauri/icons/icon.png').read_bytes())
        with self.assertRaises(ValueError):
            verify(self.root)
        icon.unlink()
        with self.assertRaises((ValueError, FileNotFoundError)):
            verify(self.root)

    def test_existing_output_cannot_be_overwritten_accidentally(self):
        output = Path(self.temp.name) / 'existing.AppImage'
        output.write_bytes(b'previous build')
        result = subprocess.run([sys.executable, str(PROJECT / 'scripts/repack_appimage.py'),
            '--appdir', str(self.root), '--output', str(output)], capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(output.read_bytes(), b'previous build')


if __name__ == '__main__':
    unittest.main()
