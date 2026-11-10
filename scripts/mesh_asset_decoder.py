"""Bounded decoder for legacy Roblox FileMesh visual geometry.

This decodes mesh asset bytes, not Roblox's generated physics collision model.
Supported signatures are version 1.00, 1.01, and 2.00. Newer formats fail
closed. Fetching uses the existing private RobloxSession cookie, never prints
it, and strips it from cross-host CDN redirects.
"""
from __future__ import annotations

from dataclasses import dataclass
import gzip
import hashlib
import io
import math
import re
import struct
import concurrent.futures
import urllib.error
import urllib.parse
import urllib.request

MAX_INPUT = 16 * 1024 * 1024
MAX_VERTICES = 500_000
MAX_FACES = 1_000_000
_VECTOR = re.compile(rb"\[\s*([-+0-9.eE]+)\s*,\s*([-+0-9.eE]+)\s*,\s*([-+0-9.eE]+)\s*\]")


class MeshAssetError(ValueError):
    """Stable local mesh decoder failure."""


def mesh_asset_id(reference) -> str:
    """Normalize only a captured rbxassetid reference, without exposing it."""
    if isinstance(reference,bytes):
        try: reference=reference.decode('ascii')
        except UnicodeDecodeError: raise MeshAssetError('mesh_invalid_reference') from None
    if not isinstance(reference,str) or len(reference)>512:
        raise MeshAssetError('mesh_invalid_reference')
    match=re.fullmatch(r'(?i)rbxassetid://([0-9]{1,20})',reference)
    if not match or not 0<int(match[1])<2**64:
        raise MeshAssetError('mesh_invalid_reference')
    return str(int(match[1]))


class _MeshAssetRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self,request,fp,code,message,headers,newurl):
        try:
            old=urllib.parse.urlsplit(request.full_url); target=urllib.parse.urlsplit(newurl)
            host=(target.hostname or '').lower(); port=target.port
        except ValueError:
            raise MeshAssetError('mesh_redirect_malformed') from None
        is_cdn=bool(re.fullmatch(r'[a-z0-9.-]+\.rbxcdn\.com',host))
        is_roblox=bool(host=='roblox.com' or host.endswith('.roblox.com'))
        allowed=(host=='assetdelivery.roblox.com' or is_cdn or is_roblox)
        if target.scheme!='https': raise MeshAssetError('mesh_redirect_scheme')
        if not allowed:
            category='rbxcdn' if is_cdn else 'roblox' if is_roblox else 'other'
            raise MeshAssetError('mesh_redirect_host_'+category)
        if target.username or target.password: raise MeshAssetError('mesh_redirect_credentials')
        if port not in (None,443): raise MeshAssetError('mesh_redirect_port')
        redirected=super().redirect_request(request,fp,code,message,headers,newurl)
        if redirected is not None and host != (old.hostname or '').lower():
            redirected.remove_header('Cookie'); redirected.remove_header('Authorization')
        return redirected


def fetch_mesh_asset(reference, *, max_bytes=MAX_INPUT):
    """Fetch a mesh referenced by the authorized local capture using saved session.

    RobloxSession supplies the existing owner-only cookie file. Credentials are
    sent only to Asset Delivery; CDN redirects are HTTPS- and host-checked and
    never receive Cookie or Authorization headers.
    """
    asset_id=mesh_asset_id(reference)
    if type(max_bytes) is not int or not 0<max_bytes<=64*1024*1024:
        raise MeshAssetError('mesh_asset_size_limit')
    from roblox_session import RobloxSession
    try: cookie=RobloxSession()._cookie
    except Exception: raise MeshAssetError('mesh_asset_auth_unavailable') from None
    request=urllib.request.Request('https://assetdelivery.roblox.com/v1/asset/?id='+asset_id,
        headers={'Cookie':'.ROBLOSECURITY='+cookie,'User-Agent':'LocalRobloxResearch/1.0',
                 'Accept-Encoding':'identity'})
    opener=urllib.request.build_opener(_MeshAssetRedirect())
    try:
        with opener.open(request,timeout=15) as response:
            raw=response.read(max_bytes+1)
    except urllib.error.HTTPError as error:
        if type(error.code) is int and 100<=error.code<=599:
            raise MeshAssetError('mesh_asset_http_'+str(error.code)) from None
        raise MeshAssetError('mesh_asset_fetch_failed') from None
    except MeshAssetError:
        raise
    except Exception as error:
        raise MeshAssetError('mesh_asset_fetch_failed_'+type(error).__name__) from None
    if len(raw)>max_bytes: raise MeshAssetError('mesh_asset_size_limit')
    if not raw: raise MeshAssetError('mesh_asset_empty')
    return raw


@dataclass(frozen=True)
class DecodedMeshAsset:
    vertices: tuple[tuple[float, float, float], ...]
    triangles: tuple[tuple[int, int, int], ...]
    format_version: str
    sha256: str


def _finite_vertices(values):
    if not values or len(values) > MAX_VERTICES:
        raise MeshAssetError('mesh_vertex_limit')
    out=[]
    for vertex in values:
        if len(vertex)!=3 or any(not math.isfinite(float(v)) for v in vertex):
            raise MeshAssetError('mesh_invalid_vertex')
        out.append(tuple(float(v) for v in vertex))
    return tuple(out)


def _faces(faces, vertex_count):
    if not faces or len(faces)>MAX_FACES:
        raise MeshAssetError('mesh_face_limit')
    out=[]
    for face in faces:
        if len(face)!=3 or any(type(i) is not int or i<0 or i>=vertex_count for i in face):
            raise MeshAssetError('mesh_invalid_face')
        out.append(tuple(face))
    return tuple(out)


def decode_filemesh(raw: bytes) -> DecodedMeshAsset:
    """Decode legacy FileMesh v1/v2 visual triangle data with strict bounds."""
    if not isinstance(raw,bytes) or not raw or len(raw)>MAX_INPUT:
        raise MeshAssetError('mesh_asset_size_limit')
    source_sha256=hashlib.sha256(raw).hexdigest()
    data=raw
    if raw.startswith(b'\x1f\x8b'):
        try:
            with gzip.GzipFile(fileobj=io.BytesIO(raw)) as stream:
                data=stream.read(MAX_INPUT+1)
        except (OSError,EOFError):
            raise MeshAssetError('mesh_invalid_compression') from None
        if len(data)>MAX_INPUT: raise MeshAssetError('mesh_asset_expansion_limit')
    first,sep,body=data.partition(b'\n')
    # Many legacy text meshes use CRLF line endings.
    first=first.rstrip(b'\r')
    try: version=first.decode('ascii','strict') if first else ''
    except UnicodeDecodeError: raise MeshAssetError('mesh_invalid_header') from None
    if not sep or version not in ('version 1.00','version 1.01','version 2.00'):
        raise MeshAssetError('mesh_unsupported_version')
    if version in ('version 1.00','version 1.01'):
        count_line,sep,body=body.partition(b'\n')
        if not sep or not re.fullmatch(rb'\s*[0-9]+\s*',count_line):
            raise MeshAssetError('mesh_invalid_header')
        face_count=int(count_line)
        if not 0<face_count<=MAX_FACES:
            raise MeshAssetError('mesh_face_limit')
        vectors=_VECTOR.findall(body)
        if len(vectors)!=face_count*9:
            raise MeshAssetError('mesh_invalid_data')
        if _VECTOR.sub(b'',body).strip(): raise MeshAssetError('mesh_invalid_data')
        vertices=[]
        for face_index in range(face_count):
            for corner in range(3):
                group=vectors[face_index*9+corner*3]
                point=[float(v) for v in group]
                if version=='version 1.00': point=[v*.5 for v in point]
                vertices.append(point)
        mesh_vertices=_finite_vertices(vertices)
        triangles=tuple((3*i,3*i+1,3*i+2) for i in range(face_count))
        # Legacy v1 data carries normals/UVs which this collision helper ignores.
    else:
        if len(body)<12: raise MeshAssetError('mesh_truncated')
        header_size,vertex_size,face_size,vertex_count,face_count=struct.unpack_from('<HBBII',body,0)
        if header_size!=12 or vertex_size not in (36,40) or face_size!=12:
            raise MeshAssetError('mesh_invalid_header')
        if not (0<vertex_count<=MAX_VERTICES and 0<face_count<=MAX_FACES):
            raise MeshAssetError('mesh_count_limit')
        expected=header_size+vertex_count*vertex_size+face_count*face_size
        if len(body)!=expected: raise MeshAssetError('mesh_truncated_or_trailing_data')
        vertices=[]; offset=header_size
        for _ in range(vertex_count):
            xyz=struct.unpack_from('<3f',body,offset)
            vertices.append(xyz); offset+=vertex_size
        mesh_vertices=_finite_vertices(vertices)
        faces=[]
        for _ in range(face_count):
            faces.append(struct.unpack_from('<3I',body,offset)); offset+=12
        triangles=_faces(faces,vertex_count)
    triangles=_faces(triangles,len(mesh_vertices))
    return DecodedMeshAsset(mesh_vertices,triangles,version,source_sha256)


def visual_mesh_world_geometry(asset: DecodedMeshAsset, position, rotation, scale, *, source=None):
    """Apply an explicit local-to-world transform to decoded *visual* vertices.

    This result is useful for visibly approximate map queries, but is marked
    visual-only: Roblox may use Box, Hull, voxel hull, or decomposed collision
    geometry instead of the render triangles. Callers must opt in explicitly.
    """
    from physics_ground import TriangleMesh, _vector
    from movement_collision import rotation_matrix
    position=_vector(position,'mesh world position'); scale=_vector(scale,'mesh scale')
    if any(v<=0 for v in scale): raise MeshAssetError('mesh_invalid_scale')
    try: matrix=rotation_matrix(rotation)
    except ValueError: raise MeshAssetError('mesh_invalid_rotation') from None
    vertices=[]
    for local in asset.vertices:
        scaled=tuple(local[i]*scale[i] for i in range(3))
        world=tuple(position[r]+sum(matrix[3*r+c]*scaled[c] for c in range(3)) for r in range(3))
        vertices.append(world)
    return TriangleMesh(tuple(vertices),asset.triangles,source,
                        (f"visual FileMesh {asset.format_version} sha256={asset.sha256}; Roblox CollisionFidelity geometry is unknown and may differ",))


def visual_meshes_from_world(world, assets_by_reference):
    """Build world-space visual mesh candidates from exact received MeshPart inputs.

    ``assets_by_reference`` maps the captured MeshContent bytes (or their
    decoded ASCII string form) to DecodedMeshAsset values. Mesh vertices are
    fitted componentwise to the received Size and centered in the part frame;
    this is an explicitly approximate visual reconstruction because pivot and
    CollisionFidelity internals are unavailable in the replication capture.
    """
    from physics_ground import (_part_size, _property_type_map, _received_part_frame,
                                _received_rotation, _vector)
    from movement_collision import rotation_matrix
    schema=getattr(world,'schema',None); instances=getattr(world,'instances',None)
    if not isinstance(schema,dict) or not isinstance(instances,dict):
        raise MeshAssetError('mesh_invalid_world')
    classes=schema.get('classes',[]); result={}
    from physics_ground import TriangleMesh
    for key,instance in instances.items():
        if not isinstance(instance,dict) or type(instance.get('class')) is not int:
            continue
        ci=instance['class']
        if not 0<=ci<len(classes) or classes[ci].get('name')!='MeshPart':
            continue
        props=instance.get('properties',{})
        reference=props.get('MeshContent') if isinstance(props,dict) else None
        types=_property_type_map(schema,ci)
        if types.get('MeshContent',(None,None))[0]!=61 or not isinstance(reference,bytes): continue
        asset=assets_by_reference.get(reference)
        if asset is None:
            try: asset=assets_by_reference.get(reference.decode('ascii'))
            except UnicodeDecodeError: continue
        if not isinstance(asset,DecodedMeshAsset): continue
        frame,record_type=_received_part_frame(world,key,props,types)
        rotation=_received_rotation(frame,record_type) if frame is not None else None
        if rotation is None: continue
        try:
            position=_vector(frame.get('position'),'MeshPart.CFrame.position')
            size=_vector(_part_size(props,types),'MeshPart.Size')
        except (AttributeError,ValueError):
            continue
        if any(v<=0 for v in size): continue
        lo=tuple(min(p[i] for p in asset.vertices) for i in range(3))
        hi=tuple(max(p[i] for p in asset.vertices) for i in range(3))
        extents=tuple(hi[i]-lo[i] for i in range(3))
        if any(not math.isfinite(v) or v<0 for v in extents): continue
        center=tuple((hi[i]+lo[i])*.5 for i in range(3))
        # Preserve a truly planar visual mesh on collapsed local axes; there is
        # no intrinsic extent from which a trustworthy scale can be recovered.
        scale=tuple(size[i]/extents[i] if extents[i]>1e-9 else 1. for i in range(3))
        matrix=rotation_matrix(rotation)
        world_vertices=[]
        for local in asset.vertices:
            scaled=tuple((local[i]-center[i])*scale[i] for i in range(3))
            world_vertices.append(tuple(position[r]+sum(matrix[3*r+c]*scaled[c] for c in range(3)) for r in range(3)))
        try:
            result[key]=TriangleMesh(tuple(world_vertices),asset.triangles,key,
                (f"FileMesh {asset.format_version} source_sha256={asset.sha256}; fitted to received Size; MeshPart pivot and CollisionFidelity geometry remain unverified",))
        except ValueError:
            continue
    return result


class MeshAssetQueue:
    """Bounded, async fetch/decode cache for captured MeshContent references.

    The caller passes only asset refs already present in an authorized World.
    Submission and draining never perform network I/O or wait for a future.
    Errors contain fixed local codes/statuses and never remote response text.
    """
    def __init__(self, *, fetcher=fetch_mesh_asset, max_assets=128, workers=2,
                 max_cache_bytes=64*1024*1024):
        if type(max_assets) is not int or not 1<=max_assets<=2048:
            raise ValueError('mesh asset request limit is outside supported bounds')
        if type(workers) is not int or not 1<=workers<=8:
            raise ValueError('mesh asset worker count is outside supported bounds')
        if type(max_cache_bytes) is not int or not 0<=max_cache_bytes<=512*1024*1024:
            raise ValueError('mesh asset cache budget is outside supported bounds')
        self.fetcher=fetcher; self.max_assets=max_assets; self.max_cache_bytes=max_cache_bytes
        self.executor=concurrent.futures.ThreadPoolExecutor(max_workers=workers,thread_name_prefix='mesh-assets')
        self.pending={}; self.cache={}; self.errors_by_id={}; self.cache_bytes=0
        self.references_by_id={}; self.submitted=set(); self.closed=False

    def _load(self, asset_id):
        try:
            raw=self.fetcher('rbxassetid://'+asset_id)
            return decode_filemesh(raw)
        except MeshAssetError as error:
            return {'error':str(error)}
        except Exception:
            return {'error':'mesh_asset_unavailable'}

    @staticmethod
    def _estimated_size(asset):
        # Conservative Python-object estimate for tuple/list and float overhead.
        return len(asset.vertices)*160+len(asset.triangles)*96

    def submit(self, references):
        """Schedule bytes/string ContentIds already received on MeshParts."""
        if self.closed: raise RuntimeError('mesh asset queue is closed')
        for reference in references:
            try:
                asset_id=mesh_asset_id(reference)
                reference_bytes=(reference if isinstance(reference,bytes) else reference.encode('ascii'))
            except (MeshAssetError,UnicodeEncodeError):
                continue
            if reference_bytes in self.submitted: continue
            if len(self.submitted)>=self.max_assets: break
            self.submitted.add(reference_bytes)
            self.references_by_id.setdefault(asset_id,set()).add(reference_bytes)
            if asset_id in self.cache or asset_id in self.errors_by_id or asset_id in self.pending:
                continue
            self.pending[asset_id]=self.executor.submit(self._load,asset_id)

    def drain(self):
        """Collect completed results; return decoded assets keyed by received ref bytes."""
        if self.closed: return {}
        for asset_id,future in list(self.pending.items()):
            if not future.done(): continue
            result=future.result()
            if isinstance(result,DecodedMeshAsset):
                size=self._estimated_size(result)
                if self.cache_bytes+size>self.max_cache_bytes:
                    self.errors_by_id[asset_id]='mesh_asset_cache_size_limit'
                else:
                    self.cache[asset_id]=result; self.cache_bytes+=size
            else:
                self.errors_by_id[asset_id]=result.get('error','mesh_asset_unavailable')
            del self.pending[asset_id]
        result={}
        for asset_id,asset in self.cache.items():
            for reference in self.references_by_id.get(asset_id,()): result[reference]=asset
        return result

    def errors(self):
        """Return safe per-reference failure reasons without asset IDs."""
        return {reference:self.errors_by_id.get(asset_id,'mesh_asset_pending')
                for asset_id,refs in self.references_by_id.items()
                for reference in refs if asset_id in self.errors_by_id}

    def reset(self):
        """Discard one World's pending/cache entries while keeping workers reusable."""
        if self.closed: return
        for future in self.pending.values(): future.cancel()
        self.pending.clear(); self.cache.clear(); self.references_by_id.clear()
        self.errors_by_id.clear(); self.submitted.clear(); self.cache_bytes=0

    def close(self):
        if self.closed: return
        self.closed=True
        self.executor.shutdown(wait=False,cancel_futures=True)
        self.pending.clear(); self.cache.clear(); self.references_by_id.clear()
        self.errors_by_id.clear(); self.submitted.clear(); self.cache_bytes=0
