"""Bounded visual assets referenced by a render frame, separate from physics.

FileMesh layout references: MaximumADHD's FileMesh specification and
https://github.com/PrintedScript/RBXMesh/blob/main/RBXMesh.py.
This renderer consumes rigid visual geometry, not skinning or FACS animation.
"""
import os
import base64
import gzip
import io
import json
import math
import re
import struct
import threading
from collections import OrderedDict
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

from mesh_asset_decoder import MAX_INPUT, MeshAssetError, _VECTOR, decode_filemesh, fetch_mesh_asset
from render_fonts import RenderFonts


MAC_CONTENT_DIR = Path('/Applications/Roblox.app/Contents/Resources/content')
DOWNLOADED_CONTENT_DIR = Path(__file__).resolve().parent.parent / 'roblox-content' / 'content'


def default_content_dir():
    """Prefer `rove.py content` downloads; fall back to the installed macOS app."""
    return DOWNLOADED_CONTENT_DIR if DOWNLOADED_CONTENT_DIR.is_dir() else MAC_CONTENT_DIR


def asset_reference(value):
    if not isinstance(value, str) or len(value) > 512:
        raise ValueError('Invalid visual asset reference')
    value=value.strip()
    if re.fullmatch(r'rbxasset://(?:textures|avatar)/[A-Za-z0-9_./-]+\.(?:png|jpg|jpeg|mesh)', value) and '..' not in value:
        return value
    match = re.fullmatch(r'rbxassetid://([0-9]{1,20})', value, re.I)
    identifier = match[1] if match else None
    if identifier is None:
        url = urlsplit(value)
        if url.scheme in ('http', 'https') and url.hostname in ('www.roblox.com', 'roblox.com', 'assetdelivery.roblox.com') and url.path.rstrip('/') in ('/asset', '/v1/asset') and not url.username and not url.password and url.port in (None, 80, 443):
            identifier = parse_qs(url.query).get('id', [''])[0]
    if not identifier or not identifier.isascii() or not identifier.isdigit() or not 0 < int(identifier) < 2**64:
        raise ValueError('Unsupported visual asset reference')
    return 'rbxassetid://' + str(int(identifier))


def fetch_visual_asset(reference, *, max_bytes=MAX_INPUT):
    if reference.startswith('rbxasset://'):
        root = Path(os.environ.get('ROBLOX_CONTENT_DIR') or default_content_dir()).expanduser().resolve()
        target = (root / reference.removeprefix('rbxasset://')).resolve()
        if not target.is_relative_to(root) or not target.is_file() or not 0 < target.stat().st_size <= max_bytes:
            raise ValueError('Installed Roblox visual asset is unavailable')
        return target.read_bytes()
    return fetch_mesh_asset(reference, max_bytes=max_bytes)


def decode_render_mesh(raw):
    if not isinstance(raw, bytes) or not 0 < len(raw) <= MAX_INPUT:
        raise ValueError('Visual mesh exceeds size limit')
    data = raw
    if raw.startswith(b'\x1f\x8b'):
        with gzip.GzipFile(fileobj=io.BytesIO(raw)) as stream:
            data = stream.read(MAX_INPUT + 1)
        if len(data) > MAX_INPUT:
            raise ValueError('Expanded mesh exceeds size limit')
    version, newline, body = data.partition(b'\n')
    if not newline:
        raise ValueError('Missing mesh header')
    positions, normals, uv, indices = [], [], [], []
    bones = 0
    if version in (b'version 1.00', b'version 1.01'):
        mesh = decode_filemesh(data)
        if len(mesh.vertices) > 100000:
            raise ValueError('Visual mesh vertex budget exceeded')
        positions = [n for v in mesh.vertices for n in v]
        vectors = _VECTOR.findall(body.partition(b'\n')[2])
        for i in range(0, len(vectors), 3):
            normals.extend(float(n) for n in vectors[i+1])
            u, v, _ = (float(n) for n in vectors[i+2])
            uv.extend((u, 1-v if version == b'version 1.00' else v))
        indices = [i for face in mesh.triangles for i in face]
    else:
        lods, tail = 0, 0
        if version == b'version 2.00' and len(body) >= 12:
            header, stride, face_stride, vertices, faces = struct.unpack_from('<HBBII', body)
            if header != 12 or face_stride != 12 or stride not in (36, 40):
                raise ValueError('Invalid v2 mesh header')
        elif version in (b'version 3.00', b'version 3.01') and len(body) >= 16:
            header, stride, face_stride, lod_size, lods, vertices, faces = struct.unpack_from('<HBBHHII', body)
            if header != 16 or stride not in (36, 40) or face_stride != 12 or lod_size != 4:
                raise ValueError('Invalid v3 mesh header')
        elif version in (b'version 4.00', b'version 4.01', b'version 5.00') and len(body) >= 24:
            header, _, vertices, faces, lods, bones, names, subsets, _, _ = struct.unpack_from('<HHIIHHIHBB', body)
            expected_header = 32 if version == b'version 5.00' else 24
            if header != expected_header or len(body) < header:
                raise ValueError('Invalid modern mesh header')
            stride = 40
            tail = bones*60 + names + subsets*72
            if header == 32:
                _, facs_size = struct.unpack_from('<II', body, 24)
                tail += facs_size
        else:
            raise ValueError('Unsupported visual mesh version')
        if not 0 < vertices <= 100000 or not 0 < faces <= 200000 or lods > 256:
            raise ValueError('Visual mesh geometry budget exceeded')
        face_start = header + vertices*stride + (vertices*8 if bones else 0)
        lod_start = face_start + faces*12
        if len(body) != lod_start + lods*4 + tail:
            raise ValueError('Truncated or inconsistent visual mesh')
        start, end = 0, faces
        if lods:
            offsets = struct.unpack_from('<' + 'I'*lods, body, lod_start)
            if offsets[0] != 0 or any(a > b for a, b in zip(offsets, offsets[1:])) or offsets[-1] > faces:
                raise ValueError('Invalid mesh LOD range')
            if lods > 1:
                end = offsets[1]
        for i in range(vertices):
            values = struct.unpack_from('<8f', body, header+i*stride)
            positions.extend(values[:3]); normals.extend(values[3:6]); uv.extend(values[6:8])
        for i in range(start, end):
            face = struct.unpack_from('<III', body, face_start+i*12)
            if any(v >= vertices for v in face):
                raise ValueError('Visual mesh index out of range')
            indices.extend(face)
    if not indices or any(not math.isfinite(n) or abs(n) > 1e8 for n in positions+normals+uv):
        raise ValueError('Invalid visual mesh coordinates')
    return {'positions': positions, 'normals': normals, 'uv': uv, 'indices': indices,
            'version': version.decode('ascii'), 'skinned': bones > 0}


class RenderAssets:
    def __init__(self, fetcher=fetch_visual_asset, max_bytes=48*1024*1024):
        self.fetcher = fetcher
        self.max_bytes = max_bytes
        self.cache = OrderedDict()
        self.bytes = 0
        self.allowed = set()
        self.lock = threading.Lock()
        self.workers = threading.BoundedSemaphore(4)
        self.fonts=RenderFonts(lambda reference,**kwargs:self.fetcher(reference,**kwargs))

    def observe(self, frame):
        self.fonts.observe(frame)
        allowed = set()
        def add(kind, ref):
            try:
                allowed.add((kind, asset_reference(ref)))
            except ValueError:
                pass
        for part in frame.get('parts', [])[:1800]:
            mesh = part.get('mesh') or {}
            add('mesh', mesh.get('id')); add('texture', mesh.get('texture'))
            for field in ('colorMap','normalMap','roughnessMap','metalnessMap'):
                add('texture',(part.get('appearance') or {}).get(field))
            for decal in part.get('decals', [])[:12]:
                add('texture', decal.get('texture'))
            for ref in (part.get('clothing') or {}).values():
                add('texture', ref)
        with self.lock:
            self.allowed = allowed

    def get(self, kind, reference, *, weight=None, style=None):
        if kind=='font':return self.fonts.get(reference,weight,style)
        if weight is not None or style is not None:raise ValueError('Unexpected visual asset font options')
        if kind not in ('mesh', 'texture'):
            raise ValueError('Invalid visual asset kind')
        key = (kind, asset_reference(reference))
        with self.lock:
            if key not in self.allowed:
                raise ValueError('Asset is not referenced by the displayed world')
            if key in self.cache:
                self.cache.move_to_end(key)
                return self.cache[key]
        if not self.workers.acquire(blocking=False):
            raise ValueError('Visual asset queue is busy')
        try:
            raw = self.fetcher(key[1], max_bytes=MAX_INPUT)
            if kind == 'mesh':
                value = {'ok': True, 'mesh': decode_render_mesh(raw)}
            else:
                mime = 'image/png' if raw.startswith(b'\x89PNG\r\n\x1a\n') else 'image/jpeg' if raw.startswith(b'\xff\xd8\xff') else None
                if not mime or len(raw) > 8*1024*1024:
                    raise ValueError('Unsupported or oversized visual texture')
                if mime == 'image/png':
                    if len(raw) < 24:
                        raise ValueError('Truncated visual texture')
                    width,height=struct.unpack_from('>II',raw,16)
                    if not 0<width<=8192 or not 0<height<=8192 or width*height>16*1024*1024:
                        raise ValueError('Visual texture dimensions exceed budget')
                value = {'ok': True, 'mime': mime, 'data': base64.b64encode(raw).decode('ascii')}
            size = len(json.dumps(value))
            if size > min(16*1024*1024,self.max_bytes):
                raise ValueError('Decoded visual asset exceeds budget')
            with self.lock:
                if key not in self.cache:
                    while self.cache and (self.bytes+size > self.max_bytes or len(self.cache) >= 256):
                        _, old = self.cache.popitem(last=False)
                        self.bytes -= len(json.dumps(old))
                    self.cache[key] = value; self.bytes += size
            return value
        except MeshAssetError as error:
            raise ValueError(str(error)) from None
        finally:
            self.workers.release()
