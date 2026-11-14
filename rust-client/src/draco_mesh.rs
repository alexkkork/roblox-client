//! Chunked FileMesh versions 6.00 and 7.00, including the Google Draco geometry
//! that 7.00's COREMESH 2 chunk uses (bitstream 2.2, sequential connectivity,
//! attributes stored raw, as integers or quantized with difference prediction).
//! Other Draco modes (edgebreaker, mesh-aware prediction, octahedral normals,
//! metadata) are refused.
//!
//! Ported from Rook's mesh decoder (crates/rook-content/src/decode/mesh,
//! Apache-2.0; license and NOTICE in vendor/rook, see THIRD_PARTY.md).

const MAX_ELEMENTS: usize = 4_000_000;

#[derive(Debug, Clone, PartialEq)]
pub struct DracoError(pub String);
type Result<T> = std::result::Result<T, DracoError>;
fn fail<T>(text: impl Into<String>) -> Result<T> {
    Err(DracoError(text.into()))
}

/// Flat positions (xyz), normals (xyz), uvs (uv) and triangle indices.
pub struct ChunkedMesh {
    pub positions: Vec<f64>,
    pub normals: Vec<f64>,
    pub uvs: Vec<f64>,
    pub indices: Vec<u32>,
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
    end: usize,
}
impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0, end: data.len() }
    }
    fn remaining(&self) -> usize {
        self.end - self.at
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return fail("mesh data is cut short");
        }
        let out = &self.data[self.at..self.at + n];
        self.at += n;
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn i8(&mut self) -> Result<i8> {
        Ok(self.u8()? as i8)
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn varint(&mut self) -> Result<u64> {
        let mut out = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.u8()?;
            out |= ((b & 0x7f) as u64) << shift;
            if b < 0x80 {
                return Ok(out);
            }
        }
        fail("a varint runs past 64 bits")
    }
    fn count(&mut self, value: Option<u64>) -> Result<usize> {
        let n = match value {
            Some(v) => v,
            None => self.u32()? as u64,
        };
        if n > MAX_ELEMENTS as u64 {
            return fail(format!("a count of {n} is out of range"));
        }
        Ok(n as usize)
    }
}

/// rANS decoder over a probability table; the coded bytes are read from their end.
struct Rans<'a> {
    precision: u64,
    lookup: Vec<u32>,
    table: Vec<(u64, u64)>,
    data: &'a [u8],
    at: usize,
    state: u64,
}
impl<'a> Rans<'a> {
    fn new(r: &mut Reader<'a>, length: u32) -> Result<Self> {
        let precision_bits = (3 * length / 2).clamp(12, 20);
        let precision = 1u64 << precision_bits;
        let varint = r.varint()?;
        let count = r.count(Some(varint))?;
        if count / 64 > r.remaining() {
            return fail("Draco rANS table is cut short");
        }
        let mut table: Vec<(u64, u64)> = Vec::with_capacity(count);
        let mut lookup_symbols = Vec::new();
        let mut filled = 0u64;
        while table.len() < count {
            let b = r.u8()?;
            let token = b & 3;
            if token == 3 {
                let zeros = (b >> 2) as usize + 1;
                if table.len() + zeros > count {
                    return fail("Draco rANS table runs past its count");
                }
                table.extend(std::iter::repeat((0, filled)).take(zeros));
                continue;
            }
            let mut p = (b >> 2) as u64;
            for i in 0..token {
                p |= (r.u8()? as u64) << (8 * (i as u64 + 1) - 2);
            }
            if filled + p > precision {
                return fail("Draco rANS probabilities pass the precision");
            }
            lookup_symbols.push((table.len(), p));
            table.push((p, filled));
            filled += p;
        }
        if filled != precision {
            return fail("Draco rANS probabilities don't add up");
        }
        let mut lookup = vec![0u32; precision as usize];
        for (symbol, p) in lookup_symbols {
            let start = table[symbol].1 as usize;
            lookup[start..start + p as usize].fill(symbol as u32);
        }
        let size = r.varint()? as usize;
        let data = r.take(size)?;
        let Some(last) = data.last() else { return fail("Draco rANS data is empty") };
        let width = (*last >> 6) as usize + 1;
        if width > size {
            return fail("Draco rANS data is cut short");
        }
        let at = size - width;
        let mut head = 0u64;
        for (i, b) in data[at..].iter().enumerate() {
            head |= (*b as u64) << (8 * i);
        }
        let state = (head & ((1u64 << (8 * width - 2)) - 1)) + 4 * precision;
        if state >= 4 * precision * 256 {
            return fail("Draco rANS state is out of range");
        }
        Ok(Self { precision, lookup, table, data, at, state })
    }
    fn read(&mut self) -> u32 {
        let low = 4 * self.precision;
        while self.state < low && self.at > 0 {
            self.at -= 1;
            self.state = (self.state << 8) | self.data[self.at] as u64;
        }
        let quotient = self.state / self.precision;
        let slot = self.state % self.precision;
        let symbol = self.lookup[slot as usize];
        let (p, start) = self.table[symbol as usize];
        self.state = quotient * p + slot - start;
        symbol
    }
}

fn symbols(r: &mut Reader<'_>, n: usize, components: usize) -> Result<Vec<u32>> {
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(n);
    match r.u8()? {
        0 => {
            let mut tags = Rans::new(r, 5)?;
            let start = r.at;
            let limit = (r.end - start) * 8;
            let mut bit = 0usize;
            while out.len() < n {
                let length = tags.read() as usize;
                for _ in 0..components.min(n - out.len()) {
                    if length > 32 || bit + length > limit {
                        return fail("Draco bits are cut short");
                    }
                    let mut value = 0u32;
                    for i in 0..length {
                        value |= (((r.data[start + (bit >> 3)] >> (bit & 7)) & 1) as u32) << i;
                        bit += 1;
                    }
                    out.push(value);
                }
            }
            r.take(bit.div_ceil(8))?;
        }
        1 => {
            let length = r.u8()?;
            if !(1..=18).contains(&length) {
                return fail(format!("{length}-bit Draco symbols"));
            }
            let mut decoder = Rans::new(r, length as u32)?;
            for _ in 0..n {
                out.push(decoder.read());
            }
        }
        scheme => return fail(format!("Draco symbol scheme {scheme}")),
    }
    Ok(out)
}

fn connectivity(r: &mut Reader<'_>, faces: usize, points: usize) -> Result<Vec<u32>> {
    if faces == 0 || points == 0 {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(faces * 3);
    match r.u8()? {
        0 => {
            let mut last: i64 = 0;
            for s in symbols(r, faces * 3, 1)? {
                let diff = (s >> 1) as i64;
                last += if s & 1 != 0 { -diff } else { diff };
                if last < 0 {
                    return fail("a Draco index is negative");
                }
                out.push(last as u32);
            }
        }
        1 => {
            for _ in 0..faces * 3 {
                out.push(if points < 256 {
                    r.u8()? as u32
                } else if points < 65536 {
                    r.u16()? as u32
                } else if points < 2_097_152 {
                    r.varint()? as u32
                } else {
                    r.u32()?
                });
            }
        }
        method => return fail(format!("Draco connectivity {method}")),
    }
    Ok(out)
}

const POSITION: u8 = 0;
const NORMAL: u8 = 1;
const TEX_COORD: u8 = 3;
const GENERIC: u8 = 4;
const FLOAT32: u8 = 9;

struct Attribute {
    kind: u8,
    data_type: u8,
    components: usize,
    coding: u8,
    values: Vec<f64>,
    integers: Vec<i64>,
}

fn raw_size(data_type: u8) -> Option<usize> {
    Some(match data_type {
        1 | 2 | 11 => 1,
        3 | 4 => 2,
        5 | 6 | FLOAT32 => 4,
        7 | 8 | 10 => 8,
        _ => return None,
    })
}

fn values(r: &mut Reader<'_>, attr: &mut Attribute, points: usize) -> Result<()> {
    let n = points * attr.components;
    if attr.coding == 0 {
        let size = raw_size(attr.data_type).ok_or_else(|| DracoError(format!("Draco data type {}", attr.data_type)))?;
        let raw = r.take(n * size)?;
        if attr.data_type == FLOAT32 {
            attr.values = raw.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap()) as f64).collect();
        }
        return Ok(());
    }
    let wrap = match r.i8()? {
        -2 => false,
        0 => {
            let transform = r.i8()?;
            if transform != 1 {
                return fail(format!("Draco transform {transform}"));
            }
            true
        }
        prediction => return fail(format!("Draco prediction {prediction}")),
    };
    let raw_symbols: Vec<u32> = if r.u8()? > 0 {
        symbols(r, n, attr.components)?
    } else {
        let size = r.u8()? as usize;
        if !(1..=4).contains(&size) {
            return fail(format!("{size}-byte Draco values"));
        }
        let raw = r.take(n * size)?;
        raw.chunks_exact(size)
            .map(|c| c.iter().enumerate().fold(0u32, |v, (i, b)| v | (*b as u32) << (8 * i)))
            .collect()
    };
    let corrections: Vec<i64> = raw_symbols
        .iter()
        .map(|s| if s & 1 != 0 { -((*s >> 1) as i64) - 1 } else { (*s >> 1) as i64 })
        .collect();
    if wrap {
        let (lo, hi) = (r.i32()? as i64, r.i32()? as i64);
        if lo > hi {
            return fail("a Draco wrap range runs backwards");
        }
        let span = hi - lo + 1;
        let c = attr.components;
        let mut out: Vec<i64> = Vec::with_capacity(corrections.len());
        for (i, corr) in corrections.iter().enumerate() {
            let predicted = if i >= c { out[i - c] } else { 0 };
            let mut v = predicted.clamp(lo, hi) + corr;
            if v > hi {
                v -= span;
            } else if v < lo {
                v += span;
            }
            out.push(v);
        }
        attr.integers = out;
    } else {
        attr.integers = corrections;
    }
    Ok(())
}

fn dequantize(r: &mut Reader<'_>, attr: &mut Attribute) -> Result<()> {
    let c = attr.components;
    let mut mins = Vec::with_capacity(c);
    for _ in 0..c {
        mins.push(r.f32()? as f64);
    }
    let range = r.f32()? as f64;
    let bits = r.u8()?;
    if !(1..=31).contains(&bits) {
        return fail(format!("{bits}-bit Draco quantization"));
    }
    let delta = range / ((1u64 << bits) - 1) as f64;
    attr.values = attr.integers.iter().enumerate().map(|(i, q)| *q as f64 * delta + mins[i % c]).collect();
    attr.integers.clear();
    Ok(())
}

fn attributes(r: &mut Reader<'_>, points: usize) -> Result<Vec<Attribute>> {
    let mut out: Vec<Attribute> = Vec::new();
    for _ in 0..r.u8()? {
        let first = out.len();
        let varint = r.varint()?;
        for _ in 0..r.count(Some(varint))? {
            let (kind, data_type, components) = (r.u8()?, r.u8()?, r.u8()? as usize);
            r.u8()?; // normalized
            r.varint()?; // unique id
            if components == 0 {
                return fail("a Draco attribute has no components");
            }
            out.push(Attribute { kind, data_type, components, coding: 0, values: Vec::new(), integers: Vec::new() });
        }
        for attr in &mut out[first..] {
            let coding = r.u8()?;
            if coding > 2 {
                return fail(format!("Draco attribute coding {coding}"));
            }
            attr.coding = coding;
        }
        for attr in &mut out[first..] {
            values(r, attr, points)?;
            if attr.coding == 1 {
                attr.integers.clear();
            }
        }
        for attr in &mut out[first..] {
            if attr.coding == 2 {
                dequantize(r, attr)?;
            }
        }
    }
    Ok(out)
}

/// Decode a Draco 2.2 sequential mesh.
pub fn decode_draco(data: &[u8]) -> Result<ChunkedMesh> {
    let mut r = Reader::new(data);
    if r.take(5)? != b"DRACO" {
        return fail("COREMESH isn't Draco");
    }
    let (major, minor) = (r.u8()?, r.u8()?);
    if (major, minor) != (2, 2) {
        return fail(format!("Draco {major}.{minor}"));
    }
    let (kind, method, flags) = (r.u8()?, r.u8()?, r.u16()?);
    if kind != 1 || method != 0 {
        return fail(format!("Draco encoding {kind}/{method}"));
    }
    if flags != 0 {
        return fail(format!("Draco flags {flags:#x}"));
    }
    let v = r.varint()?;
    let faces = r.count(Some(v))?;
    let v = r.varint()?;
    let points = r.count(Some(v))?;
    let indices = connectivity(&mut r, faces, points)?;
    let attrs = attributes(&mut r, points)?;
    let find = |kinds: &[u8], components: usize, what: &str| -> Result<Vec<f64>> {
        attrs
            .iter()
            .find(|a| kinds.contains(&a.kind) && a.data_type == FLOAT32 && a.components == components)
            .map(|a| a.values.clone())
            .ok_or_else(|| DracoError(format!("the Draco mesh has no {what}")))
    };
    // Roblox stores normals as a generic attribute, not Draco's normal type.
    Ok(ChunkedMesh {
        positions: find(&[POSITION], 3, "positions")?,
        normals: find(&[NORMAL, GENERIC], 3, "normals")?,
        uvs: find(&[TEX_COORD], 2, "UVs")?,
        indices,
    })
}

/// Decode a 6.00/7.00 body (after the version line), highest level of detail.
pub fn decode_chunked(body: &[u8]) -> Result<ChunkedMesh> {
    let mut r = Reader::new(body);
    let mut mesh: Option<(Vec<f64>, Vec<f64>, Vec<f64>)> = None;
    let mut faces: Vec<u32> = Vec::new();
    let mut lods: Vec<u32> = Vec::new();
    while r.remaining() > 0 {
        let name = r.take(8)?;
        let version = r.u32()?;
        let size = r.u32()? as usize;
        let start = r.at;
        r.take(size)?;
        let mut chunk = Reader { data: body, at: start, end: start + size };
        match (name, version) {
            (b"COREMESH", 1) => {
                let vertices = chunk.count(None)?;
                let raw = chunk.take(vertices * 40)?;
                let (mut positions, mut normals, mut uvs) = (Vec::new(), Vec::new(), Vec::new());
                for v in raw.chunks_exact(40) {
                    let f = |i: usize| f32::from_le_bytes(v[i * 4..i * 4 + 4].try_into().unwrap()) as f64;
                    positions.extend([f(0), f(1), f(2)]);
                    normals.extend([f(3), f(4), f(5)]);
                    uvs.extend([f(6), f(7)]);
                }
                let count = chunk.count(None)?;
                faces = chunk
                    .take(count * 12)?
                    .chunks_exact(4)
                    .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                    .collect();
                mesh = Some((positions, normals, uvs));
            }
            (b"COREMESH", 2) => {
                let length = chunk.u32()? as usize;
                let decoded = decode_draco(chunk.take(length)?)?;
                faces = decoded.indices;
                mesh = Some((decoded.positions, decoded.normals, decoded.uvs));
            }
            (b"COREMESH", other) => return fail(format!("COREMESH {other}")),
            (b"LODS\0\0\0\0", 1) => {
                chunk.u16()?; // LOD type
                chunk.u8()?; // high quality LOD count
                let count = chunk.count(None)?;
                lods = chunk
                    .take(count * 4)?
                    .chunks_exact(4)
                    .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                    .collect();
            }
            _ => {}
        }
    }
    let Some((positions, normals, uvs)) = mesh else { return fail("no COREMESH chunk") };
    let face_count = (faces.len() / 3) as u32;
    let mut end = face_count;
    if lods.len() >= 2 && lods[0] <= lods[1] && lods[1] <= face_count && lods[1] > lods[0] {
        end = lods[1];
    }
    faces.truncate(end as usize * 3);
    let vertices = (positions.len() / 3) as u32;
    if faces.iter().any(|i| *i >= vertices) {
        return fail("a mesh index is out of range");
    }
    Ok(ChunkedMesh { positions, normals, uvs, indices: faces })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(name: &[u8; 8], version: u32, body: &[u8]) -> Vec<u8> {
        let mut out = name.to_vec();
        out.extend(version.to_le_bytes());
        out.extend((body.len() as u32).to_le_bytes());
        out.extend(body);
        out
    }

    #[test]
    fn plain_coremesh_with_lods_uses_the_highest_detail() {
        let mut core = 3u32.to_le_bytes().to_vec();
        for v in 0..3 {
            for f in [v as f32, 0.0, 0.0, 0.0, 1.0, 0.0, 0.5, 0.5, 0.0, 0.0] {
                core.extend(f.to_le_bytes());
            }
        }
        core.extend(2u32.to_le_bytes());
        for i in [0u32, 1, 2, 2, 1, 0] {
            core.extend(i.to_le_bytes());
        }
        let mut lods = vec![0u8, 0, 1];
        lods.extend(3u32.to_le_bytes());
        for v in [0u32, 1, 2] {
            lods.extend(v.to_le_bytes());
        }
        let mut body = chunk(b"COREMESH", 1, &core);
        body.extend(chunk(b"LODS\0\0\0\0", 1, &lods));
        let mesh = decode_chunked(&body).unwrap();
        assert_eq!(mesh.positions.len(), 9);
        assert_eq!(mesh.indices, vec![0, 1, 2]);
    }

    #[test]
    fn other_draco_modes_are_refused() {
        let mut draco = b"DRACO".to_vec();
        draco.extend([2, 2, 1, 1, 0, 0]); // edgebreaker
        let mut core = (draco.len() as u32).to_le_bytes().to_vec();
        core.extend(&draco);
        let err = decode_chunked(&chunk(b"COREMESH", 2, &core)).err().unwrap();
        assert_eq!(err.0, "Draco encoding 1/1");
        assert!(decode_chunked(b"COREMESH").is_err());
    }
}
