"""Chunked FileMesh versions 6.00 and 7.00, including the Google Draco geometry that
7.00's COREMESH 2 chunk uses (bitstream 2.2, sequential connectivity, attributes stored
raw, as integers or quantized with difference prediction). Other Draco modes
(edgebreaker, mesh-aware prediction, octahedral normals, metadata) are refused.

Ported from Rook's mesh decoder (crates/rook-content/src/decode/mesh, Apache-2.0,
see THIRD_PARTY.md)."""
import struct

MAX_ELEMENTS = 4_000_000


class DracoError(ValueError):
    pass


class _Reader:
    def __init__(self, data, at=0, end=None):
        self.data = data
        self.at = at
        self.end = len(data) if end is None else end

    def remaining(self):
        return self.end - self.at

    def take(self, n):
        if n < 0 or self.at + n > self.end:
            raise DracoError('mesh data is cut short')
        out = self.data[self.at:self.at + n]
        self.at += n
        return out

    def u8(self):
        return self.take(1)[0]

    def i8(self):
        v = self.u8()
        return v - 256 if v > 127 else v

    def u16(self):
        return struct.unpack('<H', self.take(2))[0]

    def u32(self):
        return struct.unpack('<I', self.take(4))[0]

    def i32(self):
        return struct.unpack('<i', self.take(4))[0]

    def f32(self):
        return struct.unpack('<f', self.take(4))[0]

    def varint(self):
        out = 0
        for shift in range(0, 64, 7):
            b = self.u8()
            out |= (b & 0x7f) << shift
            if b < 0x80:
                return out
        raise DracoError('a varint runs past 64 bits')

    def count(self, value=None):
        n = self.u32() if value is None else value
        if n > MAX_ELEMENTS:
            raise DracoError(f'a count of {n} is out of range')
        return n


class _Rans:
    """rANS decoder over a probability table; the coded bytes are read from their end."""

    def __init__(self, r, length):
        precision_bits = min(max(3 * length // 2, 12), 20)
        self.precision = precision = 1 << precision_bits
        count = r.count(r.varint())
        if count // 64 > r.remaining():
            raise DracoError('Draco rANS table is cut short')
        table, lookup = [], bytearray()
        lookup_symbols = []
        filled = 0
        while len(table) < count:
            b = r.u8()
            token = b & 3
            if token == 3:
                zeros = (b >> 2) + 1
                if len(table) + zeros > count:
                    raise DracoError('Draco rANS table runs past its count')
                table.extend([(0, filled)] * zeros)
                continue
            p = b >> 2
            for i in range(token):
                p |= r.u8() << (8 * (i + 1) - 2)
            if filled + p > precision:
                raise DracoError('Draco rANS probabilities pass the precision')
            lookup_symbols.append((len(table), p))
            table.append((p, filled))
            filled += p
        if filled != precision:
            raise DracoError("Draco rANS probabilities don't add up")
        slots = [0] * precision
        for symbol, p in lookup_symbols:
            start = table[symbol][1]
            slots[start:start + p] = [symbol] * p
        self.lookup = slots
        self.table = table
        size = r.varint()
        data = r.take(size)
        if not data:
            raise DracoError('Draco rANS data is empty')
        width = (data[-1] >> 6) + 1
        at = size - width
        if at < 0:
            raise DracoError('Draco rANS data is cut short')
        head = int.from_bytes(data[at:], 'little')
        state = (head & ((1 << (8 * width - 2)) - 1)) + 4 * precision
        if state >= 4 * precision * 256:
            raise DracoError('Draco rANS state is out of range')
        self.data, self.at, self.state = data, at, state

    def read(self):
        precision = self.precision
        low = 4 * precision
        state, at, data = self.state, self.at, self.data
        while state < low and at > 0:
            at -= 1
            state = (state << 8) | data[at]
        quotient, slot = divmod(state, precision)
        symbol = self.lookup[slot]
        p, start = self.table[symbol]
        self.state = quotient * p + slot - start
        self.at = at
        return symbol


def _symbols(r, n, components):
    if n == 0:
        return []
    out = []
    scheme = r.u8()
    if scheme == 0:
        tags = _Rans(r, 5)
        data, start = r.data, r.at
        limit = (r.end - start) * 8
        bit = 0
        while len(out) < n:
            length = tags.read()
            for _ in range(min(components, n - len(out))):
                if length > 32 or bit + length > limit:
                    raise DracoError('Draco bits are cut short')
                value = 0
                for i in range(length):
                    value |= ((data[start + (bit >> 3)] >> (bit & 7)) & 1) << i
                    bit += 1
                out.append(value)
        r.take((bit + 7) // 8)
    elif scheme == 1:
        length = r.u8()
        if not 1 <= length <= 18:
            raise DracoError(f'{length}-bit Draco symbols')
        decoder = _Rans(r, length)
        read = decoder.read
        out = [read() for _ in range(n)]
    else:
        raise DracoError(f'Draco symbol scheme {scheme}')
    return out


def _connectivity(r, faces, points):
    if faces == 0 or points == 0:
        return []
    method = r.u8()
    out = []
    if method == 0:
        last = 0
        for s in _symbols(r, faces * 3, 1):
            diff = s >> 1
            last += -diff if s & 1 else diff
            if last < 0:
                raise DracoError('a Draco index is negative')
            out.append(last)
    elif method == 1:
        for _ in range(faces * 3):
            if points < 256:
                out.append(r.u8())
            elif points < 65536:
                out.append(r.u16())
            elif points < 2097152:
                out.append(r.varint())
            else:
                out.append(r.u32())
    else:
        raise DracoError(f'Draco connectivity {method}')
    return out


POSITION, NORMAL, TEX_COORD, GENERIC = 0, 1, 3, 4
FLOAT32 = 9
_RAW_SIZES = {1: 1, 2: 1, 11: 1, 3: 2, 4: 2, 5: 4, 6: 4, FLOAT32: 4, 7: 8, 8: 8, 10: 8}


def _values(r, attr, points):
    n = points * attr['components']
    if attr['coding'] == 0:
        size = _RAW_SIZES.get(attr['type'])
        if size is None:
            raise DracoError(f"Draco data type {attr['type']}")
        raw = r.take(n * size)
        if attr['type'] == FLOAT32:
            attr['values'] = list(struct.unpack(f'<{n}f', raw))
        return
    prediction = r.i8()
    if prediction == -2:
        wrap = False
    elif prediction == 0:
        transform = r.i8()
        if transform != 1:
            raise DracoError(f'Draco transform {transform}')
        wrap = True
    else:
        raise DracoError(f'Draco prediction {prediction}')
    if r.u8() > 0:
        symbols = _symbols(r, n, attr['components'])
    else:
        size = r.u8()
        if not 1 <= size <= 4:
            raise DracoError(f'{size}-byte Draco values')
        raw = r.take(n * size)
        symbols = [int.from_bytes(raw[i:i + size], 'little') for i in range(0, n * size, size)]
    corrections = [-(s >> 1) - 1 if s & 1 else s >> 1 for s in symbols]
    if wrap:
        lo, hi = r.i32(), r.i32()
        if lo > hi:
            raise DracoError('a Draco wrap range runs backwards')
        span = hi - lo + 1
        c = attr['components']
        out = []
        for i, corr in enumerate(corrections):
            predicted = out[i - c] if i >= c else 0
            v = min(max(predicted, lo), hi) + corr
            if v > hi:
                v -= span
            elif v < lo:
                v += span
            out.append(v)
        attr['integers'] = out
    else:
        attr['integers'] = corrections


def _dequantize(r, attr):
    c = attr['components']
    mins = [r.f32() for _ in range(c)]
    value_range = r.f32()
    bits = r.u8()
    if not 1 <= bits <= 31:
        raise DracoError(f'{bits}-bit Draco quantization')
    delta = value_range / ((1 << bits) - 1)
    attr['values'] = [q * delta + mins[i % c] for i, q in enumerate(attr['integers'])]
    attr['integers'] = []


def _attributes(r, points):
    out = []
    for _ in range(r.u8()):
        first = len(out)
        for _ in range(r.count(r.varint())):
            kind, data_type, components = r.u8(), r.u8(), r.u8()
            r.u8()  # normalized
            r.varint()  # unique id
            if components == 0:
                raise DracoError('a Draco attribute has no components')
            out.append({'kind': kind, 'type': data_type, 'components': components,
                        'coding': 0, 'values': [], 'integers': []})
        for attr in out[first:]:
            coding = r.u8()
            if coding not in (0, 1, 2):
                raise DracoError(f'Draco attribute coding {coding}')
            attr['coding'] = coding
        for attr in out[first:]:
            _values(r, attr, points)
            if attr['coding'] == 1:
                attr['integers'] = []
        for attr in out[first:]:
            if attr['coding'] == 2:
                _dequantize(r, attr)
    return out


def decode_draco(data):
    """Return (positions, normals, uvs, indices) as flat lists."""
    r = _Reader(data)
    if r.take(5) != b'DRACO':
        raise DracoError("COREMESH isn't Draco")
    major, minor = r.u8(), r.u8()
    if (major, minor) != (2, 2):
        raise DracoError(f'Draco {major}.{minor}')
    kind, method, flags = r.u8(), r.u8(), r.u16()
    if kind != 1 or method != 0:
        raise DracoError(f'Draco encoding {kind}/{method}')
    if flags:
        raise DracoError(f'Draco flags {flags:#x}')
    faces = r.count(r.varint())
    points = r.count(r.varint())
    indices = _connectivity(r, faces, points)
    attributes = _attributes(r, points)

    def find(kinds, components, what):
        for attr in attributes:
            if attr['kind'] in kinds and attr['type'] == FLOAT32 and attr['components'] == components:
                return attr['values']
        raise DracoError(f'the Draco mesh has no {what}')
    # Roblox stores normals as a generic attribute, not Draco's normal type.
    return (find((POSITION,), 3, 'positions'), find((NORMAL, GENERIC), 3, 'normals'),
            find((TEX_COORD,), 2, 'UVs'), indices)


def decode_chunked(body):
    """Decode a 6.00/7.00 body (after the version line) into (positions, normals, uvs,
    indices) using the highest level of detail."""
    r = _Reader(body)
    mesh = None
    faces = []
    lods = []
    while r.remaining():
        name = r.take(8)
        version = r.u32()
        size = r.u32()
        chunk = _Reader(r.data, r.at, r.at + size)
        r.take(size)
        if name == b'COREMESH' and version == 1:
            vertices = chunk.count()
            raw = chunk.take(vertices * 40)
            positions, normals, uvs = [], [], []
            for i in range(vertices):
                v = struct.unpack_from('<8f', raw, i * 40)
                positions.extend(v[:3]); normals.extend(v[3:6]); uvs.extend(v[6:8])
            count = chunk.count()
            faces = list(struct.unpack(f'<{count * 3}I', chunk.take(count * 12)))
            mesh = (positions, normals, uvs)
        elif name == b'COREMESH' and version == 2:
            length = chunk.u32()
            positions, normals, uvs, faces = decode_draco(chunk.take(length))
            mesh = (positions, normals, uvs)
        elif name == b'COREMESH':
            raise DracoError(f'COREMESH {version}')
        elif name == b'LODS\0\0\0\0' and version == 1:
            chunk.u16()  # LOD type
            chunk.u8()  # high quality LOD count
            count = chunk.count()
            lods = list(struct.unpack(f'<{count}I', chunk.take(count * 4)))
    if mesh is None:
        raise DracoError('no COREMESH chunk')
    face_count = len(faces) // 3
    end = face_count
    if len(lods) >= 2 and lods[0] <= lods[1] <= face_count and lods[1] > lods[0]:
        end = lods[1]
    return mesh[0], mesh[1], mesh[2], faces[:end * 3]
