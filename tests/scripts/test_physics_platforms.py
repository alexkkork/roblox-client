import math
import unittest
from types import SimpleNamespace

from physics_platforms import (PlatformPose, carry_grounded_point,
    platform_poses_from_world,
    inherit_platform_velocity, platform_point_velocity)


class MovingPlatformTests(unittest.TestCase):
    def test_world_pose_map_tracks_only_received_valid_frames_by_instance_key(self):
        classes = [{'name': 'Part', 'properties': [0, 1, 2]}]
        schema = {'classes': classes, 'properties': [
            {'name': 'Anchored', 'type': 9}, {'name': 'CFrame', 'type': 27},
            {'name': 'ReplicationPV', 'type': 57}], 'enums': []}
        identity = [1, 0, 0, 0, 1, 0, 0, 0, 1]
        world = SimpleNamespace(schema=schema, instances={
            41: {'class': 0, 'properties': {'Anchored': True,
                'CFrame': {'position': [1, 2, 3], 'rotation': identity}}},
            42: {'class': 0, 'properties': {'Anchored': False,
                'ReplicationPV': {'position': [4, 5, 6], 'rotation': 1,
                                  'position_present': True}}},
            43: {'class': 0, 'properties': {'Anchored': False,
                'CFrame': {'position': [7, 8, 9], 'rotation': 0}}},
            44: {'class': 0, 'properties': {'CFrame': {'position': [0, 0, 0],
                                                       'rotation': identity}}},
            45: {'class': 0, 'properties': {'Anchored': False}},
            46: {'class': 0, 'properties': {'Anchored': True,
                'CFrame': {'position': [7, 8, 9], 'rotation': 5}}},
        }, property_versions={})
        poses = platform_poses_from_world(world)
        self.assertEqual(set(poses), {41, 42, 46})
        self.assertEqual(poses[41].position, (1., 2., 3.))
        self.assertEqual(poses[42].position, (4., 5., 6.))
        # Selector 0 without its six quaternion bytes is opaque and not guessed;
        # selector 5 expands through the pinned native axis-pair table (+X, -Y).
        self.assertNotIn(43, poses)
        self.assertEqual(poses[46].position, (7., 8., 9.))

    def test_world_pose_map_uses_newest_received_frame(self):
        schema = {'classes': [{'name': 'Part', 'properties': [0]}],
                  'properties': [{'name': 'CFrame', 'type': 27}], 'enums': []}
        world = SimpleNamespace(schema=schema, instances={7: {'class': 0,
            'properties': {'Anchored': False,
                'CFrame': {'position': [9, 0, 0],
                           'rotation': [1, 0, 0, 0, 1, 0, 0, 0, 1]}}}},
            property_versions={7: {'CFrame': 3, 'ReplicationPV': 4}})
        # No typed ReplicationPV property is present, so the received CFrame wins.
        self.assertEqual(platform_poses_from_world(world)[7].position, (9., 0., 0.))

    def test_translation_carries_grounded_point_and_velocity(self):
        old = PlatformPose((0, 0, 0))
        new = PlatformPose((4, 2, -1))
        p = (1, 3, 5)
        self.assertEqual(carry_grounded_point(p, old, new), (5., 5., 4.))
        self.assertEqual(platform_point_velocity(p, old, new, 2), (2., 1., -.5))

    def test_rotation_carries_point_and_computes_tangential_velocity(self):
        old = PlatformPose((0, 0, 0))
        new = PlatformPose((0, 0, 0), (0, -1, 0, 1, 0, 0, 0, 0, 1))
        point = (1, 0, 0)
        self.assertAlmostEqual(carry_grounded_point(point, old, new)[1], 1.)
        velocity = platform_point_velocity(point, old, new, 1.)
        self.assertAlmostEqual(velocity[0], 0.)
        self.assertAlmostEqual(velocity[1], math.pi/2)
        self.assertAlmostEqual(velocity[2], 0.)

    def test_jump_inherits_translation_and_angular_momentum(self):
        old = PlatformPose((0, 0, 0))
        new = PlatformPose((0, 2, 0), (0, -1, 0, 1, 0, 0, 0, 0, 1))
        result = inherit_platform_velocity((0, 5, 0), (1, 2, 0), old, new, 1.)
        self.assertAlmostEqual(result[0], 0.)
        self.assertAlmostEqual(result[1], 7. + math.pi/2)
        self.assertAlmostEqual(result[2], 0.)

    def test_requires_valid_received_pose_interval(self):
        pose = PlatformPose((0, 0, 0))
        with self.assertRaises(ValueError):
            platform_point_velocity((0, 0, 0), pose, pose, 0)
        with self.assertRaises(ValueError):
            PlatformPose((0, 0, 0), (1, 0, 0, 0, 2, 0, 0, 0, 1))


if __name__ == '__main__':
    unittest.main()
