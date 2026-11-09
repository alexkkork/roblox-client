#!/usr/bin/env python3
"""One entry point for building, checking, and running Rove."""
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT / 'scripts'))
from platform_environment import runtime_environment

HELP = """Rove — setup, check, run

  python rove.py setup [--jobs 2] [--native-prefix PATH] [--with-ui]
  python rove.py check
  python rove.py content          (built-in meshes/textures for the renderer on Windows/Linux)
  python rove.py terminal [--place ID_OR_INVITE]
  python rove.py ui [--port PORT]

With no command: browser UI on macOS; terminal on Windows and Linux.
Setup builds the runtime, plus the browser UI on macOS. On other systems,
add --with-ui only if you want the browser. No Windows app is required.
Use `py` on Windows or `python3` on macOS/Linux in place of `python`.
Prerequisites and troubleshooting: docs/setup.md
"""


def launch_command(arguments, root=ROOT, platform=sys.platform):
    """Resolve commands without importing the network client or reading an account."""
    arguments = list(arguments)
    default = 'ui' if platform == 'darwin' else 'terminal'
    action = arguments.pop(0) if arguments and not arguments[0].startswith('-') else default
    if '--no-web' in arguments and action == default:
        action = 'terminal'
    if action not in ('setup', 'check', 'terminal', 'ui', 'content'):
        raise ValueError(f'Unknown command: {action}. Run python rove.py --help.')
    windows = platform == 'win32'
    if action == 'content':
        # Standard library only, so it also works before setup.
        return [sys.executable, str(root / 'scripts' / 'download_content.py'), *arguments]
    if action == 'setup':
        script = 'setup_windows.py' if windows else 'setup_terminal.py'
        if platform == 'darwin' and '--with-ui' not in arguments:
            arguments.insert(0, '--with-ui')
        return [sys.executable, str(root / 'scripts' / script), *arguments]
    python = root / ('.venv/Scripts/python.exe' if windows else '.venv/bin/python')
    if not python.is_file():
        raise ValueError('Setup is missing. Run python rove.py setup first (see docs/setup.md).')
    script = {'check': 'check_portability.py', 'terminal': 'terminal_client.py', 'ui': 'launcher.py'}[action]
    if action == 'terminal' and '--no-web' not in arguments:
        arguments.insert(0, '--no-web')
    if action == 'ui' and not all((root / 'ui' / name).is_file()
                                 for name in ('avatar-viewer.js', 'world-viewer.js')):
        raise ValueError('Browser UI is not built. Run python rove.py setup --with-ui first.')
    return [str(python), str(root / 'scripts' / script), *arguments]


def prepare_local_folders(root=ROOT):
    """Create the git-ignored output folders a fresh clone lacks.

    Session tools write reports into outputs/ subfolders and would otherwise fail
    with FileNotFoundError on the first Play. Owner-only on POSIX like secrets/.
    """
    outputs = Path(root) / 'outputs'
    for folder in (outputs, outputs / 'logs', outputs / 'gamejoin'):
        if folder.is_symlink():
            raise ValueError(f'{folder} must not be a link.')
        folder.mkdir(exist_ok=True)
    if os.name != 'nt':
        outputs.chmod(0o700)


def main(argv=None):
    arguments = list(sys.argv[1:] if argv is None else argv)
    if arguments in (['-h'], ['--help'], ['help']):
        print(HELP)
        return 0
    if sys.version_info < (3, 12):
        print('Rove needs Python 3.12 or newer.', file=sys.stderr)
        return 1
    try:
        command = launch_command(arguments)
        prepare_local_folders()
        env =os.environ.copy() if arguments[:1] == ['setup'] else runtime_environment(ROOT)
        env['PYTHONUTF8'] = '1'
        child = subprocess.Popen(command, cwd=ROOT, env=env)
        try:
            return child.wait()
        except KeyboardInterrupt:
            # The foreground child receives Ctrl+C too and owns session cleanup.
            return child.wait()
    except (OSError, ValueError) as error:
        print(f'Rove: {error}', file=sys.stderr)
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
