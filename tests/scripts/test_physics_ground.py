import math
import unittest
from types import SimpleNamespace

from physics_ground import (TriangleMesh, VoxelGrid, decode_collision_geometry,
                            extract_ground, ground_raycast, step_vertical, GroundGeometry)


def make_world(parts, *, cframe_type=27):
    classes = [{'name': name} for name in ('Part', 'Terrain', 'MeshPart', 'Seat', 'SpawnLocation')]
    properties = [
        {'name': 'CanCollide', 'type': 9}, {'name': 'Anchored', 'type': 9},
        {'name': 'size', 'type': 22}, {'name': 'CFrame', 'type': cframe_type},
        {'name': 'shape', 'type': 7, 'enum': 0}, {'name': 'ReplicationPV', 'type': 57},
    ]
    classes[0]['properties'] = list(range(len(properties)))
    classes[1]['properties'] = []
    classes[2]['properties'] = []
    classes[3]['properties'] = list(range(len(properties)))
    classes[4]['properties'] = list(range(len(properties)))
    return SimpleNamespace(schema={'classes': classes, 'properties': properties,
                                   'enums': [{'name': 'PartType'}]}, instances=parts,
                           creation={}, property_versions={})


def part(class_index=0, **properties):
    if class_index == 0:
        properties.setdefault('shape', 1)  # Enum.PartType.Block in the fixture schema.
    if 'Size' in properties:
        properties['size'] = properties.pop('Size')
    if 'Shape' in properties:
        properties['shape'] = properties.pop('Shape')
    return {'class': class_index, 'parent': None, 'properties': properties}


class GroundExtractionTests(unittest.TestCase):
    def test_extract_explicit_axis_aligned_part(self):
        world = make_world({1: part(CanCollide=True, Anchored=True,
                                    CFrame={'position': [0, 0, 0], 'rotation': [1,0,0,0,1,0,0,0,1]},
                                    Size=[20, 2, 20])})
        geometry = extract_ground(world)
        self.assertEqual(len(geometry.colliders), 1)
        self.assertEqual(geometry.colliders[0].minimum, (-10., -1., -10.))
        self.assertEqual(geometry.colliders[0].maximum, (10., 1., 10.))
        self.assertFalse(geometry.unsupported)

    def test_type27_identity_selector_uses_layout_matched_native_mapping(self):
        world = make_world({1: part(CanCollide=True, Anchored=True,
                                    CFrame={'position': [0, 0, 0], 'rotation_id': 2},
                                    Size=[20, 2, 20])})
        self.assertEqual(len(extract_ground(world).colliders), 1)

    def test_type27_other_selectors_do_not_create_identity_boxes(self):
        # Selector 1 expands code 0 (+X, +X): not an orthogonal axis pair, so
        # the native expander rejects it and no box may be assumed.
        world = make_world({1: part(CanCollide=True, Anchored=True,
                                    CFrame={'position': [0, 0, 0], 'rotation_id': 1},
                                    Size=[20, 2, 20])})
        geometry = extract_ground(world)
        self.assertEqual(geometry.colliders, ())
        self.assertIn('rotation', dict(geometry.unsupported)[1])

    def test_type27_axis_pair_selector_creates_oriented_box(self):
        # Selector 14 expands code 13: first axis +Z, second +Y, so local X
        # points along world Z (a 90 degree yaw), matching the native reader.
        world = make_world({1: part(CanCollide=True, Anchored=True,
                                    CFrame={'position': [0, 0, 0], 'rotation_id': 14,
                                            'packed_rotation': None},
                                    Size=[20, 2, 4])})
        geometry = extract_ground(world)
        self.assertEqual(len(geometry.colliders), 1)
        box = geometry.colliders[0]
        self.assertEqual(box.assumptions, ())
        for actual, expected in zip(box.minimum + box.maximum, (-2., -1., -10., 2., 1., 10.)):
            self.assertAlmostEqual(actual, expected, places=6)
        self.assertIsNotNone(ground_raycast((0., 1.2, 9.5), geometry, foot_offset=0., max_distance=.5))
        self.assertIsNone(ground_raycast((9.5, 1.2, 0.), geometry, foot_offset=0., max_distance=.5))

    def test_type27_packed_quaternion_creates_oriented_box(self):
        from physics_rotation import encode_native_quaternion
        # 45 degree yaw about +Y as (x, y, z, w); only its diagonal footprint
        # can tell a decoded rotation from an assumed identity.
        half = math.sqrt(.5)
        packed = encode_native_quaternion((0., math.sin(math.pi/8), 0., math.cos(math.pi/8))).hex()
        world = make_world({1: part(CanCollide=True, Anchored=True,
                                    CFrame={'position': [0, 0, 0], 'rotation_id': 0,
                                            'packed_rotation': packed},
                                    Size=[20, 2, 2])})
        geometry = extract_ground(world)
        self.assertEqual(len(geometry.colliders), 1)
        box = geometry.colliders[0]
        self.assertAlmostEqual(box.maximum[0], 11*half, places=2)
        self.assertAlmostEqual(box.maximum[2], 11*half, places=2)
        self.assertIsNotNone(ground_raycast((6., 1.2, -6.), geometry, foot_offset=0., max_distance=.5))
        self.assertIsNone(ground_raycast((6., 1.2, 6.), geometry, foot_offset=0., max_distance=.5))
        bad = make_world({1: part(CanCollide=True, Anchored=True,
                                  CFrame={'position': [0, 0, 0], 'rotation_id': 0,
                                          'packed_rotation': 'abcd'},
                                  Size=[20, 2, 2])})
        self.assertEqual(extract_ground(bad).colliders, ())

    def test_type51_and_type57_selectors_and_quaternions_create_oriented_boxes(self):
        from physics_rotation import encode_native_quaternion
        packed = encode_native_quaternion((0., math.sin(math.pi/8), 0., math.cos(math.pi/8))).hex()
        selector51 = make_world({1: part(CanCollide=True, Anchored=True,
                                         CFrame={'position': [0, 0, 0], 'rotation': 14},
                                         Size=[20, 2, 4])}, cframe_type=51)
        quaternion51 = make_world({1: part(CanCollide=True, Anchored=True,
                                           CFrame={'position': [0, 0, 0], 'rotation': packed},
                                           Size=[20, 2, 2])}, cframe_type=51)
        box = extract_ground(selector51).colliders[0]
        self.assertAlmostEqual(box.maximum[2], 10., places=6)
        self.assertAlmostEqual(box.maximum[0], 2., places=6)
        box = extract_ground(quaternion51).colliders[0]
        self.assertAlmostEqual(box.maximum[0], 11*math.sqrt(.5), places=2)
        # Type 57 compact selectors use the separate physics table; selector 1
        # is identity there and 2 is a different axis pair (+X, +Z).
        for selector, expected_z in ((1, 2.), (2, 1.)):
            instance = part(CanCollide=True, Anchored=True,
                            ReplicationPV={'position': [0, 0, 0], 'rotation': selector,
                                           'position_present': True},
                            Size=[20, 2, 4])
            world = make_world({1: instance})
            world.schema['properties'][3] = {'name': 'CFrame', 'type': 27}
            geometry = extract_ground(world)
            self.assertEqual(len(geometry.colliders), 1, selector)
            self.assertAlmostEqual(geometry.colliders[0].maximum[2], expected_z, places=6)
        instance['properties']['ReplicationPV']['rotation'] = packed
        geometry = extract_ground(world)
        self.assertAlmostEqual(geometry.colliders[0].maximum[0], 12*math.sqrt(.5), places=2)
        instance['properties']['ReplicationPV']['rotation'] = [0.]*7
        self.assertEqual(extract_ground(world).colliders, ())

    def test_type51_identity_selector_is_two(self):
        world = make_world({1: part(CanCollide=True, Anchored=True,
                                    CFrame={'position': [0, 0, 0], 'rotation': 2},
                                    Size=[20, 2, 20])}, cframe_type=51)
        self.assertEqual(len(extract_ground(world).colliders), 1)

    def test_type57_identity_selector_requires_received_position(self):
        instance = part(CanCollide=True, Anchored=True,
                        ReplicationPV={'position': [0, 0, 0], 'rotation': 1,
                                       'position_present': True},
                        Size=[20, 2, 20])
        world = make_world({1: instance})
        world.schema['properties'][3] = {'name': 'CFrame', 'type': 27}
        self.assertEqual(len(extract_ground(world).colliders), 1)
        instance['properties']['ReplicationPV']['position_present'] = False
        geometry = extract_ground(world)
        self.assertEqual(geometry.colliders, ())
        self.assertIn('position-bearing', dict(geometry.unsupported)[1])

    def test_unknown_or_nonblock_part_shape_is_rejected(self):
        unknown = part(CanCollide=True, Anchored=True,
                       CFrame={'position': [0, 0, 0], 'rotation': [1,0,0,0,1,0,0,0,1]},
                       Size=[20, 2, 20])
        del unknown['properties']['shape']
        nonblock = part(CanCollide=True, Anchored=True, Shape=0,
                        CFrame={'position': [0, 0, 0], 'rotation': [1,0,0,0,1,0,0,0,1]},
                        Size=[20, 2, 20])
        geometry = extract_ground(make_world({1: unknown, 2: nonblock}))
        self.assertEqual(geometry.colliders, ())
        self.assertTrue(all('shape' in reason.lower() for _, reason in geometry.unsupported))

    def test_default_cancollide_only_for_fresh_complete_ordinary_part(self):
        instance = part(Anchored=True,
                        CFrame={'position': [0, 0, 0], 'rotation': [1,0,0,0,1,0,0,0,1]},
                        Size=[20, 2, 20])
        world = make_world({1: instance})
        world.creation[1] = {'fresh': True, 'complete': True, 'ordinary': True}
        self.assertEqual(len(extract_ground(world).colliders), 1)
        world.creation[1]['complete'] = False
        geometry = extract_ground(world)
        self.assertEqual(geometry.colliders, ())
        self.assertIn('CanCollide default unavailable', dict(geometry.unsupported)[1])

    def test_experimental_network_defaults_are_opt_in_and_provenance_marked(self):
        instance = part(Anchored=True,
                        CFrame={'position': [0, 0, 0], 'rotation': [1,0,0,0,1,0,0,0,1]},
                        Size=[20, 2, 20])
        del instance['properties']['shape']
        world = make_world({1: instance})
        world.creation[1] = {'fresh': True, 'complete': True,
                             'ordinary': False, 'property_lists': 1}

        strict = extract_ground(world)
        self.assertEqual(strict.colliders, ())
        self.assertIn('CanCollide default unavailable', dict(strict.unsupported)[1])

        experimental = extract_ground(world, allow_network_constructor_defaults=True)
        self.assertEqual(len(experimental.colliders), 1)
        assumptions = experimental.colliders[0].assumptions
        self.assertEqual(len(assumptions), 2)
        self.assertIn('CanCollide=true', assumptions[0])
        self.assertIn('network reset path is unproven', assumptions[0])
        self.assertIn('PartType.Block(1)', assumptions[1])
        self.assertIn('exact-build constructor/network reset path is unproven', assumptions[1])

    def test_experimental_defaults_do_not_override_explicit_shape_or_collision(self):
        ball = part(Anchored=True, CanCollide=True, Shape=0,
                    CFrame={'position': [0, 0, 0], 'rotation': [1,0,0,0,1,0,0,0,1]},
                    Size=[20, 2, 20])
        noncollidable = part(Anchored=True, CanCollide=False,
                             CFrame={'position': [0, 0, 0], 'rotation': [1,0,0,0,1,0,0,0,1]},
                             Size=[20, 2, 20])
        world = make_world({1: ball, 2: noncollidable})
        world.creation = {1: {'fresh': True, 'complete': True, 'ordinary': False},
                          2: {'fresh': True, 'complete': True, 'ordinary': False}}
        geometry = extract_ground(world, allow_network_constructor_defaults=True)
        self.assertEqual(geometry.colliders, ())
        self.assertIn('shape', dict(geometry.unsupported)[1].lower())

    def test_lowercase_schema_size_is_used_and_schema_is_required(self):
        world = make_world({1: part(CanCollide=True, Anchored=True,
                                    CFrame={'position': [0, 0, 0], 'rotation': [1,0,0,0,1,0,0,0,1]},
                                    Size=[20, 2, 20])})
        self.assertEqual(len(extract_ground(world).colliders), 1)
        world.schema['properties'][2] = {'name': 'someArray', 'type': 29}
        geometry = extract_ground(world)
        self.assertEqual(geometry.colliders, ())
        self.assertIn('Vector3 size', dict(geometry.unsupported)[1])

    def test_part_descendant_uses_its_measured_defaults_only_when_opted_in(self):
        seat = part(class_index=3, Anchored=True,
                    CFrame={'position': [0, 0, 0], 'rotation': [1,0,0,0,1,0,0,0,1]},
                    Size=[20, 2, 20])
        world = make_world({1: seat})
        world.creation[1] = {'fresh': True, 'complete': True,
                             'ordinary': False, 'property_lists': 1}
        self.assertEqual(extract_ground(world).colliders, ())
        geometry = extract_ground(world, allow_network_constructor_defaults=True)
        self.assertEqual(len(geometry.colliders), 1)
        assumptions = geometry.colliders[0].assumptions
        self.assertEqual(len(assumptions), 2)
        self.assertIn('Seat constructor default', assumptions[0])
        self.assertIn('Seat network creation', assumptions[1])

    def test_part_descendant_explicit_collision_false_is_not_overridden(self):
        spawn = part(class_index=4, Anchored=True, CanCollide=False,
                     CFrame={'position': [0, 0, 0], 'rotation': [1,0,0,0,1,0,0,0,1]},
                     Size=[4, 1, 4])
        world = make_world({1: spawn})
        world.creation[1] = {'fresh': True, 'complete': True,
                             'ordinary': False, 'property_lists': 1}
        geometry = extract_ground(world, allow_network_constructor_defaults=True)
        self.assertEqual(geometry.colliders, ())

    def test_accepts_rotated_block_but_rejects_terrain_mesh_and_unanchored(self):
        identity = {'position': [0,0,0], 'rotation': [1,0,0,0,1,0,0,0,1]}
        world = make_world({
            'rot': part(CanCollide=True, Anchored=True,
                        CFrame={'position': [0,0,0], 'rotation': [0,0,1,0,1,0,-1,0,0]}, Size=[2,2,2]),
            'terrain': part(1), 'mesh': part(2),
            'moving': part(CanCollide=True, Anchored=False, CFrame=identity, Size=[2,2,2]),
        })
        geometry = extract_ground(world)
        self.assertEqual(len(geometry.colliders), 1)
        self.assertEqual(geometry.colliders[0].source, 'rot')
        reasons = dict(geometry.unsupported)
        self.assertNotIn('rot', reasons)
        self.assertIn('terrain', reasons['terrain'])
        self.assertIn('mesh', reasons['mesh'])
        self.assertIn('anchored', reasons['moving'])

    def test_unanchored_dynamic_blocks_require_explicit_opt_in_and_received_pose(self):
        moving=part(CanCollide=True,Anchored=False,
                    CFrame={'position':[2,3,4],'rotation':[1,0,0,0,1,0,0,0,1]},Size=[2,2,2])
        world=make_world({'moving':moving})
        self.assertFalse(extract_ground(world).colliders)
        geometry=extract_ground(world,include_unanchored=True)
        self.assertEqual(len(geometry.colliders),1)
        self.assertEqual(geometry.colliders[0].source,'moving')
        del moving['properties']['CFrame']
        geometry=extract_ground(world,include_unanchored=True)
        self.assertFalse(geometry.colliders)
        self.assertIn('position-bearing',dict(geometry.unsupported)['moving'])

    def test_unknown_wire_rotation_is_not_assumed_to_be_identity(self):
        # The live replication reader exposes generic type-27 CFrames as a
        # rotation_id plus optional opaque bytes. A selector value alone is
        # not enough evidence to turn that part into a collision box.
        world = make_world({1: part(CanCollide=True, Anchored=True,
                                    CFrame={'position': [0,0,0], 'rotation_id': 1,
                                            'packed_rotation': None},
                                    Size=[20, 2, 20])})
        geometry = extract_ground(world)
        self.assertEqual(geometry.colliders, ())
        self.assertIn('rotation', dict(geometry.unsupported)[1])

    def test_malformed_instance_data_is_reported_and_skipped(self):
        geometry = extract_ground(make_world({1: None,
                                              2: {'class': 0, 'parent': None,
                                                  'properties': None}}))
        self.assertEqual(geometry.colliders, ())
        reasons = dict(geometry.unsupported)
        self.assertEqual(reasons[1], 'invalid instance record')
        self.assertEqual(reasons[2], 'invalid properties record')


class GroundMovementTests(unittest.TestCase):
    def setUp(self):
        world = make_world({1: part(CanCollide=True, Anchored=True,
                                   CFrame={'position': [0,0,0], 'rotation': [1,0,0,0,1,0,0,0,1]},
                                   Size=[10,2,10])})
        self.geometry = extract_ground(world)

    def test_raycast_supports_interior_not_edge_outside(self):
        self.assertIsNotNone(ground_raycast((0, 4, 0), self.geometry, foot_offset=3, max_distance=2))
        self.assertIsNone(ground_raycast((6, 4, 0), self.geometry, foot_offset=3, max_distance=2))

    def test_grounded_step_stays_on_floor(self):
        result = step_vertical((0, 4, 0), 0, 0.1, self.geometry, True)
        self.assertTrue(result.grounded)
        self.assertEqual(result.position, (0., 4., 0.))
        self.assertEqual(result.vertical_velocity, 0)

    def test_grounded_step_recovers_when_foot_offset_grows(self):
        scaled = step_vertical((0, 4, 0), 0, 0.1, self.geometry, True,
                               foot_offset=5)
        self.assertTrue(scaled.grounded)
        self.assertEqual(scaled.position, (0., 6., 0.))
        self.assertEqual(scaled.vertical_velocity, 0.)
        self.assertIs(scaled.hit, self.geometry.colliders[0])

        airborne = step_vertical((0, 4, 0), 0, 0.1, self.geometry, False,
                                 foot_offset=5)
        self.assertFalse(airborne.grounded)
        self.assertLess(airborne.position[1], 4.)

    def test_walk_off_edge_starts_falling_then_accelerates(self):
        first = step_vertical((5.1, 4, 0), 0, 0.1, self.geometry, True)
        second = step_vertical(first.position, first.vertical_velocity, 0.1, self.geometry, first.grounded)
        self.assertFalse(first.grounded)
        self.assertLess(first.position[1], 4)
        self.assertLess(second.position[1], first.position[1])
        self.assertLess(second.vertical_velocity, first.vertical_velocity)

    def test_landing_clamps_fall_at_ground(self):
        result = step_vertical((0, 5, 0), -20, 0.1, self.geometry, False)
        self.assertTrue(result.grounded)
        self.assertEqual(result.position, (0., 4., 0.))
        self.assertEqual(result.vertical_velocity, 0)

    def test_initial_airborne_actor_falls_lands_and_stops(self):
        state = step_vertical((0, 12, 0), 0, 0.1, self.geometry, False)
        self.assertFalse(state.grounded)
        self.assertLess(state.position[1], 12)
        self.assertLess(state.vertical_velocity, 0)
        for _ in range(30):
            if state.grounded: break
            state = step_vertical(state.position, state.vertical_velocity, 0.1, self.geometry, state.grounded)
        self.assertTrue(state.grounded)
        self.assertEqual(state.position, (0., 4., 0.))
        self.assertEqual(state.vertical_velocity, 0)
        held = step_vertical(state.position, state.vertical_velocity, 0.1, self.geometry, state.grounded)
        self.assertEqual(held, state)

    def test_validates_timestep(self):
        with self.assertRaises(ValueError): step_vertical((0, 4, 0), 0, 0.3, self.geometry, True)


class DecodedGeometryTests(unittest.TestCase):
    def test_triangle_mesh_hits_sloped_triangle_and_respects_actual_footprint(self):
        mesh=TriangleMesh(((0,0,0),(4,4,0),(0,0,4)),((0,2,1),),'slope')
        geom=GroundGeometry((mesh,))
        hit=ground_raycast((1,5,1),geom,foot_offset=0,max_distance=5)
        self.assertIsNotNone(hit)
        self.assertAlmostEqual(hit.y,1.)
        self.assertGreater(hit.normal[1],0)
        self.assertIsNone(ground_raycast((3,5,3),geom,foot_offset=0,max_distance=5))

    def test_mesh_validation_rejects_bad_or_degenerate_indices(self):
        for vertices,triangles in [(((0,0,0),(1,0,0)),((0,1,2),)),
                                   (((0,0,0),(1,0,0),(2,0,0)),((0,1,2),))]:
            with self.assertRaises(ValueError): TriangleMesh(vertices,triangles)

    def test_voxel_top_face_uses_explicit_occupancy_and_honors_holes(self):
        # dimensions x=2,y=1,z=1; first cell occupied, second is empty
        grid=VoxelGrid((0,0,0),(2,3,2),(2,1,1),(True,False),'terrain')
        geom=GroundGeometry((grid,))
        self.assertEqual(ground_raycast((1,5,1),geom,foot_offset=0,max_distance=5).y,3.)
        self.assertIsNone(ground_raycast((3,5,1),geom,foot_offset=0,max_distance=5))

    def test_decoded_payload_validation_and_extraction(self):
        payload={'kind':'triangle_mesh','vertices':[[0,0,0],[2,0,0],[0,0,2]],'triangles':[[0,1,2]]}
        decoded=decode_collision_geometry(payload,source='asset')
        self.assertIsInstance(decoded,TriangleMesh)
        with self.assertRaises(ValueError): decode_collision_geometry({'kind':'triangle_mesh','vertices':[],'triangles':[]})
        world=make_world({'mesh':part(2,CollisionGeometry=payload)})
        geom=extract_ground(world)
        self.assertEqual(len(geom.colliders),1)
        world.instances['mesh']['properties']['CanCollide'] = False
        self.assertFalse(extract_ground(world).colliders)
        bad=make_world({'terrain':part(1,CollisionGeometry={'kind':'voxel_grid','origin':[0,0,0],
            'cell_size':[1,1,1],'dimensions':[1,1,1],'occupancy':[1]})})
        result=extract_ground(bad)
        self.assertFalse(result.colliders)
        self.assertIn('malformed decoded',dict(result.unsupported)['terrain'])

    def test_opaque_mesh_or_terrain_is_still_unsupported(self):
        geom=extract_ground(make_world({'terrain':part(1),'mesh':part(2)}))
        reasons=dict(geom.unsupported)
        self.assertIn('voxels are not modeled',reasons['terrain'])
        self.assertIn('collision shape is not modeled',reasons['mesh'])



if __name__ == '__main__':
    unittest.main()
