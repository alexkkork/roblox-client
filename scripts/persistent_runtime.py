"""Persistent Rust/Luau worker and receipt-ordered live world adapter."""
import collections
import json
import math
import os
import queue
import signal
import subprocess
import threading
import time
from platform_process import group_options, stop_group

from local_runtime import RUST_HOST, MAX_SOURCE_BYTES
from world_snapshot import make_world_snapshot, make_world_delta
from script_catalog import script_catalog, CodePreparationQueue
from session_integrity import observe_runtime_thread_report, observe_runtime_protected_report
from physics_runtime_prediction import RuntimePhysicsPrediction
from animation_assets import AnimationAssetQueue
from animation_intents import AnimationIntentOutbox
from runtime_rpc_lane import RuntimeRpcLane
from display_state import validate_display
from chat_protocol import delivery_error


WORLD_OP_WAIT_SECONDS = 35  # > the supervisor's 30 s world-op deadline


class PersistentLuauHost:
    def __init__(self, snapshot, *, memory_mb=None, text_service_factory=None):
        if memory_mb is None:
            memory_mb = 512
        self.memory_mb = memory_mb
        if not RUST_HOST.is_file():
            raise RuntimeError('Build the Rust host before using persistent scripts')
        self.closed = False
        self.text_service = None
        self.text_service_factory = text_service_factory
        self.failure_reason = None
        self.failure_context = None
        self.lock = threading.RLock()
        self.responses = queue.Queue(maxsize=8)
        self.process = subprocess.Popen([str(RUST_HOST), 'session'], stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                        **group_options())
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()
        try:
            result = self.call({'op': 'init', 'snapshot': snapshot, 'memory_mb': memory_mb})
            self.initial_result = result
            if not result['ok']: raise RuntimeError(result.get('error', 'persistent initialization failed'))
        except BaseException:
            self.close(graceful=False)
            raise

    def _read(self):
        try:
            while True:
                line = self.process.stdout.readline(1024 * 1024 + 2)
                if not line: raise RuntimeError('Persistent worker closed its result pipe')
                if len(line) > 1024 * 1024 + 1 or not line.endswith(b'\n'):
                    raise RuntimeError('Invalid persistent result frame')
                self.responses.put(json.loads(line))
        except Exception as error:
            self.responses.put(error)

    def call(self, command):
        with self.lock:
            command=dict(command)
            if self.text_service is not None and command.get('op') not in ('init','render','host_io','physics_feedback','close'):
                ready=self.text_service.drain()
                if ready:command['text_service_responses']=ready+command.get('text_service_responses',[])
            data = json.dumps(command, ensure_ascii=True, allow_nan=False).encode() + b'\n'
            if len(data) > 608 * 1024 * 1024: raise ValueError('Persistent request exceeds 608 MiB')
            if self.closed or self.process.poll() is not None:
                reason = self.failure_reason or f'Persistent worker exited (status {self.process.poll()}); script state was lost'
                raise RuntimeError(reason)
            started = time.monotonic()
            try:
                self.process.stdin.write(data)
                self.process.stdin.flush()
                # The Rust supervisor allows world init/update 30 s (session.rs
                # WORLD_OP_WALL_SECONDS); wait slightly longer so it reports first.
                result = self.responses.get(timeout=WORLD_OP_WAIT_SECONDS if command.get('op') in ('init', 'update') else 7)
                if isinstance(result, Exception): raise result
                if (not isinstance(result, dict) or type(result.get('ok')) is not bool
                        or not isinstance(result.get('values'), list)):
                    raise RuntimeError('Invalid persistent runtime result')
                if result.get('state_lost'):
                    raise RuntimeError(result.get('error', 'Persistent script state was lost'))
                if result.get('text_service_requests') and self.text_service is None and self.text_service_factory is not False:
                    from text_service import TextServiceQueue
                    self.text_service=(self.text_service_factory or TextServiceQueue)()
                if self.text_service is not None and 'active_text_service_requests' in result:
                    self.text_service.submit(result.get('text_service_requests',[]),result['active_text_service_requests'])
                return result
            except Exception as error:
                # queue.Empty has an empty string. Retaining that as a runtime
                # failure makes callers retry a dead worker and lose the cause.
                reason = (f'Persistent worker response deadline exceeded ({WORLD_OP_WAIT_SECONDS if command.get("op") in ("init", "update") else 7} seconds)'
                          if isinstance(error, queue.Empty) else str(error) or type(error).__name__)
                self.failure_reason = reason
                self.failure_context = {
                    'operation': str(command.get('op', 'unknown'))[:64],
                    'request_bytes': len(data),
                    'elapsed_seconds': round(time.monotonic()-started, 6),
                    'error_type': type(error).__name__,
                    'process_status_before_cleanup': self.process.poll(),
                }
                self.close(graceful=False)
                if isinstance(error, queue.Empty):
                    raise TimeoutError(reason) from error
                raise

    def host_io(self, action, **fields):
        result = self.call({'op': 'host_io', 'command': {'action': action, **fields}})
        if not result['ok']:
            raise RuntimeError(result.get('error', 'Host I/O failed'))
        return result['host_io']

    def handoff_teleport(self, snapshot):
        """Transfer queued code once to an initialized destination world.

        The caller supplies the destination after arranging a real teleport.
        This method does not request a game-server join or fabricate one.
        """
        with self.lock:
            queued = self.host_io('teleport_peek')
            destination = type(self)(snapshot, memory_mb=self.memory_mb)
            try:
                self.host_io('teleport_ack', ids=[entry['id'] for entry in queued])
            except BaseException:
                destination.close()
                raise
            destination.teleport_results = []
            try:
                for entry in queued:
                    result = destination.call({'op': 'execute', 'source': entry['source'], 'timeout_ms': 1000})
                    destination.teleport_results.append(result)
            except BaseException:
                destination.close()
                raise
            finally:
                self.close()
            return destination

    def close(self, *, graceful=True):
        with self.lock:
            if self.closed: return
            if graceful and self.process.poll() is None:
                try: self.call({'op': 'close'})
                except Exception: pass
            self.closed = True
            if self.text_service is not None:self.text_service.close()
            if self.process.poll() is None:
                try: self.process.wait(timeout=.5)
                except subprocess.TimeoutExpired:
                    try: stop_group(self.process)
                    except ProcessLookupError: pass
                    try: self.process.wait(timeout=.5)
                    except subprocess.TimeoutExpired:
                        try: stop_group(self.process, force=True)
                        except ProcessLookupError: pass
                        self.process.wait(timeout=1)
            for stream in (self.process.stdin, self.process.stdout):
                if stream: stream.close()
            self.reader.join(timeout=.5)

    def __enter__(self): return self
    def __exit__(self, *_): self.close()


class PersistentClientScriptRuntime:
    def __init__(self, world_provider, user_id, job_id=None, *, background_budget_ms=50,
                 input_capabilities=None, viewport=None, display=None,
                 session_stats_provider=None,
                 allow_visual_mesh_approximation=False):
        if type(background_budget_ms) is not int or not 1 <= background_budget_ms <= 50:
            raise ValueError('Background budget must be 1..50 ms')
        if type(allow_visual_mesh_approximation) is not bool:
            raise ValueError('Visual mesh approximation opt-in must be a boolean')
        self.world_provider, self.user_id, self.job_id = world_provider, user_id, job_id
        self.background_budget_ms = background_budget_ms
        if session_stats_provider is not None and not callable(session_stats_provider):
            raise TypeError('Session statistics provider must be callable')
        self.session_stats_provider = session_stats_provider
        self._session_stats_sent = {}
        self.allow_visual_mesh_approximation = allow_visual_mesh_approximation
        if viewport is not None and not isinstance(viewport, dict):
            raise ValueError('Viewport must be an object')
        # Copy frontend measurements; the native host validates geometry before use.
        self.viewport = json.loads(json.dumps(viewport, allow_nan=False))
        if input_capabilities is not None and not isinstance(input_capabilities, dict):
            raise ValueError('Input capabilities must be an object')
        self.input_capabilities = dict(input_capabilities or {})
        allowed_inputs = {'TouchEnabled', 'KeyboardEnabled', 'MouseEnabled', 'GamepadEnabled',
                          'AccelerometerEnabled', 'GyroscopeEnabled', 'VREnabled'}
        if any(key not in allowed_inputs or type(value) is not bool
               for key, value in self.input_capabilities.items()):
            raise ValueError('Invalid local input capability')
        self.display = validate_display(display) if display is not None else None
        self.host = None
        self.world = None
        self.revision = 0
        self._action_journal = None
        self._action_reader = None
        self.executions = 0
        self.last_poll = 0.
        self.messages = collections.deque(maxlen=128)
        self.results = {}
        self.submitted = {}
        self.stats = {}
        self.failure = None
        self.failure_context = None
        self.movement_preview = None
        self.movement_last_step = None
        self._physics_feedback_token = None
        self.chat_bridge = None
        self.chat_jobs = {}
        self.remote_client_replies = collections.deque()
        self.remote_server_events = collections.deque()
        self.remote_server_calls = collections.deque()
        self.remote_server_results = collections.deque()
        self.active_remote_server_requests = set()
        self.output_sequence = 0
        self.output_events = collections.deque(maxlen=128)
        self.animation_assets = None
        self.animation_outbox = None
        self._engine_pose_pending = None
        self._engine_pose_player_key = None
        self._background_lane = None
        self._background_context = None
        self._background_source_pending = None
        self._background_source_results = {}
        self._background_source_next_token = 1
        self._catalog_catchup_needed = False

    def _record(self, result):
        now=time.monotonic()
        if type(result.get('engine_pose_frames_applied')) is int:
            self.stats['engine_pose_frames_applied'] = (
                self.stats.get('engine_pose_frames_applied',0)+result['engine_pose_frames_applied'])
        if type(result.get('engine_owned_part_frames_applied')) is int:
            self.stats['engine_owned_part_frames_applied'] = (
                self.stats.get('engine_owned_part_frames_applied',0)+result['engine_owned_part_frames_applied'])
        if self.animation_outbox is not None:
            self.animation_outbox.submit(result.get('animation_intents', []))
            self.stats['animation_intents'] = self.animation_outbox.report()
        if result.get('animation_requests'):
            if self.animation_assets is None:
                self.animation_assets = AnimationAssetQueue()
            self.animation_assets.submit(result['animation_requests'])
        if 'runtime_thread_integrity' in result:
            observe_runtime_thread_report(self.host, result['runtime_thread_integrity'])
        if 'runtime_protected_integrity' in result:
            observe_runtime_protected_report(self.host, result['runtime_protected_integrity'])
        if 'script_runtime' in result:
            self.stats['script_runtime'] = {key: value for key, value in result['script_runtime'].items()
                                            if key != 'details'}
        intents=result.get('movement_intents', [])
        authored_parts = getattr(self, 'authored_part_poses', None)
        if authored_parts is not None and self.world is not None:
            authored_parts.observe(self.world, result.get('mutations', []), result.get('world_revision'), now=now)
        if self.world is not None and getattr(self.world, 'instances', None):
            if self.movement_preview is None:
                self.movement_preview=RuntimePhysicsPrediction(
                    allow_visual_mesh_approximation=self.allow_visual_mesh_approximation)
            self.movement_preview.advance(self.world,self.user_id,now,intents,
                                          mutations=result.get('mutations', []),
                                          observed_revision=result.get('world_revision'))
            self._publish_physics_feedback()
        if self.movement_preview is not None:
            result['local_movement']=self.movement_preview.report()
            self.movement_last_step=now
        if 'active_chat_requests' in result:
            active = set(result['active_chat_requests'])
            self.chat_jobs = {key: value for key, value in self.chat_jobs.items() if key in active}
        for request in result.get('chat_requests', []):
            token = request['request_id']
            if token in self.chat_jobs:
                raise RuntimeError('Duplicate runtime chat request')
            self.chat_jobs[token] = {'request': request, 'message_id': None}
        self.messages.append(result)
        replies = result.get('remote_client_replies', [])
        if not isinstance(replies, list) or len(self.remote_client_replies) + len(replies) > 512:
            raise ValueError('Remote callback reply queue exceeds limit')
        self.remote_client_replies.extend(replies)
        events = result.get('remote_server_events', [])
        if not isinstance(events, list) or len(self.remote_server_events) + len(events) > 512:
            raise ValueError('Remote server event queue exceeds limit')
        self.remote_server_events.extend(events)
        calls = result.get('remote_server_calls', [])
        if not isinstance(calls, list) or len(self.remote_server_calls) + len(calls) > 512:
            raise ValueError('Remote server invocation queue exceeds limit')
        self.remote_server_calls.extend(calls)
        if 'active_remote_server_requests' in result:
            self.active_remote_server_requests = set(result['active_remote_server_requests'])
        self.stats.update({key: result[key] for key in ('active_jobs', 'pending_tasks', 'pending_events', 'pending_event_kinds', 'world_revision',
                           'server_actions', 'server_events', 'unresolved_server_events') if key in result})
        self.stats['task_errors_total'] = (self.stats.get('task_errors_total', 0)
                                           + len(result.get('task_errors', [])))
        if self.movement_preview is not None:
            movement=self.movement_preview.report()
            self.stats['movement_intents_accepted']=movement['accepted_intents']
            self.stats['movement_intents_rejected']=movement['rejected_intents']
        if (result.get('output') or result.get('console_events') or result.get('task_errors') or result.get('completed_jobs') or result.get('chat_outcomes')
                or 'job_id' in result or not result.get('ok', True)):
            self.output_sequence += 1
            event = {key: result[key] for key in ('ok', 'output', 'output_truncated',
                     'task_errors', 'completed_jobs', 'job_id', 'state', 'completed',
                     'error', 'error_type', 'values', 'pending_tasks', 'chat_outcomes', 'console_events') if key in result}
            event['sequence'] = self.output_sequence
            self.output_events.append(event)
        result['runtime_sequence'] = self.output_sequence
        for completed in result.get('completed_jobs', []):
            self.results[completed['job_id']] = completed
        if 'job_id' in result:self.submitted[result['job_id']]=result
        # Diagnostics describe the scheduler snapshot attached to this host
        # result. Never carry an older "sleeping" observation forward when a
        # later response omitted that job or omitted diagnostics entirely.
        for job_id, submitted in self.submitted.items():
            if job_id not in self.results:
                submitted.pop('diagnostic', None)
        for diagnostic in result.get('job_diagnostics', []):
            job_id = diagnostic.get('job_id')
            if job_id in self.submitted and job_id not in self.results:
                self.submitted[job_id]['diagnostic'] = diagnostic
        while len(self.results) > 128:
            expired=next(iter(self.results));self.results.pop(expired);self.submitted.pop(expired,None)
        return result

    def _publish_physics_feedback(self):
        """Feed the local predictor's accepted state into Luau on the next turn."""
        if self.host is None or self.movement_preview is None:
            return
        bridge = self.movement_preview.bridge
        controller = bridge.controller
        if controller is None:
            return
        report = self.movement_preview.report().get('controller') or {}
        predicted = report.get('predicted_state') or {}
        state = predicted.get('humanoid_state')
        humanoid_id = getattr(controller, 'humanoid_id', None)
        feedback_journal = getattr(controller, 'feedback_events', None)
        journal = feedback_journal() if callable(feedback_journal) else None
        move_to = journal.get('move_to', []) if isinstance(journal, dict) else []
        move_to_token = tuple((item['sequence'], item['reached']) for item in move_to)
        journal_sequence = journal.get('through_sequence') if isinstance(journal, dict) else None
        if not isinstance(state, dict) or not isinstance(state.get('name'), str):
            token = (humanoid_id, None, report.get('simulated_seconds'), move_to_token, journal_sequence)
            if humanoid_id and token != self._physics_feedback_token:
                feedback = {'humanoid_id': humanoid_id, 'state': None,
                            'transitions': [], 'signals': []}
                if move_to: feedback['move_to'] = move_to
                response = self.host.call({'op': 'physics_feedback',
                                           'feedback': feedback,
                                           'timeout_ms': self.background_budget_ms})
                if not self._accept_physics_feedback(response, 'Could not clear local Humanoid state feedback'):
                    return
                if journal_sequence is not None:
                    # Cleared state makes earlier state edges obsolete. Drain
                    # their bounded prefix too, so they cannot hide a later
                    # movement completion while the body remains suspended.
                    controller.ack_feedback(journal_sequence)
                    token = (humanoid_id, None, report.get('simulated_seconds'), (), journal_sequence)
                self._physics_feedback_token = token
            return
        name = state['name']
        value = state.get('value')
        if type(value) is not int:
            return
        # The live physics trial and this runtime can advance the same
        # prediction on different cadences. A report contains only the most
        # recent step's transitions, so prefer the controller's unacknowledged
        # event journal when available. It is acked only after native feedback
        # succeeds; a stale-character deferral therefore leaves events queued.
        journal_sequence = None
        if isinstance(journal, dict):
            journal_sequence = journal.get('through_sequence')
            transitions = journal.get('transitions', [])
            signals = journal.get('signals', [])
        else:
            transitions = predicted.get('humanoid_state_transitions', [])
            signals = predicted.get('humanoid_state_signals', [])
        transitions = [item for item in transitions if isinstance(item, dict)
                       and isinstance(item.get('name'), str) and type(item.get('value')) is int]
        signals = [item for item in signals if isinstance(item, dict)
                   and item.get('name') in ('Jumping', 'FreeFalling', 'PlatformStanding')
                   and type(item.get('active')) is bool]
        # A live physics tick can land between report() and feedback_events().
        # When the journal contains state edges, its last edge is the stable
        # state for that batch and keeps native transition-chain validation
        # consistent even if report() observed the preceding tick.
        if journal_sequence is not None and transitions:
            state = transitions[-1]
            name = state['name']
            value = state['value']
        velocity = predicted.get('linear_velocity')
        running_speed = None
        motion = None
        if (isinstance(velocity, (list, tuple)) and len(velocity) == 3
                and all(type(v) in (int, float) and math.isfinite(v) for v in velocity)):
            running_speed = math.hypot(velocity[0], velocity[2]) if name in ('Running','RunningNoPhysics') else 0.
            root_id = getattr(controller, 'root_id', None)
            if isinstance(root_id, str) and root_id:
                motion = {'root_id': root_id, 'linear_velocity': list(velocity)}
        token = (humanoid_id, name, value, running_speed, journal_sequence,
                 tuple((item['name'], item['value']) for item in transitions),
                 tuple((item['name'], item['active']) for item in signals))
        movers=journal.get('movers',[]) if isinstance(journal,dict) else []
        mover_token=json.dumps(movers,sort_keys=True,separators=(',',':'))
        motion_token=json.dumps(motion,sort_keys=True,separators=(',',':'))
        token += (mover_token,motion_token,move_to_token)
        if token == self._physics_feedback_token:
            return
        feedback = {'humanoid_id': humanoid_id,
                    'state': {'name': name, 'value': value},
                    'transitions': transitions, 'signals': signals}
        if move_to: feedback['move_to'] = move_to
        if isinstance(journal,dict) and 'movers' in journal:
            feedback['movers']=movers
        if running_speed is not None:
            feedback['running_speed'] = running_speed
        if motion is not None:
            feedback['motion'] = motion
        response = self.host.call({'op': 'physics_feedback', 'feedback': feedback,
                                   'timeout_ms': self.background_budget_ms})
        if not self._accept_physics_feedback(response, 'Could not publish local Humanoid state feedback'):
            return
        if journal_sequence is not None:
            acknowledge = getattr(controller, 'ack_feedback', None)
            if callable(acknowledge):
                acknowledge(journal_sequence)
                # The acknowledged batch will no longer be present on the
                # next read. Cache the post-ack token so that an empty followup
                # does not issue a redundant state-only feedback call.
                token = (humanoid_id, name, value, running_speed, journal_sequence, (), (), mover_token,motion_token,())
        self._physics_feedback_token = token

    def _accept_physics_feedback(self, response, fallback):
        if response.get('ok', False):
            return True
        if response.get('error') in ('physics feedback Humanoid is not the current local character',
                                      'physics feedback root is not the current local character'):
            # Prediction sees the latest receipts while the VM may still be
            # consuming earlier delta chunks. The native guard rejected this
            # obsolete identity without applying anything. Try the current
            # actor again after the next update, without acknowledging it.
            self._physics_feedback_token = None
            self.stats['physics_feedback_deferred'] = self.stats.get('physics_feedback_deferred', 0) + 1
            return False
        raise RuntimeError(response.get('error', fallback))

    def _background_step_budget(self):
        """Choose the test-profile slice from current authored-job states.

        Large local tests need longer turns while authored roots are runnable,
        but a waiting root gains nothing from those turns. Use the ordinary
        background slice only when every currently active root is explicitly
        asleep or waiting. Missing, stale, or mixed diagnostics keep the larger
        slice so runnable authored code retains its existing throughput.
        """
        active_jobs = self.stats.get('active_jobs', 0)
        if os.environ.get('RCR_LUAU_LARGE_TEST') != '1':
            return self.background_budget_ms
        if type(active_jobs) is int and active_jobs == 0:
            return self.background_budget_ms
        if type(active_jobs) is not int or active_jobs < 0:
            return 250

        active_ids = [job_id for job_id in self.submitted if job_id not in self.results]
        if len(active_ids) != active_jobs:
            return 250
        blocked_states = {'sleeping', 'waiting'}
        for job_id in active_ids:
            submitted = self.submitted.get(job_id)
            diagnostic = submitted.get('diagnostic') if isinstance(submitted, dict) else None
            if (not isinstance(diagnostic, dict)
                    or diagnostic.get('job_id') != job_id
                    or diagnostic.get('scheduler_state') not in blocked_states):
                return 250
        return self.background_budget_ms

    def enable_animation_intents(self):
        """Collect authored track operations for a session-owned consumer."""
        if self.animation_outbox is None:
            self.animation_outbox = AnimationIntentOutbox()
        return self.animation_outbox

    def queue_engine_poses(self, character_id, root_id, frames):
        """Coalesce computed local-engine frames, keeping received World intact."""
        if (not isinstance(character_id,str) or not isinstance(root_id,str)
                or not 0<len(character_id)<=256 or not 0<len(root_id)<=256
                or not isinstance(frames,list) or not 0<len(frames)<=32):
            raise ValueError('Invalid local-engine pose batch')
        captured=[]; seen=set()
        for item in frames:
            if not isinstance(item,dict) or not isinstance(item.get('id'),str) or not 0<len(item['id'])<=256 or item['id'] in seen:
                raise ValueError('Invalid local-engine pose target')
            seen.add(item['id'])
            encoded=item.get('cframe')
            frame=encoded.get('$cframe') if isinstance(encoded,dict) else None
            if not isinstance(frame,dict):
                raise ValueError('Invalid local-engine CFrame')
            for name,count in (('position',3),('rotation',9)):
                value=frame.get(name)
                if (not isinstance(value,(list,tuple)) or len(value)!=count
                        or any(type(v) not in (int,float) or not math.isfinite(v) for v in value)):
                    raise ValueError('Invalid local-engine CFrame')
            captured.append({'id':item['id'],'cframe':{'$cframe':{
                'position':list(frame['position']),'rotation':list(frame['rotation'])}}})
        self._engine_pose_pending={'character_id':character_id,'root_id':root_id,'frames':captured}

    def _take_engine_poses(self, world):
        pending=self._engine_pose_pending
        if pending is None:return None
        self._engine_pose_pending=None
        player=world.instances.get(self._engine_pose_player_key)
        if player is None or player.get('properties',{}).get('UserId')!=self.user_id:
            self._engine_pose_player_key=world.avatar(self.user_id).get('player_key')
            player=world.instances.get(self._engine_pose_player_key)
        from world_snapshot import instance_id
        current=player.get('properties',{}).get('Character') if player is not None else None
        if instance_id(current)!=pending['character_id']:
            return None
        # A received removal/reparent can race the previously computed frame.
        # Drop that obsolete batch before it reaches the native atomic updater.
        for identifier in [pending['root_id'], *(item['id'] for item in pending['frames'])]:
            try:
                key=tuple(int(value) for value in identifier.split(':'))
            except ValueError:
                return None
            if len(key)!=2 or instance_id(key)!=identifier:
                return None
            visited=set()
            while key!=current:
                if key in visited or len(visited)>=128:
                    return None
                visited.add(key)
                node=world.instances.get(key)
                if node is None:
                    return None
                key=node.get('parent')
            if current not in world.instances:
                return None
        return pending

    def _session_statistics(self):
        value = self.session_stats_provider() if self.session_stats_provider is not None else {}
        if (not isinstance(value, dict) or set(value) - {'network_rtt_ms'}
                or any(type(v) not in (int, float) or not math.isfinite(v) or v < 0
                       for v in value.values())):
            raise ValueError('Invalid measured session statistics')
        return value.copy()

    def _take_engine_owned_parts(self, world):
        authored = getattr(self, 'authored_part_poses', None)
        if authored is not None and getattr(authored, 'simulation', None) is not None:
            initial = getattr(self.host, 'initial_result', {})
            if not isinstance(initial, dict) or initial.get('engine_owned_part_feedback_version') != 1:
                raise RuntimeError('Owned-part dynamics requires a worker with engine feedback version 1')
        return authored.engine_updates(world) if authored is not None else None

    def _ensure(self):
        if self.failure: raise RuntimeError(self.failure)
        world = self.world_provider()
        if world is None: raise ValueError('Client world is not ready')
        if self.host is None:
            if not hasattr(world, '_script_preparation_queue'):
                world._script_preparation_queue = CodePreparationQueue()
            initial_ready = 13 in world.tags
            snapshot = make_world_snapshot(world, self.user_id, job_id=self.job_id,
                                           include_script_sources=initial_ready)
            if os.environ.get('RCR_BUILTIN_PLAYER_SCRIPTS', '1') == '1':
                from builtin_player_scripts import player_scripts
                snapshot['builtin_player_scripts'] = player_scripts()
            snapshot['chat_enabled'] = self.chat_bridge is not None
            snapshot['session_stats'] = self._session_statistics()
            snapshot['input_capabilities'] = self.input_capabilities.copy()
            if self.display is not None:
                snapshot['display'] = validate_display(self.display)
            if self.viewport is not None:
                snapshot['viewport'] = json.loads(json.dumps(self.viewport))
            if not snapshot['local_player'] or not snapshot['services']:
                raise ValueError('Local Player and services must be replicated before starting persistent scripts')
            try:
                self.host = PersistentLuauHost(snapshot)
            except BaseException:
                world._script_preparation_queue.close()
                del world._script_preparation_queue
                raise
            self.world = world
            self._session_stats_sent = snapshot['session_stats']
            self.revision = snapshot['sequence']
            if hasattr(world.actions, 'retain'):
                self._action_journal = world.actions
                self._action_reader = self._action_journal.retain(self.revision)
            self.script_catalog = snapshot.get('script_sources', {})
            self.initial_replication_complete = snapshot.get('initial_replication_complete', False)
            self.shared_entry_count = len(getattr(world, 'shared', {}))
            self._record(self.host.initial_result)
        elif world is not self.world:
            raise RuntimeError('World schema changed; the existing persistent VM cannot be reused')
        return world

    def update_input_capabilities(self, capabilities):
        """Attach/detach frontend devices on the session owner thread."""
        allowed = {'TouchEnabled', 'KeyboardEnabled', 'MouseEnabled', 'GamepadEnabled',
                   'AccelerometerEnabled', 'GyroscopeEnabled', 'VREnabled'}
        if (not isinstance(capabilities, dict) or any(
                key not in allowed or type(value) is not bool
                for key, value in capabilities.items())):
            raise ValueError('Invalid local input capability')
        capabilities = dict(capabilities)
        if self.host is None:
            self.input_capabilities.update(capabilities)
            return {'ok': True, 'values': [], 'state': 'pending_runtime'}
        self.synchronize(timeout_ms=20)
        result = self.host.call({'op': 'update', 'timeout_ms': 20, 'delta': {
            'version': 1, 'delta': True, 'actions': [],
            'input_capabilities': capabilities}})
        self._record(result)
        if result.get('ok'):
            self.input_capabilities.update(capabilities)
        return result

    def deliver_input(self, events):
        """Deliver keyboard/mouse/touch events on the session owner thread.

        Device capabilities must be enabled in the runtime configuration.
        InputObject positions and full-screen mouse_location are separate;
        the frontend is responsible for supplying its coordinate transforms.
        """
        if not isinstance(events, list) or len(events) > 128:
            raise ValueError('input batch must be a list of at most 128 events')
        self.synchronize(timeout_ms=20)
        result = self.host.call({'op': 'update', 'timeout_ms': 20, 'delta': {
            'version': 1, 'delta': True, 'actions': [], 'input_events': events}})
        self._record(result)
        return result

    def update_display(self, profile):
        """Called on the session owner thread, serialized with source execution."""
        profile = validate_display(profile)
        if self.host is None:
            self.display = profile
            return {'ok': True, 'display': profile, 'state': 'pending_runtime'}
        if profile == self.display:
            return {'ok': True, 'display': profile, 'state': 'unchanged'}
        self.synchronize(timeout_ms=20)
        result = self.host.call({'op': 'update', 'timeout_ms': 20, 'delta': {
            'version': 1, 'delta': True, 'actions': [], 'display': profile}})
        self._record(result)
        if result.get('ok'):
            self.display = profile
        return {'ok': result.get('ok', False), 'display': self.display,
                **({'error': result['error']} if 'error' in result else {})}

    @staticmethod
    def _is_script_class(name):
        return name in ('Script', 'LocalScript', 'ModuleScript')

    def _catalog_needs_refresh(self, world, delta, initial_ready):
        if initial_ready != self.initial_replication_complete:
            return True
        if not initial_ready:
            return False
        if len(getattr(world, 'shared', {})) != self.shared_entry_count:
            return True
        queue = getattr(world, '_script_preparation_queue', None)
        if queue and any(future.done() for _, future in queue.pending.values()):
            return True
        for action in delta.get('actions', ()):
            kind = action.get('kind')
            if kind == 'create':
                node = action.get('node', {})
                if (self._is_script_class(node.get('class_name'))
                        or {'Source', 'Bytecode', 'ScriptBytecode'} & set(node.get('properties', {}))):
                    return True
            elif kind == 'set' and (action.get('script_code_changed') or {'Source', 'Bytecode', 'ScriptBytecode'} & set(action.get('fields', {}))):
                return True
            elif kind == 'remove' and action.get('id') in self.script_catalog:
                return True
        return False

    def _acknowledge_world(self, revision):
        self.revision = revision
        if self._action_journal is not None:
            self._action_journal.acknowledge(self._action_reader, revision)
            self.stats['world_replay_retention'] = self._action_journal.retention_stats()

    def _world_update(self, world):
        delta = make_world_delta(world, self.revision, max_actions=2048, max_bytes=4*1024*1024)
        statistics = self._session_statistics()
        if statistics != self._session_stats_sent:
            delta['session_stats'] = statistics
        caught_up = delta['sequence'] >= world.revision
        initial_ready = self.initial_replication_complete or (13 in world.tags and caught_up)
        refresh = caught_up and (self._catalog_catchup_needed or
                                self._catalog_needs_refresh(world, delta, initial_ready))
        if not caught_up:
            self._catalog_catchup_needed = True
        return delta, initial_ready, refresh, caught_up

    def synchronize(self, *, timeout_ms=20):
        # Foreground scripts see all preceding receipts. Background polling
        # uses one bounded chunk per transaction to keep network I/O responsive.
        result = None
        while True:
            current = self._synchronize_once(timeout_ms=timeout_ms)
            if current is not None:
                result = current
            if self.world.revision <= self.revision:
                return result

    def _synchronize_once(self, *, timeout_ms=20):
        self._finish_background(wait=True)
        world = self._ensure()
        delta, initial_ready, refresh_catalog, caught_up = self._world_update(world)
        animations = self.animation_assets.drain() if self.animation_assets else []
        engine_poses=self._take_engine_poses(world) if caught_up else None
        engine_owned_parts=self._take_engine_owned_parts(world) if caught_up else None
        if not delta['actions'] and 'session_stats' not in delta and not refresh_catalog and not animations and engine_poses is None and engine_owned_parts is None and initial_ready == self.initial_replication_complete:
            return None
        if engine_poses is not None:
            delta['engine_poses']=engine_poses
        if engine_owned_parts is not None:
            delta['engine_owned_part_updates']=engine_owned_parts
        if animations:
            delta['animation_responses'] = animations
        catalog = script_catalog(world) if refresh_catalog else self.script_catalog
        delta['initial_replication_complete'] = initial_ready
        if catalog != self.script_catalog:
            delta['script_sources'] = catalog
        result = self._record(self.host.call({'op': 'update', 'delta': delta,
                                              'timeout_ms': min(50, max(1, timeout_ms))}))
        if not result['ok']: raise RuntimeError(result.get('error', 'Persistent world update failed'))
        if 'session_stats' in delta:
            self._session_stats_sent = delta['session_stats']
        self._acknowledge_world(delta['sequence'])
        if caught_up:
            self._catalog_catchup_needed = False
        self.script_catalog = catalog
        self.initial_replication_complete = initial_ready
        self.shared_entry_count = len(getattr(world, 'shared', {}))
        return result

    def render_frame(self):
        # The owner thread supplies a read-only VM projection. No per-frame
        # Luau jobs, script catalog copies, or replay-journal snapshots.
        self.synchronize(timeout_ms=10)
        return self.host.call({'op':'render'})

    def gui_action(self, action):
        self.synchronize(timeout_ms=10)
        return self._record(self.host.call({'op':'gui_action','action':action,'timeout_ms':20}))

    def execute(self, source, *, timeout_ms=1000):
        if not isinstance(source, str) or len(source.encode('utf-8')) > MAX_SOURCE_BYTES:
            raise ValueError('Source must be UTF-8 Luau text up to 100 MiB')
        if type(timeout_ms) is not int or not 1 <= timeout_ms <= 2000:
            raise ValueError('CPU step budget must be 1..2000 ms')
        self.synchronize()
        result = self._record(self.host.call({'op': 'execute', 'source': source, 'timeout_ms': timeout_ms}))
        self.executions += 1
        return {**result, 'execution': self.executions, 'runtime_backend': 'rust-persistent',
                'world_snapshot': False, 'mutations_replicated_to_server': False}

    def execute_user_chat(self, text, channel='RBXGeneral'):
        """Run the dedicated UI's typed submission through ordinary Luau.

        The local input UI calls this host operation; it is not exposed in Luau
        or accepted as a flag on ordinary execute/send requests. A script typed
        into the code editor is not itself a user-entered chat message.
        """
        from chat_input import user_chat_source
        source = user_chat_source(text, channel)
        bridge = self.chat_bridge
        if bridge is None or bridge.failure or not bridge.chat.ready_sent:
            raise ValueError('The chat service is not ready')
        if channel == 'RBXGlobal':
            bridge.chat.require_global_route()
            target = bridge.chat.service
            route = 'universe'
        else:
            target = bridge.chat._channel('RBXGeneral')
            bridge.chat._source(target)  # Enforce received permissions first.
            route = 'channel'
        target_id = '%d:%d' % target
        result = self.execute(source)
        candidates = [r for r in result.get('chat_requests', [])
                      if r.get('text') == text and r.get('metadata', '') == '']
        matching = [r for r in candidates
                    if r.get('channel_id') == target_id
                    and r.get('route', 'channel') == route]
        if not result.get('ok') or len(candidates) != 1 or len(matching) != 1:
            # Do not let an ambiguous UI operation fall through to an ordinary
            # post. The coroutine still receives a real local error next tick.
            for request in candidates:
                job = self.chat_jobs.get(request.get('request_id'))
                if job is not None:
                    job['local_error'] = 'Could not bind the typed chat input'
            raise ValueError('Could not bind the typed chat input to one SendAsync call')
        try:
            bridge.chat.record_typed_submission(matching[0])
        except Exception:
            self.chat_jobs[matching[0]['request_id']]['local_error'] = 'Could not bind the typed chat input'
            raise
        return result

    def poll(self, *, force=False):
        completed = self._finish_background(wait=True)
        if completed is not None:
            return completed
        now = time.monotonic()
        if self.failure or (not force and now-self.last_poll < .05): return None
        if self.host is None:
            world=self.world_provider()
            if (world is None or world.avatar(self.user_id)['player_key'] is None
                    or not any(key in world.top_containers for key in world.instances)):
                return None
            try:self._ensure()
            except Exception as error:
                self.failure=str(error)
                raise
            self.last_poll=now
            return self.host.initial_result
        self.last_poll = now
        try:
            if self.remote_server_results:
                responses = [self.remote_server_results.popleft()
                             for _ in range(min(128, len(self.remote_server_results)))]
                return self._record(self.host.call({'op': 'remote_server_results',
                    'responses': responses, 'timeout_ms': min(20, self.background_budget_ms)}))
            # Resolve already-pending chat work first. It is a real adapter
            # request and cannot be deferred merely because a world delta is
            # also waiting; the chat operation itself advances the scheduler.
            chat_result = self._poll_chat(timeout_ms=min(20, self.background_budget_ms))
            if chat_result is not None:
                return chat_result
            synchronized = self.synchronize(timeout_ms=self.background_budget_ms)
            if synchronized is not None:
                # update already runs the scheduler; a second poll in the same
                # network tick spends CPU without adding another fair slice.
                return synchronized
            return self._record(self.host.call({'op': 'poll',
                                                'timeout_ms': self.background_budget_ms}))
        except Exception as error:
            self.failure = str(error)
            self.failure_context = getattr(self.host, 'failure_context', None) or self.failure_context
            self.close()
            raise

    def _prepare_chat_responses(self):
        responses = []
        for token, job in list(self.chat_jobs.items()):
            try:
                if job.get('local_error'):
                    raise ValueError(job['local_error'])
                if self.chat_bridge is None:
                    raise RuntimeError('Chat bridge is disabled')
                if job['message_id'] is None:
                    if not self.chat_bridge.chat.ready_sent and not self.chat_bridge.failure:
                        continue
                    # Submission occurs once on the network event loop. An
                    # ambiguous send failure is surfaced, never retried.
                    result = self.chat_bridge.runtime_submit(job['request'])
                    job['message_id'] = result['message_id']
                result = self.chat_bridge.runtime_status(job['message_id'])
                if result['state'] == 'submitted':
                    continue
                if result['state'] in ('server_success', 'server_rejected', 'server_status_unknown') and 'response' in result:
                    responses.append({'request_id': token, 'message': result['response']})
                else:
                    responses.append({'request_id': token, 'error': delivery_error(result)})
            except Exception as error:
                responses.append({'request_id': token, 'error': str(error)[:1024]})
            self.chat_jobs.pop(token, None)
        return responses

    def _poll_chat(self, *, timeout_ms=20):
        responses = self._prepare_chat_responses()
        if responses:
            result = self.host.call({'op': 'chat', 'responses': responses,
                                     'timeout_ms': min(50, max(1, timeout_ms))})
            result['chat_outcomes'] = [{'request_id': item['request_id'],
                                       'status': item['message']['status'] if 'message' in item else None,
                                       'error': item.get('error')} for item in responses]
            self._record(result)
            if not result['ok']:
                raise RuntimeError(result.get('error', 'Runtime chat completion failed'))
            return result
        return None

    def _finish_background(self, *, wait=False):
        if self._background_context is None:
            return None
        context = self._background_context
        try:
            result = self._background_lane.wait() if wait else self._background_lane.take()
        except Exception:
            self._background_context = None
            raise
        if result is None:
            return None
        self._background_context = None
        if not result['ok'] and context['op'] != 'source':
            self.failure_context = result.get('runtime_failure_context')
            raise RuntimeError(result.get('error', 'Persistent background operation failed'))
        if context['op'] == 'chat':
            result['chat_outcomes'] = context['outcomes']
        self._record(result)
        if context['op'] == 'source':
            # The worker only performs the RPC. World, movement, animation and
            # result bookkeeping remain on the owner thread in _record above.
            self._background_source_results[context['token']] = dict(result)
            while len(self._background_source_results) > 8:
                self._background_source_results.pop(next(iter(self._background_source_results)))
        if context['op'] == 'update':
            if 'session_stats' in context:
                self._session_stats_sent = context['session_stats']
            # Only acknowledge the captured revision. The network thread can
            # have received more actions while this immutable command ran.
            self._acknowledge_world(context['revision'])
            self.script_catalog = context['catalog']
            self.initial_replication_complete = context['initial_ready']
            self.shared_entry_count = context['shared_entry_count']
            if context.get('catalog_caught_up', True):
                self._catalog_catchup_needed = False
        return result

    def queue_background_source(self, source, *, label='<background-source>', timeout_ms=250):
        """Queue one short Luau source job for the background RPC lane.

        The source is copied into an immutable request and will run after any
        earlier receipt/chat transaction. This never calls the host on the
        caller thread. A full single-flight lane is reported as None so the
        owner can retain and retry its transition.
        """
        if not isinstance(source, str) or not source.strip() or len(source.encode('utf-8')) > MAX_SOURCE_BYTES:
            raise ValueError('Background source must be nonempty UTF-8 Luau within the source limit')
        if type(label) is not str or not label or len(label) > 128:
            raise ValueError('Background source label must be a short nonempty string')
        if type(timeout_ms) is not int or not 1 <= timeout_ms <= 2000:
            raise ValueError('Background source timeout must be 1..2000 ms')
        if self.failure or self.host is None:
            return None
        if self._background_source_pending is not None:
            return None
        if self._background_context is not None and self._background_context.get('op') == 'source':
            return None
        if self._background_source_results:
            # Do not overwrite an undelivered transition result.
            return None
        token = self._background_source_next_token
        self._background_source_next_token += 1
        world = self._ensure()
        watermark = getattr(world, 'revision', self.revision)
        character_key = None
        try:
            if world is not None:
                character_key = world.avatar(self.user_id).get('character_key')
        except (AttributeError, KeyError, TypeError, ValueError):
            pass
        self._background_source_pending = {
            'token': token, 'source': source, 'label': label,
            'timeout_ms': timeout_ms, 'watermark': watermark,
            'character_key': character_key,
        }
        return token

    def take_background_source_result(self, token):
        """Take a completed background source result on the owner thread."""
        if type(token) is not int or token < 1:
            raise ValueError('Background source token must be a positive integer')
        return self._background_source_results.pop(token, None)

    def poll_background(self):
        """Advance one host transaction without waiting on the network loop.

        World inspection, chat/network work, and result delivery stay on the
        caller. The sole background lane receives an immutable command copy.
        Events remain receipt ordered; an in-flight delta is never replaced.
        Foreground execution drains it through synchronize() first.
        """
        if self.failure:
            return None
        completed = None
        try:
            if self._background_context is not None:
                completed = self._finish_background()
                if completed is None or not self.remote_server_results:
                    return completed
                # The previous transaction has released the sole host lane.
                # Deliver its result below while handing the free lane to an
                # already-queued server reply, without another idle tick.
            if self.host is None:
                # Initialization happens once, before any asynchronous calls.
                return self.poll()
            now = time.monotonic()
            if completed is None and now-self.last_poll < .05:
                return None
            self.last_poll = now
            budget = self._background_step_budget()
            responses = [] if self.remote_server_results else self._prepare_chat_responses()
            if self.remote_server_results:
                command = {'op': 'remote_server_results', 'responses': [
                    self.remote_server_results.popleft()
                    for _ in range(min(128, len(self.remote_server_results)))],
                    'timeout_ms': min(20, self.background_budget_ms)}
                context = {'op': 'remote_server_results'}
            elif responses:
                command = {'op': 'chat', 'responses': responses,
                           'timeout_ms': min(20, self.background_budget_ms)}
                context = {'op': 'chat', 'outcomes': [
                    {'request_id': item['request_id'],
                     'status': item['message']['status'] if 'message' in item else None,
                     'error': item.get('error')} for item in responses]}
            elif self.stats.get('pending_events', 0) >= 512:
                # Keep receiving packets on the network thread, but let the
                # VM drain callbacks before feeding it another delta batch.
                # The receipt watermark is unchanged: queued world actions
                # are applied in their original order after pressure falls.
                command = {'op': 'poll', 'timeout_ms': budget}
                context = {'op': 'poll'}
            else:
                world = self._ensure()
                pending = self._background_source_pending
                if (pending is not None
                        and self.revision >= pending['watermark']):
                    # Run at the source's captured receipt watermark, before
                    # consuming newer property deltas, animation responses or
                    # engine poses. Those remain queued for the next update.
                    # This prevents a continuous pose stream from starving a
                    # one-shot animation transition.
                    self._background_source_pending = None
                    current_character_key = None
                    try:
                        current_character_key = world.avatar(self.user_id).get('character_key')
                    except (AttributeError, KeyError, TypeError, ValueError):
                        pass
                    if (pending['character_key'] is not None
                            and current_character_key != pending['character_key']):
                        # The driver binding changed while this job waited for
                        # the receipt watermark. Surface a normal transition
                        # failure instead of playing stale role assets.
                        self._background_source_results[pending['token']] = {
                            'ok': False, 'values': [], 'error_type': 'StaleCharacter'}
                        return None
                    command = {'op': 'execute', 'source': pending['source'],
                               'timeout_ms': pending['timeout_ms']}
                    context = {'op': 'source', 'token': pending['token'],
                               'label': pending['label'],
                               'watermark': pending['watermark'],
                               'character_key': pending['character_key']}
                else:
                    delta, initial_ready, refresh, caught_up = self._world_update(world)
                    revision = delta['sequence']
                    animations = self.animation_assets.drain() if self.animation_assets else []
                    engine_poses=self._take_engine_poses(world) if caught_up else None
                    engine_owned_parts=self._take_engine_owned_parts(world) if caught_up else None
                    needs_update = (bool(delta['actions']) or refresh or bool(animations)
                                    or 'session_stats' in delta
                                    or engine_poses is not None
                                    or engine_owned_parts is not None
                                    or initial_ready != self.initial_replication_complete)
                    if needs_update:
                        if engine_poses is not None:
                            delta['engine_poses']=engine_poses
                        if engine_owned_parts is not None:
                            delta['engine_owned_part_updates']=engine_owned_parts
                        if animations:
                            delta['animation_responses'] = animations
                        catalog = script_catalog(world) if refresh else self.script_catalog
                        delta['initial_replication_complete'] = initial_ready
                        if catalog != self.script_catalog:
                            delta['script_sources'] = catalog
                        command = {'op': 'update', 'delta': delta,
                                   'timeout_ms': budget}
                        context = {'op': 'update', 'revision': revision,
                                   'catalog': catalog, 'initial_ready': initial_ready,
                                   'catalog_caught_up': caught_up,
                                   'shared_entry_count': len(getattr(world, 'shared', {}))}
                        if 'session_stats' in delta:
                            context['session_stats'] = delta['session_stats']
                    else:
                        command = {'op': 'poll', 'timeout_ms': budget}
                        context = {'op': 'poll'}
            if self._background_lane is None:
                self._background_lane = RuntimeRpcLane(self.host.call)
            # `command` is freshly built from JSON-only snapshots/deltas and
            # immutable catalog/animation/chat values. Keep it untouched until
            # completion so the lane can avoid a main-thread deepcopy.
            if not self._background_lane.submit_owned(command):
                if context.get('op') == 'source':
                    self._background_source_pending = {
                        'token': context['token'],
                        'source': command['source'],
                        'timeout_ms': command['timeout_ms'],
                        'label': context['label'],
                        'watermark': context['watermark'],
                        'character_key': context['character_key'],
                    }
                    return None
                raise RuntimeError('Background runtime transaction overlap')
            self._background_context = context
            return completed
        except Exception as error:
            self.failure = str(error)
            self.failure_context = getattr(self.host, 'failure_context', None) or self.failure_context
            self.close()
            raise

    def close(self):
        if self._action_journal is not None:
            self._action_journal.release(self._action_reader)
            self._action_journal = self._action_reader = None
        if self._background_lane is not None:self._background_lane.close()
        self._background_context = None
        self._background_source_pending = None
        self._background_source_results.clear()
        self._engine_pose_pending = None
        if self.animation_assets is not None:self.animation_assets.close()
        if self.movement_preview is not None:self.movement_preview.close()
        if self.host is not None: self.host.close()
        if self.world is not None and hasattr(self.world, '_script_preparation_queue'):
            self.world._script_preparation_queue.close()
            del self.world._script_preparation_queue

    def events(self, after=0):
        if type(after) is not int or after < 0:
            raise ValueError('Invalid runtime output cursor')
        reset = after > self.output_sequence
        if reset: after = 0
        overrun = bool(self.output_events and after < self.output_events[0]['sequence']-1)
        batch, size = [], 0
        for event in self.output_events:
            if event['sequence'] <= after: continue
            length = len(json.dumps(event, ensure_ascii=True).encode())
            if batch and size + length > 512*1024: break
            batch.append(event);size += length
        cursor = batch[-1]['sequence'] if batch else self.output_sequence
        return {'ok': True, 'values': [], 'events': batch, 'cursor': cursor,
                'reset': reset, 'overrun': overrun, 'pending_tasks': self.stats.get('pending_tasks', 0)}

    def status(self, job_id, *, advance=True):
        if type(job_id) is not int or job_id<1:raise ValueError('job_id must be a positive integer')
        if type(advance) is not bool:raise ValueError('advance must be a boolean')
        if advance:
            self.poll(force=True)
        result=self.results.get(job_id) or self.submitted.get(job_id)
        if result is None:raise ValueError('Unknown or expired script job')
        if self.failure and job_id not in self.results:
            # The last successful poll is historical once the host has failed.
            # Do not present its cached "ready" stack as a still-running job.
            return {'ok':False,'job_id':job_id,'completed':True,'state':'failed',
                    'values':[],'persistent':True,'runtime_backend':'rust-persistent',
                    'error_type':'RuntimeStopped',
                    'error':f'Runtime stopped before job completion was observed: {self.failure}'}
        response={**result,'persistent':True,'runtime_backend':'rust-persistent'}
        response['queue_diagnostic']={key:self.stats[key] for key in
                                     ('pending_events','pending_tasks','pending_event_kinds') if key in self.stats}
        if self.movement_preview is not None:response['local_movement']=self.movement_preview.report()
        return response
