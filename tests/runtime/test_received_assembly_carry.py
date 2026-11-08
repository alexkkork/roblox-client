"""Received root poses carry rigidly joined members (remote characters).

Physics replication for other players sends only the assembly root's CFrame.
Joined parts must follow it, keeping their solved offsets, unless the same
received batch gives them their own CFrame.
"""
import copy
import unittest

import test_cframe_transforms as transforms
import test_joint_poses  # module import: keeps its TestCase out of this file's discovery
from test_part_transforms import pose

ROT_Y_90 = (0, 0, 1, 0, 1, 0, -1, 0, 0)  # row-major, +90 degrees about Y


class ReceivedAssemblyCarryTests(unittest.TestCase):
    host = test_joint_poses.JointPoseTests.host
    execute = test_joint_poses.JointPoseTests.execute
    partial_snapshot = test_joint_poses.JointPoseTests.partial_snapshot

    def receive(self, h, *changes):
        # Sequences must keep increasing across batches; stale ones are ignored.
        self.sequence = getattr(self, 'sequence', transforms.SNAPSHOT['sequence'])
        actions = []
        for node, fields in changes:
            self.sequence += 1
            actions.append({'sequence': self.sequence, 'kind': 'set', 'id': node, 'fields': fields})
        result = h.call({'op': 'update', 'delta': {'version': 1, 'delta': True, 'actions': actions}})
        self.assertTrue(result['ok'], result)
        self.assertFalse(result.get('joint_pose_errors'), result)

    def rig(self, extra_nodes=()):
        data = self.partial_snapshot()
        data['nodes'] += list(extra_nodes)
        h = self.host(data)
        self.execute(h, 'getgenv().a=workspace.PA;getgenv().b=workspace.PB')
        return h

    def test_received_root_move_carries_joined_part(self):
        h = self.rig()
        self.receive(h, ('pa', {'CFrame': pose((10, 0, 0))}))
        self.execute(h, 'vec(a.Position,Vector3.new(10,0,0));vec(b.Position,Vector3.new(14,0,0))')

    def test_received_root_rotation_rotates_joined_offset(self):
        h = self.rig()
        self.receive(h, ('pa', {'CFrame': pose((0, 0, 0), ROT_Y_90)}))
        self.execute(h, 'vec(b.Position,(a.CFrame*CFrame.new(4,0,0)).Position)')

    def test_member_with_its_own_received_pose_keeps_it(self):
        h = self.rig()
        self.receive(h, ('pa', {'CFrame': pose((10, 0, 0))}), ('pb', {'CFrame': pose((0, 5, 0))}))
        self.execute(h, 'vec(a.Position,Vector3.new(10,0,0));vec(b.Position,Vector3.new(0,5,0))')

    def test_received_pose_on_non_root_member_does_not_move_the_root(self):
        h = self.rig()
        self.receive(h, ('pb', {'CFrame': pose((70, 2, 3))}))
        self.execute(h, 'vec(a.Position,Vector3.zero);vec(b.Position,Vector3.new(70,2,3))')

    def test_repeated_moves_do_not_accumulate_drift(self):
        h = self.rig()
        for x in (3, -7, 12.5, 0):
            self.receive(h, ('pa', {'CFrame': pose((x, 2, 1))}))
        self.execute(h, 'vec(a.Position,Vector3.new(0,2,1));vec(b.Position,Vector3.new(4,2,1))')

    def test_unjoined_part_is_not_moved(self):
        loose = {'id': 'pc', 'class_name': 'Part', 'name': 'PC', 'parent': 'scope:workspace',
                 'properties': {'CFrame': pose((20, 0, 0)), 'Anchored': False, 'Massless': False,
                                'RootPriority': 0, 'Size': {'$vector3': [2, 2, 2]}}}
        h = self.rig([loose])
        self.receive(h, ('pa', {'CFrame': pose((10, 0, 0))}))
        self.execute(h, 'vec(workspace.PC.Position,Vector3.new(20,0,0))')


class PlayerCharacterTests(unittest.TestCase):
    host = test_joint_poses.JointPoseTests.host
    execute = test_joint_poses.JointPoseTests.execute

    def test_unloaded_character_reads_as_nil(self):
        data = copy.deepcopy(transforms.SNAPSHOT)
        players = data['services'].get('Players')
        if players is None:
            self.skipTest('fixture has no Players service')
        data['nodes'].append({'id': 'other-player', 'class_name': 'Player', 'name': 'Other',
                              'parent': players, 'properties': {}})
        h = self.host(data)
        r = self.execute(h, 'return game:GetService("Players"):FindFirstChild("Other").Character==nil')
        self.assertEqual(r['values'], [True])


if __name__ == '__main__':
    unittest.main()
