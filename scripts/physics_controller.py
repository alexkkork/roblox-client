"""Bounded, local-only own-character walking prediction.

This consumes movement intentions and produces a predicted pose. It never
changes decoded replication/world state and does not construct or send packets.
The horizontal integrator is shared with ``movement_controller.Controller``.
"""
import math
import json

from movement_controller import Controller, MAX_STEP_SECONDS
from movement_collision import rotation_matrix
from movement_camera import camera_movement_error, movement_direction
from physics_foot_offset import DEFAULT_FOOT_OFFSET, derive_foot_offset
from physics_ground import (GroundGeometry, GroundStep, step_vertical, follow_ground,
                            ground_raycast, footprint_ground_raycast)
from physics_body_collision import (move_horizontal_body, clip_horizontal,
                                    step_vertical_body, step_vertical_accelerated, vertical_body_clearance)
from physics_stairs import walkable_normal, slope_slide_acceleration
from physics_character_states import (classify_state, transition_events, STATE_VALUES,
                                      state_transition_records, state_signal_events)
from physics_platforms import (carry_grounded_point, platform_point_velocity,
                               inherit_platform_velocity, PlatformPose)
from physics_forces import predicted_assembly_mass, MASS_SOURCES, velocity_servo_values, position_servo_values, received_character_motion_parts, received_motion_joint_parts
from physics_velocity_servo import solve_movers
from physics_target_sweep import curve_reaches_target
from physics_linear_velocity import validated_constraint, constraint_acceleration
from collision_groups import CollisionContext, part_group
from no_collision_constraints import topology_records


def _cframe_position(value):
    if not isinstance(value, dict):
        return None
    cframe = value.get('$cframe')
    if isinstance(cframe, dict):
        position = cframe.get('position')
    else:
        position = value.get('$vector3')
    if isinstance(position, (list, tuple)) and len(position) == 3:
        return position
    return None


def _initial_heading(root):
    """Use a known rigid CFrame, otherwise label the model's initial heading."""
    frame = root.get('properties', {}).get('CFrame', {}) if root else {}
    frame = frame.get('$cframe', {}) if isinstance(frame, dict) else {}
    raw = frame.get('rotation') if isinstance(frame, dict) else None
    if isinstance(raw, (list, tuple)) and len(raw) == 3 and all(isinstance(row, (list, tuple)) and len(row) == 3 for row in raw):
        raw = [value for row in raw for value in row]
    try:
        rotation = rotation_matrix(raw)
    except ValueError:
        return 0., 'local_default_heading'
    if math.hypot(rotation[2], rotation[8]) <= 1e-9:
        return 0., 'local_default_heading'
    return math.atan2(rotation[2], rotation[8]), 'received_cframe'


def _resolve(snapshot):
    """Resolve own Player, its Character, Humanoid and root from a snapshot."""
    if not isinstance(snapshot, dict) or not isinstance(snapshot.get('nodes'), list):
        raise ValueError('snapshot must contain a nodes list')
    nodes = {n.get('id'): n for n in snapshot['nodes'] if isinstance(n, dict)}
    player_id = snapshot.get('local_player')
    player = nodes.get(player_id)
    if not player:
        return player_id, None, None, None, None, None, 'own_player_unresolved'
    char_ref = player.get('properties', {}).get('Character', {})
    character_id = char_ref.get('$ref') if isinstance(char_ref, dict) else None
    character = nodes.get(character_id)
    if not character or character.get('class_name') != 'Model':
        return player_id, character_id, None, None, None, None, 'character_unresolved'
    descendants = [n for n in nodes.values() if n.get('parent') == character_id]
    humanoid = next((n for n in descendants if n.get('class_name') == 'Humanoid'), None)
    root = next((n for n in descendants if n.get('name') == 'HumanoidRootPart'), None)
    if not humanoid or not root:
        return player_id, character_id, humanoid, None, None, None, 'character_parts_unresolved'
    props = root.get('properties', {})
    origins = root.get('origins')
    anchored = props.get('Anchored')
    if isinstance(origins, dict):
        anchored_origin = origins.get('Anchored')
        received_value = anchored_origin == 'received'
        proven_default = anchored_origin == 'class_default' and anchored is False
        authored_value = snapshot.get('allow_authored_motion') is True and anchored_origin == 'local'
        if not (received_value or proven_default or authored_value):
            return player_id, character_id, humanoid, root, None, None, 'anchored_state_unconfirmed'
    if type(anchored) is not bool:
        return player_id, character_id, humanoid, root, None, anchored, 'anchored_state_unknown'
    # A welded anchored member immobilizes the entire body, even if the
    # HumanoidRootPart itself remains explicitly unanchored.
    assembly=received_character_motion_parts(snapshot,root['id'],character_id,include_anchored=True)
    if any(nodes[key].get('properties',{}).get('Anchored') is True for key in assembly):
        anchored=True
    if anchored:
        return player_id, character_id, humanoid, root, None, anchored, 'anchored'
    pose = props.get('Position')
    position = pose.get('$vector3') if isinstance(pose, dict) else None
    if position is None:
        position = _cframe_position(props.get('CFrame'))
    if position is None:
        return player_id, character_id, humanoid, root, None, anchored, 'received_pose_unavailable'
    speed = humanoid.get('properties', {}).get('WalkSpeed', 16.0)
    try:
        mover = Controller(position, speed)
    except (TypeError, ValueError):
        return player_id, character_id, humanoid, root, None, anchored, 'invalid_initial_pose_or_speed'
    return player_id, character_id, humanoid, root, mover, anchored, None



# Option for unknown assembly mass: BodyVelocity servos whose force limits are
# effectively unbounded (far above any character's weight) behave as a stiff
# velocity constraint, independent of mass. Only those qualify.
_UNBOUNDED_SERVO_FORCE = 1e5
_STIFF_SERVO_MASS = 1e-6


def _stiff_velocity_servos_only(velocity_sources, position_sources, linear_sources):
    if not velocity_sources or position_sources or linear_sources:
        return False
    return all(source.get('gain', 0) > 0 and any(limit > 0 for limit in source['max_force'])
               and all(limit == 0 or limit >= _UNBOUNDED_SERVO_FORCE for limit in source['max_force'])
               for source in velocity_sources)


class OwnCharacterPhysicsController:
    """Simulate authored movement for the snapshot's own movable character body.

    Native intention schema: ``sequence``, ``kind`` (``Move``, ``MoveTo`` or
    ``Stop`` or ``Jump``), ``instance_id``, ``vector`` for the first two, and optional
    ``relative_to_camera``. Coordinates and speed use Roblox world studs.
    """

    def __init__(self, snapshot, local_player_id=None, *, geometry=None,
                 gravity=196.2, foot_offset=None, max_fall_speed=512.0,
                 max_step_height=1.0):
        self.controller = None
        self.target = None
        self._move_to_elapsed = 0.0
        self._move_to_request = None
        self._move_to_feedback = []
        self.character_id = None
        self.root_id = None
        self.player_id = local_player_id if local_player_id is not None else snapshot.get('local_player')
        self.humanoid_id = None
        self.anchored = None
        self.jump_settings = {}
        self.max_slope_angle = 89.0
        self.max_step_height = max_step_height
        self.auto_rotate = True
        self._health_reason = None
        self.yaw_origin = 'local_default_heading'
        self._jumping = False
        self._environment = {'water_regions': (), 'ladder_regions': (), 'seat': None}
        self._state_enabled = {name: True for name in STATE_VALUES}
        self._vertical_move = 0.0
        self._previous_humanoid_state = None
        self._feedback_sequence = 0
        self._feedback_acknowledged = 0
        self._feedback_transitions = []
        self._feedback_signals = []
        self._platform_poses_previous = {}
        self._platform_poses_current = {}
        self._platform_pose_dt = 0.0
        self._platform_carry_pending = False
        self._ground_source = None
        self._seat_delta = (0., 0., 0.)
        self._seat_dt = 0.0
        self._seat_carry_pending = False
        self._seat_pose_at = None
        self._surface_velocities = {}
        self.last_sequence = 0
        self.accepted = 0
        self.rejected = 0
        self.last_reason = None
        self.simulated_seconds = 0.0
        self.yaw = 0.0
        self.linear_velocity = (0.0, 0.0, 0.0)
        self._impulse_velocity = (0.0, 0.0, 0.0)
        self.angular_velocity = (0.0, 0.0, 0.0)
        self._slope_velocity = (0.0, 0.0, 0.0)
        self.body_half_size = None
        self.assembly_mass = None
        self.assembly_mass_source = None
        self._force_sources = ()
        self._velocity_sources = ()
        self.stiff_servo_fallback_steps = 0  # servo solves that used the unknown-mass stiff limit
        self._position_sources = (); self._mover_feedback = []; self._reached_targets = {}; self._linear_velocity_sources = ()
        self._body_collision = {'available': False, 'initial_overlap': False, 'contacts': 0, 'limited': False}
        if geometry is not None and not isinstance(geometry, GroundGeometry):
            raise ValueError('geometry must be GroundGeometry or None')
        if (isinstance(max_step_height, bool) or not isinstance(max_step_height, (int, float))
                or not math.isfinite(max_step_height) or max_step_height < 0):
            raise ValueError('max_step_height must be finite and non-negative')
        self.geometry = geometry if geometry is not None else GroundGeometry((), ())
        self._unfiltered_geometry = self.geometry
        self._collision_context = None
        self._root_collision_group = 'Default'
        self._root_collision_error = None
        self._collision_error = None
        self._collision_filter_key = None
        self._collision_overlay = ()
        self._collision_removed = ()
        self._collision_overlay_signature = ''
        self.gravity = gravity
        self._dynamic_foot_offset = foot_offset is None
        self._fallback_foot_offset = DEFAULT_FOOT_OFFSET if foot_offset is None else foot_offset
        if self._dynamic_foot_offset:
            self.foot_offset, self.foot_offset_origin = derive_foot_offset(snapshot)
        else:
            self.foot_offset = foot_offset
            self.foot_offset_origin = 'explicit_configuration'
        self.max_fall_speed = max_fall_speed
        # Validate the optional physical configuration through the helper.
        step_vertical((0.0, 0.0, 0.0), 0.0, 0.0, self.geometry, False,
                      foot_offset=self.foot_offset, gravity=gravity,
                      max_fall_speed=max_fall_speed)
        self.vertical_velocity = 0.0
        self.grounded = False
        self._last_timestamp = None
        self._last_report = None
        self._motion_topology_history = set()
        self._last_authored_pose_generation = None
        self._last_authored_rotation_generation = None
        self._platform_stand_request = None
        self._platform_stand_input = None
        self._activate(snapshot, initial=True)

    def _activate(self, snapshot, initial=False):
        self._last_snapshot = snapshot if isinstance(snapshot, dict) else None
        self._update_foot_offset(snapshot)
        (player_id, character_id, humanoid, root, controller, anchored,
         reason) = _resolve({**snapshot,'motion_topology_history':self._motion_topology_history})
        if self.player_id is None:
            self.player_id = player_id
        if player_id != self.player_id:
            self._deactivate('different_player')
            return
        replaced = character_id != self.character_id
        root_replaced = root is not None and root.get('id') != self.root_id
        if replaced or root_replaced or initial:
            self._last_authored_pose_generation=None;self._last_authored_rotation_generation=None
        reactivated = self.controller is None and controller is not None
        self.character_id = character_id
        self.humanoid_id = humanoid.get('id') if humanoid else None
        self.root_id = root.get('id') if root else None
        self._collision_overlay=topology_records(snapshot.get('nodes',()))
        self._collision_removed=tuple(sorted(snapshot.get('removed_physics_instances',())))
        self._collision_overlay_signature=json.dumps((self._collision_overlay,self._collision_removed),sort_keys=True)
        try:
            self._root_collision_group = part_group(root.get('properties', {})) if root else 'Default'
            self._root_collision_error = None
        except ValueError as exc:
            self._root_collision_error = str(exc)
        self._apply_collision_filter()
        if replaced or root_replaced or initial:self._motion_topology_history.clear()
        self._motion_topology_history.update(received_motion_joint_parts(snapshot))
        self._motion_parts = received_character_motion_parts(
            {**snapshot,'motion_topology_history':self._motion_topology_history},self.root_id,character_id)
        self.anchored = anchored
        mass, mass_source = predicted_assembly_mass(snapshot, self.root_id)
        self.assembly_mass = mass
        self.assembly_mass_source = mass_source
        self.jump_settings = dict(humanoid.get('properties', {})) if humanoid else {}
        stand_input=(self.humanoid_id,self.jump_settings.get('PlatformStand',False),
            humanoid.get('property_revisions',{}).get('PlatformStand') if humanoid else None,
            humanoid.get('authored_control_generations',{}).get('PlatformStand') if humanoid else None)
        if initial or replaced or stand_input!=self._platform_stand_input:
            self._platform_stand_request=None
        self._platform_stand_input=stand_input
        slope = self.jump_settings.get('MaxSlopeAngle', 89.0)
        # The local body proxy accepts a bounded walkable-slope range. Keep
        # missing or malformed received data from making the character fall
        # through otherwise supported geometry.
        if type(slope) in (int, float) and math.isfinite(slope):
            self.max_slope_angle = min(89.0, max(0.0, float(slope)))
        else:
            self.max_slope_angle = 89.0
        self.auto_rotate = self.jump_settings.get('AutoRotate', True) is True
        health = self.jump_settings.get('Health', 100.)
        self._health_reason = ('invalid_humanoid_health' if type(health) not in (int, float) or not math.isfinite(health)
                               else 'humanoid_not_alive' if health <= 0 else None)
        self.body_half_size = None
        size = root.get('properties', {}).get('Size') if root else None
        origins = root.get('origins') if root else None
        known_size = not isinstance(origins, dict) or origins.get('Size') in ('received', 'derived_received', 'local')
        vector = size.get('$vector3') if isinstance(size, dict) else None
        if (known_size and isinstance(vector, (tuple, list)) and len(vector) == 3
                and all(type(v) in (int, float) and math.isfinite(v) and v > 0 for v in vector)):
            self.body_half_size = tuple(v/2 for v in vector)
        # Keep local prediction separate from refreshed received poses. Reset
        # only when the controlled character/root itself is replaced.
        if replaced or root_replaced or reactivated or initial:
            if replaced or root_replaced or reactivated:
                self._feedback_sequence = 0
                self._feedback_acknowledged = 0
                self._feedback_transitions = []
                self._feedback_signals = []
                self._move_to_feedback = []
            self.controller = controller
            self.target = None
            self.linear_velocity = (0.0, 0.0, 0.0)
            self._impulse_velocity = (0.0, 0.0, 0.0)
            self._force_sources = ()
            self._velocity_sources = ()
            self._position_sources = (); self._mover_feedback = []; self._reached_targets = {}; self._linear_velocity_sources = ()
            self.angular_velocity = (0.0, 0.0, 0.0)
            self._slope_velocity = (0.0, 0.0, 0.0)
            self.vertical_velocity = 0.0
            self.grounded = False
            self._ground_source = None
            self._jumping = False
            self._vertical_move = 0.0
            self._previous_humanoid_state = None
            self.yaw, self.yaw_origin = _initial_heading(root)
            self.vertical_position = controller.position[1] if controller is not None else 0.0
            if controller is not None and anchored is False:
                initial_ground = step_vertical(
                    (controller.position[0], self.vertical_position, controller.position[2]),
                    0.0, 0.0, self.geometry, False, foot_offset=self.foot_offset,
                    gravity=self.gravity, max_fall_speed=self.max_fall_speed,
                    support_half_size=self.body_half_size,
                    max_slope_angle=self.max_slope_angle)
                self.vertical_position = initial_ground.position[1]
                self.grounded = initial_ground.grounded
                self._ground_source = (getattr(initial_ground.hit, 'source', None)
                                       if initial_ground.grounded else None)
        elif reason == 'anchored':
            # Suspension is not despawn. Keep predicted pose and momentum so
            # unanchoring the same body cannot reset it to an old receipt.
            # step() emits no active prediction while this body is anchored.
            pass
        elif reason in ('anchored_state_unknown', 'anchored_state_unconfirmed'):
            self._deactivate(reason)
        elif self.controller is not None and anchored is False:
            # A Humanoid can change WalkSpeed while retaining the same
            # Character/root IDs. Refresh that property on every world update
            # instead of keeping the value captured at spawn forever.
            if controller is not None:
                self.controller.walk_speed = controller.walk_speed
            elif reason == 'invalid_initial_pose_or_speed':
                # Invalid live movement properties must not leave a stale
                # speed in effect. Keep gravity/position prediction, but stop
                # horizontal movement until a valid speed arrives again.
                self.controller.walk_speed = 0.0
                self.controller.stop()
                self.target = None
        elif reason is not None:
            # A despawn, incomplete hierarchy, or missing received pose must
            # not leave the previous character simulating off stale state.
            self._deactivate(reason)
        if self.controller is not None and self._health_reason:
            self.controller.stop()
            self.target = None
            self._jumping = False
        if self._platform_standing():self._stop_humanoid_locomotion()
        self.last_reason = reason or self._health_reason
        if snapshot.get('allow_authored_motion') is True and root is not None:
            self._apply_authored_pose(root)
        self._last_report = None

    def _apply_authored_pose(self,root):
        generation=root.get('authored_pose_generation')
        token=(root['id'],generation)
        if (type(generation) is not int or generation<=0 or token==self._last_authored_pose_generation
                or self.controller is None):return
        props=root.get('properties',{});origins=root.get('origins',{})
        rotation_generation=root.get('authored_rotation_generation')
        try:
            yaw=None
            if origins.get('CFrame')=='local':
                frame=props.get('CFrame')
                frame=frame.get('$cframe') if isinstance(frame,dict) else None
                if not isinstance(frame,dict):raise ValueError('invalid authored CFrame')
                position=frame.get('position')
                if 'rotation' in frame:
                    raw=frame['rotation']
                    if isinstance(raw,(tuple,list)) and len(raw)==3 and all(isinstance(row,(tuple,list)) and len(row)==3 for row in raw):
                        raw=[v for row in raw for v in row]
                    rotation=rotation_matrix(raw)
                    if (rotation_generation is not None and rotation_generation!=self._last_authored_rotation_generation
                            and math.hypot(rotation[2],rotation[8])>1e-9):yaw=math.atan2(rotation[2],rotation[8])
            elif origins.get('Position')=='local':
                value=props.get('Position');position=value.get('$vector3') if isinstance(value,dict) else None
            else:return
            # Validate every component before changing any controller state.
            validated=Controller(position,0).position
        except (ValueError,TypeError):
            self._deactivate('invalid_authored_pose');return
        self.controller.resolve_position(validated)
        # Authored root teleports end the current walk request. A received pose
        # refresh alone does not take ownership away from prediction.
        if self.target is not None:
            self.target = None
            self.controller.stop()
            self.linear_velocity = (self._impulse_velocity[0], self.vertical_velocity,
                                    self._impulse_velocity[2])
        self.vertical_position=validated[1]
        if yaw is not None:self.yaw=yaw;self.yaw_origin='authored_cframe';self.angular_velocity=(0.,0.,0.)
        self.grounded=False;self._ground_source=None
        self._platform_carry_pending=False;self._seat_carry_pending=False
        self._slope_velocity=(0.,0.,0.)
        self._body_collision={'available':self.body_half_size is not None,'initial_overlap':False,'contacts':0,'limited':False}
        self._last_authored_pose_generation=token
        self._last_authored_rotation_generation=rotation_generation

    def _update_foot_offset(self, snapshot):
        if not self._dynamic_foot_offset:
            return
        offset, origin = derive_foot_offset(snapshot, self._fallback_foot_offset)
        changed = offset != self.foot_offset or origin != self.foot_offset_origin
        self.foot_offset = offset
        self.foot_offset_origin = origin
        if changed and self._last_report is not None:
            self._last_report = {**self._last_report, 'foot_offset': offset,
                                 'foot_offset_origin': origin}

    def _platform_standing(self):
        requested=(self._platform_stand_request if self._platform_stand_request is not None
                   else self.jump_settings.get('PlatformStand',False) is True)
        return requested and self._state_enabled.get('PlatformStanding',True)

    def _stop_humanoid_locomotion(self):
        if self.controller is not None:self.controller.stop()
        self.target=None;self._vertical_move=0.;self._jumping=False

    def _deactivate(self, reason):
        self._motion_parts = frozenset()
        self.controller = None
        self.target = None
        self.linear_velocity = (0.0, 0.0, 0.0)
        self._impulse_velocity = (0.0, 0.0, 0.0)
        self._force_sources = ()
        self._velocity_sources = ()
        self._position_sources = (); self._mover_feedback = []; self._reached_targets = {}; self._linear_velocity_sources = ()
        self.angular_velocity = (0.0, 0.0, 0.0)
        self._slope_velocity = (0.0, 0.0, 0.0)
        self.vertical_velocity = 0.0
        self.grounded = False
        self._ground_source = None
        self._jumping = False
        self._vertical_move = 0.0
        self.last_reason = reason

    def update_snapshot(self, snapshot):
        """Refresh properties while preserving the same body's predicted pose."""
        self._activate(snapshot)

    def update_geometry(self, geometry):
        """Replace supported collision geometry without inferring missing ground."""
        if not isinstance(geometry, GroundGeometry):
            raise ValueError('geometry must be GroundGeometry')
        self._unfiltered_geometry = geometry
        self._apply_collision_filter()

    def update_collision_context(self, context):
        if not isinstance(context, CollisionContext):
            raise ValueError('collision context must be CollisionContext')
        self._collision_context = context
        self._apply_collision_filter()

    def _apply_collision_filter(self):
        key=(id(self._unfiltered_geometry),id(self._collision_context),
             self._root_collision_group,self._root_collision_error,self._collision_overlay_signature)
        if key==self._collision_filter_key:return
        self._collision_filter_key=key
        self._last_report=None
        self._collision_error=self._root_collision_error
        self.geometry=self._unfiltered_geometry
        if self._collision_context is not None and self._collision_error is None:
            try:
                allowed=self._collision_context.allowed_sources(self._root_collision_group,
                                                                self._collision_overlay,self._collision_removed)
                self.geometry=GroundGeometry(tuple(c for c in self._unfiltered_geometry.colliders
                                                  if c.source in allowed),
                                             self._unfiltered_geometry.unsupported)
                if self._ground_source is not None and self._ground_source not in allowed:
                    # Do not carry/jump from support which has just stopped colliding.
                    self.grounded=False;self._ground_source=None
                    self._platform_carry_pending=False
            except ValueError as exc:
                self._collision_error=str(exc)

    def update_environment(self, *, water_regions=(), ladder_regions=(), seat=None,
                           timestamp=None):
        """Install explicit received region evidence for local state modeling.

        Region records use `{min:[x,y,z], max:[x,y,z], source:'received_geometry'}`.
        Missing or malformed regions are ignored; this never guesses terrain water
        or ladders from visual appearance.
        """
        def regions(values):
            result = []
            for item in values or ():
                if not isinstance(item, dict) or item.get('source') != 'received_geometry':
                    continue
                lower, upper = item.get('min'), item.get('max')
                if (not isinstance(lower, (list, tuple)) or not isinstance(upper, (list, tuple))
                        or len(lower) != 3 or len(upper) != 3):
                    continue
                try:
                    lo, hi = tuple(float(v) for v in lower), tuple(float(v) for v in upper)
                except (TypeError, ValueError):
                    continue
                if (not all(math.isfinite(v) for v in lo+hi)
                        or any(lo[i] >= hi[i] for i in range(3))):
                    continue
                result.append({'min': lo, 'max': hi, 'source': 'received_geometry',
                               'id': item.get('id')})
            return tuple(result)
        if (timestamp is not None and (isinstance(timestamp, bool)
                or not isinstance(timestamp, (int, float)) or not math.isfinite(timestamp))):
            raise ValueError('environment timestamp must be finite')
        accepted_seat = (dict(seat) if isinstance(seat, dict)
                         and seat.get('source') in ('received_seat_link',
                                                    'received_sit_property') else None)
        if accepted_seat and 'position' in accepted_seat:
            position = accepted_seat.get('position')
            if (not isinstance(position, (list, tuple)) or len(position) != 3
                    or any(type(v) not in (int, float) or not math.isfinite(v) for v in position)):
                accepted_seat['position'] = None
        old_seat = self._environment.get('seat')
        self._seat_delta = (0., 0., 0.)
        self._seat_dt = 0.0
        self._seat_carry_pending = False
        if (old_seat and accepted_seat and old_seat.get('source') == 'received_seat_link'
                and accepted_seat.get('source') == 'received_seat_link'
                and old_seat.get('id') == accepted_seat.get('id')
                and isinstance(old_seat.get('position'), (list, tuple))
                and isinstance(accepted_seat.get('position'), (list, tuple))
                and len(old_seat['position']) == len(accepted_seat['position']) == 3):
            old_position = tuple(float(v) for v in old_seat['position'])
            new_position = tuple(float(v) for v in accepted_seat['position'])
            if (all(math.isfinite(v) for v in old_position+new_position)
                    and timestamp is not None and self._seat_pose_at is not None
                and timestamp > self._seat_pose_at):
                self._seat_delta = tuple(new_position[i]-old_position[i] for i in range(3))
                self._seat_dt = float(timestamp-self._seat_pose_at)
                self._seat_carry_pending = any(abs(v) > 1e-9 for v in self._seat_delta)
        self._seat_pose_at = timestamp if accepted_seat and accepted_seat.get('position') is not None else None
        self._environment = {'water_regions': regions(water_regions),
                             'ladder_regions': regions(ladder_regions), 'seat': accepted_seat}

    def update_platform_poses(self, previous, current, dt):
        """Refresh source keyed poses from received platform transforms only."""
        if not isinstance(previous, dict) or not isinstance(current, dict):
            raise ValueError('platform poses must be mappings')
        if (isinstance(dt, bool) or not isinstance(dt, (int, float))
                or not math.isfinite(dt) or dt < 0):
            raise ValueError('platform pose interval must be finite and non-negative')
        self._platform_poses_previous = {key: pose for key, pose in previous.items()
                                         if isinstance(pose, PlatformPose)}
        self._platform_poses_current = {key: pose for key, pose in current.items()
                                        if isinstance(pose, PlatformPose)}
        self._platform_pose_dt = float(dt)
        self._platform_carry_pending = self._platform_pose_dt > 0 and any(
            self._platform_poses_previous.get(key) != pose
            for key, pose in self._platform_poses_current.items())

    def update_surface_velocities(self, velocities):
        """Install per-collider velocities backed by received part properties."""
        if not isinstance(velocities, dict):
            raise ValueError('surface velocities must be a mapping')
        accepted = {}
        for key, value in velocities.items():
            if not isinstance(value, (list, tuple)) or len(value) != 3:
                continue
            if any(type(v) not in (int, float) or not math.isfinite(v) for v in value):
                continue
            accepted[key] = tuple(float(v) for v in value)
        self._surface_velocities = accepted

    @property
    def state_enabled(self):
        return dict(self._state_enabled)

    def restore_state_enabled(self, values):
        """Restore a same-Humanoid local state-enable map after root rebind."""
        if not isinstance(values, dict):
            raise ValueError('state enablement must be a mapping')
        for name, enabled in values.items():
            if name in STATE_VALUES and type(enabled) is bool:
                self._state_enabled[name] = enabled

    def update_state_enabled(self, state_name, enabled):
        if state_name not in STATE_VALUES or type(enabled) is not bool:
            raise ValueError('invalid Humanoid state enablement')
        self._state_enabled[state_name] = enabled

    def feedback_events(self):
        """Return a consistent feedback batch without consuming it.

        The caller acknowledges the batch only after its feedback sink accepts
        it. Records and stable state come from the same latest completed step;
        the watermark makes retries safe even when reporting and advancing run
        on separate callbacks.
        """
        report = self._last_report if isinstance(self._last_report, dict) else {}
        predicted = report.get('predicted_state')
        humanoid_state = (predicted.get('humanoid_state')
                          if isinstance(predicted, dict) else None)
        state = ({'name': humanoid_state.get('name'), 'value': humanoid_state.get('value')}
                 if isinstance(humanoid_state, dict)
                 and isinstance(humanoid_state.get('name'), str)
                 and type(humanoid_state.get('value')) is int else None)
        velocity = predicted.get('linear_velocity') if isinstance(predicted, dict) else None
        # Keep every event queued, but publish a prefix the native receiver can
        # accept. Acknowledging that prefix must not discard later completions.
        through = self._feedback_sequence
        for records, limit in ((self._feedback_transitions,16),
                               (self._feedback_signals,8), (self._move_to_feedback,256)):
            if len(records)>limit:
                through = min(through, records[limit-1]['_sequence'])
        return {
            'through_sequence': through,
            'movers': [dict(item) for item in self._mover_feedback],
            'move_to': [{key: value for key, value in item.items()
                         if key != '_sequence'} for item in self._move_to_feedback
                        if item['_sequence']<=through],
            'state': state,
            'linear_velocity': (list(velocity) if isinstance(velocity, (list, tuple))
                                and len(velocity) == 3 else None),
            'transitions': [{key: value for key, value in item.items()
                             if key != '_sequence'} for item in self._feedback_transitions
                            if item['_sequence']<=through],
            'signals': [{key: value for key, value in item.items()
                        if key != '_sequence'} for item in self._feedback_signals
                        if item['_sequence']<=through],
        }

    def ack_feedback(self, through_sequence):
        """Discard queued feedback through an acknowledged event watermark."""
        if (type(through_sequence) is not int or through_sequence < self._feedback_acknowledged
                or through_sequence > self._feedback_sequence):
            raise ValueError('feedback acknowledgement is outside the queued sequence')
        if through_sequence == self._feedback_acknowledged:
            return
        self._feedback_transitions = [item for item in self._feedback_transitions
                                      if item['_sequence'] > through_sequence]
        self._feedback_signals = [item for item in self._feedback_signals
                                  if item['_sequence'] > through_sequence]
        self._move_to_feedback = [item for item in self._move_to_feedback
                                  if item['_sequence'] > through_sequence]
        self._feedback_acknowledged = through_sequence

    def update_force_sources(self, sources, *, mass=None, mass_source=None):
        """Install resolved forces, retaining attachment heading for local turns."""
        if mass is not None:
            if (type(mass) not in (int, float) or not math.isfinite(mass) or mass <= 0
                    or mass_source not in MASS_SOURCES):
                raise ValueError('force mass needs explicit received provenance')
            self.assembly_mass = float(mass)
            self.assembly_mass_source = mass_source
        elif mass_source is not None:
            raise ValueError('mass_source requires a mass')
        accepted = [];servos=[];springs=[];constraints=[]
        for source in sources or ():
            if (not isinstance(source, dict) or source.get('source') not in ('received_force','local_force')
                    or source.get('part_id') != self.root_id
                    or source.get('kind') not in ('BodyForce', 'VectorForce','BodyVelocity','BodyPosition','LinearVelocity')
                    or source.get('relative_to') != 'World'):
                continue
            if source.get('kind')=='LinearVelocity':
                try:values=validated_constraint(source)
                except (ValueError,KeyError,TypeError):continue
                constraints.append({'id':source.get('id'),**values});continue
            if source.get('kind')=='BodyVelocity':
                try:values=velocity_servo_values(source.get('target_velocity'),source.get('max_force'),source.get('gain'))
                except ValueError:continue
                servos.append({'id':source.get('id'),**values});continue
            if source.get('kind')=='BodyPosition':
                try:values=position_servo_values(source.get('target_position'),source.get('max_force'),source.get('gain'),source.get('damping'))
                except ValueError:continue
                if not isinstance(source.get('id'),str):continue
                offset=source.get('position_offset',(0.,0.,0.))
                heading=source.get('root_reference_yaw',0.)
                if (not isinstance(offset,(list,tuple)) or len(offset)!=3
                        or any(type(v) not in (int,float) or not math.isfinite(v) for v in offset)
                        or type(heading) not in (int,float) or not math.isfinite(heading)):continue
                springs.append({'id':source.get('id'),**values,
                                'parent_id':source.get('parent_id',self.root_id),
                                'position_offset':tuple(offset),'root_reference_yaw':heading,
                                'target_revision':source.get('target_revision'),
                                'target_generation':source.get('target_generation',0)})
                continue
            raw_force = source.get('force')
            if (not isinstance(raw_force, (list, tuple)) or len(raw_force) != 3
                    or any(type(v) not in (int, float) or not math.isfinite(v)
                           or abs(v) > 1_000_000 for v in raw_force)):
                continue
            force = tuple(float(v) for v in raw_force)
            record = {'id': source.get('id'), 'force': force,
                      'relative_to': 'World', 'part_id': self.root_id}
            if 'root_reference_yaw' in source:
                heading = source['root_reference_yaw']
                if (source.get('kind') != 'VectorForce'
                        or source.get('direction_frame') not in ('Attachment0','Attachment1')
                        or type(heading) not in (int, float) or not math.isfinite(heading)):
                    continue
                record['root_reference_yaw'] = float(heading)
            accepted.append(record)
        self._force_sources = tuple(accepted)
        self._velocity_sources = tuple(servos)
        self._position_sources = tuple(springs)
        self._linear_velocity_sources = tuple(constraints)
        ids={s['id'] for s in springs}
        self._reached_targets={key:value for key,value in self._reached_targets.items() if key in ids}
        self._mover_feedback=[item for item in self._mover_feedback if item['id'] in ids]

    def _mover_position_offset(self, source, *, yaw=None):
        x,y,z=source['position_offset']
        angle=(self.yaw if yaw is None else yaw)-source['root_reference_yaw']
        c,s=math.cos(angle),math.sin(angle)
        return (c*x+s*z,y,-s*x+c*z)

    def _force_acceleration(self):
        if not self._force_sources or not self.assembly_mass:
            return (0., 0., 0.)
        total = [0., 0., 0.]
        for source in self._force_sources:
            x, y, z = source['force']
            if 'root_reference_yaw' in source:
                angle = self.yaw - source['root_reference_yaw']
                c, s = math.cos(angle), math.sin(angle)
                x, z = c*x+s*z, -s*x+c*z
            for i, value in enumerate((x,y,z)):
                total[i] += value
        acceleration = tuple(value/self.assembly_mass for value in total)
        return tuple(max(-self.max_fall_speed, min(self.max_fall_speed, value))
                     for value in acceleration)

    def _grounded_platform_poses(self):
        source = self._ground_source
        return (self._platform_poses_previous.get(source),
                self._platform_poses_current.get(source))

    def _inside_environment(self, kind, point):
        return any(all(region['min'][i] <= point[i] <= region['max'][i]
                       for i in range(3)) for region in self._environment[kind])

    def consume(self, intents):
        for intent in intents:
            if not isinstance(intent, dict):
                raise ValueError('each movement intention must be a record')
            sequence = intent.get('sequence')
            if type(sequence) is not int or sequence <= self.last_sequence:
                raise ValueError('movement intentions must have increasing integer sequence IDs')
            self.last_sequence = sequence
            kind = intent.get('kind')
            permitted_receivers = ((self.player_id, self.humanoid_id, *self._motion_parts)
                                   if kind in ('Impulse', 'SetLinearVelocity') else (self.player_id, self.humanoid_id))
            if intent.get('instance_id') not in permitted_receivers:
                reason = 'different_character'
            elif self.controller is None:
                reason = self.last_reason or 'character_not_resolved'
            elif self.anchored is not False:
                reason = 'anchored' if self.anchored is True else 'anchored_state_unknown'
            elif kind in ('Impulse','SetLinearVelocity') and intent.get('assembly_root_id',self.root_id)!=self.root_id:
                reason = 'assembly_changed_at_call'
            elif camera_movement_error(intent):
                reason = camera_movement_error(intent)
            elif kind == 'Jump' and intent.get('instance_id') != self.humanoid_id:
                reason = 'jump_requires_humanoid'
            elif kind == 'Impulse' and intent.get('instance_id') not in (self.humanoid_id, *self._motion_parts):
                reason = 'impulse_requires_own_character_part'
            elif kind == 'SetLinearVelocity' and intent.get('instance_id') not in self._motion_parts:
                reason = 'velocity_requires_own_character_part'
            elif kind in ('ChangeState', 'SetStateEnabled') and intent.get('instance_id') != self.humanoid_id:
                reason = 'state_change_requires_humanoid'
            elif kind not in ('Move', 'MoveTo', 'Stop', 'Jump', 'Impulse', 'SetLinearVelocity',
                              'ChangeState', 'SetStateEnabled'):
                raise ValueError('unsupported movement intention kind')
            elif kind != 'Stop' and self._health_reason:
                reason = self._health_reason
            elif kind in ('Move','MoveTo','Jump') and self._platform_standing():
                reason = 'platform_standing'
            elif kind in ('Move', 'MoveTo', 'Jump') and self._environment.get('seat') and self._state_enabled.get('Seated', True):
                reason = 'seated'
            elif kind == 'Impulse':
                reason = self._validate_impulse(intent)
            elif kind == 'SetLinearVelocity':
                reason = self._validate_linear_velocity(intent)
            elif kind == 'ChangeState':
                reason = self._change_state(intent)
            elif kind == 'SetStateEnabled':
                name, enabled = intent.get('state'), intent.get('enabled')
                if (name not in STATE_VALUES or type(enabled) is not bool
                        or intent.get('state_value', STATE_VALUES.get(name)) != STATE_VALUES.get(name)):
                    reason = 'invalid_state_enablement'
                else:
                    reason = None
            else:
                reason = None
            if reason:
                self.rejected += 1
                self.last_reason = reason
                continue
            if kind == 'SetLinearVelocity':
                velocity = tuple(float(v) for v in intent['vector'])
                # Replace total horizontal speed, including authored walking,
                # rather than accumulating another impulse on top of it.
                if self.target is not None:
                    p = self.controller.position
                    self.controller.set_direction((self.target[0]-p[0], 0., self.target[2]-p[2]), normalize=True)
                walking = self.controller.step(0.)['velocity']
                self._impulse_velocity = (velocity[0]-walking[0], 0., velocity[2]-walking[2])
                self._slope_velocity = (0., 0., 0.)
                self.vertical_velocity = velocity[1]
                self.linear_velocity = velocity
                if velocity[1] > 0:
                    self.grounded = False
                    self._jumping = False
                    self._ground_source = None
                self._last_report = None
            elif kind == 'Impulse':
                impulse = tuple(float(v) for v in Controller(intent.get('vector')).position)
                dv = tuple(v / self.assembly_mass for v in impulse)
                self._impulse_velocity = (self._impulse_velocity[0] + dv[0], 0.,
                                          self._impulse_velocity[2] + dv[2])
                self.vertical_velocity += dv[1]
                if dv[1] > 0:
                    self.grounded = False
                    self._jumping = False
                    self._ground_source = None
            elif kind == 'Jump':
                reason = self._request_jump(intent)
                if reason:
                    self.rejected += 1
                    self.last_reason = reason
                    continue
            elif kind == 'ChangeState':
                pass
            elif kind == 'SetStateEnabled':
                self.update_state_enabled(intent['state'], intent['enabled'])
            elif kind == 'Stop':
                self.controller.stop()
                self.target = None
            elif kind == 'Move':
                self.controller.set_direction(movement_direction(intent))
                vector = intent.get('vector')
                self._vertical_move = (float(vector[1]) if isinstance(vector, (list, tuple))
                                       and len(vector) == 3 and type(vector[1]) in (int, float)
                                       and math.isfinite(vector[1]) else 0.)
                self.target = None
            else:
                target = Controller(intent.get('vector')).position
                self.target = target
                self._move_to_request = sequence
                self._move_to_elapsed = 0.0
                x, _, z = self.controller.position
                self.controller.set_direction((target[0] - x, 0.0, target[2] - z), normalize=True)
            self.accepted += 1
            self.last_reason = None

    def _request_jump(self, intent):
        if self._collision_error is not None:return 'collision_group_state_unavailable'
        if self._platform_standing():return 'platform_standing'
        if not self._state_enabled.get('Jumping', True):
            return 'jump_state_disabled'
        if not self.grounded:
            return 'jump_requires_ground'
        supplied = intent.get('jump_settings', {})
        if not isinstance(supplied, dict):
            return 'invalid_jump_settings'
        settings = {**self.jump_settings, **supplied}
        platform_stand=(settings.get('PlatformStand',False) if self._platform_stand_request is None
                        else self._platform_standing())
        if platform_stand or settings.get('Sit', False):
            return 'jump_state_not_supported'
        health = settings.get('Health', 100.)
        if type(health) not in (int, float) or not math.isfinite(health) or health <= 0:
            return 'humanoid_not_alive'
        use_power = settings.get('UseJumpPower', True)
        if type(use_power) is not bool:
            return 'invalid_jump_settings'
        strength = settings.get('JumpPower', 50.) if use_power else settings.get('JumpHeight', 7.2)
        if type(strength) not in (int, float) or not math.isfinite(strength) or strength < 0:
            return 'invalid_jump_settings'
        if use_power and strength > 1000:
            return 'invalid_jump_settings'
        speed = strength if use_power else math.sqrt(self.gravity)*math.sqrt(strength)*math.sqrt(2.)
        if not math.isfinite(speed):
            return 'invalid_jump_settings'
        if speed <= 0:
            return 'jump_disabled'
        # Recheck support: the floor may have been removed since the last step.
        point = (self.controller.position[0], self.vertical_position,
                 self.controller.position[2])
        previous_pose, current_pose = self._grounded_platform_poses()
        if previous_pose is not None and current_pose is not None:
            point = carry_grounded_point(point, previous_pose, current_pose)
        elif self._ground_source in self._surface_velocities:
            # Part velocity is only inherited at departure; stepping the
            # carried point is handled by the surface mover path.
            pass
        support = self._ground_support(point, foot_offset=self.foot_offset,
                                       max_distance=.05)
        if support is None:
            self.grounded = False
            return 'jump_requires_ground'
        source = getattr(support.collider, 'source', None)
        previous_pose = self._platform_poses_previous.get(source)
        current_pose = self._platform_poses_current.get(source)
        if (previous_pose is not None and current_pose is not None
                and previous_pose != current_pose and self._platform_pose_dt > 0):
            inherited = inherit_platform_velocity(
                self.linear_velocity, point, previous_pose, current_pose,
                self._platform_pose_dt)
            self._impulse_velocity = (self._impulse_velocity[0]+inherited[0], 0.,
                                      self._impulse_velocity[2]+inherited[2])
            speed += inherited[1]
        elif self._ground_source in self._surface_velocities:
            inherited = self._surface_velocities[self._ground_source]
            self._impulse_velocity = (self._impulse_velocity[0]+inherited[0], 0.,
                                      self._impulse_velocity[2]+inherited[2])
            speed += inherited[1]
        self.vertical_velocity = speed
        self.grounded = False
        self._ground_source = None
        self._jumping = True
        self._last_report = None
        return None

    def _finish_move_to(self, reached):
        """Queue one result until the VM acknowledges its feedback batch."""
        if self.target is None:
            return
        self.target = None
        self.controller.stop()
        self.linear_velocity = (self._impulse_velocity[0]+self._slope_velocity[0],
            0. if self.grounded else self.vertical_velocity,
            self._impulse_velocity[2]+self._slope_velocity[2])
        self._feedback_sequence += 1
        self._move_to_feedback.append({'sequence': self._move_to_request,
            'reached': reached, '_sequence': self._feedback_sequence})
        self._move_to_request = None

    def _ground_support(self, position, *, foot_offset=None, max_distance=.05):
        """Prefer center foot contact, then use only walkable footprint support."""
        if foot_offset is None:
            foot_offset = self.foot_offset
        center = ground_raycast(position, self.geometry, foot_offset=foot_offset,
                                max_distance=max_distance)
        if center is not None or self.body_half_size is None:
            return center
        return footprint_ground_raycast(
            position, self.geometry, half_size=self.body_half_size,
            foot_offset=foot_offset, max_distance=max_distance,
            max_slope_angle=self.max_slope_angle, probe_count=9)

    def _validate_linear_velocity(self, intent):
        try:
            velocity = Controller(intent.get('vector')).position
        except (TypeError, ValueError):
            return 'invalid_linear_velocity'
        if any(abs(v) > self.max_fall_speed for v in velocity):
            return 'linear_velocity_exceeds_local_limit'
        if self._environment.get('seat') and self._state_enabled.get('Seated', True):
            return 'seated'
        return None

    def _validate_impulse(self, intent):
        """Require a finite impulse and explicitly received root assembly mass."""
        if intent.get('position') is not None:
            return 'point_impulse_rotation_unavailable'
        try:
            vector = Controller(intent.get('vector')).position
        except (TypeError, ValueError):
            return 'invalid_impulse'
        if any(not math.isfinite(v) for v in vector):
            return 'invalid_impulse'
        mass = self.assembly_mass
        if (self.assembly_mass_source not in MASS_SOURCES
                or type(mass) not in (int, float) or not math.isfinite(mass) or mass <= 0):
            return 'received_mass_unavailable'
        if any(abs(component / float(mass)) > self.max_fall_speed for component in vector):
            return 'impulse_out_of_bounds'
        return None

    def _change_state(self, intent):
        """Accept a state request only when current local evidence supports it."""
        name = intent.get('state')
        if name not in ('Jumping', 'Seated', 'Running', 'Freefall', 'Climbing', 'Swimming','PlatformStanding'):
            return 'unsupported_humanoid_state'
        if not self._state_enabled.get(name, True):
            return 'humanoid_state_disabled'
        if name=='PlatformStanding':
            self._platform_stand_request=True;self._stop_humanoid_locomotion();return None
        point = (self.controller.position[0], self.vertical_position,
                 self.controller.position[2])
        seated = self._environment.get('seat') is not None
        swimming = self._inside_environment('water_regions', point)
        climbing = self._inside_environment('ladder_regions', point) and not swimming
        supported = {
            'Seated': seated,
            'Swimming': swimming,
            'Climbing': climbing,
            'Running': self.grounded and not (seated or swimming or climbing),
            'Freefall': not self.grounded and not (seated or swimming or climbing),
        }
        if name == 'Jumping':
            return self._request_jump(intent)
        if supported.get(name,False):
            self._platform_stand_request=False;return None
        return f'{name.lower()}_state_not_supported'

    def step(self, dt, timestamp=None):
        """Advance a bounded step and return a clearly labeled local prediction."""
        if isinstance(dt, bool) or not isinstance(dt, (int, float)) or not math.isfinite(dt) or not 0 <= dt <= MAX_STEP_SECONDS:
            raise ValueError('dt must be finite and within 0..0.25 seconds')
        if timestamp is not None and (isinstance(timestamp, bool) or not isinstance(timestamp, (int, float)) or not math.isfinite(timestamp)):
            raise ValueError('timestamp must be finite')
        dt = float(dt)
        if self._collision_error is not None:
            # Preserve pose and momentum while the received rules are unknown.
            # Simulating against an empty map would silently fall through it.
            self._last_timestamp=timestamp
            self._last_report={
                'mode':'local_prediction','source':'authored_movement_intentions',
                'server_confirmed':False,'network_packets_sent':0,
                'character_id':self.character_id,'predicted_state':None,
                'received_state_modified':False,'timestamp':timestamp,
                'simulated_seconds':self.simulated_seconds,
                'last_reason':'collision_group_state_unavailable',
                'collision_filter':{'model':'root_box_group','group':self._root_collision_group,
                                    'error':self._collision_error},
                'accepted_intents':self.accepted,'rejected_intents':self.rejected,
            }
            return self._last_report
        move_to_remaining = max(0., 8. - self._move_to_elapsed)
        if self.target is not None:
            self._move_to_elapsed += dt
        platform_standing=self._platform_standing()
        if platform_standing:self._stop_humanoid_locomotion()
        contact_offset=(self.body_half_size[1] if self.body_half_size is not None else 0.) if platform_standing else self.foot_offset
        force_acceleration = self._force_acceleration()
        pre_force_velocity = self._impulse_velocity
        has_forces=bool(self._force_sources or self._velocity_sources or self._position_sources or self._linear_velocity_sources)
        spring_forces = {}
        mover_curve = None
        mover_start_grounded = self.grounded
        mover_start_yaw = self.yaw
        mover_mass=self.assembly_mass
        if not mover_mass and _stiff_velocity_servos_only(self._velocity_sources,self._position_sources,self._linear_velocity_sources):
            # Mass unknown, but every BodyVelocity force limit is far above any
            # character weight: its result is then the stiff limit (velocity
            # reaches the target), which this tiny solver mass reproduces. It
            # is used for this servo solve only, never stored as assembly mass.
            mover_mass=_STIFF_SERVO_MASS;self.stiff_servo_fallback_steps+=1
        if (dt>0 and (self._velocity_sources or self._position_sources or self._linear_velocity_sources) and mover_mass and self.controller is not None
                and self.anchored is False and not self._health_reason):
            if self.target is not None:
                p=self.controller.position
                self.controller.set_direction((self.target[0]-p[0],0.,self.target[2]-p[2]),normalize=True)
            requested=self.controller.step(0.)['velocity']
            current=(requested[0]+pre_force_velocity[0],self.vertical_velocity,requested[2]+pre_force_velocity[2])
            external=(force_acceleration[0],force_acceleration[1]-self.gravity,force_acceleration[2])
            springs=[]
            for source in self._position_sources:
                offset=self._mover_position_offset(source)
                springs.append({**source,'target_position':tuple(source['target_position'][i]-offset[i] for i in range(3))})
            servo,spring_forces=solve_movers(current,self._velocity_sources,mover_mass,dt,external,
                position=(self.controller.position[0],self.vertical_position,self.controller.position[2]),
                position_sources=springs)
            force_acceleration=tuple(force_acceleration[i]+servo[i] for i in range(3))
            constraints=[]
            for source in self._linear_velocity_sources:
                axes=source['axes']
                if 'root_reference_yaw' in source:
                    angle=self.yaw-source['root_reference_yaw'];c,s=math.cos(angle),math.sin(angle)
                    axes=tuple((c*x+s*z,y,-s*x+c*z) for x,y,z in axes)
                constraints.append({**source,'axes':axes})
            free=tuple(current[i]+(force_acceleration[i]-(self.gravity if i==1 else 0.))*dt for i in range(3))
            projected=constraint_acceleration(free,constraints,mover_mass,dt)
            force_acceleration=tuple(force_acceleration[i]+projected[i] for i in range(3))
            mover_curve = ((self.controller.position[0],self.vertical_position,self.controller.position[2]),
                           current,(force_acceleration[0],force_acceleration[1]-self.gravity,force_acceleration[2]))
        if (dt > 0 and has_forces and self.controller is not None
                and self.anchored is False and not self._health_reason):
            self._impulse_velocity = tuple(
                0. if i == 1 else max(-self.max_fall_speed, min(self.max_fall_speed,
                    pre_force_velocity[i]+force_acceleration[i]*dt))
                for i in range(3))
        force_displacement_velocity = tuple(
            (pre_force_velocity[i]+self._impulse_velocity[i])*.5 for i in range(3))
        self.angular_velocity = (0.0, 0.0, 0.0)
        self._body_collision = {'available': self.body_half_size is not None,
                                'model': 'axis_aligned_root_box', 'contacts': 0,
                                'initial_overlap': False, 'limited': False}
        horizontal_contact_normals = ()
        if self.controller is not None and self.anchored is False:
            seated = (not platform_standing and self._environment.get('seat') is not None
                      and self._state_enabled.get('Seated', True))
            if seated:
                self.controller.stop()
                self.target = None
            before = self.controller.position
            remaining = dt
            if self.target is not None:
                remaining = min(remaining, move_to_remaining)
                distance = math.hypot(self.target[0] - before[0], self.target[2] - before[2])
                self.controller.set_direction((self.target[0]-before[0], 0., self.target[2]-before[2]), normalize=True)
                if self.controller.walk_speed > 0.0:
                    remaining = min(remaining, distance / self.controller.walk_speed)
            result = self.controller.step(0. if seated else remaining)
            input_delta = (self.controller.position[0]-before[0], 0.,
                           self.controller.position[2]-before[2])
            base = (before[0], self.vertical_position, before[2])
            original_base = base
            platform_velocity = (0., 0., 0.)
            if (self.grounded and not seated and self._platform_pose_dt > 0
                    and self._platform_carry_pending):
                previous_pose, current_pose = self._grounded_platform_poses()
                if previous_pose is not None and current_pose is not None:
                    base = carry_grounded_point(base, previous_pose, current_pose)
                    platform_velocity = platform_point_velocity(
                        base, previous_pose, current_pose, self._platform_pose_dt)
            if seated and self._seat_carry_pending and self._seat_dt > 0:
                base = tuple(base[i]+self._seat_delta[i] for i in range(3))
                platform_velocity = tuple(self._seat_delta[i]/self._seat_dt for i in range(3))
            current_surface_velocity = (self._surface_velocities.get(self._ground_source, (0., 0., 0.))
                                        if self.grounded and not seated else (0., 0., 0.))
            previous_pose, current_pose = self._grounded_platform_poses()
            pose_moved = (previous_pose is not None and current_pose is not None
                          and previous_pose != current_pose)
            if not pose_moved and any(abs(v) > 0 for v in current_surface_velocity):
                base = tuple(base[i]+current_surface_velocity[i]*dt for i in range(3))
                platform_velocity = current_surface_velocity
            carry_delta = tuple(base[i]-original_base[i] for i in range(3))
            horizontal_velocity = (result['velocity'][0]+self._impulse_velocity[0]+platform_velocity[0],
                                   0., result['velocity'][2]+self._impulse_velocity[2]+platform_velocity[2])
            x = base[0]+input_delta[0]+force_displacement_velocity[0]*dt
            z = base[2]+input_delta[2]+force_displacement_velocity[2]*dt
            previous_y = base[1]
            slope_support = (self._ground_support((before[0], previous_y, before[2]),
                                            foot_offset=contact_offset, max_distance=.05)
                             if self.grounded and not seated else None)
            if slope_support is not None and not walkable_normal(
                    slope_support.normal, self.max_slope_angle):
                acceleration = slope_slide_acceleration(slope_support.normal, self.gravity)
                self._slope_velocity = tuple(
                    max(-self.max_fall_speed, min(self.max_fall_speed,
                        self._slope_velocity[i] + acceleration[i]*dt)) for i in range(3))
                x += self._slope_velocity[0]*dt
                z += self._slope_velocity[2]*dt
                horizontal_velocity = (horizontal_velocity[0]+self._slope_velocity[0], 0.,
                                       horizontal_velocity[2]+self._slope_velocity[2])
            else:
                self._slope_velocity = (0., 0., 0.)
            if self.body_half_size is not None:
                body = move_horizontal_body((before[0], self.vertical_position, before[2]),
                                            (x-before[0], 0., z-before[2]), self.body_half_size,
                                            self.geometry, grounded=self.grounded,
                                            foot_offset=contact_offset,
                                            max_slope_angle=self.max_slope_angle,
                                            max_step_height=0. if platform_standing else self.max_step_height)
                x, _, z = body.position
                horizontal_contact_normals = body.normals
                self.controller.resolve_position((x, before[1], z))
                horizontal_velocity = ((0., 0., 0.) if body.initial_overlap or body.limited
                                       else clip_horizontal(horizontal_velocity, body.normals))
                self._body_collision.update(contacts=len(body.normals), initial_overlap=body.initial_overlap,
                                            limited=body.limited)
                if dt > 0 and abs(carry_delta[1]) > 1e-9:
                    carried, carry_hit = step_vertical_body(
                        (x, self.vertical_position, z), carry_delta[1]/dt, dt,
                        self.geometry, False, half_size=self.body_half_size,
                        foot_offset=contact_offset, gravity=0.,
                        max_fall_speed=self.max_fall_speed,
                        max_slope_angle=self.max_slope_angle)
                    base = (base[0], carried.position[1], base[2])
                    if carry_hit is not None:
                        self._body_collision['contacts'] += len(carry_hit.normals)
                        self._body_collision['ceiling_contact'] = bool(carry_hit.normals)
                        self._body_collision['initial_overlap'] |= carry_hit.initial_overlap
            # Carry authored impulse displacement through the shared horizontal
            # integrator after collision resolution, including without a sized
            # root proxy.
            self.controller.resolve_position((x, before[1], z))
            point = (x, base[1], z)
            swimming = (not platform_standing and self._inside_environment('water_regions', point)
                        and self._state_enabled.get('Swimming', True))
            climbing = (not platform_standing and self._inside_environment('ladder_regions', point)
                        and self._state_enabled.get('Climbing', True) and not swimming)
            rising_jump_velocity = (self.vertical_velocity
                                    if self._jumping and self.vertical_velocity > 0 else None)
            rising_forced=has_forces and force_acceleration[1]>self.gravity
            followed = (follow_ground((base[0], previous_y, base[2]),
                                      (x-base[0], 0., z-base[2]), self.geometry,
                                      foot_offset=contact_offset)
                        if self.grounded and not (seated or swimming or climbing or rising_forced) else None)
            if seated:
                vertical = step_vertical_body(
                    point, 0., 0., self.geometry, self.grounded,
                    half_size=self.body_half_size, foot_offset=contact_offset,
                    gravity=self.gravity, max_fall_speed=self.max_fall_speed,
                    max_slope_angle=self.max_slope_angle)[0]
                ceiling = None
                self.vertical_velocity = 0.
            elif climbing:
                climb_velocity = self._vertical_move*self.controller.walk_speed
                vertical = GroundStep((x, self.vertical_position+climb_velocity*dt, z),
                                     climb_velocity, False)
                ceiling = None
            elif swimming:
                swim_velocity = (self._vertical_move*self.controller.walk_speed
                                 if self._vertical_move else self.vertical_velocity*.9)
                vertical, ceiling = step_vertical_body(
                    point, swim_velocity, dt, self.geometry, self.grounded,
                    half_size=self.body_half_size, foot_offset=contact_offset,
                    gravity=0., max_fall_speed=self.max_fall_speed,
                    max_slope_angle=self.max_slope_angle)
            elif followed is not None:
                vertical, ceiling = followed, None
            elif has_forces:
                vertical,ceiling=step_vertical_accelerated(
                    point,self.vertical_velocity,dt,self.geometry,self.grounded,
                    acceleration=force_acceleration[1]-self.gravity,
                    half_size=self.body_half_size,foot_offset=contact_offset,
                    max_fall_speed=self.max_fall_speed,max_slope_angle=self.max_slope_angle)
            else:
                vertical, ceiling = step_vertical_body(
                    point, self.vertical_velocity, dt, self.geometry, self.grounded,
                    half_size=self.body_half_size, foot_offset=contact_offset,
                    gravity=self.gravity,
                    max_fall_speed=self.max_fall_speed,
                    max_slope_angle=self.max_slope_angle)
            if (rising_jump_velocity is not None and ceiling is not None
                    and ceiling.initial_overlap and not vertical.grounded):
                # Initial overlap has no unique collision normal. Preserve a
                # real upward jump request; do not reinterpret a nearby wall
                # overlap as ground or clear Jumping from the model.
                # The unresolved contact holds position, not the ballistic
                # clock: preserving the same upward speed every tick would
                # leave this character permanently in Jumping.
                overlap_velocity = max(-self.max_fall_speed,
                                       rising_jump_velocity-self.gravity*dt)
                vertical = GroundStep(vertical.position, overlap_velocity, False,
                                      vertical.hit)
            # The root-box fallback can contact a floor before the Humanoid
            # foot point reaches it (or start in a shallow floor overlap).
            # Standing uses the rig clearance; PlatformStanding uses the root
            # collision proxy without a Humanoid stance lift.
            # Only apply this correction when an explicit upward support is
            # found directly below the root within its collision half-height.
            if (not (seated or swimming or climbing) and self.body_half_size is not None
                    and (vertical.grounded or (ceiling is not None
                         and ceiling.initial_overlap and rising_jump_velocity is None and not rising_forced))):
                foot_support = self._ground_support(
                    vertical.position, foot_offset=contact_offset,
                    max_distance=.05)
                start_support = self._ground_support(
                    point, foot_offset=contact_offset, max_distance=.05)
                if foot_support is None:
                    support = self._ground_support(
                        vertical.position, foot_offset=0.,
                        max_distance=self.body_half_size[1]+.05)
                else:
                    support = foot_support
                if (start_support is None and support is not None
                        and walkable_normal(support.normal, self.max_slope_angle)):
                    target_y = support.y+contact_offset
                    rise = target_y-point[1]
                    if rise > 1e-6:
                        if vertical_body_clearance(
                                point, rise, self.body_half_size, self.geometry,
                                support_collider=support.collider):
                            vertical = GroundStep(
                                (vertical.position[0], target_y, vertical.position[2]),
                                0., True, support.collider)
                        else:
                            # Keep the supported root-box contact if any other
                            # obstacle intersects the correction sweep.
                            contact_y = support.y+self.body_half_size[1]+1e-6
                            vertical = GroundStep(
                                (vertical.position[0], contact_y, vertical.position[2]),
                                0., True, support.collider)
            if ceiling is not None:
                self._body_collision['contacts'] += len(ceiling.normals)
                self._body_collision['initial_overlap'] |= ceiling.initial_overlap
                self._body_collision['ceiling_contact'] = bool(ceiling.normals)
            if vertical.grounded or vertical.vertical_velocity <= 0:
                self._jumping = False
            self.vertical_position = vertical.position[1]
            self.vertical_velocity = vertical.vertical_velocity
            self.grounded = vertical.grounded
            if (not self.grounded and any(abs(v) > 1e-9 for v in current_surface_velocity)):
                self._impulse_velocity = (self._impulse_velocity[0]+current_surface_velocity[0], 0.,
                                          self._impulse_velocity[2]+current_surface_velocity[2])
                self.vertical_velocity += current_surface_velocity[1]
            final_support = (self._ground_support(vertical.position,
                                            foot_offset=contact_offset, max_distance=.05)
                             if self.grounded else None)
            self._ground_source = (getattr(final_support.collider, 'source', None)
                                   if final_support is not None else None)
            if self.grounded and dt > 0:
                decay = max(0., 1.-8.*dt)
                self._impulse_velocity = (self._impulse_velocity[0]*decay, 0.,
                                          self._impulse_velocity[2]*decay)
            if horizontal_contact_normals:
                # Contact response must persist into the next step. Clipping
                # only the reported velocity leaves old knockback pushing into
                # the wall after the contact, fighting walking and mover input.
                # Apply this after platform inheritance so it cannot restore
                # the blocked component. Unknown overlaps have no normal and
                # therefore do not invent a momentum correction.
                self._impulse_velocity = clip_horizontal(self._impulse_velocity, horizontal_contact_normals)
                self._slope_velocity = clip_horizontal(self._slope_velocity, horizontal_contact_normals)
            contact_velocity = self.vertical_velocity
            if followed is not None:
                contact_velocity = 0.
                if dt > 0:
                    support = self._ground_support(vertical.position,
                                                   foot_offset=contact_offset,
                                                   max_distance=.05)
                    if support is not None:
                        nx, ny, nz = support.normal
                        contact_velocity = -(nx*horizontal_velocity[0]+nz*horizontal_velocity[2])/ny
            contact_velocity += platform_velocity[1]
            self.linear_velocity = (horizontal_velocity[0], contact_velocity,
                                    horizontal_velocity[2])
            # Facing follows authored movement, not sideways knockback or a
            # conveyor. Using total velocity here also makes local thrusters
            # rotate themselves repeatedly in response to their own force.
            vx, _, vz = input_delta
            if self.auto_rotate and (vx != 0.0 or vz != 0.0):
                # Roblox CFrame faces along -Z at yaw zero. For a Y rotation,
                # LookVector is (-sin(yaw), 0, -cos(yaw)), so invert the
                # horizontal velocity's X sign when deriving the facing yaw.
                next_yaw = math.atan2(-vx, -vz)
                delta = (next_yaw - self.yaw + math.pi) % (2 * math.pi) - math.pi
                self.angular_velocity = (0.0, delta / remaining if remaining > 0.0 else 0.0, 0.0)
                self.yaw = next_yaw
                self.yaw_origin = 'authored_motion'
            self.simulated_seconds += dt
            if self.target is not None:
                x, _, z = self.controller.position
                if math.hypot(self.target[0] - x, self.target[2] - z) <= 1.:
                    self._finish_move_to(True)
        elif self.controller is None:
            self.linear_velocity = (0.0, 0.0, 0.0)
        if self.target is not None and self._move_to_elapsed >= 8. - 1e-9:
            self._finish_move_to(False)
        state = None
        if self.controller is not None and self.anchored is False:
            point = (self.controller.position[0], self.vertical_position,
                     self.controller.position[2])
            swimming = (not platform_standing and self._inside_environment('water_regions', point)
                        and self._state_enabled.get('Swimming', True))
            climbing = (not platform_standing and self._inside_environment('ladder_regions', point)
                        and self._state_enabled.get('Climbing', True) and not swimming)
            seated = (not platform_standing and self._environment.get('seat') is not None
                      and self._state_enabled.get('Seated', True))
            humanoid_state = classify_state(
                grounded=self.grounded, vertical_velocity=self.vertical_velocity,
                jumping=self._jumping and self._state_enabled.get('Jumping', True),
                swimming=swimming, climbing=climbing,
                seated=seated, dead=self._health_reason == 'humanoid_not_alive',
                platform_standing=platform_standing)
            if (isinstance(self._previous_humanoid_state, dict)
                    and not self._state_enabled.get(humanoid_state['name'], True)):
                # A disabled automatic state does not become the model's new
                # state; retain the last enabled state label until a supported
                # enabled transition becomes available.
                humanoid_state = dict(self._previous_humanoid_state)
            # Keep the established public model label stable for existing
            # consumers while individual state names become more specific.
            humanoid_state['model'] = 'supported_ground_contact_subset'
            state_events = transition_events(self._previous_humanoid_state, humanoid_state)
            state_events = tuple(event for event in state_events
                                 if self._state_enabled.get(event, True))
            state_transitions = state_transition_records(
                self._previous_humanoid_state, humanoid_state)
            state_transitions = [event for event in state_transitions
                                 if self._state_enabled.get(event['name'], True)]
            state_signals = state_signal_events(self._previous_humanoid_state, humanoid_state)
            for event in state_transitions:
                self._feedback_sequence += 1
                self._feedback_transitions.append({**event, '_sequence': self._feedback_sequence})
            for event in state_signals:
                self._feedback_sequence += 1
                self._feedback_signals.append({**event, '_sequence': self._feedback_sequence})
            self._previous_humanoid_state = dict(humanoid_state)
            state = {
                'position': [self.controller.position[0], self.vertical_position,
                             self.controller.position[2]],
                'linear_velocity': list(self.linear_velocity),
                'angular_velocity': list(self.angular_velocity),
                'yaw': self.yaw,
                'yaw_origin': self.yaw_origin,
                'auto_rotate': self.auto_rotate,
                'target': list(self.target) if self.target is not None else None,
                'vertical_velocity': self.vertical_velocity,
                'grounded': self.grounded,
                'humanoid_state': humanoid_state,
                'humanoid_state_events': list(state_events),
                'humanoid_state_transitions': state_transitions,
                'humanoid_state_signals': state_signals,
                'environment_evidence': {
                    'water': 'received_geometry' if swimming else None,
                    'ladder': 'received_geometry' if climbing else None,
                    'seat': 'received_seat_link' if seated else None,
                },
            }
        if dt>0:
            self._mover_feedback=[]
            if state is not None:
                swept_curve = None
                if (mover_curve is not None and not (seated or swimming or climbing)
                        and not self._body_collision['contacts']
                        and not self._body_collision['initial_overlap']
                        and not self._body_collision['limited']):
                    start,velocity,acceleration=mover_curve
                    if mover_start_grounded and self.grounded and abs(start[1]-state['position'][1])<=1e-9:
                        velocity=(velocity[0],0.,velocity[2])
                        acceleration=(acceleration[0],0.,acceleration[2])
                    expected=tuple(start[i]+velocity[i]*dt+.5*acceleration[i]*dt*dt for i in range(3))
                    # Contacts, carry, terminal-speed limits and other movement
                    # modes can change the path. Only sweep the analytic curve
                    # when it describes the displacement actually integrated.
                    if math.dist(expected,state['position'])<=1e-7:
                        swept_curve=(start,velocity,acceleration)
                for source in self._position_sources:
                    if source['id'] not in spring_forces:continue
                    target=source['target_position']
                    token=(target,source['target_revision'],source['target_generation'])
                    was_reached=self._reached_targets.get(source['id'])==token
                    offset=self._mover_position_offset(source)
                    point=tuple(state['position'][i]+offset[i] for i in range(3))
                    reached=was_reached or math.dist(point,target)<=.1
                    if not reached and mover_curve is not None:
                        start_offset=self._mover_position_offset(source,yaw=mover_start_yaw)
                        start_point=tuple(mover_curve[0][i]+start_offset[i] for i in range(3))
                        reached=math.dist(start_point,target)<=.1
                    if not reached and swept_curve is not None:
                        # A rotating offset traces an arc rather than the root's
                        # parabola. Retain endpoint checks for that separate path.
                        if self.yaw==mover_start_yaw or not any(source['position_offset']):
                            start,velocity,acceleration=swept_curve
                            start=tuple(start[i]+offset[i] for i in range(3))
                            reached=curve_reaches_target(start,velocity,acceleration,dt,target)
                    if reached:self._reached_targets[source['id']]=token
                    self._mover_feedback.append({
                        'id':source['id'],'target_position':list(target),
                        'parent_id':source['parent_id'],
                        'target_revision':source['target_revision'],
                        'target_generation':source['target_generation'],
                        'last_force':spring_forces[source['id']], 'reached':reached})
        self._last_timestamp = timestamp
        self._platform_carry_pending = False
        self._seat_carry_pending = False
        self._last_report = {
            'mode': 'local_prediction', 'source': 'authored_movement_intentions',
            'server_confirmed': False, 'network_packets_sent': 0,
            'character_id': self.character_id, 'predicted_state': state,
            'assembly_mass': self.assembly_mass, 'assembly_mass_source': self.assembly_mass_source,
            'stiff_servo_fallback_steps': self.stiff_servo_fallback_steps,
            'received_state_modified': False, 'timestamp': timestamp,
            'simulated_seconds': self.simulated_seconds,
            'foot_offset': self.foot_offset,
            'foot_offset_origin': self.foot_offset_origin,
            'body_collision': dict(self._body_collision),
            'collision_filter': {'model':'root_box_group','group':self._root_collision_group,
                                 'applied':self._collision_context is not None,
                                 'colliders':len(self.geometry.colliders),'error':None},
            'jump_model': 'grounded_ballistic_subset',
            'accepted_intents': self.accepted, 'rejected_intents': self.rejected,
            'last_reason': self.last_reason,
        }
        return self._last_report

    @property
    def vertical_position(self):
        """Current predicted Y coordinate, independent of received snapshots."""
        return getattr(self, '_vertical_position', 0.0)

    @vertical_position.setter
    def vertical_position(self, value):
        self._vertical_position = float(value)

    def report(self, timestamp=None):
        """Return the latest prediction, or a current-state report before stepping."""
        if self._last_report is not None and (timestamp is None or timestamp == self._last_timestamp):
            return self._last_report
        return self.step(0.0, timestamp=timestamp)
