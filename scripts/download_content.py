#!/usr/bin/env python3
"""Download the official client's built-in content packages for the renderer.

Windows and Linux have no installed Roblox.app to read rbxasset:// files from.
This fetches the public, unauthenticated content packages for the current
client version from setup.rbxcdn.com, verifies each against the MD5 in
rbxPkgManifest.txt, and unpacks them into the same layout the official client
uses. No account or cookie is involved.
"""
import argparse
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import shutil
import sys
import tempfile
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_DEST = ROOT / 'roblox-content'
VERSION_URL = 'https://clientsettingscdn.roblox.com/v2/client-version/WindowsPlayer'
CDN = 'https://setup.rbxcdn.com'
USER_AGENT = 'Rove-content/1.0'
MAX_PACKAGE = 512 * 1024 * 1024

# package -> folder under the destination (the official installer's layout)
PACKAGES = {
    'content-avatar.zip': 'content/avatar',
    'content-configs.zip': 'content/configs',
    'content-fonts.zip': 'content/fonts',
    'content-models.zip': 'content/models',
    'content-sky.zip': 'content/sky',
    'content-sounds.zip': 'content/sounds',
    'content-textures2.zip': 'content/textures',
    'content-textures3.zip': 'PlatformContent/pc/textures',
    'content-terrain.zip': 'PlatformContent/pc/terrain',
    'content-platform-fonts.zip': 'PlatformContent/pc/fonts',
    'extracontent-models.zip': 'ExtraContent/models',
    'extracontent-textures.zip': 'ExtraContent/textures',
}


def fetch(url, limit=MAX_PACKAGE):
    request = urllib.request.Request(url, headers={'User-Agent': USER_AGENT})
    with urllib.request.urlopen(request, timeout=60) as response:
        data = response.read(limit + 1)
    if len(data) > limit:
        raise ValueError(f'{url} exceeds the size limit')
    return data


def parse_manifest(text):
    """Return {package name: md5} from rbxPkgManifest.txt (name, md5, sizes...)."""
    lines = [line.strip() for line in text.replace('\r', '').split('\n')]
    result = {}
    for index, line in enumerate(lines[:-1]):
        if line.endswith('.zip') and len(lines[index + 1]) == 32:
            result[line] = lines[index + 1].lower()
    return result


def safe_extract(data, destination):
    """Extract a Windows-built zip: backslash separators, no path escapes."""
    destination = Path(destination).resolve()
    written = 0
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        for info in archive.infolist():
            name = info.filename.replace('\\', '/').lstrip('/')
            if not name or name.endswith('/'):
                continue
            parts = PurePosixPath(name).parts
            if any(part in ('', '.', '..') for part in parts):
                raise ValueError(f'unsafe path in package: {info.filename!r}')
            target = destination.joinpath(*parts).resolve()
            if not target.is_relative_to(destination):
                raise ValueError(f'unsafe path in package: {info.filename!r}')
            target.parent.mkdir(parents=True, exist_ok=True)
            with archive.open(info) as source, open(target, 'wb') as output:
                shutil.copyfileobj(source, output)
            written += 1
    return written


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--dest', type=Path, default=DEFAULT_DEST)
    args = parser.parse_args(argv)
    version = json.loads(fetch(VERSION_URL, 1 << 16))['clientVersionUpload']
    if not (version.startswith('version-') and version[8:].isalnum()):
        raise ValueError('Unexpected client version format')
    manifest = parse_manifest(fetch(f'{CDN}/{version}-rbxPkgManifest.txt', 1 << 20).decode())
    staging = Path(tempfile.mkdtemp(prefix='rove-content-', dir=args.dest.parent if args.dest.parent.exists() else None))
    try:
        for package, folder in PACKAGES.items():
            expected = manifest.get(package)
            if expected is None:
                print(f'skip {package} (not in this version)')
                continue
            data = fetch(f'{CDN}/{version}-{package}')
            if hashlib.md5(data).hexdigest() != expected:
                raise ValueError(f'{package} failed its manifest MD5 check')
            count = safe_extract(data, staging / folder)
            print(f'ok   {package} -> {folder} ({count} files)')
        (staging / 'version.txt').write_text(version + '\n')
        if args.dest.exists():
            shutil.rmtree(args.dest)
        staging.replace(args.dest)
    finally:
        if staging.exists():
            shutil.rmtree(staging, ignore_errors=True)
    print(f'Roblox content {version} is in {args.dest}')
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError) as error:
        print(f'Content download failed: {error}', file=sys.stderr)
        sys.exit(1)
