#!/usr/bin/env python3
"""Join a selected place and use Luau from the same terminal."""
import argparse
from datetime import datetime
import json
import os
from pathlib import Path
import queue
import shlex
import select
import socket
import subprocess
from platform_process import group_options, stop_group
import sys
import threading
import time
from urllib.parse import parse_qs, urlsplit
import re

# Cloud-backed bytecode caches can block imports even when the source is local.
if sys.pycache_prefix is None:
    sys.pycache_prefix = str(Path.home()/'.cache/roblox-client-research/python')
os.environ.setdefault('PYTHONPYCACHEPREFIX', sys.pycache_prefix)

from join_allocation import write_private
from roblox_session import ROOT, SessionError
from run_world_luau import read_source
from script_console import send_source, send_request
from run_ussi import resolve_export_path


HELP = """Type Luau and press Enter to execute it. Examples:
  print(\"hello\", game:GetService(\"Players\").LocalPlayer.Name)
  = workspace:GetChildren()
  :load /absolute/path/script.luau
  :input 1 response       answer a pending rconsoleinput request
  :status 3               show a job's scheduler state and wait target
  :chat hello there       send typed text to RBXGeneral
  :chat --global hello    send typed text to RBXGlobal
  :saveinstance <output> [object/path] save a place or selected instance
    relative outputs go in rove exports/ in the project directory
    e.g. :saveinstance map.rbxlx Workspace/Model
  :paste                  paste multiple lines, then :end to execute
  :help                   show these commands
  :view                   open the Rove workspace for this session
  :quit                   leave and close this session
print/warn, returned values and errors appear here.
Globals and background tasks persist. Yielded results, prints and errors continue
appearing while scripts run. Automatic game scripts are partial.
"""


def log(message, kind="info"):
    print(f"[{datetime.now():%H:%M:%S}] [{kind}] {message}", flush=True)


def selection_command(target):
    if target.isascii() and target.isdigit() and 1 <= int(target) < 2**53:
        return [sys.executable, str(ROOT / 'scripts/select_test_place.py'),
                '--place-id', str(int(target))]
    parsed = urlsplit(target)
    query = parse_qs(parsed.query)
    codes = query.get('code', [])
    if (parsed.scheme == 'https' and parsed.netloc == 'www.roblox.com'
            and parsed.path in ('/share', '/share-links')
            and query.get('type') == ['Server'] and len(codes) == 1
            and re.fullmatch(r'[a-fA-F0-9]{32}', codes[0])):
        return [sys.executable, str(ROOT / 'scripts/select_private_server.py')]
    raise ValueError('Enter a numeric place ID or a Roblox /share?code=…&type=Server link.')


def ensure_console_free(path):
    # Check before changing the shared target/allocation files.
    from local_transport import probe
    try: probe(path, .3)
    except (FileNotFoundError, ConnectionRefusedError): return
    except (OSError, ValueError):
        raise SessionError('Cannot check the local console; another client may be using it.') from None
    raise SessionError('A client is already running. Close it before launching another.')


def prepare_target(target):
    command = selection_command(target)
    ensure_console_free(ROOT / 'secrets/client-script.sock')
    if not target.isdigit():
        write_private(ROOT / 'secrets/private-server-link.json', {'url': target})
    log('Checking account access and resolving target…', 'join')
    result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, timeout=120)
    if result.returncode:
        # These selector tools return sanitized error messages, never raw replies.
        try:
            error = json.loads(result.stdout)
        except ValueError:
            error = {}
        if not isinstance(error, dict):
            error = {}
        if error.get('error'):
            raise SessionError(error['error'])
        # A local crash (e.g. a missing folder) is not an account or place problem.
        if isinstance(error.get('error_type'), str) and error['error_type'].isidentifier():
            raise SessionError(f"Target selection crashed locally ({error['error_type']}). "
                               'See outputs/logs, or run the selector directly for details.')
        raise SessionError('Target selection failed. Check account access and the place ID or invite.')
    selected = json.loads((ROOT / 'outputs/gamejoin/target-selection.json').read_text())
    if not selected.get('is_playable'):
        raise SessionError('This account cannot play the selected place.')
    return selected['place_id']


def session_command(place_id, duration, *, physics=True, walk_for_minute=False, log_level='verbose'):
    if log_level not in ('verbose','normal','off'): raise ValueError('Invalid log level')
    if walk_for_minute and not physics:
        raise ValueError('The one-minute walk requires physics')
    command = [sys.executable, '-u', str(ROOT / 'scripts/game_session.py'),
               '--refresh', '--protocol-sync', '--luau-range', '--ticket',
               '--time-sync', '--bytecode-auth', '--require-spawn',
               '--duration', str(duration), '--progress', '--script-console',
               '--persistent-scripts', '--chat-console','--log-level',log_level]
    if physics:
        command.append('--physics')
    if walk_for_minute:
        command.append('--walk-for-minute')
    if place_id == 6252985844 and (ROOT / 'secrets/private-client-framework-rpc.json').is_file():
        command.append('--framework-init')
    return command


def show_result(result):
    for event in result.get('console_events', []):
        kind = event.get('kind')
        if kind == 'print':
            print(event.get('text', ''), end='', flush=True)
        elif kind == 'clear':
            if sys.stdout.isatty(): print('\x1b[2J\x1b[H', end='', flush=True)
        elif kind == 'title':
            title = ''.join(c for c in event.get('text', '') if c.isprintable())
            if sys.stdout.isatty(): print('\x1b]0;' + title + '\x07', end='', flush=True)
        elif kind == 'input':
            log(f"Console input requested: :input {event['id']} <response>", 'console')
        elif kind in ('create', 'destroy'):
            log('Opened.' if kind == 'create' else 'Closed.', 'console')
    for line in result.get('output', '').splitlines():
        log(line, 'luau')
    if result.get('values'):
        log(json.dumps(result['values'], ensure_ascii=False), 'return')
    if not result.get('ok'):
        log(result.get('error') or result.get('error_type') or 'Script failed', 'error')
    for error in result.get('task_errors', []):
        log(str(error), 'task error')


def submit(source, timeout_ms, follower=None):
    try:
        result=send_source(ROOT / 'secrets/client-script.sock', source, timeout_ms)
        if follower is None: show_result(result)
        else: follower.display({**result,'sequence':result.get('runtime_sequence')})
    except (OSError, ValueError) as error:
        log(str(error), 'error')


def console_input_source(line):
    parts = line.split(' ', 2)
    if len(parts) != 3 or not parts[1].isascii() or not parts[1].isdigit() or int(parts[1]) < 1:
        raise ValueError('Usage: :input <request ID> <response>')
    # Luau supports hexadecimal byte escapes rather than JSON's \u00XX form.
    literal = re.sub(r'\\u00([0-9a-fA-F]{2})', lambda m: '\\x' + m[1], json.dumps(parts[2], ensure_ascii=False))
    return f'rconsoleinputsubmit({int(parts[1])}, {literal})'


def show_job_status(job_id):
    result = send_request(ROOT/'secrets/client-script.sock', {'op':'status','job_id':job_id})
    # Pending status contains the submission's cached output. Replaying it
    # makes old warnings look like fresh failures on every status request.
    if result.get('completed') or not result.get('ok'):
        show_result(result)
    if not result.get('ok') and not result.get('completed'):return
    if result.get('completed'):
        log(f'Job {job_id}: {result.get("state", "completed")}')
        return
    diagnostic = result.get('diagnostic')
    if not diagnostic:
        log(f'Job {job_id}: pending; this worker has no wait diagnostics.')
        return
    log(f'Job {job_id}: {diagnostic["scheduler_state"]}')
    if 'resume_count' in diagnostic:
        log(f'Resumes: {diagnostic["resume_count"]}; '
            f'execution time: {diagnostic["execution_ms"]:.1f} ms; '
            f'slice interruptions: {diagnostic["slice_interruptions"]}; '
            f'yields: {diagnostic["yield_count"]}')
    wait = diagnostic.get('wait')
    if wait:
        log(f'Waiting for {wait["kind"]} {wait["name"]!r} on '
            f'{wait.get("parent_id",wait.get("instance_id","?"))}')
    for frame in diagnostic.get('frames', []):
        log(f'{frame["source"]}:{frame["line"]} {frame.get("function", "")}')


def submit_chat(text, follower=None, *, channel='RBXGeneral'):
    """Submit an explicitly typed terminal message through the host chat path."""
    if not text.strip():
        log('Usage: :chat [--global] <message text>', 'error')
        return
    try:
        # Keep typed chat separate from Luau source. The session host binds this
        # dedicated operation to its one-use input receipt before SendAsync.
        request = {'op': 'user_chat', 'text': text}
        if channel != 'RBXGeneral':
            request['channel'] = channel
        result = send_request(ROOT / 'secrets/client-script.sock', request)
        if follower is None:
            show_result(result)
        else:
            follower.display({**result, 'sequence': result.get('runtime_sequence')})
    except (OSError, ValueError) as error:
        log(str(error), 'error')


def parse_chat_command(line):
    """Return the typed message and explicit route for a :chat command."""
    text = line[len(':chat'):].lstrip()
    channel = 'RBXGeneral'
    if text == '--global' or text.startswith('--global '):
        channel = 'RBXGlobal'
        text = text[len('--global'):].lstrip()
    return text, channel


def parse_saveinstance_command(line):
    """Parse a local save target without interpreting it as shell syntax."""
    try:
        parts = shlex.split(line)
    except ValueError as error:
        raise ValueError('Usage: :saveinstance <output.rbxl|rbxlx|rbxm|rbxmx> [object/path]') from error
    if len(parts) not in (2, 3) or parts[0] != ':saveinstance' or not parts[1]:
        raise ValueError('Usage: :saveinstance <output.rbxl|rbxlx|rbxm|rbxmx> [object/path]')
    output = resolve_export_path(parts[1])
    return str(output), (parts[2] if len(parts) == 3 else None)


def saveinstance_command(socket_path, output, object_path=None):
    """Build a direct argv for the exporter; never start a command shell."""
    command = [sys.executable, '-u', str(ROOT / 'scripts/run_ussi.py'),
               '--socket', str(socket_path), '--output', str(output)]
    if object_path is not None:
        command.extend(['--object', object_path])
    return command


def submit_saveinstance(socket_path, output, object_path=None):
    """Run export orchestration while the existing client session stays up."""
    try:
        result = subprocess.run(saveinstance_command(socket_path, output, object_path),
                                cwd=ROOT, capture_output=True, text=True)
    except OSError as error:
        log(f'Could not start local save: {error}', 'error')
        return False
    message = (result.stdout or result.stderr or '').strip()
    message = ''.join(char for char in message if char in '\n\t' or char.isprintable())
    if len(message) > 16 * 1024:
        message = message[:16 * 1024] + '\n[save output truncated]'
    if result.returncode:
        log(message or 'Save failed; no output file was reported.', 'error')
        return False
    log(message or f'Saved {output}', 'save')
    return True


class OutputFollower:
    def __init__(self):
        self.cursor=0;self.seen=set();self.completed=set();self.available=True;self.last_poll=0

    def display(self, result):
        sequence=result.get('sequence')
        if type(sequence) is int:
            if sequence in self.seen:return
            self.seen.add(sequence)
            if len(self.seen)>512:self.seen.remove(min(self.seen))
        has_completion=bool(result.get('completed_jobs') or result.get('completed') or result.get('state')=='failed')
        initial={key:value for key,value in result.items()
                 if not has_completion or key not in ('values','error','error_type')}
        if has_completion:initial['ok']=True
        show_result(initial)
        for outcome in result.get('chat_outcomes',[]):
            log('Chat failed: '+outcome['error'] if outcome.get('error') else 'Chat: '+str(outcome.get('status')),
                'error' if outcome.get('error') or outcome.get('status')!='Success' else 'chat')
        jobs=list(result.get('completed_jobs',[]))
        if not jobs and (result.get('completed') or result.get('state')=='failed'):
            jobs=[result]
        for job in jobs:
            token=job.get('job_id')
            if token is not None and token in self.completed:continue
            if token is not None:self.completed.add(token)
            if len(self.completed)>512:self.completed.remove(min(self.completed))
            show_result(job)
            if job.get('ok') and not job.get('values'):log(f'Job {token} completed.' if token else 'Script completed.')
        if result.get('state')=='running':log(f"Job {result['job_id']} is running; waiting for its result.")

    def poll(self):
        now=time.monotonic()
        if not self.available or now-self.last_poll<.25:return []
        self.last_poll=now
        try:
            result=send_request(ROOT/'secrets/client-script.sock',{'op':'events','after':self.cursor})
            if not result.get('ok'):
                self.available=False
                return []
            if result.get('reset'):self.seen.clear();self.completed.clear()
            self.cursor=result['cursor']
            return [event for event in result.get('events',[]) if event['sequence'] not in self.seen]
        except (OSError,ValueError):return []


def console(process, timeout_ms, duration=0):
    lines = queue.Queue()

    def read_logs():
        for line in process.stdout:
            lines.put(line.rstrip('\n'))
        lines.put(None)

    reader = threading.Thread(target=read_logs, daemon=True)
    reader.start()
    ready, ended, prompt = False, False, False
    pasted = None
    paste_bytes = 0
    pending_input = bytearray()
    windows_input = None
    if os.name == 'nt':
        from platform_input import WindowsLineInput
        windows_input = WindowsLineInput()
    follower=OutputFollower()
    log('Joining. Scripts become available when your Player is replicated.', 'join')
    log(('No session time limit.' if duration == 0 else f'Session duration: {duration} seconds.')
        + ' :help for commands; :quit or Ctrl+C to leave.')
    while not ended:
        while not lines.empty():
            line = lines.get_nowait()
            if prompt:
                print(flush=True)
                prompt = False
            if line is None:
                ended = True
                break
            try:
                event = json.loads(line)
            except ValueError:
                log(line, 'session')
                continue
            if isinstance(event, dict) and 'diagnostic_log' in event:
                log('Session log: '+event['diagnostic_log'], 'log')
            elif isinstance(event, dict) and 'disconnect_category' in event:
                log(f"Session ended at {event.get('elapsed_seconds')}s: {event['disconnect_category']}", 'connection')
            elif isinstance(event, dict) and 'milestone' in event:
                ready = ready or event['milestone'] in ('own_player_in_players', 'character_spawned')
                log(f"{event.get('seconds', 0):.3f}s  {event['milestone'].replace('_', ' ')}", 'join')
            else:
                log(line, 'session')
        if ended:
            break
        if ready:
            events=follower.poll()
            if events and prompt:
                print(flush=True);prompt=False
            for event in events:follower.display(event)
        if not prompt:
            print('...> ' if pasted is not None else 'luau> ', end='', flush=True)
            prompt = True
        if b'\n' not in pending_input:
            if windows_input is not None:
                chunk = windows_input.read()
                if chunk is None:
                    time.sleep(.02)
                    continue
            else:
                if not select.select([sys.stdin], [], [], .1)[0]: continue
                chunk = os.read(sys.stdin.fileno(), 65536)
            if not chunk:
                break
            pending_input.extend(chunk)
            if b'\n' not in pending_input:
                if len(pending_input) > 128 * 1024:
                    log('Input exceeds 128 KiB; closing console.', 'error')
                    break
                continue
        raw, _, rest = pending_input.partition(b'\n')
        pending_input = bytearray(rest)
        if len(raw)>128*1024:
            log('Input exceeds 128 KiB; closing console.', 'error'); break
        line = raw.decode('utf-8').rstrip('\r')
        prompt = False
        if line.strip() == ':quit':
            break
        if pasted is not None:
            if line.strip() == ':end':
                submit('\n'.join(pasted), timeout_ms, follower)
                pasted = None
                paste_bytes = 0
            elif line.strip() == ':cancel':
                pasted = None
                paste_bytes = 0
                log('Paste cancelled.')
            else:
                paste_bytes += len(line.encode('utf-8')) + 1
                if paste_bytes > 128 * 1024:
                    log('Paste exceeds 128 KiB; cancelled.', 'error')
                    pasted = None
                    paste_bytes = 0
                else:
                    pasted.append(line)
            continue
        if not line.strip():
            continue
        if line.strip() == ':help':
            print(HELP, flush=True)
            continue
        if line.strip()==':view':
            import webbrowser
            if getattr(process,'rove_view_url',None):
                webbrowser.open(process.rove_view_url)
                log(process.rove_view_url,'view')
            else:
                try:
                    helper=subprocess.run([sys.executable,str(ROOT/'scripts/desktop_backend.py')],
                        cwd=ROOT,capture_output=True,text=True,timeout=15)
                    if helper.returncode==0 and helper.stdout.startswith('http://127.0.0.1:'):
                        process.rove_view_url=helper.stdout.strip()+'workspace'
                        webbrowser.open(process.rove_view_url)
                        log(process.rove_view_url,'view')
                    else:log('Could not open Rove. Start rove.py to attach.','error')
                except (OSError,subprocess.TimeoutExpired):
                    log('The display could not open; your terminal session is still connected.','error')
            continue
        if not ready:
            log('Your Player is still loading; wait for the join milestones.', 'join')
            continue
        if line.strip() == ':paste':
            pasted = []
            log('Paste Luau; :end to run or :cancel to discard.')
            continue
        if line == ':chat' or line.startswith(':chat '):
            text, channel = parse_chat_command(line)
            submit_chat(text, follower, channel=channel)
            continue
        if line == ':input' or line.startswith(':input '):
            try: submit(console_input_source(line), timeout_ms, follower)
            except ValueError as error: log(str(error), 'error')
            continue
        if line == ':saveinstance' or line.startswith(':saveinstance '):
            try:
                output, object_path = parse_saveinstance_command(line)
                submit_saveinstance(ROOT / 'secrets/client-script.sock', output, object_path)
            except ValueError as error:
                log(str(error), 'error')
            continue
        if line == ':status' or line.startswith(':status '):
            try:
                job_id = int(line.partition(' ')[2])
                if job_id < 1:raise ValueError('positive job ID required')
                show_job_status(job_id)
            except (OSError,ValueError) as error:log(f':status <job ID>: {error}', 'error')
            continue
        if line.startswith(':load '):
            try:
                source = read_source(Path(line[6:].strip().strip('\"\'')).expanduser())
            except (OSError, ValueError) as error:
                log(str(error), 'error')
                continue
        elif line.startswith(':'):
            log('Unknown command. Type :help.', 'error')
            continue
        else:
            source = 'return ' + line[1:] if line.startswith('=') else line
        submit(source, timeout_ms, follower)
    print(flush=True)


def stop(process):
    if process.poll() is None:
        import signal
        process.send_signal(signal.CTRL_BREAK_EVENT if os.name == 'nt' else signal.SIGINT)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            stop_group(process)
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                stop_group(process, force=True)
                process.wait()


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--place', help='Place ID or private-server link; otherwise prompt')
    parser.add_argument('--duration', type=int, default=0, help='Session seconds; 0 means no time limit (default)')
    parser.add_argument('--log-level',choices=('verbose','normal','off'),default='verbose',
                        help='Session timeline in outputs/logs (default: verbose)')
    parser.add_argument('--timeout-ms', type=int, default=1000, help='Per-script CPU budget (1..2000)')
    parser.add_argument('--no-web', action='store_true', help='Keep only the terminal console; do not open Rove')
    parser.add_argument('--no-physics', action='store_true', help='Disable continuous own-character gravity physics')
    parser.add_argument('--walk-for-minute', action='store_true',
                        help='Walk a small circuit for one minute after spawning, then leave')
    args = parser.parse_args(argv)
    if args.duration < 0 or not 1 <= args.timeout_ms <= 2000:
        parser.error('Duration must be nonnegative (0 means unlimited) and timeout 1..2000 ms')
    if args.walk_for_minute and args.no_physics:
        parser.error('--walk-for-minute requires physics; remove --no-physics')
    return args


def main():
    args = parse_args()
    if not sys.stdin.isatty():
        raise SystemExit('Run this in an interactive terminal')
    target = args.place or input('Roblox place ID or private server share link: ').strip()
    place_id = prepare_target(target)
    log(f'Target verified: place {place_id}. Requesting a fresh join.', 'join')
    process = subprocess.Popen(session_command(place_id, args.duration,
                                               physics=not args.no_physics,
                                               walk_for_minute=args.walk_for_minute,
                                               log_level=args.log_level), cwd=ROOT,
                               stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, text=True, bufsize=1,
                               **group_options())
    try:
        if not args.no_web:
            try:
                helper=subprocess.run([sys.executable,str(ROOT/'scripts/desktop_backend.py')],
                    cwd=ROOT,capture_output=True,text=True,timeout=15)
                if helper.returncode==0 and helper.stdout.startswith('http://127.0.0.1:'):
                    import webbrowser
                    process.rove_view_url=helper.stdout.strip()+'workspace'
                    webbrowser.open(process.rove_view_url)
                    log('Rove: '+process.rove_view_url, 'web')
                else:log('Open Rove to use the workspace. The terminal is still available.', 'web')
            except (OSError, subprocess.TimeoutExpired):
                log('Rove could not open. Use :view to retry; the terminal session is available.', 'web')
        console(process, args.timeout_ms, args.duration)
    finally:
        stop(process)
        process.stdout.close()
    log('Session ended.', 'join')
    return 0 if process.returncode in (0, -2, 130) else process.returncode


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (KeyboardInterrupt, EOFError):
        print('\nClosed.', flush=True)
        raise SystemExit(0)
    except (SessionError, ValueError, OSError, subprocess.TimeoutExpired) as error:
        log(str(error), 'error')
        raise SystemExit(1)
