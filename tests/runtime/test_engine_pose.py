"""Atomic local engine-pose overlays in the persistent Luau host."""
import copy
import json
import os
from pathlib import Path
import sys
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'scripts'))
import persistent_runtime


IDENTITY = [1, 0, 0, 0, 1, 0, 0, 0, 1]
def pose(position):
    return {'$cframe': {'position': list(position), 'rotation': IDENTITY}}


class EnginePoseTests(unittest.TestCase):
    def setUp(self):
        self.snapshot = json.loads((ROOT / 'tests/fixtures/world_snapshot.json').read_text())
        self.snapshot['nodes'].append({
            'id': 'scope:torso-1', 'class_name': 'Part', 'name': 'UpperTorso',
            'parent': 'scope:character-1',
            'properties': {'CFrame': pose((1, 4, 3)), 'Anchored': False},
        })
        host_path = Path(os.environ.get('ENGINE_POSE_PERSISTENT_HOST',
                                       os.environ.get('CFRAME_PERSISTENT_HOST',
                                                      str(ROOT / 'rust-client/target/release/headless-luau'))))
        with mock.patch.object(persistent_runtime, 'RUST_HOST', host_path):
            self.host = persistent_runtime.PersistentLuauHost(copy.deepcopy(self.snapshot))
        self.addCleanup(self.host.close)

    def execute(self, source):
        result = self.host.call({'op': 'execute', 'source': source, 'timeout_ms': 1000})
        self.assertTrue(result['ok'], result)
        self.assertFalse(result['task_errors'], result)
        return result

    def delta(self, *frames):
        return {'version': 1, 'delta': True, 'actions': [], 'engine_poses': {
            'character_id': 'scope:character-1', 'root_id': 'scope:root-1',
            'frames': [{'id': node_id, 'cframe': cf} for node_id, cf in frames],
        }}

    def test_welded_accessory_follows_its_posed_part(self):
        snapshot = copy.deepcopy(self.snapshot)
        snapshot['class_parents'].update({'Weld': 'JointInstance', 'JointInstance': 'Instance',
                                          'Accessory': 'Accoutrement', 'Accoutrement': 'Instance'})
        snapshot['nodes'] += [
            {'id': 'scope:hat-1', 'class_name': 'Accessory', 'name': 'Hat', 'parent': 'scope:character-1', 'properties': {}},
            {'id': 'scope:handle-1', 'class_name': 'Part', 'name': 'Handle', 'parent': 'scope:hat-1',
             'properties': {'CFrame': pose((1, 6, 3)), 'Anchored': False}},
            {'id': 'scope:weld-1', 'class_name': 'Weld', 'name': 'AccessoryWeld', 'parent': 'scope:handle-1',
             'properties': {'Part0': {'$ref': 'scope:handle-1'}, 'Part1': {'$ref': 'scope:torso-1'},
                            'C0': pose((0, -2, 0)), 'C1': pose((0, 0, 0)), 'Enabled': True}},
        ]
        host_path = Path(os.environ.get('ENGINE_POSE_PERSISTENT_HOST',
                                       os.environ.get('CFRAME_PERSISTENT_HOST',
                                                      str(ROOT / 'rust-client/target/release/headless-luau'))))
        with mock.patch.object(persistent_runtime, 'RUST_HOST', host_path):
            host = persistent_runtime.PersistentLuauHost(snapshot)
        self.addCleanup(host.close)
        # The torso moves by (+6, +4, +6); the welded Handle keeps its offset.
        result = host.call({'op': 'update', 'delta': self.delta(
            ('scope:root-1', pose((4, 5, 6))), ('scope:torso-1', pose((7, 8, 9)))), 'timeout_ms': 100})
        self.assertTrue(result['ok'], result)
        self.assertEqual(result['engine_pose_frames_applied'], 2)
        self.assertEqual(result['engine_pose_frames_carried'], 1)
        moved = host.call({'op': 'execute', 'timeout_ms': 1000, 'source': '''
local h=game:GetService("Players").LocalPlayer.Character.Hat.Handle
return h.Position.X,h.Position.Y,h.Position.Z
'''})
        self.assertTrue(moved['ok'], moved)
        self.assertEqual(moved['values'], [7, 10, 9])

    def test_batch_is_visible_before_pose_property_callbacks_and_stays_local(self):
        self.execute('''
getgenv().root=game:GetService("Players").LocalPlayer.Character.HumanoidRootPart
getgenv().torso=game:GetService("Players").LocalPlayer.Character.UpperTorso
getgenv().pose_events=0
root:GetPropertyChangedSignal("CFrame"):Connect(function()
  pose_events+=1
  assert(torso.Position.X==7 and torso.Position.Y==8 and torso.Position.Z==9)
end)
''')
        result = self.host.call({'op': 'update', 'delta': self.delta(
            ('scope:root-1', pose((4, 5, 6))), ('scope:torso-1', pose((7, 8, 9)))),
            'timeout_ms': 100})
        self.assertTrue(result['ok'], result)
        self.assertEqual(result['engine_pose_status'], 'applied')
        self.assertEqual(result['engine_pose_frames_applied'], 2)
        self.assertEqual(result['mutations'], [])
        self.assertEqual(result['server_actions'], 0)
        self.assertEqual(result['world_revision'], self.snapshot['sequence'])
        self.assertEqual(result['pending_events'], 0)
        self.execute('''
assert(pose_events==1)
assert(root.Position.X==4 and root.Position.Y==5 and root.Position.Z==6)
assert(client.propertyOrigin(root,"CFrame")=="local")
assert(torso.Position.X==7 and torso.Position.Y==8 and torso.Position.Z==9)
''')
        received = self.host.call({'op': 'update', 'delta': {
            'version': 1, 'delta': True, 'actions': [{
                'sequence': self.snapshot['sequence'] + 1, 'kind': 'set',
                'id': 'scope:root-1', 'fields': {'CFrame': pose((1, 2, 3))},
            }]}, 'timeout_ms': 100})
        self.assertTrue(received['ok'], received)
        self.execute('''
assert(pose_events==2)
assert(root.Position.X==1 and root.Position.Y==2 and root.Position.Z==3)
assert(client.propertyOrigin(root,"CFrame")=="received")
''')

    def test_foreign_target_rejects_entire_batch_without_events_or_partial_pose(self):
        self.snapshot['nodes'] += [
            {'id': 'foreign-character', 'class_name': 'Model', 'name': 'OtherCharacter',
             'parent': 'scope:workspace', 'properties': {}},
            {'id': 'foreign-root', 'class_name': 'Part', 'name': 'HumanoidRootPart',
             'parent': 'foreign-character', 'properties': {'CFrame': pose((20, 0, 0))}},
        ]
        self.host.close()
        host_path = Path(os.environ.get('ENGINE_POSE_PERSISTENT_HOST',
                                       os.environ.get('CFRAME_PERSISTENT_HOST',
                                                      str(ROOT / 'rust-client/target/release/headless-luau'))))
        with mock.patch.object(persistent_runtime, 'RUST_HOST', host_path):
            self.host = persistent_runtime.PersistentLuauHost(copy.deepcopy(self.snapshot))
        self.execute('''
getgenv().root=game:GetService("Players").LocalPlayer.Character.HumanoidRootPart
getgenv().events=0
root:GetPropertyChangedSignal("CFrame"):Connect(function()events+=1 end)
''')
        result = self.host.call({'op': 'update', 'delta': self.delta(
            ('scope:root-1', pose((99, 99, 99))), ('foreign-root', pose((21, 0, 0)))),
            'timeout_ms': 100})
        self.assertFalse(result['ok'], result)
        self.execute('''
assert(events==0)
assert(root.Position.X==1 and root.Position.Y==2 and root.Position.Z==3)
''')

    def test_rejects_wrong_character_duplicate_and_missing_root_atomically(self):
        baseline = {'version': 1, 'delta': True, 'actions': [], 'engine_poses': {
            'character_id': 'foreign-character', 'root_id': 'scope:root-1',
            'frames': [{'id': 'scope:root-1', 'cframe': pose((8, 8, 8))}],
        }}
        stale = self.host.call({'op': 'update', 'delta': baseline, 'timeout_ms': 100})
        self.assertTrue(stale['ok'], stale)
        self.assertEqual(stale['engine_pose_status'], 'stale_character')
        for invalid in [self.delta(('scope:root-1', pose((8, 8, 8))),
                       ('scope:root-1', pose((9, 9, 9)))),
            self.delta(('scope:torso-1', pose((9, 9, 9))))]:
            result = self.host.call({'op': 'update', 'delta': invalid, 'timeout_ms': 100})
            self.assertFalse(result['ok'], result)
        self.execute('''
local root=game:GetService("Players").LocalPlayer.Character.HumanoidRootPart
assert(root.Position.X==1 and root.Position.Y==2 and root.Position.Z==3)
''')

    def test_stale_character_pose_is_skipped_after_respawn_update(self):
        result = self.host.call({'op': 'update', 'delta': {
            'version': 1, 'delta': True,
            'actions': [{'sequence': self.snapshot['sequence'] + 1, 'kind': 'set',
                         'id': 'scope:player-1', 'fields': {'Character': None}}],
            'engine_poses': self.delta(('scope:root-1', pose((8, 8, 8))))['engine_poses'],
        }, 'timeout_ms': 100})
        self.assertTrue(result['ok'], result)
        self.assertEqual(result['engine_pose_status'], 'stale_character')
        self.assertEqual(result['engine_pose_frames_applied'], 0)
        self.assertEqual(result['server_actions'], 1)
