"""Conservative ground extraction and vertical stepping for offline movement.

The default mode accepts received Block shape and collision properties, plus
the established strict constructor-default rule. An explicit experimental
option may infer omitted Part defaults for fresh complete network creations;
every inferred default is recorded on its collider. Oriented blocks come from
complete rigid rotation matrices, from the pinned native reader's axis-pair
selectors (property CFrame types 27/51 and ReplicationPV type 57), and from its
six-byte packed quaternion form; these are the same decoders the snapshot,
animation and owned-part code already use for received poses. Explicit decoded
triangle meshes and voxel occupancy may be supplied as collision geometry;
opaque Terrain/MeshPart assets remain unsupported (their bounds are never
substituted for collision shape).
"""
from __future__ import annotations

from dataclasses import dataclass, field
from functools import lru_cache
import json
import math
from pathlib import Path
from typing import Any

from movement_collision import ray_aabb, ray_obb, rotation_matrix
from physics_rotation import (RotationCodecError, RotationField, decode_native_quaternion,
                              native_compact_selector_to_matrix,
                              native_property_selector_to_matrix, rotation_is_identity)

Vector3 = tuple[float, float, float]
_ROOT = Path(__file__).resolve().parents[1]


@lru_cache(maxsize=1)
def _constructor_properties():
    """Native-measured constructor defaults for this pinned client build."""
    try:
        data = json.loads((_ROOT / 'runtime' / 'class-constructors-745.json').read_text())
        return {name: spec.get('properties', {}) for name, spec in data['classes'].items()}
    except (OSError, KeyError, TypeError, json.JSONDecodeError):
        return {}


@lru_cache(maxsize=1)
def _class_parents():
    """Exact-build class ancestry used to admit only true Part descendants."""
    try:
        data = json.loads((_ROOT / 'runtime' / 'class-parents-745.json').read_text())
        return data.get('parents', {}) if isinstance(data, dict) else {}
    except (OSError, KeyError, TypeError, json.JSONDecodeError):
        return {}


def _is_part_descendant(class_name: str) -> bool:
    """Return true only for Part or a class whose pinned ancestry reaches Part."""
    parents = _class_parents()
    current = class_name
    seen = set()
    while isinstance(current, str) and current not in seen:
        if current == 'Part':
            return True
        seen.add(current)
        current = parents.get(current)
    return False


@dataclass(frozen=True)
class AABB:
    minimum: Vector3
    maximum: Vector3
    source: Any = None
    assumptions: tuple[str, ...] = ()


@dataclass(frozen=True)
class OBB:
    center: Vector3
    half_size: Vector3
    rotation: tuple[float, ...]
    source: Any = None
    assumptions: tuple[str, ...] = ()
    minimum: Vector3 = field(init=False)
    maximum: Vector3 = field(init=False)

    def __post_init__(self):
        center,half = _vector(self.center,'center'),_vector(self.half_size,'half_size')
        if any(v <= 0 for v in half):raise ValueError('half_size must be positive')
        matrix = rotation_matrix(self.rotation)
        extent = tuple(sum(abs(matrix[3*i+j])*half[j] for j in range(3)) for i in range(3))
        lower,upper = tuple(center[i]-extent[i] for i in range(3)),tuple(center[i]+extent[i] for i in range(3))
        if any(not math.isfinite(v) for v in lower+upper) or any(lower[i] >= upper[i] for i in range(3)):
            raise ValueError('oriented box world bounds must be finite and nondegenerate')
        for key,value in [('center',center),('half_size',half),('rotation',matrix),('minimum',lower),('maximum',upper)]:
            object.__setattr__(self,key,value)


@dataclass(frozen=True)
class TriangleMesh:
    """Exact triangle surface in world coordinates, supplied by a decoder."""
    vertices: tuple[Vector3, ...]
    triangles: tuple[tuple[int, int, int], ...]
    source: Any = None
    assumptions: tuple[str, ...] = ()
    minimum: Vector3 = field(init=False)
    maximum: Vector3 = field(init=False)

    def __post_init__(self):
        vertices = tuple(_vector(v, 'mesh vertex') for v in self.vertices)
        if not vertices:
            raise ValueError('mesh must contain vertices')
        triangles = []
        for tri in self.triangles:
            if not isinstance(tri, (tuple, list)) or len(tri) != 3 or any(type(i) is not int for i in tri):
                raise ValueError('each triangle must contain three integer indices')
            if any(i < 0 or i >= len(vertices) for i in tri) or len(set(tri)) != 3:
                raise ValueError('triangle indices must be distinct and in range')
            a,b,c=(vertices[i] for i in tri)
            ab=tuple(b[j]-a[j] for j in range(3)); ac=tuple(c[j]-a[j] for j in range(3))
            cross=(ab[1]*ac[2]-ab[2]*ac[1],ab[2]*ac[0]-ab[0]*ac[2],ab[0]*ac[1]-ab[1]*ac[0])
            if sum(v*v for v in cross) <= 1e-24:
                raise ValueError('degenerate triangle')
            triangles.append(tuple(tri))
        if not triangles:
            raise ValueError('mesh must contain triangles')
        lo=tuple(min(v[i] for v in vertices) for i in range(3)); hi=tuple(max(v[i] for v in vertices) for i in range(3))
        object.__setattr__(self,'vertices',vertices); object.__setattr__(self,'triangles',tuple(triangles))
        object.__setattr__(self,'minimum',lo); object.__setattr__(self,'maximum',hi)


@dataclass(frozen=True)
class VoxelGrid:
    """Explicit occupancy grid; occupancy is a flat x-major, then y, then z tuple."""
    origin: Vector3
    cell_size: Vector3
    dimensions: tuple[int, int, int]
    occupancy: tuple[bool, ...]
    source: Any = None
    minimum: Vector3 = field(init=False)
    maximum: Vector3 = field(init=False)

    def __post_init__(self):
        origin=_vector(self.origin,'voxel origin'); cell=_vector(self.cell_size,'voxel cell_size')
        if any(v <= 0 for v in cell): raise ValueError('voxel cell_size must be positive')
        dims=self.dimensions
        if not isinstance(dims,(tuple,list)) or len(dims)!=3 or any(type(v) is not int or v<=0 for v in dims):
            raise ValueError('voxel dimensions must be three positive integers')
        occ=tuple(self.occupancy)
        if len(occ)!=dims[0]*dims[1]*dims[2] or any(type(v) is not bool for v in occ):
            raise ValueError('voxel occupancy must contain one boolean per cell')
        object.__setattr__(self,'origin',origin); object.__setattr__(self,'cell_size',cell)
        object.__setattr__(self,'dimensions',tuple(dims)); object.__setattr__(self,'occupancy',occ)
        object.__setattr__(self,'minimum',origin)
        object.__setattr__(self,'maximum',tuple(origin[i]+cell[i]*dims[i] for i in range(3)))


def decode_collision_geometry(data: object, *, source=None):
    """Validate a structured, already-decoded collision payload.

    Supported mappings have `kind=triangle_mesh` with vertices/triangles in
    world coordinates, or `kind=voxel_grid` with origin/cell_size/dimensions/
    occupancy. This function does not decode Roblox asset bytes.
    """
    if not isinstance(data, dict): raise ValueError('collision geometry must be a mapping')
    kind=data.get('kind')
    if kind=='triangle_mesh': return TriangleMesh(data.get('vertices',()),data.get('triangles',()),source)
    if kind=='voxel_grid': return VoxelGrid(data.get('origin'),data.get('cell_size'),data.get('dimensions'),data.get('occupancy'),source)
    raise ValueError('unsupported or missing decoded collision geometry kind')


@dataclass(frozen=True)
class GroundGeometry:
    colliders: tuple[AABB | OBB | TriangleMesh | VoxelGrid, ...]
    unsupported: tuple[tuple[Any, str], ...] = ()


@dataclass(frozen=True)
class GroundStep:
    position: Vector3
    vertical_velocity: float
    grounded: bool
    hit: AABB | OBB | TriangleMesh | VoxelGrid | None = None


@dataclass(frozen=True)
class GroundHit:
    y: float
    distance: float
    collider: AABB | OBB | TriangleMesh | VoxelGrid
    normal: Vector3 = (0., 1., 0.)


def _vector(value: object, label: str) -> Vector3:
    if not isinstance(value, (tuple, list)) or len(value) != 3:
        raise ValueError(f'{label} must have three finite components')
    out = []
    for component in value:
        if isinstance(component, bool) or not isinstance(component, (int, float)):
            raise ValueError(f'{label} must have three finite components')
        component = float(component)
        if not math.isfinite(component):
            raise ValueError(f'{label} must have three finite components')
        out.append(component)
    return tuple(out)


def _identity_rotation(cframe: object, record_type: int | None = None) -> bool:
    """Accept explicit identity matrices or exact-format mapped identity IDs."""
    if not isinstance(cframe, dict):
        return False
    rotation = cframe.get('rotation')
    if isinstance(rotation, (tuple, list)) and len(rotation) == 9:
        try:
            values = _vector9(rotation)
        except ValueError:
            return False
        identity = (1., 0., 0., 0., 1., 0., 0., 0., 1.)
        return all(abs(a - b) <= 1e-6 for a, b in zip(values, identity))
    if isinstance(rotation, (tuple, list)) and len(rotation) == 3 \
            and all(isinstance(row, (tuple, list)) and len(row) == 3 for row in rotation):
        flat = tuple(v for row in rotation for v in row)
        try:
            values = _vector9(flat)
        except ValueError:
            return False
        identity = (1., 0., 0., 0., 1., 0., 0., 0., 1.)
        return all(abs(a - b) <= 1e-6 for a, b in zip(values, identity))
    if type(record_type) is int and type(rotation) is int:
        try:
            if record_type == 51:
                field = RotationField(0x80 | rotation, 'selector', rotation)
                return rotation_is_identity(field, 'type51') is True
            if record_type == 57:
                field = RotationField(rotation, 'selector', rotation)
                return rotation_is_identity(field, 'type57') is True
            if record_type == 27:
                field = RotationField(rotation, 'selector', None)
                return rotation_is_identity(field, 'type27') is True
        except (ValueError, TypeError):
            return False
    return False


def _vector9(value):
    result = []
    for item in value:
        if isinstance(item, bool) or not isinstance(item, (int, float)) or not math.isfinite(item):
            raise ValueError('invalid matrix')
        result.append(float(item))
    return tuple(result)


def _quaternion_matrix(q):
    """Row-major basis for a unit quaternion in the decoder's (x, y, z, w) order."""
    x, y, z, w = q
    return (1-2*(y*y+z*z), 2*(x*y-z*w), 2*(x*z+y*w),
            2*(x*y+z*w), 1-2*(x*x+z*z), 2*(y*z-x*w),
            2*(x*z-y*w), 2*(y*z+x*w), 1-2*(x*x+y*y))


def _native_wire_rotation(cframe, record_type):
    """Expand a received selector or packed quaternion through the pinned native readers.

    Type 27/51 property CFrames carry an axis-pair selector (1..36, expanded as
    ``selector - 1``) or, for selector 0, a six-byte quaternion. Type 57
    ReplicationPV carries a compact selector (1..24) or the same quaternion
    bytes. Anything else (unknown selector, seven-float type-57 branch, bad
    bytes) stays opaque and returns None.
    """
    rotation = cframe.get('rotation')
    try:
        if record_type in (27, 51):
            if type(rotation) is int and 1 <= rotation <= 36:
                return native_property_selector_to_matrix(rotation)
            packed = cframe.get('packed_rotation') if record_type == 27 else rotation
            if (record_type == 51 or rotation == 0) and isinstance(packed, str):
                raw = bytes.fromhex(packed)
                if len(raw) != 6:
                    return None
                return _quaternion_matrix(decode_native_quaternion(raw)[0])
        elif record_type == 57:
            if type(rotation) is int and 1 <= rotation <= 24:
                return native_compact_selector_to_matrix(rotation)
            if isinstance(rotation, str):
                raw = bytes.fromhex(rotation)
                if len(raw) != 6:
                    return None
                return _quaternion_matrix(decode_native_quaternion(raw)[0])
    except (RotationCodecError, ValueError, TypeError):
        return None
    return None


def _received_rotation(cframe, record_type):
    """Return a validated row-major rotation from the received frame, or None.

    Accepts complete matrices, the established identity selectors, the native
    axis-pair selectors and the native packed quaternion. Every result passes
    ``rotation_matrix`` so only orthonormal, right-handed bases become colliders.
    """
    rotation = cframe.get('rotation')
    if isinstance(rotation,(tuple,list)):
        if len(rotation) == 3 and all(isinstance(row,(tuple,list)) and len(row) == 3 for row in rotation):
            rotation = tuple(v for row in rotation for v in row)
        try:return rotation_matrix(rotation)
        except ValueError:return None
    if _identity_rotation(cframe,record_type):return (1.,0.,0.,0.,1.,0.,0.,0.,1.)
    expanded = _native_wire_rotation(cframe, record_type)
    if expanded is None:
        return None
    try:
        return rotation_matrix(expanded)
    except ValueError:
        return None


def _property_type_map(schema: dict, class_index: int) -> dict[str, tuple[int, int | None]]:
    """Map the class's serialized property names to (type, enum) metadata."""
    try:
        class_properties = schema['classes'][class_index]['properties']
        properties = schema['properties']
        result = {}
        for property_index in class_properties:
            spec = properties[property_index]
            result[spec['name']] = (spec['type'], spec.get('enum'))
        return result
    except (KeyError, IndexError, TypeError):
        return {}


def _block_shape(schema: dict, property_types: dict[str, tuple[int, int | None]], props: dict,
                 *, allow_default: bool = False) -> tuple[bool, bool]:
    """Return (supported, inferred); missing shape inference is opt-in only."""
    shape_names = ('Shape', 'shape', 'NetworkShape', 'PartType', 'PrimitiveType')
    present = [name for name in shape_names if name in props]
    enum_names = [entry.get('name') for entry in schema.get('enums', [])
                  if isinstance(entry, dict)]
    if not present:
        meta = property_types.get('shape')
        if (allow_default and meta is not None and meta[0] == 7
                and type(meta[1]) is int and 0 <= meta[1] < len(enum_names)
                and enum_names[meta[1]] == 'PartType'):
            return True, True
        return False, False
    values = []
    for name in present:
        meta = property_types.get(name)
        value = props[name]
        if meta is None or type(value) is not int or meta[0] != 7:
            return False, False
        enum_id = meta[1]
        if type(enum_id) is not int or not 0 <= enum_id < len(enum_names):
            return False, False
        enum_name = enum_names[enum_id]
        block_value = {'PartType': 1, 'PrimitiveType': 3}.get(enum_name)
        if block_value is None:
            return False, False
        values.append(value == block_value)
    return (bool(values) and all(values)), False


def _fresh_complete_creation(world: object, key: Any) -> bool:
    creation = getattr(world, 'creation', None)
    state = creation.get(key) if isinstance(creation, dict) else None
    return isinstance(state, dict) and state.get('fresh') is True and state.get('complete') is True


def _default_can_collide(world: object, key: Any, class_name: str,
                         *, allow_network_constructor_defaults: bool = False) -> tuple[bool | None, bool]:
    """Return (value, inferred); network-path inference is opt-in and experimental."""
    if not _is_part_descendant(class_name) or not _fresh_complete_creation(world, key):
        return None, False
    creation = getattr(world, 'creation', None)
    state = creation.get(key) if isinstance(creation, dict) else None
    if not allow_network_constructor_defaults and not (
            isinstance(state, dict) and state.get('ordinary') is True):
        return None, False
    spec = _constructor_properties().get(class_name, {}).get('CanCollide')
    if not isinstance(spec, dict) or spec.get('type') != 'boolean' or type(spec.get('value')) is not bool:
        return None, False
    return spec['value'], not bool(state.get('ordinary') is True)


def _part_size(props: dict, property_types: dict[str, tuple[int, int | None]]):
    """Find a received Vector3 size using its schema name and three-float tag."""
    names = [name for name in ('size', 'Size') if name in props]
    if len(names) != 1:
        return None
    name = names[0]
    meta = property_types.get(name)
    # These schema tags use the same exact three-float Vector3 wire payload in
    # World.value. Requiring a matching schema entry avoids treating arbitrary
    # arrays or size-like names as geometry.
    if meta is None or meta[0] not in (19, 22, 36):
        return None
    return props[name]


def _received_part_frame(world: object, key: Any, props: dict,
                         property_types: dict[str, tuple[int, int | None]]):
    """Select the newest received CFrame/ReplicationPV with known wire type."""
    versions = getattr(world, 'property_versions', None)
    versions = versions.get(key, {}) if isinstance(versions, dict) else {}
    candidates = []
    for name in ('CFrame', 'ReplicationPV'):
        frame = props.get(name)
        meta = property_types.get(name)
        if not isinstance(frame, dict) or 'position' not in frame or meta is None:
            continue
        value_type = meta[0]
        if value_type not in (27, 51, 57):
            continue
        if value_type == 57 and frame.get('position_present') is not True:
            continue
        normalized = frame
        if value_type == 27 and 'rotation_id' in frame:
            # Keep the selector-zero quaternion bytes; they are the only
            # rotation evidence for parts with arbitrary orientation.
            normalized = {'position': frame.get('position'),
                          'rotation': frame.get('rotation_id'),
                          'packed_rotation': frame.get('packed_rotation')}
        candidates.append((versions.get(name, 0), name == 'CFrame', normalized, value_type))
    if not candidates:
        return None, None
    _, _, frame, value_type = max(candidates, key=lambda item: (item[0], item[1]))
    return frame, value_type


def extract_ground(world: object, *, allow_network_constructor_defaults: bool = False,
                   include_unanchored: bool = False,
                   visual_meshes: dict[Any, TriangleMesh] | None = None,
                   allow_visual_mesh_approximation: bool = False) -> GroundGeometry:
    """Extract supported static block colliders from a received ``World`` object.

    Only explicitly anchored, collidable Block ``Part`` objects with a
    schema-typed Vector3 size and a decodable rigid rotation (full matrix,
    native axis-pair selector or native packed quaternion) are included.
    ``allow_network_constructor_defaults`` enables an experimental inference
    for missing Part ``CanCollide``/Block shape on fresh complete creations;
    inferred fields are listed on each collider's ``assumptions`` tuple. The flag
    defaults off because constructor metadata does not prove every network
    creation path applies those defaults. ``include_unanchored`` is an opt-in
    for received dynamic Block parts; it requires explicit Anchored=False and
    otherwise follows the same collision, shape, size, and frame evidence rules.
    ``visual_meshes`` is a caller-provided mapping of MeshPart source keys to
    world-space visual FileMesh triangles. Those meshes are included only when
    ``allow_visual_mesh_approximation`` is true, CanCollide is explicitly true,
    and Anchored/pose are received. They remain labelled as approximations of
    Roblox collision fidelity.
    """
    schema = getattr(world, 'schema', None)
    instances = getattr(world, 'instances', None)
    if not isinstance(schema, dict) or not isinstance(instances, dict):
        raise ValueError('world must expose schema and instances dictionaries')
    classes = schema.get('classes')
    if not isinstance(classes, list):
        raise ValueError('world schema has no classes list')

    colliders: list[AABB | OBB] = []
    unsupported: list[tuple[Any, str]] = []
    for key, instance in instances.items():
        if not isinstance(instance, dict):
            unsupported.append((key, 'invalid instance record'))
            continue
        try:
            class_name = classes[instance['class']]['name']
        except (KeyError, IndexError, TypeError):
            unsupported.append((key, 'invalid class reference'))
            continue
        props = instance.get('properties', {})
        if not isinstance(props, dict):
            unsupported.append((key, 'invalid properties record'))
            continue
        if class_name in ('Terrain', 'MeshPart') and props.get('CanCollide') is False:
            continue
        collision_payload=props.get('CollisionGeometry')
        if class_name in ('Terrain','MeshPart') and collision_payload is not None:
            try:
                collider=decode_collision_geometry(collision_payload,source=key)
            except (TypeError,ValueError) as exc:
                unsupported.append((key,f'malformed decoded collision geometry: {exc}'))
            else:
                colliders.append(collider)
            continue
        if class_name == 'MeshPart' and allow_visual_mesh_approximation:
            mesh=(visual_meshes.get(key) if isinstance(visual_meshes,dict) else None)
            anchored=props.get('Anchored')
            admitted_anchor=(anchored is True or (include_unanchored and anchored is False))
            frame,record_type=_received_part_frame(world,key,props,_property_type_map(schema,instance['class']))
            if (isinstance(mesh,TriangleMesh) and mesh.source==key
                    and props.get('CanCollide') is True and admitted_anchor
                    and frame is not None and _received_rotation(frame,record_type) is not None):
                colliders.append(TriangleMesh(mesh.vertices,mesh.triangles,key,
                    mesh.assumptions+('opt-in visual mesh approximation; MeshPart CollisionFidelity/collision asset is not available',)))
            else:
                unsupported.append((key,'visual mesh approximation missing or lacks explicit collidable, anchor, and pose evidence'))
            continue
        if class_name == 'Terrain':
            unsupported.append((key, 'terrain voxels are not modeled'))
            continue
        if class_name == 'MeshPart':
            unsupported.append((key, 'mesh collision shape is not modeled'))
            continue
        if not _is_part_descendant(class_name):
            continue
        property_types = _property_type_map(schema, instance['class'])
        can_collide_inferred = False
        if 'CanCollide' in props:
            can_collide = props['CanCollide']
        else:
            can_collide, can_collide_inferred = _default_can_collide(
                world, key, class_name,
                allow_network_constructor_defaults=allow_network_constructor_defaults)
        if can_collide is not True:
            if can_collide is None:
                unsupported.append((key, 'CanCollide default unavailable without enabled fresh complete creation inference'))
            continue
        anchored=props.get('Anchored')
        if anchored is not True and not (include_unanchored and anchored is False):
            unsupported.append((key, 'part Anchored state is unavailable or unanchored support was not enabled'))
            continue
        shape_ok, shape_inferred = _block_shape(
            schema, property_types, props,
            allow_default=(allow_network_constructor_defaults
                           and _fresh_complete_creation(world, key)))
        if not shape_ok:
            unsupported.append((key, 'Part shape is unknown or not a supported Block'))
            continue
        cframe, record_type = _received_part_frame(world, key, props, property_types)
        if cframe is None:
            unsupported.append((key, 'no position-bearing supported CFrame/ReplicationPV property'))
            continue
        size = _part_size(props, property_types)
        rotation = _received_rotation(cframe,record_type)
        if rotation is None:
            unsupported.append((key, 'rotation is opaque or not a valid rigid matrix'))
            continue
        try:
            center = _vector(cframe.get('position'), 'CFrame.position')
            extent = _vector(size, 'Size')
        except (AttributeError, ValueError):
            unsupported.append((key, 'invalid or missing CFrame/schema-typed Vector3 size'))
            continue
        if any(v <= 0 for v in extent):
            unsupported.append((key, 'Size must be positive'))
            continue
        half = tuple(v / 2 for v in extent)
        assumptions = []
        if can_collide_inferred:
            assumptions.append(f'CanCollide=true: exact-build {class_name} constructor default applied to fresh complete network creation; network reset path is unproven')
        if shape_inferred:
            assumptions.append(f'shape=PartType.Block(1): documented new-Part default applied to fresh complete {class_name} network creation; exact-build constructor/network reset path is unproven')
        try:
            if rotation == (1.,0.,0.,0.,1.,0.,0.,0.,1.):
                lower,upper=tuple(center[i]-half[i] for i in range(3)),tuple(center[i]+half[i] for i in range(3))
                if any(not math.isfinite(v) for v in lower+upper) or any(lower[i] >= upper[i] for i in range(3)):
                    raise ValueError('invalid world bounds')
                collider=AABB(lower,upper,key,tuple(assumptions))
            else:
                collider=OBB(center,half,rotation,key,tuple(assumptions))
        except ValueError:
            unsupported.append((key,'world collision bounds are nonfinite or degenerate'))
            continue
        colliders.append(collider)
    return GroundGeometry(tuple(colliders), tuple(unsupported))


def ground_raycast(position: object, geometry: GroundGeometry, *,
                   foot_offset: float = 3.0, max_distance: float = 0.25) -> GroundHit | None:
    """Find the closest upward-facing supported surface beneath actor center."""
    x, y, z = _vector(position, 'position')
    if isinstance(foot_offset,bool) or not isinstance(foot_offset,(int,float)) or not math.isfinite(foot_offset) or foot_offset < 0:
        raise ValueError('foot_offset must be finite and non-negative')
    if isinstance(max_distance,bool) or not isinstance(max_distance,(int,float)) or not math.isfinite(max_distance) or max_distance < 0:
        raise ValueError('max_distance must be finite and non-negative')
    foot = (x, y - foot_offset, z)
    margin=1e-10*max(1.,abs(x),abs(foot[1]),abs(z),max_distance)
    hits = []
    for collider in geometry.colliders:
        # The query is vertical, so cached world bounds cheaply discard boxes
        # outside its X/Z column or finite Y segment before exact face testing.
        lower,upper=collider.minimum,collider.maximum
        if (x < lower[0]-margin or x > upper[0]+margin or z < lower[2]-margin or z > upper[2]+margin
                or foot[1] < lower[1]-margin or foot[1]-max_distance > upper[1]+margin):
            continue
        if isinstance(collider,TriangleMesh):
            hit=_ray_triangle_mesh(foot,collider,max_distance,margin)
        elif isinstance(collider,VoxelGrid):
            hit=_ray_voxels(foot,collider,max_distance,margin)
        elif isinstance(collider,OBB):
            hit = ray_obb(foot,(0.,-1.,0.),collider.center,collider.half_size,collider.rotation,max_distance)
        else:
            hit = ray_aabb(foot, (0., -1., 0.), collider.minimum, collider.maximum, max_distance)
        if hit is not None and hit.normal[1] > 1e-7:
            hits.append(GroundHit(foot[1] - hit.distance, hit.distance, collider,hit.normal))
    return min(hits, key=lambda item: item.distance) if hits else None


def footprint_ground_raycast(position: object, geometry: GroundGeometry, *,
                              half_size: object, foot_offset: float = 3.0,
                              max_distance: float = 0.25,
                              max_slope_angle: float = 89.0,
                             probe_count: int = 9, prefer_center: bool = True) -> GroundHit | None:
    """Find walkable support under a bounded set of points inside a root footprint.

    The center probe wins whenever it finds a walkable surface. Otherwise the
    nearest horizontal probe wins, with higher support preferred on equal-radius
    probes. With prefer_center=False, the highest surface wins so a falling
    footprint stops at its earliest vertical contact. Every sample uses
    ``ground_raycast`` so mesh holes, voxel occupancy,
    and exact box faces remain respected; bounds are never used as support.
    """
    x, y, z = _vector(position, 'position')
    half = _vector(half_size, 'half_size')
    if any(value <= 0 for value in half):
        raise ValueError('footprint half sizes must be positive')
    if not isinstance(geometry, GroundGeometry):
        raise ValueError('geometry must be GroundGeometry')
    if (isinstance(foot_offset, bool) or not isinstance(foot_offset, (int, float))
            or not math.isfinite(foot_offset) or foot_offset < 0):
        raise ValueError('foot_offset must be finite and non-negative')
    if (isinstance(max_distance, bool) or not isinstance(max_distance, (int, float))
            or not math.isfinite(max_distance) or max_distance < 0):
        raise ValueError('max_distance must be finite and non-negative')
    if (isinstance(max_slope_angle, bool) or not isinstance(max_slope_angle, (int, float))
            or not math.isfinite(max_slope_angle) or not 0 <= max_slope_angle <= 89):
        raise ValueError('max_slope_angle must be finite and between 0 and 89 degrees')
    if type(probe_count) is not int or probe_count not in (5, 9):
        raise ValueError('probe_count must be 5 or 9')
    if type(prefer_center) is not bool:
        raise ValueError('prefer_center must be boolean')

    # All offsets are strictly inside the root's horizontal footprint.
    edge_x, edge_z = half[0]*.8, half[2]*.8
    probes = [(0., 0.)]
    probes.extend(((edge_x, 0.), (-edge_x, 0.), (0., edge_z), (0., -edge_z)))
    if probe_count == 9:
        corner_x, corner_z = half[0]*.65, half[2]*.65
        probes.extend(((corner_x, corner_z), (corner_x, -corner_z),
                       (-corner_x, corner_z), (-corner_x, -corner_z)))

    minimum_normal_y = math.cos(math.radians(max_slope_angle))
    candidates = []
    for index, (dx, dz) in enumerate(probes):
        hit = ground_raycast((x+dx, y, z+dz), geometry, foot_offset=foot_offset,
                             max_distance=max_distance)
        if hit is None or hit.normal[1] + 1e-12 < minimum_normal_y:
            continue
        if index == 0 and prefer_center:
            return hit
        ordering = (dx*dx+dz*dz, -hit.y) if prefer_center else (-hit.y, dx*dx+dz*dz)
        candidates.append((*ordering, index, hit))
    if not candidates:
        return None
    return min(candidates, key=lambda candidate: candidate[:3])[3]


def _ray_triangle_mesh(origin: Vector3, mesh: TriangleMesh, max_distance: float, margin: float):
    x,y,z=origin; best=None
    if x < mesh.minimum[0]-margin or x > mesh.maximum[0]+margin or z < mesh.minimum[2]-margin or z > mesh.maximum[2]+margin:
        return None
    for indices in mesh.triangles:
        a,b,c=(mesh.vertices[i] for i in indices)
        ab=tuple(b[i]-a[i] for i in range(3)); ac=tuple(c[i]-a[i] for i in range(3))
        n=(ab[1]*ac[2]-ab[2]*ac[1],ab[2]*ac[0]-ab[0]*ac[2],ab[0]*ac[1]-ab[1]*ac[0])
        if n[1] <= 1e-12: continue
        yy=a[1]-(n[0]*(x-a[0])+n[2]*(z-a[2]))/n[1]
        distance=y-yy
        if distance < -margin or distance > max_distance+margin: continue
        # Barycentric containment in XZ projection, valid for upward-facing faces.
        den=(b[2]-c[2])*(a[0]-c[0])+(c[0]-b[0])*(a[2]-c[2])
        if abs(den)<1e-15: continue
        u=((b[2]-c[2])*(x-c[0])+(c[0]-b[0])*(z-c[2]))/den
        v=((c[2]-a[2])*(x-c[0])+(a[0]-c[0])*(z-c[2]))/den
        w=1-u-v
        if min(u,v,w)<-1e-10: continue
        d=max(0.,distance)
        if best is None or d<best.distance: best=GroundHit(yy,d,mesh,tuple(q/math.sqrt(sum(t*t for t in n)) for q in n))
    return best


def _ray_voxels(origin: Vector3, grid: VoxelGrid, max_distance: float, margin: float):
    x,y,z=origin; ox,oy,oz=grid.origin; sx,sy,sz=grid.cell_size; nx,ny,nz=grid.dimensions
    ix=math.floor((x-ox)/sx); iz=math.floor((z-oz)/sz)
    if not (0<=ix<nx and 0<=iz<nz): return None
    best=None
    for iy in range(ny):
        index=(ix*ny+iy)*nz+iz
        if not grid.occupancy[index]: continue
        if iy+1<ny and grid.occupancy[(ix*ny+iy+1)*nz+iz]: continue
        top=oy+(iy+1)*sy; distance=y-top
        if -margin<=distance<=max_distance+margin:
            candidate=GroundHit(top,max(0.,distance),grid,(0.,1.,0.))
            if best is None or candidate.distance<best.distance: best=candidate
    return best


def follow_ground(position: object, displacement: object, geometry: GroundGeometry, *,
                  foot_offset: float = 3.0, probe_distance: float = .05) -> GroundStep | None:
    """Continue a grounded step along its support plane when support persists.

    Probe around the old face's extrapolated height, not an elevated global
    search origin. This prevents snapping onto overhead floors or across drops.
    This is point-foot contact, not a capsule, stair or wall collision solver.
    """
    x,y,z = _vector(position,'position');dx,_,dz = _vector(displacement,'displacement')
    if isinstance(probe_distance,bool) or not isinstance(probe_distance,(int,float)) or not math.isfinite(probe_distance) or probe_distance < 0:
        raise ValueError('probe_distance must be finite and non-negative')
    support = ground_raycast((x,y,z),geometry,foot_offset=foot_offset,max_distance=probe_distance)
    if support is None:return None
    nx,ny,nz = support.normal
    # Vertical faces cannot support a standing point. Extremely steep faces
    # are not extrapolated into arbitrarily large vertical movement.
    if ny < math.cos(math.radians(89)):return None
    expected_y = support.y-(nx*dx+nz*dz)/ny
    target = (x+dx,expected_y+foot_offset+probe_distance,z+dz)
    hit = ground_raycast(target,geometry,foot_offset=foot_offset,max_distance=2*probe_distance)
    if hit is None or abs(hit.y-expected_y) > probe_distance+1e-9:return None
    return GroundStep((target[0],hit.y+foot_offset,target[2]),0.,True,hit.collider)


def step_vertical(position: object, vertical_velocity: float, dt: float,
                  geometry: GroundGeometry, grounded: bool, *, foot_offset: float = 3.0,
                  gravity: float = 196.2, max_fall_speed: float = 512.0,
                  probe_distance: float = 0.05, support_half_size=None,
                  max_slope_angle: float = 89.0) -> GroundStep:
    """Integrate gravity and clamp downward motion at supported ground surfaces.

    Position is the actor center. This is a vertical-only helper; it does not
    perform horizontal/ceiling collision, slope sliding, moving-platform motion, or
    full Roblox character physics. ``dt`` is capped at 0.25 seconds.
    """
    x, y, z = _vector(position, 'position')
    values = (vertical_velocity, dt, foot_offset, gravity, max_fall_speed, probe_distance)
    if any(isinstance(v, bool) or not isinstance(v, (int, float)) or not math.isfinite(v)
           for v in values):
        raise ValueError('step parameters must be finite numbers')
    if dt < 0 or dt > 0.25 or foot_offset < 0 or gravity < 0 or max_fall_speed <= 0 or probe_distance < 0:
        raise ValueError('step parameters are outside supported bounds')
    if support_half_size is not None:
        support_half_size = _vector(support_half_size, 'support_half_size')
        if any(v <= 0 for v in support_half_size):
            raise ValueError('support half sizes must be positive')
        if (type(max_slope_angle) not in (int, float) or not math.isfinite(max_slope_angle)
                or not 0 <= max_slope_angle <= 89):
            raise ValueError('max_slope_angle must be finite and between 0 and 89 degrees')

    def support_query(distance, *, prefer_center=True):
        center = ground_raycast((x, y, z), geometry, foot_offset=foot_offset,
                                max_distance=distance)
        if support_half_size is None or center is not None and prefer_center:
            return center
        edge = footprint_ground_raycast(
            (x, y, z), geometry, half_size=support_half_size,
            foot_offset=foot_offset, max_distance=distance,
            max_slope_angle=max_slope_angle, prefer_center=prefer_center)
        # Center collision remains available on steep slopes so the existing
        # controller can slide there. Additional footprint support is walkable.
        return min((hit for hit in (center, edge) if hit is not None),
                   key=lambda hit: hit.distance, default=None)

    # A grounded actor remains attached only while the support probe still sees
    # an upward face. This makes walking off an edge transition into a fall.
    if grounded:
        support = support_query(probe_distance)
        if support is not None and abs((y - foot_offset) - support.y) <= probe_distance + 1e-9:
            return GroundStep((x, support.y + foot_offset, z), 0.0, True, support.collider)
        # A received rig resize can increase the offset enough to put the new
        # foot point below the previous support. Recover only an already-grounded
        # non-rising actor, searching no farther than its root-to-foot segment.
        if vertical_velocity <= 0 and foot_offset > probe_distance:
            support = ground_raycast((x, y, z), geometry, foot_offset=0.,
                                     max_distance=foot_offset + probe_distance)
            if support is not None:
                return GroundStep((x, support.y + foot_offset, z), 0.0, True, support.collider)

    # Split at the apex so descending collision probes start at the highest
    # point, even when the net displacement over this frame is upward.
    if gravity > 0 and 0 < vertical_velocity < gravity*dt:
        up_time = vertical_velocity/gravity
        apex = (x, y+vertical_velocity*up_time/2, z)
        return step_vertical(apex, 0., dt-up_time, geometry, False,
                             foot_offset=foot_offset, gravity=gravity,
                             max_fall_speed=max_fall_speed, probe_distance=probe_distance,
                             support_half_size=support_half_size, max_slope_angle=max_slope_angle)
    # Integrate constant acceleration exactly, including a terminal-speed
    # crossing within the interval. This keeps JumpHeight independent of dt.
    start_v = max(-max_fall_speed, vertical_velocity)
    accelerating = min(dt, (start_v+max_fall_speed)/gravity) if gravity > 0 else dt
    next_v = max(-max_fall_speed, start_v-gravity*accelerating)
    displacement = start_v*accelerating-gravity*accelerating*accelerating/2 + next_v*(dt-accelerating)
    if displacement <= 0 and next_v <= 0:
        hit = support_query(-displacement + probe_distance, prefer_center=False)
        if hit is not None:
            return GroundStep((x, hit.y + foot_offset, z), 0.0, True, hit.collider)
    return GroundStep((x, y + displacement, z), next_v, False)
