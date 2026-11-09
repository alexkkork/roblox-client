"""First-run readiness and local account-session storage for the Rove UI."""
import os
from pathlib import Path
import platform
import sys
import uuid

from platform_files import open_nofollow, protect_private, read_private
from roblox_session import RobloxSession, SessionError
from client_session_lease import acquire_session_lease
from start_ui import runtime_files


def readiness(root):
    root=Path(root)
    required=runtime_files(root)
    checks=[{'label':label,'ready':path.is_file()} for label,path in required.items()]
    checks.insert(0,{'label':'Python 3.12 or newer','ready':sys.version_info>=(3,12)})
    configured=False
    try: configured=bool(read_private(root/'secrets/roblox-cookie.txt',32768).strip())
    except (OSError,ValueError): pass
    return {'launcherVersion':2,'platform':platform.system(),'checks':checks,
            'runtimeReady':all(c['ready'] for c in checks),'accountConfigured':configured,
            'setupFile':'Setup Rove.cmd' if os.name=='nt' else 'Setup Rove.command',
            'logsFolder':'outputs/logs'}


def connect_account(root, value, *, session_factory=RobloxSession):
    root=Path(root)
    if not isinstance(value,str) or not value.strip() or len(value)>32768:
        raise ValueError('Enter a valid account session value.')
    candidate=session_factory(cookie=value)
    # Validate in memory; a failed or challenged session never replaces the
    # current account file. No endpoint returns the stored credential.
    reply=candidate.request('users.roblox.com','/v1/users/authenticated')
    data=reply.get('data')
    # Say which check failed (never echo the value): a truncated paste, a
    # signed-out cookie and an account challenge need different fixes.
    length=len(candidate._cookie)
    hint=(f' The pasted value is {length} characters; complete sessions are usually over 500, '
          'so it may have been cut off while copying.' if length<500 else '')
    if reply.get('challenge_present'):
        raise SessionError('Roblox asked this account for an extra verification step. '
                           'Sign in once in a browser on this network, then copy the session again.')
    if reply.get('http_status')==401:
        raise SessionError('Roblox rejected this account session (signed out or expired). '
                           'Sign in again and copy a fresh session.'+hint)
    if (reply.get('http_status')!=200 or not isinstance(data,dict)
            or type(data.get('id')) is not int or data['id']<=0):
        raise SessionError(f"This account session could not be verified (HTTP {reply.get('http_status')}). "
                           'Check it and try again.'+hint)
    directory=root/'secrets'; directory.mkdir(exist_ok=True); protect_private(directory)
    # Never switch the account underneath a running game or another writer.
    with acquire_session_lease(root):
        temporary=directory/('account-'+uuid.uuid4().hex+'.tmp')
        try:
            fd=open_nofollow(temporary,os.O_CREAT|os.O_EXCL|os.O_WRONLY)
            with os.fdopen(fd,'w',encoding='utf-8') as output:
                output.write(candidate._cookie); output.flush(); os.fsync(output.fileno())
            destination=directory/'roblox-cookie.txt'
            if destination.is_symlink(): raise SessionError('The account file must be a regular local file.')
            os.replace(temporary,destination)
        finally:
            temporary.unlink(missing_ok=True)
    return {'connected':True,'name':str(data.get('name','')),'displayName':str(data.get('displayName') or data.get('name',''))}
