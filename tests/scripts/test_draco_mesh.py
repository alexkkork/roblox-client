"""Mesh versions 6.00/7.00 and their Draco geometry (writer ported from Rook's tests, Apache-2.0)."""
import struct
import unittest

from draco_mesh import DracoError, decode_draco
from render_assets import decode_render_mesh


class _Writer:
    def __init__(self):
        self.data = bytearray()

    def u8(self, v):
        self.data.append(v); return self

    def raw(self, b):
        self.data.extend(b); return self

    def varint(self, v):
        while True:
            b, v = v & 0x7f, v >> 7
            if v == 0:
                return self.u8(b)
            self.u8(b | 0x80)

    def rans(self, length, probs, symbols):
        precision = 1 << min(max(3 * length // 2, 12), 20)
        assert sum(probs) == precision
        self.varint(len(probs))
        for p in probs:
            if p < 64:
                self.u8(p << 2)
            else:
                self.u8(((p & 63) << 2) | 1).u8(p >> 6)
        starts, s = [], 0
        for p in probs:
            starts.append(s); s += p
        low = 4 * precision
        state, out = low, bytearray()
        for sym in reversed(symbols):
            p = probs[sym]
            while state >= low // precision * 256 * p:
                out.append(state & 0xff); state >>= 8
            state = state // p * precision + state % p + starts[sym]
        state -= low
        if state < 64:
            tail = bytes([state])
        elif state < 16384:
            tail = (1 << 14 | state).to_bytes(4, 'little')[:2]
        else:
            tail = (2 << 22 | state).to_bytes(4, 'little')[:3]
        out.extend(tail)
        return self.varint(len(out)).raw(out)


def floats(*values):
    return struct.pack(f'<{len(values)}f', *values)


def triangle():
    """One triangle laid out as Roblox stores it: raw positions and UVs, quantized normals
    with difference prediction, then an integer attribute that is stepped over."""
    w = _Writer()
    w.raw(b'DRACO').u8(2).u8(2).u8(1).u8(0).raw(b'\0\0')
    w.varint(1).varint(3).u8(1).raw(bytes([0, 2, 1]))
    w.u8(1).varint(4)
    for kind, data_type, components in ((0, 9, 3), (4, 9, 3), (3, 9, 2), (2, 2, 4)):
        w.u8(kind).u8(data_type).u8(components).u8(0).varint(0)
    w.raw(bytes([0, 2, 0, 1]))
    w.raw(floats(0, 0, 0, 1, 0, 0, 0, 1, 0))
    normals = [[1, 1, 2], [1, 1, 2], [3, 1, 1]]
    before, corrections = [0, 0, 0], []
    for n in normals:
        for c in range(3):
            d = n[c] - before[c]
            corrections.append((-d - 1) * 2 + 1 if d < 0 else d * 2)
        before = n
    w.u8(0).u8(1).u8(1).u8(1).u8(2).rans(2, [820, 819, 819, 819, 819], corrections)
    w.raw(struct.pack('<ii', 0, 3))
    w.raw(floats(0, 0, 1, 0, 0, 1))
    w.raw(bytes([0xfe, 0, 1])).raw(bytes(12))
    w.raw(floats(-1, -1, -1, 2)).u8(2)
    return bytes(w.data)


def chunk(name, version, body):
    return name + struct.pack('<II', version, len(body)) + body


class DracoMeshTests(unittest.TestCase):
    def test_reads_a_draco_triangle(self):
        positions, normals, uvs, indices = decode_draco(triangle())
        self.assertEqual(positions, [0, 0, 0, 1, 0, 0, 0, 1, 0])
        self.assertEqual(indices, [0, 2, 1])
        n = lambda q: q * (2 / 3) - 1
        for got, want in zip(normals, [n(1), n(1), n(2), n(1), n(1), n(2), n(3), n(1), n(1)]):
            self.assertAlmostEqual(got, want, places=5)
        self.assertEqual(uvs, [0, 0, 1, 0, 0, 1])

    def test_damage_is_an_error(self):
        full = triangle()
        for end in range(len(full)):
            with self.assertRaises(DracoError, msg=f'cut at {end}'):
                decode_draco(full[:end])
        old = bytearray(full); old[6] = 1
        with self.assertRaises(DracoError):
            decode_draco(bytes(old))

    def test_renderer_decodes_version_7_draco_mesh(self):
        draco = triangle()
        body = chunk(b'COREMESH', 2, struct.pack('<I', len(draco)) + draco)
        body += chunk(b'LODS\0\0\0\0', 1, struct.pack('<HBI', 0, 1, 2) + struct.pack('<II', 0, 1))
        body += chunk(b'FACS\0\0\0\0', 1, b'skipped')
        mesh = decode_render_mesh(b'version 7.00\n' + body)
        self.assertEqual(mesh['version'], 'version 7.00')
        self.assertEqual(mesh['indices'], [0, 2, 1])
        self.assertEqual(len(mesh['positions']), 9)
        self.assertFalse(mesh['skinned'])

    def test_renderer_decodes_version_6_plain_core_mesh(self):
        vertices = b''.join(floats(x, y, 0, 0, 0, 1, x, y) + bytes(8) for x, y in ((0, 0), (1, 0), (0, 1)))
        body = chunk(b'COREMESH', 1, struct.pack('<I', 3) + vertices + struct.pack('<I', 1) + struct.pack('<III', 0, 1, 2))
        mesh = decode_render_mesh(b'version 6.00\n' + body)
        self.assertEqual(mesh['indices'], [0, 1, 2])
        self.assertEqual(mesh['uv'], [0, 0, 1, 0, 0, 1])


if __name__ == '__main__':
    unittest.main()
