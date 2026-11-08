"""Small, mass-aware helpers for local impulse prediction.

Impulse is measured in mass * studs/second, so delta velocity is J / m.
This helper only models translation. It does not solve contacts, joints,
rotation, or ongoing force constraints, and it never invents an assembly mass.
"""
from __future__ import annotations

import math
import numbers
import json
from functools import lru_cache
from pathlib import Path
from movement_collision import rotation_matrix
from physics_linear_velocity import vector as constraint_vector, scalar as constraint_scalar, unit as constraint_unit

MASS_SOURCES = ('received_assembly_mass', 'received_assembly_parts',
                'received_primitive_density', 'received_geometry_density_approximate',
                'authored_joint_assembly_parts','authored_joint_primitive_density','authored_joint_geometry_density_approximate',
                'authored_mass_assembly_parts','authored_mass_primitive_density','authored_mass_geometry_density_approximate')


def _primitive_volume(part, size, allow_authored=False):
    """Return supported primitive volume, or None when geometry is unreceived.

    Round primitives use their real circular diameter, not an ellipsoid:
    https://devforum.roblox.com/t/improvements-to-part-shape-size/2443389
    """
    if any(axis <= 0 for axis in size):
        raise ValueError('Size must be positive')
    kind = part.get('class_name')
    if kind == 'WedgePart':
        shape = 3
    elif kind == 'CornerWedgePart':
        shape = 4
    elif kind == 'VehicleSeat':
        shape = 1
    elif kind in {'Part', 'Seat', 'SpawnLocation', 'Platform', 'SkateboardPlatform', 'FlagStand'}:
        if not _force_available(part, 'Shape', allow_authored):
            return None
        shape = part.get('properties', {}).get('Shape')
        if isinstance(shape, dict) and set(shape) == {'$enum'}:
            item = shape['$enum']
            if not isinstance(item, dict) or item.get('type') != 'PartType':
                raise ValueError('invalid Shape enum')
            shape = item.get('value')
        if type(shape) is not int or not 0 <= shape <= 4:
            raise ValueError('invalid primitive Shape')
    else:
        return None
    x, y, z = size
    if shape == 0:
        return math.pi * min(size)**3 / 6
    if shape == 2:
        return math.pi * x * min(y, z)**2 / 4
    return x * y * z * {1: 1., 3: .5, 4: 1/3}[shape]


def _vector3(value, field):
    if not isinstance(value, (list, tuple)) or len(value) != 3:
        raise ValueError(f'{field} must contain three finite numbers')
    result = []
    for item in value:
        if isinstance(item, bool) or not isinstance(item, numbers.Real):
            raise ValueError(f'{field} must contain three finite numbers')
        item = float(item)
        if not math.isfinite(item) or abs(item) > 1_000_000:
            raise ValueError(f'{field} must contain three finite bounded numbers')
        result.append(item)
    return tuple(result)


def received_assembly_mass(snapshot, root_id=None, *, motion_members=None, allow_authored=False):
    """Return mass and provenance from received assembly or member evidence.

    The fallback finds the root's rigid-joint component, then sums received
    per-part ``Mass`` or supported primitive volume times explicit custom
    density/material density. Missing shape/mesh geometry retains the existing
    bounding-box estimate and its explicitly approximate provenance.
    """
    if root_id is None:
        root = snapshot
    else:
        nodes = {node['id']: node for node in snapshot.get('nodes', [])
                 if isinstance(node, dict) and isinstance(node.get('id'), str)} if isinstance(snapshot, dict) else {}
        root = nodes.get(root_id)
    if not isinstance(root, dict):
        return None, None
    props = root.get('properties', {})
    origins = root.get('origins', {})
    if not isinstance(props, dict) or not isinstance(origins, dict):
        return None, None
    if origins.get('Anchored') == 'received' and props.get('Anchored') is True:
        return None, None
    mass = props.get('AssemblyMass') if isinstance(props, dict) else None
    origin = origins.get('AssemblyMass') if isinstance(origins, dict) else None
    if motion_members is None and origin == 'received' and not isinstance(mass, bool) and isinstance(mass, numbers.Real):
        mass = float(mass)
        if math.isfinite(mass) and mass > 0:
            return mass, 'received_assembly_mass'
    if (root_id is None or not isinstance(snapshot, dict)
            or (motion_members is None and snapshot.get('initial_replication_complete') is not True)):
        return None, None
    parent_path = Path(__file__).resolve().parents[1] / 'runtime/class-parents-745.json'
    try:
        parents = json.loads(parent_path.read_text()).get('parents', {})
    except (OSError, ValueError, TypeError):
        parents = {}

    def is_base_part(node):
        name, seen = node.get('class_name'), set()
        while isinstance(name, str) and name not in seen:
            if name == 'BasePart':
                return True
            seen.add(name)
            name = parents.get(name)
        return node.get('class_name') in {'Part', 'MeshPart', 'UnionOperation', 'WedgePart',
                                           'CornerWedgePart', 'TrussPart', 'Seat', 'VehicleSeat',
                                           'SpawnLocation', 'PartOperation'}

    character_id = root.get('parent')
    player = nodes.get(snapshot.get('local_player'))
    character = _ref(player.get('properties', {}).get('Character')) if player else None
    character_id = character or character_id
    descendants = set()
    for node in nodes.values():
        cursor = node
        seen = set()
        while isinstance(cursor, dict) and isinstance(cursor.get('id'), str) and cursor['id'] not in seen:
            if cursor.get('parent') == character_id:
                descendants.add(node['id'])
                break
            seen.add(cursor['id'])
            cursor = nodes.get(cursor.get('parent'))
    parts = [node for node in nodes.values() if node.get('id') in descendants and is_base_part(node)]
    if root_id not in {part.get('id') for part in parts}:
        return None, None

    # An assembly may include a part outside the character hierarchy, such as
    # an equipped object or a welded platform. Follow those connections too.
    all_parts = {node['id']: node for node in nodes.values() if is_base_part(node)}
    if motion_members is not None:
        component=set(motion_members)
        if root_id not in component or not component.issubset(all_parts):return None,None
        component_parts=[part for key,part in all_parts.items() if key in component]
    else:
        part_ids = set(all_parts)
        graph = {part_id: set() for part_id in part_ids}
        unresolved = set()
        rigid = {'Weld', 'Snap', 'ManualWeld', 'Motor', 'Motor6D', 'WeldConstraint'}
        for joint in nodes.values():
            if joint.get('class_name') not in rigid:
                continue
            props = joint.get('properties', {})
            origins = joint.get('origins', {})
            if not isinstance(props, dict) or not isinstance(origins, dict):
                continue
            if any(origins.get(key) == 'received' and props.get(key) is False
                   for key in ('Enabled', 'Active')):
                continue
            if origins.get('Part0') != 'received' or origins.get('Part1') != 'received':
                continue
            a, b = _ref(props.get('Part0')), _ref(props.get('Part1'))
            if a in graph and b in graph:
                graph[a].add(b); graph[b].add(a)
            elif a in graph or b in graph:
                # A dangling reference cannot prove a complete assembly mass.
                if a in graph and b is not None:
                    unresolved.add(a)
                if b in graph and a is not None:
                    unresolved.add(b)
        component, pending = set(), [root_id]
        while pending:
            current = pending.pop()
            if current in component:
                continue
            component.add(current)
            pending.extend(graph.get(current, ()))
        if component & unresolved:
            return None, None
        component_parts = [all_parts[part_id] for part_id in sorted(component)]
        if len(component_parts) == 1 and len(parts) > 1:
            # Do not mistake an absent/unsupported joint graph for a one-part assembly.
            known_roots = [_ref(part.get('properties', {}).get('AssemblyRootPart'))
                           if _received(part, 'AssemblyRootPart') else None for part in parts]
            if any(value is None for value in known_roots) or known_roots.count(root_id) != 1:
                return None, None

    material_density = {
        256: .7, 272: .7, 288: .7, 512: .35, 528: .35, 784: 2.563,
        788: 2.691, 800: 2.691, 804: 2.691, 816: 2.403, 820: 2.691,
        832: 2.691, 836: 2.691, 848: 1.922, 864: 2.403, 880: 2.691,
        896: 2.691, 912: 2.691, 1040: 7.85, 1056: 7.85, 1072: 2.7,
        1088: 7.85, 1280: .9, 1284: .9, 1296: 1.602, 1312: .7,
        1328: .9, 1344: .9, 1360: .9, 1376: 2.36, 1392: 2.165,
        1536: .919, 1552: .919, 1568: 2.403, 1584: 2.403,
        2304: .7, 2305: 1.1, 2306: 2.4, 2307: 2., 2308: 2.36,
        2309: .86, 2310: .75, 2311: 1.3,
    }
    total = 0.0
    used_geometry_density = False
    approximate_geometry = False
    actual_root = (_ref(root.get('properties', {}).get('AssemblyRootPart'))
                   if _received(root, 'AssemblyRootPart') else None)
    if actual_root is not None and actual_root not in component and not allow_authored:
        return None, None
    if actual_root is None and len(component_parts) == 1:
        actual_root = root_id
    if allow_authored and any(part.get('properties',{}).get('Massless') is True for part in component_parts):
        actual_root=_authored_mass_root(component_parts)
        if actual_root is None:return None,None
    for part in component_parts:
        p = part.get('properties', {})
        o = part.get('origins', {})
        if not isinstance(p, dict) or not isinstance(o, dict):
            return None, None
        # Values omitted from a known-complete creation keep their constructor
        # default (e.g. Material=Plastic on avatar parts). Work on a view that
        # carries them as received so the checks below stay unchanged.
        defaults = {key: value for key in ('Massless', 'CustomPhysicalProperties', 'Material', 'MaterialVariant')
                    if not _force_available(part, key, allow_authored)
                    and (value := _constructor_default(part, key)) is not _MISSING}
        # Size arrives under its wire name `size`; the snapshot copies it to
        # Size as derived_received. It is the received value, not an estimate.
        wire_size = (o.get('Size') == 'derived_received' and isinstance(p.get('size'), (list, dict))
                     and o.get('size', 'received') == 'received' and p.get('Size') == p.get('size'))
        if defaults or wire_size:
            part = {**part, 'properties': {**p, **defaults},
                    'origins': {**o, **{key: 'received' for key in defaults},
                                **({'Size': 'received'} if wire_size else {})}}
            p, o = part['properties'], part['origins']
        if _received(part, 'Anchored') and p.get('Anchored') is True:
            return None, None
        if not _force_available(part,'Massless',allow_authored) or type(p.get('Massless')) is not bool:
            return None, None
        if p['Massless'] and part['id'] != actual_root:
            continue
        part_mass = p.get('Mass')
        geometry_authored=allow_authored and _mass_geometry_authored(part)
        if not geometry_authored and o.get('Mass') == 'received' and not isinstance(part_mass, bool) and isinstance(part_mass, numbers.Real):
            part_mass = float(part_mass)
        else:
            try:
                size = _snapshot_vector(p.get('Size')) if _force_available(part, 'Size',allow_authored) else None
                volume = _primitive_volume(part, size,allow_authored) if size is not None else None
            except ValueError:
                return None, None
            custom = p.get('CustomPhysicalProperties')
            density = None
            if not _force_available(part, 'CustomPhysicalProperties',allow_authored):
                return None, None
            if isinstance(custom, dict) and set(custom) == {'$physical_properties'}:
                custom = custom['$physical_properties']
            if isinstance(custom, (list, tuple)) and len(custom) in (5, 6):
                density = custom[0]
            elif custom is None and _force_available(part, 'Material',allow_authored):
                density = material_density.get(_enum_value(p.get('Material'),'Material'))
            if (size is None or isinstance(density, bool)
                    or not isinstance(density, numbers.Real)):
                return None, None
            if not math.isfinite(float(density)) or float(density) <= 0:
                return None, None
            if custom is None and _force_available(part, 'MaterialVariant',allow_authored) and p.get('MaterialVariant') not in ('', None):
                return None, None
            if volume is None:
                # An edited mesh cannot reuse its old received Mass or claim
                # that its bounding box is its new physical volume.
                if geometry_authored:return None,None
                volume = size[0] * size[1] * size[2]
                approximate_geometry = True
            part_mass = volume * float(density)
            used_geometry_density = True
        if not math.isfinite(part_mass) or part_mass <= 0:
            return None, None
        total += part_mass
    source = ('received_geometry_density_approximate' if approximate_geometry else
              'received_primitive_density' if used_geometry_density else 'received_assembly_parts')
    return (total, source) if math.isfinite(total) and total > 0 else (None, None)


def _mass_geometry_authored(part):
    return any(part.get('origins',{}).get(key)=='local' for key in
               ('Size','Shape','Material','MaterialVariant','CustomPhysicalProperties'))


def _authored_mass_root(parts):
    """Apply the native root ranking only when massless membership needs it."""
    candidates=list(parts)
    for key in ('Anchored','Massless'):
        if any(not _force_available(p,key,True) or type(p.get('properties',{}).get(key)) is not bool for p in candidates):return None
        best=max((p['properties'][key] if key=='Anchored' else not p['properties'][key]) for p in candidates)
        candidates=[p for p in candidates if (p['properties'][key] if key=='Anchored' else not p['properties'][key])==best]
    if len(candidates)==1:return candidates[0]['id']
    priorities=[p.get('properties',{}).get('RootPriority') if _force_available(p,'RootPriority',True) else None for p in candidates]
    if any(type(v) not in (int,float) or not math.isfinite(v) or int(v)!=v or not -127<=v<=127 for v in priorities):return None
    candidates=[p for p,v in zip(candidates,priorities) if v==max(priorities)]
    if len(candidates)==1:return candidates[0]['id']
    def score(part):
        if not _force_available(part,'Size',True):raise ValueError('root Size unavailable')
        size=_snapshot_vector(part.get('properties',{}).get('Size'))
        if any(v<=0 for v in size):raise ValueError('invalid root Size')
        name=part.get('name',part.get('properties',{}).get('Name',''))
        multiplier=20 if part.get('class_name') in ('Seat','VehicleSeat') else 10 if name=='HumanoidRootPart' else 5 if name=='Torso' else 1
        return math.floor(max(size[0]*size[1],size[1]*size[2],size[0]*size[2])*50)*multiplier
    try:return max(candidates,key=score)['id']
    except (ValueError,TypeError):return None


def predicted_assembly_mass(snapshot,root_id):
    nodes={n.get('id'):n for n in snapshot.get('nodes',[]) if isinstance(n,dict)}
    root=nodes.get(root_id,{})
    assembly=received_character_motion_parts(snapshot,root_id,root.get('parent'),include_anchored=True)
    if any(nodes[key].get('properties',{}).get('Anchored') is True for key in assembly):
        return None,None
    members=received_character_motion_parts(snapshot,root_id,root.get('parent'))
    authored=snapshot.get('allow_authored_motion') and any(
        _mass_geometry_authored(nodes[key]) or any(nodes[key].get('origins',{}).get(field)=='local'
            for field in ('Massless','RootPriority')) for key in members)
    if not authored and not snapshot.get('motion_topology_changed'):
        return received_assembly_mass(snapshot,root_id)
    mass,source=received_assembly_mass(snapshot,root_id,motion_members=members,allow_authored=True)
    prefix='authored_mass_' if authored else 'authored_joint_'
    return mass,source.replace('received_',prefix,1) if source else None


def apply_impulse(velocity, impulse, mass, *, anchored=False, alive=True):
    """Apply an impulse to velocity or raise ValueError for unsafe/unknown state."""
    if anchored:
        raise ValueError('anchored')
    if not alive:
        raise ValueError('dead')
    if isinstance(mass, bool) or not isinstance(mass, numbers.Real):
        raise ValueError('mass_unavailable')
    mass = float(mass)
    if not math.isfinite(mass) or mass <= 0:
        raise ValueError('mass_unavailable')
    current = _vector3(velocity, 'velocity')
    impulse = _vector3(impulse, 'impulse')
    return tuple(v + j / mass for v, j in zip(current, impulse))


def force_impulse(force, duration):
    """Convert an explicitly supplied constant force sample to impulse F*dt."""
    force = _vector3(force, 'force')
    if isinstance(duration, bool) or not isinstance(duration, numbers.Real):
        raise ValueError('duration must be finite and non-negative')
    duration = float(duration)
    if not math.isfinite(duration) or duration < 0 or duration > 10:
        raise ValueError('duration must be finite and between 0 and 10 seconds')
    return tuple(component * duration for component in force)


def _received(node, key):
    origins = node.get('origins', {}) if isinstance(node, dict) else {}
    return isinstance(origins, dict) and origins.get(key) == 'received'


_MISSING = object()
# The generated constructor table has no enum defaults; these classes are
# constructed with Plastic (mirrors property_descriptor in world_bindings.cpp).
_PLASTIC_CLASSES = frozenset(('Part','MeshPart','WedgePart','CornerWedgePart','UnionOperation','Seat','VehicleSeat'))


@lru_cache(maxsize=1)
def _constructor_properties():
    path = Path(__file__).resolve().parents[1] / 'runtime/class-constructors-745.json'
    try:
        return json.loads(path.read_text()).get('classes', {})
    except (OSError, ValueError):
        return {}


def _constructor_default(node, key):
    """Default for a property the server omitted from a known-complete creation.

    Same rule as the native runtime's completed_creation_default: only for
    fresh, complete creation records (constructor_defaults_known), and only
    when the property was never received or authored.
    """
    if (not isinstance(node, dict) or node.get('constructor_defaults_known') is not True
            or key in node.get('properties', {})):
        return _MISSING
    if key == 'Material' and node.get('class_name') in _PLASTIC_CLASSES:
        return {'$enum': {'type': 'Material', 'value': 256, 'name': 'Plastic'}}
    descriptor = _constructor_properties().get(node.get('class_name'), {}).get('properties', {}).get(key)
    if isinstance(descriptor, dict) and 'value' in descriptor:
        return descriptor['value']
    return _MISSING


def _snapshot_vector(value):
    if isinstance(value, dict) and set(value) == {'$vector3'}:
        value = value['$vector3']
    return _vector3(value, 'force')


def _ref(value):
    if isinstance(value, dict) and set(value) == {'$ref'}:
        return value['$ref']
    return value if isinstance(value, str) else None


def _enum_value(value, enum_name):
    if type(value) is int:
        return value
    if isinstance(value, dict) and isinstance(value.get('$enum'), dict):
        item = value['$enum']
        if item.get('type') == enum_name and type(item.get('value')) is int:
            return item['value']
    return None


def velocity_servo_values(velocity, max_force, gain):
    target=_snapshot_vector(velocity)
    if isinstance(max_force,dict) and '$vector3' in max_force:max_force=max_force['$vector3']
    if not isinstance(max_force,(list,tuple)) or len(max_force)!=3:raise ValueError('invalid MaxForce')
    limits=[]
    for value in max_force:
        if value=={'$number':'inf'}:value=math.inf
        if type(value) not in (int,float) or math.isnan(value) or value<0:raise ValueError('invalid MaxForce')
        limits.append(float(value))
    if type(gain) not in (int,float) or not math.isfinite(gain) or not 0<=gain<=3.402823466e38:
        raise ValueError('invalid BodyVelocity.P')
    return {'target_velocity':target,'max_force':tuple(limits),'gain':float(gain)}


def position_servo_values(position, max_force, gain, damping):
    values = velocity_servo_values(position, max_force, gain)
    if (type(damping) not in (int, float) or not math.isfinite(damping)
            or not 0 <= damping <= 3.402823466e38):
        raise ValueError('invalid BodyPosition.D')
    return {'target_position': values['target_velocity'],
            'max_force': values['max_force'], 'gain': values['gain'],
            'damping': float(damping)}


_FORCE_PARTS = {'Part', 'MeshPart', 'UnionOperation', 'WedgePart', 'CornerWedgePart',
                'TrussPart', 'Seat', 'VehicleSeat', 'SpawnLocation', 'PartOperation'}


def received_motion_joint_parts(snapshot):
    parts=set()
    for joint in snapshot.get('nodes',[]):
        if not isinstance(joint,dict) or joint.get('class_name') not in ('Weld','Snap','ManualWeld','Motor','Motor6D','WeldConstraint'):continue
        for key in ('Part0','Part1'):
            ref=_ref(joint.get('properties',{}).get(key))
            if _force_available(joint,key,snapshot.get('allow_authored_motion',False)) and isinstance(ref,str):parts.add(ref)
    return parts


def received_character_motion_parts(snapshot, root_id, character_id, *, include_anchored=False):
    """Resolve current character motion from received rigid topology.

    Explicit assembly references are useful for compact snapshots without
    joints. Once a part has joint evidence, that graph takes precedence so a
    disabled or destroyed connection cannot retain stale assembly membership.
    """
    nodes={n.get('id'):n for n in snapshot.get('nodes',[]) if isinstance(n,dict)}
    allow_authored=snapshot.get('allow_authored_motion',False)
    root=nodes.get(root_id)
    if not root or root.get('destroyed') or root.get('parent')!=character_id:
        return frozenset()

    def in_character(node):
        current=node;seen=set()
        while current and current.get('id') not in seen:
            seen.add(current.get('id'))
            if current.get('destroyed') or current.get('class_name') in ('WorldModel','Workspace'):return False
            parent=current.get('parent')
            if parent==character_id:return True
            current=nodes.get(parent)
        return False

    parts={key:n for key,n in nodes.items() if n.get('class_name') in _FORCE_PARTS and in_character(n)}
    if root_id not in parts:return frozenset()
    graph={key:set() for key in parts}
    has_topology=set(snapshot.get('motion_topology_history',()))|received_motion_joint_parts(snapshot)
    for joint in nodes.values():
        if joint.get('class_name') not in ('Weld','Snap','ManualWeld','Motor','Motor6D','WeldConstraint'):continue
        props=joint.get('properties',{})
        if not _force_available(joint,'Part0',allow_authored) or not _force_available(joint,'Part1',allow_authored):continue
        a,b=_ref(props.get('Part0')),_ref(props.get('Part1'))
        has_topology.update(key for key in (a,b) if key in parts)
        if joint.get('destroyed') or not in_character(joint) or a not in parts or b not in parts or a==b:continue
        if any(type(parts[key].get('properties',{}).get('Anchored')) is not bool for key in (a,b)):continue
        enabled_origin=joint.get('origins',{}).get('Enabled')
        if enabled_origin not in (('received','class_default','local') if allow_authored else ('received','class_default')) or props.get('Enabled') is not True:continue
        if _received(joint,'Active') and props.get('Active') is False:continue
        # A contradictory received assembly root is not permission to join an
        # unrelated body through a partially refreshed endpoint list.
        local_topology=allow_authored and any(joint.get('origins',{}).get(key)=='local' for key in ('Part0','Part1','Parent','Enabled'))
        if not local_topology and any(_received(parts[key],'AssemblyRootPart') and
               _ref(parts[key].get('properties',{}).get('AssemblyRootPart')) not in (None,root_id)
               for key in (a,b)):continue
        graph[a].add(b);graph[b].add(a)
    component=set();pending=[root_id]
    while pending:
        key=pending.pop()
        if key in component:continue
        component.add(key);pending.extend(graph[key]-component)
    if not include_anchored and any(parts[key].get('properties',{}).get('Anchored') is True for key in component):return frozenset()
    for key,node in parts.items():
        if key in has_topology or (not include_anchored and node.get('properties',{}).get('Anchored') is True):continue
        if _received(node,'AssemblyRootPart') and _ref(node.get('properties',{}).get('AssemblyRootPart'))==root_id:
            component.add(key)
    return frozenset(component)


def _force_available(node,key,allow_authored=False):
    return _received(node,key) or (allow_authored and isinstance(node,dict)
        and node.get('origins',{}).get(key)=='local')


def _force_position(node, allow_authored=False):
    props=node.get('properties',{})
    if _force_available(node,'CFrame',allow_authored):
        raw=props.get('CFrame')
        frame=raw.get('$cframe') if isinstance(raw,dict) else None
        if isinstance(frame,dict):return _snapshot_vector(frame.get('position'))
    if _force_available(node,'Position',allow_authored):
        return _snapshot_vector(props.get('Position'))
    raise ValueError('force parent position unavailable')


def _force_rotation(node, key, allow_authored=False):
    if not _force_available(node, key, allow_authored):
        raise ValueError('force reference rotation is not received')
    frame = node.get('properties', {}).get(key)
    if isinstance(frame, dict) and '$cframe' in frame:
        frame = frame['$cframe']
    rotation = frame.get('rotation') if isinstance(frame, dict) else None
    if (isinstance(rotation, (list, tuple)) and len(rotation) == 3
            and all(isinstance(row, (list, tuple)) and len(row) == 3 for row in rotation)):
        rotation = tuple(value for row in rotation for value in row)
    return rotation_matrix(rotation)


def _force_attachment_chain(nodes, attachment):
    """Find the physical parent without treating a Bone rest pose as animated."""
    chain, seen = [], set()
    current = attachment
    while isinstance(current, dict) and current.get('class_name') == 'Attachment':
        identifier = current.get('id')
        if not isinstance(identifier, str) or identifier in seen:
            raise ValueError('invalid attachment ancestry')
        seen.add(identifier);chain.append(current)
        current = nodes.get(current.get('parent'))
    if not chain or not isinstance(current, dict) or current.get('class_name') not in _FORCE_PARTS:
        raise ValueError('attachment physical parent is unavailable')
    return chain, current


def _force_attachment_rotation(nodes, attachment, allow_authored=False):
    chain, part = _force_attachment_chain(nodes, attachment)
    # A received absolute frame can resolve a chain whose local frames have
    # not arrived. Validate ancestry separately even when using that frame.
    for index, node in enumerate(chain):
        if _force_available(node, 'WorldCFrame',allow_authored):
            rotation = _force_rotation(node, 'WorldCFrame',allow_authored)
            chain = chain[:index]
            break
    else:
        rotation = _force_rotation(part, 'CFrame',allow_authored)
    for node in reversed(chain):
        local = _force_rotation(node, 'CFrame',allow_authored)
        rotation = tuple(sum(rotation[3*i+k]*local[3*k+j] for k in range(3))
                         for i in range(3) for j in range(3))
    return rotation, part


def _attachment_point(nodes, attachment, allow_authored):
    chain,part=_force_attachment_chain(nodes,attachment)
    if _force_available(attachment,'WorldCFrame',allow_authored):
        frame=attachment.get('properties',{}).get('WorldCFrame',{}).get('$cframe',{})
        return _snapshot_vector(frame.get('position'))
    point=_force_position(part,allow_authored);rotation=_force_rotation(part,'CFrame',allow_authored)
    for node in reversed(chain):
        if not _force_available(node,'CFrame',allow_authored):raise ValueError('attachment frame missing')
        local=node.get('properties',{}).get('CFrame',{}).get('$cframe',{})
        offset=_snapshot_vector(local.get('position'))
        point=tuple(point[i]+sum(rotation[3*i+j]*offset[j] for j in range(3)) for i in range(3))
        r=_force_rotation(node,'CFrame',allow_authored)
        rotation=tuple(sum(rotation[3*i+k]*r[3*k+j] for k in range(3)) for i in range(3) for j in range(3))
    return point


def _linear_velocity_record(nodes, source, root_id, known, attached_to_root, active_parent, allow_authored):
    props=source['properties']
    def required(key):
        if not known(source,key):raise ValueError('unreceived LinearVelocity field: '+key)
        return props.get(key)
    if required('Enabled') is not True:raise ValueError('disabled constraint')
    attachment=nodes.get(_ref(required('Attachment0')))
    _,part=_force_attachment_chain(nodes,attachment)
    if not attached_to_root(part):raise ValueError('constraint is not on the owned assembly')
    mode=_enum_value(required('VelocityConstraintMode'),'VelocityConstraintMode')
    relative=_enum_value(required('RelativeTo'),'ActuatorRelativeTo')
    if mode not in (0,1,2) or relative not in (0,1,2):raise ValueError('invalid constraint mode')
    rotation=(1.,0.,0.,0.,1.,0.,0.,0.,1.);reference=(0.,0.,0.);extra={}
    if relative!=2:
        reference_attachment=attachment if relative==0 else nodes.get(_ref(required('Attachment1')))
        rotation,reference_part=_force_attachment_rotation(nodes,reference_attachment,allow_authored)
        if not active_parent(reference_part):raise ValueError('reference is outside the active world')
        if relative==0:
            root_rotation=_force_rotation(nodes[root_id],'CFrame',allow_authored)
            if math.hypot(root_rotation[2],root_rotation[8])<1e-9:raise ValueError('root heading unavailable')
            extra['root_reference_yaw']=math.atan2(root_rotation[2],root_rotation[8])
        else:
            if attached_to_root(reference_part):raise ValueError('reference is the same assembly')
            rp=reference_part.get('properties',{})
            anchored=_received(reference_part,'Anchored') and rp.get('Anchored') is True
            reaction=required('ReactionForceEnabled')
            if type(reaction) is not bool:raise ValueError('invalid reaction flag')
            if reaction and not anchored:raise ValueError('two-body reaction solver unavailable')
            if not anchored:
                if not all(_received(reference_part,k) for k in ('AssemblyLinearVelocity','AssemblyAngularVelocity')):
                    raise ValueError('reference velocity unavailable')
                reference=_snapshot_vector(rp['AssemblyLinearVelocity']);angular=_snapshot_vector(rp['AssemblyAngularVelocity'])
                if any(angular):
                    if not _received(reference_part,'AssemblyCenterOfMass'):raise ValueError('reference center of mass unavailable')
                    center=_snapshot_vector(rp['AssemblyCenterOfMass']);point=_attachment_point(nodes,reference_attachment,allow_authored)
                    x,y,z=(point[i]-center[i] for i in range(3));wx,wy,wz=angular
                    reference=tuple(reference[i]+v for i,v in enumerate((wy*z-wz*y,wz*x-wx*z,wx*y-wy*x)))
    frame_axes=tuple(tuple(rotation[3*i+j] for i in range(3)) for j in range(3))
    if mode==2:
        axes=frame_axes;targets=constraint_vector(required('VectorVelocity'))
    elif mode==0:
        axes=(constraint_unit(required('LineDirection')),) if relative==2 else (frame_axes[0],)
        targets=(constraint_scalar(required('LineVelocity')),)
    else:
        axes=(constraint_unit(required('PrimaryTangentAxis')),constraint_unit(required('SecondaryTangentAxis'))) if relative==2 else frame_axes[:2]
        if abs(sum(axes[0][i]*axes[1][i] for i in range(3)))>1e-6:raise ValueError('plane axes are not orthogonal')
        targets=constraint_vector(required('PlaneVelocity'),2)
    limits_enabled=required('ForceLimitsEnabled')
    if type(limits_enabled) is not bool:raise ValueError('invalid force limit flag')
    limit_mode='Unlimited';limits=(math.inf,)*len(axes);maximum=math.inf
    if limits_enabled:
        limit_enum=_enum_value(required('ForceLimitMode'),'ForceLimitMode')
        if limit_enum not in (0,1):raise ValueError('invalid force limit mode')
        if limit_enum==0 or mode==0:
            limit_mode='Magnitude';maximum=constraint_scalar(required('MaxForce'),limit=True)
        else:
            limit_mode='PerAxis'
            limits=constraint_vector(required('MaxAxesForce' if mode==2 else 'MaxPlanarAxesForce'),len(axes),limit=True)
    return {'axes':axes,'targets':targets,'reference_velocity':reference,'limit_mode':limit_mode,
            'limits':limits,'max_force':maximum,**extra}


def collect_received_force_sources(snapshot, root_id, *, allow_authored=False):
    """Extract supported force and velocity movers on ``root_id``.

    Fields must be received, or explicitly authored when allow_authored is set.

    Attachment0/Attachment1 force directions use received rigid frames. Own
    assembly references also carry a heading baseline for local turning.
    BodyVelocity and BodyPosition retain their targets and feedback gains.
    LinearVelocity resolves constrained axes and reference-frame velocity.
    Off-center torque, Bone poses and dynamic two-body reactions remain unsupported.
    """
    if not isinstance(snapshot, dict) or not isinstance(snapshot.get('nodes'), list):
        return []
    nodes = {node['id']: node for node in snapshot['nodes']
             if isinstance(node, dict) and isinstance(node.get('id'), str)}
    out = []
    known=lambda node,key:_force_available(node,key,allow_authored)
    root_ancestors=set();cursor=root_id
    while isinstance(cursor,str) and cursor not in root_ancestors:
        root_ancestors.add(cursor);cursor=nodes.get(cursor,{}).get('parent')

    def world_container(node):
        seen=set()
        while isinstance(node,dict) and node.get('id') not in seen:
            seen.add(node.get('id'))
            if node.get('class_name') in ('Workspace','WorldModel'):return node.get('id')
            parent=node.get('parent')
            if not isinstance(parent,str):return None
            # Compact snapshots may omit the unchanged common container.
            if parent not in nodes:return parent
            node=nodes[parent]
        return None
    root_world=world_container(nodes.get(root_id))
    root_node=nodes.get(root_id,{})
    motion_parts=received_character_motion_parts(snapshot,root_id,root_node.get('parent'))

    def active_parent(source):
        if root_world is None or world_container(source)!=root_world:return False
        cursor=source.get('parent');seen=set()
        while isinstance(cursor,str) and cursor not in seen:
            if cursor in root_ancestors:return True
            seen.add(cursor);cursor=nodes.get(cursor,{}).get('parent')
        return False

    def source_origin(source):
        pending=[source];seen=set()
        while pending:
            current=pending.pop()
            if not current or current.get('id') in seen:continue
            seen.add(current.get('id'))
            if 'local' in current.get('origins',{}).values():return 'local_force'
            pending.append(nodes.get(current.get('parent')))
            for key in ('Attachment0','Attachment1'):
                pending.append(nodes.get(_ref(current.get('properties',{}).get(key))))
        return 'received_force'
    def attached_to_root(part):
        if not isinstance(part, dict) or part.get('class_name') not in _FORCE_PARTS:
            return False
        if not active_parent(part):return False
        if part.get('id') in motion_parts:return True
        # Existing external welded reference frames may be outside the
        # Character hierarchy. Preserve their explicit received identity,
        # while character members must obey the current joint graph above.
        cursor=part;seen=set()
        while cursor and cursor.get('id') not in seen:
            seen.add(cursor.get('id'))
            if cursor.get('parent')==root_node.get('parent'):return False
            cursor=nodes.get(cursor.get('parent'))
        return (_received(part,'AssemblyRootPart') and
                _ref(part.get('properties',{}).get('AssemblyRootPart'))==root_id)

    for source in nodes.values():
        kind = source.get('class_name')
        props = source.get('properties', {})
        if not active_parent(source):
            continue
        if kind == 'LinearVelocity':
            try:values=_linear_velocity_record(nodes,source,root_id,known,attached_to_root,active_parent,allow_authored)
            except (ValueError,KeyError,TypeError,AttributeError):continue
            out.append({'source':source_origin(source),'id':source['id'],'kind':kind,
                        'part_id':root_id,'relative_to':'World',**values})
        elif kind == 'BodyForce':
            parent_part = nodes.get(source.get('parent'))
            if not attached_to_root(parent_part):
                continue
            field = 'Force' if known(source, 'Force') else 'force'
            if not known(source, field) or not isinstance(props, dict) or field not in props:
                continue
            try:
                force = _snapshot_vector(props[field])
            except ValueError:
                continue
            out.append({'source': source_origin(source), 'id': source['id'],
                        'part_id': root_id, 'kind': kind,
                        'force': list(force), 'relative_to': 'World'})
        elif kind == 'BodyVelocity':
            if not isinstance(props,dict) or not attached_to_root(nodes.get(source.get('parent'))):continue
            velocity_key='Velocity' if known(source,'Velocity') else 'velocity'
            limit_key='MaxForce' if known(source,'MaxForce') else 'maxForce'
            if not all(known(source,key) for key in (velocity_key,limit_key,'P')):continue
            try:values=velocity_servo_values(props.get(velocity_key),props.get(limit_key),props.get('P'))
            except ValueError:continue
            out.append({'source':source_origin(source),'id':source['id'],'kind':kind,
                        'part_id':root_id,'relative_to':'World',**values})
        elif kind == 'BodyPosition':
            if not isinstance(props,dict) or not attached_to_root(nodes.get(source.get('parent'))):continue
            position_key='Position' if known(source,'Position') else 'position'
            limit_key='MaxForce' if known(source,'MaxForce') else 'maxForce'
            if not all(known(source,key) for key in (position_key,limit_key,'P','D')):continue
            try:
                values=position_servo_values(props.get(position_key),props.get(limit_key),props.get('P'),props.get('D'))
                offset=(0.,0.,0.);heading=0.
                if source.get('parent')!=root_id:
                    point=_force_position(nodes[source['parent']],allow_authored)
                    root_point=_force_position(nodes[root_id],allow_authored)
                    offset=tuple(point[i]-root_point[i] for i in range(3))
                    rotation=_force_rotation(nodes[root_id],'CFrame',allow_authored)
                    if math.hypot(rotation[2],rotation[8])<=1e-9:continue
                    heading=math.atan2(rotation[2],rotation[8])
            except ValueError:continue
            out.append({'source':source_origin(source),'id':source['id'],'kind':kind,
                        'part_id':root_id,'relative_to':'World',**values,
                        'position_offset':offset,'root_reference_yaw':heading,
                        'parent_id':source['parent'],
                        'target_revision':source.get('property_revisions',{}).get(position_key),
                        'target_generation':source.get('mover_target_generation',0)})
        elif kind == 'VectorForce':
            if not isinstance(props, dict) or not known(source, 'Force') or not known(source, 'Enabled'):
                continue
            if props.get('Enabled') is not True or not known(source, 'Attachment0'):
                continue
            if not known(source, 'ApplyAtCenterOfMass') or props.get('ApplyAtCenterOfMass') is not True:
                continue
            mode = _enum_value(props.get('RelativeTo'), 'ActuatorRelativeTo')
            if not known(source, 'RelativeTo') or mode not in (0, 1, 2):
                continue
            attachment_id = _ref(props.get('Attachment0'))
            attachment = nodes.get(attachment_id)
            try:
                _, attached_part = _force_attachment_chain(nodes, attachment)
                if not attached_to_root(attached_part):
                    continue
                force = _snapshot_vector(props['Force'])
                extra = {}
                if mode != 2:
                    key = 'Attachment0' if mode == 0 else 'Attachment1'
                    if not known(source, key):
                        continue
                    reference_id = _ref(props.get(key))
                    rotation, reference_part = _force_attachment_rotation(nodes, nodes.get(reference_id),allow_authored)
                    if not active_parent(reference_part):continue
                    force = _vector3(tuple(sum(rotation[3*i+j]*force[j] for j in range(3))
                                           for i in range(3)), 'world force')
                    extra = {'direction_frame':key, 'reference_attachment_id':reference_id}
                    if attached_to_root(reference_part):
                        root_rotation = _force_rotation(nodes.get(root_id), 'CFrame',allow_authored)
                        if math.hypot(root_rotation[2], root_rotation[8]) <= 1e-9:
                            continue
                        extra['root_reference_yaw'] = math.atan2(root_rotation[2],root_rotation[8])
            except ValueError:
                continue
            out.append({'source': source_origin(source), 'id': source['id'],
                        'part_id': root_id, 'kind': kind,
                        'force': list(force), 'relative_to': 'World', **extra})
    return out
