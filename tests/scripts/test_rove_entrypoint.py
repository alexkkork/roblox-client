"""Fresh-checkout command routing; never opens a game or reads credentials."""
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('rove_entrypoint', ROOT / 'rove.py')
rove = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rove)


class RoveEntrypointTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='rove commands ')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        for name in ('.venv/Scripts/python.exe', '.venv/bin/python',
                     'ui/avatar-viewer.js', 'ui/world-viewer.js'):
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.touch()

    def test_windows_default_is_terminal_and_retains_spaced_argument(self):
        command = rove.launch_command(['--place', 'an invite with spaces'], self.root, 'win32')
        self.assertEqual(command[0], str(self.root / '.venv/Scripts/python.exe'))
        self.assertEqual(command[-3:], ['--no-web', '--place', 'an invite with spaces'])
        self.assertEqual(Path(command[1]).name, 'terminal_client.py')

    def test_mac_keeps_ui_and_has_explicit_terminal(self):
        self.assertEqual(Path(rove.launch_command([], self.root, 'darwin')[1]).name, 'launcher.py')
        self.assertEqual(Path(rove.launch_command(['terminal'], self.root, 'darwin')[1]).name,
                         'terminal_client.py')

    def test_linux_does_not_require_frontend(self):
        (self.root / 'ui/world-viewer.js').unlink()
        self.assertIn('--no-web', rove.launch_command([], self.root, 'linux'))
        with self.assertRaisesRegex(ValueError, '--with-ui'):
            rove.launch_command(['ui'], self.root, 'linux')

    def test_setup_works_without_virtualenv_and_preserves_prefix(self):
        (self.root / '.venv/Scripts/python.exe').unlink()
        arguments = ['setup', '--native-prefix', 'C:/native libraries']
        command = rove.launch_command(arguments, self.root, 'win32')
        self.assertEqual(Path(command[1]).name, 'setup_windows.py')
        self.assertEqual(command[-2:], ['--native-prefix', 'C:/native libraries'])
        self.assertEqual(arguments[0], 'setup')
        self.assertIn('--with-ui', rove.launch_command(['setup'], self.root, 'darwin'))

    def test_missing_setup_and_unknown_command_have_recovery_steps(self):
        (self.root / '.venv/Scripts/python.exe').unlink()
        with self.assertRaisesRegex(ValueError, 'rove.py setup'):
            rove.launch_command([], self.root, 'win32')
        with self.assertRaisesRegex(ValueError, '--help'):
            rove.launch_command(['typo'], self.root, 'win32')

    def test_content_works_without_virtualenv(self):
        (self.root / '.venv/bin/python').unlink()
        command = rove.launch_command(['content', '--dest', '/tmp/x'], self.root, 'linux')
        self.assertEqual(command[0], sys.executable)
        self.assertEqual(Path(command[1]).name, 'download_content.py')
        self.assertEqual(command[-2:], ['--dest', '/tmp/x'])

    def test_fresh_clone_gets_output_folders(self):
        # Regression: outputs/ is git-ignored, so Play failed with FileNotFoundError.
        rove.prepare_local_folders(self.root)
        rove.prepare_local_folders(self.root)  # idempotent
        for name in ('outputs', 'outputs/logs', 'outputs/gamejoin'):
            self.assertTrue((self.root / name).is_dir(), name)
        if sys.platform != 'win32':
            self.assertEqual((self.root / 'outputs').stat().st_mode & 0o777, 0o700)

    def test_help_does_not_require_account_or_dependencies(self):
        result = subprocess.run([sys.executable, str(ROOT / 'rove.py'), '--help'],
                                cwd=self.root, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('rove.py setup', result.stdout)
        self.assertIn('rove.py terminal', result.stdout)


if __name__ == '__main__':
    unittest.main()
