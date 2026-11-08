"""Assembly mass from omitted constructor defaults, and the unknown-mass servo fallback.

Avatar parts usually keep Material=Plastic and nil CustomPhysicalProperties, so
the server never sends them. With a fresh, complete creation record those are
the constructor defaults (the native runtime already reads them that way).
"""
import unittest

from physics_controller import _STIFF_SERVO_MASS, _UNBOUNDED_SERVO_FORCE, _stiff_velocity_servos_only
from physics_forces import received_assembly_mass
from physics_velocity_servo import solve_movers


def avatar_part(name='root', *, defaults_known=True, kind='MeshPart', **received):
    properties = {'Size': {'$vector3': [2, 2, 1]}, 'Anchored': False,
                  'AssemblyRootPart': {'$ref': 'root'}, **received}
    return {'id': name, 'class_name': kind, 'parent': 'char', 'properties': properties,
            'origins': dict.fromkeys(properties, 'received'),
            'constructor_defaults_known': defaults_known}


def state(*parts):
    return {'initial_replication_complete': True, 'nodes': list(parts)}


class ConstructorDefaultMassTests(unittest.TestCase):
    def test_omitted_plastic_material_gives_a_mass(self):
        mass, source = received_assembly_mass(state(avatar_part()), 'root')
        self.assertAlmostEqual(mass, 2 * 2 * 1 * .7)  # Plastic density 0.7
        self.assertTrue(source.startswith('received_'), source)

    def test_part_class_uses_its_real_shape_volume(self):
        mass, _ = received_assembly_mass(state(avatar_part(kind='Part', Shape=1)), 'root')
        self.assertAlmostEqual(mass, 4 * .7)

    def test_unknown_creation_state_still_refuses(self):
        self.assertEqual(received_assembly_mass(state(avatar_part(defaults_known=False)), 'root'), (None, None))

    def test_received_values_win_over_defaults(self):
        metal = avatar_part(Material=1040)  # Metal, density 7.85
        self.assertAlmostEqual(received_assembly_mass(state(metal), 'root')[0], 4 * 7.85)
        custom = avatar_part(CustomPhysicalProperties=[3., .3, .5, 1., 1.])
        self.assertAlmostEqual(received_assembly_mass(state(custom), 'root')[0], 4 * 3.)

    def test_size_received_under_its_wire_name_counts(self):
        # Avatar parts send `size`; the snapshot copies it to Size as derived_received.
        part = avatar_part()
        part['properties']['size'] = part['properties']['Size']
        part['origins'].update(Size='derived_received', size='received')
        self.assertAlmostEqual(received_assembly_mass(state(part), 'root')[0], 4 * .7)

    def test_other_derived_sizes_are_still_refused(self):
        mismatched = avatar_part()
        mismatched['properties']['size'] = {'$vector3': [9, 9, 9]}
        mismatched['origins'].update(Size='derived_received', size='received')
        self.assertEqual(received_assembly_mass(state(mismatched), 'root'), (None, None))
        no_wire = avatar_part()
        no_wire['origins']['Size'] = 'derived_received'
        self.assertEqual(received_assembly_mass(state(no_wire), 'root'), (None, None))

    def test_received_massless_true_is_respected(self):
        root = avatar_part()
        arm = avatar_part('arm', Massless=True)
        weld = {'id': 'w', 'class_name': 'WeldConstraint', 'parent': 'root',
                'properties': {'Part0': {'$ref': 'root'}, 'Part1': {'$ref': 'arm'}, 'Enabled': True},
                'origins': {'Part0': 'received', 'Part1': 'received', 'Enabled': 'received'}}
        self.assertAlmostEqual(received_assembly_mass(state(root, arm, weld), 'root')[0], 4 * .7)


def servo(max_force, gain=1e4, target=(0., 50., 0.)):
    return {'id': 's', 'target_velocity': target, 'max_force': tuple(max_force), 'gain': gain}


class StiffServoFallbackTests(unittest.TestCase):
    def test_only_effectively_unbounded_servos_qualify(self):
        big = _UNBOUNDED_SERVO_FORCE
        self.assertTrue(_stiff_velocity_servos_only([servo((big, big, big))], (), ()))
        self.assertTrue(_stiff_velocity_servos_only([servo((0, 1e9, 0))], (), ()))
        self.assertFalse(_stiff_velocity_servos_only([servo((4000, 4000, 4000))], (), ()))  # Roblox default
        self.assertFalse(_stiff_velocity_servos_only([servo((big, 10, big))], (), ()))
        self.assertFalse(_stiff_velocity_servos_only([servo((0, 0, 0))], (), ()))
        self.assertFalse(_stiff_velocity_servos_only([servo((big, big, big), gain=0)], (), ()))
        self.assertFalse(_stiff_velocity_servos_only([], (), ()))
        self.assertFalse(_stiff_velocity_servos_only([servo((big,) * 3)], [{'id': 'spring'}], ()))

    def test_stiff_mass_reaches_the_target_velocity_against_gravity(self):
        dt, start, gravity = 1 / 15, (3., -20., 0.), (0., -196.2, 0.)
        # solve_movers returns the servo's share; external acceleration is separate.
        servo_acceleration, _ = solve_movers(start, [servo((1e6,) * 3)], _STIFF_SERVO_MASS, dt, gravity)
        reached = tuple(start[i] + (gravity[i] + servo_acceleration[i]) * dt for i in range(3))
        for value, target in zip(reached, (0., 50., 0.)):
            self.assertAlmostEqual(value, target, places=3)


if __name__ == '__main__':
    unittest.main()
