"""Motion helpers for received moving-platform poses.

Pose rotations are row-major local-to-world matrices, matching
``movement_collision.rotation_matrix``. These helpers never manufacture a
pose or collider: callers must provide both received poses for the same support.
"""
from __future__ import annotations

from dataclasses import dataclass
import math

from movement_collision import rotation_matrix
from physics_ground import (_is_part_descendant, _property_type_map,
    _received_part_frame, _received_rotation)

Vector3 = tuple[float, float, float]
Matrix3 = tuple[float, ...]


def _vec(value, label: str) -> Vector3:
    if not isinstance(value, (tuple, list)) or len(value) != 3:
        raise ValueError(f'{label} must have three finite components')
    out = tuple(float(v) for v in value)
    if any(not math.isfinite(v) for v in out):
        raise ValueError(f'{label} must have three finite components')
    return out


@dataclass(frozen=True)
class PlatformPose:
    position: Vector3
    rotation: Matrix3 = (1., 0., 0., 0., 1., 0., 0., 0., 1.)

    def __post_init__(self):
        object.__setattr__(self, 'position', _vec(self.position, 'position'))
        object.__setattr__(self, 'rotation', rotation_matrix(self.rotation))


def platform_poses_from_world(world) -> dict:
    """Return source-keyed poses backed by received Part transforms in ``world``.

    Both anchored and explicitly unanchored Part descendants are eligible.
    The helper only reads CFrame/ReplicationPV properties with supported schema
    tags and a decodable rigid rotation (full matrix, native axis-pair selector
    or native packed quaternion); opaque rotations and missing/unknown
    Anchored state are omitted. It never estimates a frame from velocity,
    geometry bounds, local history, or constructor defaults. Keys are the
    original ``world.instances`` keys used by received colliders' ``source``.
    """
    schema = getattr(world, 'schema', None)
    instances = getattr(world, 'instances', None)
    if not isinstance(schema, dict) or not isinstance(instances, dict):
        raise ValueError('world must expose schema and instances dictionaries')
    classes = schema.get('classes')
    if not isinstance(classes, list):
        raise ValueError('world schema has no classes list')
    poses = {}
    for key, instance in instances.items():
        if not isinstance(instance, dict):
            continue
        try:
            class_index = instance['class']
            class_name = classes[class_index]['name']
        except (KeyError, IndexError, TypeError):
            continue
        if not isinstance(class_index, int) or not _is_part_descendant(class_name):
            continue
        props = instance.get('properties')
        if not isinstance(props, dict) or type(props.get('Anchored')) is not bool:
            continue
        try:
            types = _property_type_map(schema, class_index)
            frame, record_type = _received_part_frame(world, key, props, types)
            rotation = _received_rotation(frame, record_type) if frame is not None else None
            if frame is None or rotation is None:
                continue
            poses[key] = PlatformPose(frame.get('position'), rotation)
        except (KeyError, TypeError, ValueError):
            continue
    return poses


def _mv(m: Matrix3, v: Vector3) -> Vector3:
    return tuple(sum(m[3*i+j]*v[j] for j in range(3)) for i in range(3))


def _transpose(m: Matrix3) -> Matrix3:
    return tuple(m[3*j+i] for i in range(3) for j in range(3))


def _add(a: Vector3, b: Vector3) -> Vector3:
    return tuple(a[i]+b[i] for i in range(3))


def _sub(a: Vector3, b: Vector3) -> Vector3:
    return tuple(a[i]-b[i] for i in range(3))


def _cross(a: Vector3, b: Vector3) -> Vector3:
    return (a[1]*b[2]-a[2]*b[1], a[2]*b[0]-a[0]*b[2], a[0]*b[1]-a[1]*b[0])


def carry_grounded_point(point: Vector3, previous_pose: PlatformPose,
                         current_pose: PlatformPose) -> Vector3:
    """Carry a grounded world point by its support's received pose delta."""
    point = _vec(point, 'point')
    local = _mv(_transpose(previous_pose.rotation), _sub(point, previous_pose.position))
    return _add(current_pose.position, _mv(current_pose.rotation, local))


def _angular_velocity(previous: PlatformPose, current: PlatformPose, dt: float) -> Vector3:
    # Relative local-to-world rotation: R_delta = R_current * transpose(R_previous).
    a, b = current.rotation, _transpose(previous.rotation)
    d = tuple(sum(a[3*i+k]*b[3*k+j] for k in range(3)) for i in range(3) for j in range(3))
    cos_angle = max(-1., min(1., (d[0]+d[4]+d[8]-1.)*.5))
    angle = math.acos(cos_angle)
    if angle < 1e-8:
        return ((d[7]-d[5])*.5/dt, (d[2]-d[6])*.5/dt, (d[3]-d[1])*.5/dt)
    if math.pi-angle < 1e-5:
        # Stable axis extraction for the uncommon half-turn case.
        axis = (math.sqrt(max(0., (d[0]+1.)*.5)), math.sqrt(max(0., (d[4]+1.)*.5)), math.sqrt(max(0., (d[8]+1.)*.5)))
        if d[1] < 0: axis = (axis[0], -axis[1], axis[2])
        if d[2] < 0: axis = (axis[0], axis[1], -axis[2])
    else:
        scale = 1. / (2. * math.sin(angle))
        axis = ((d[7]-d[5])*scale, (d[2]-d[6])*scale, (d[3]-d[1])*scale)
    return tuple(v*angle/dt for v in axis)


def platform_point_velocity(point: Vector3, previous_pose: PlatformPose,
                            current_pose: PlatformPose, dt: float) -> Vector3:
    """Estimate support velocity at ``point``, including angular cross radius."""
    point = _vec(point, 'point')
    if not isinstance(dt, (int, float)) or not math.isfinite(dt) or dt <= 0:
        raise ValueError('dt must be a positive finite number')
    linear = tuple((current_pose.position[i]-previous_pose.position[i])/dt for i in range(3))
    omega = _angular_velocity(previous_pose, current_pose, dt)
    radius = _sub(point, current_pose.position)
    return _add(linear, _cross(omega, radius))


def inherit_platform_velocity(character_velocity: Vector3, point: Vector3,
                              previous_pose: PlatformPose, current_pose: PlatformPose,
                              dt: float) -> Vector3:
    """Add the support's point velocity to character velocity on departure."""
    return _add(_vec(character_velocity, 'character_velocity'),
                platform_point_velocity(point, previous_pose, current_pose, dt))
