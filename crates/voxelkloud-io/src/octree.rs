//! The octree every format here subdivides, and the two ways it is addressed.
//!
//! Potree v2, COPC and EPT partition space identically: a cube, halved on each
//! axis, one level at a time. What differs is the *naming* — Potree spells a
//! node as a string of child indices (`"r047"`), COPC and EPT as a
//! `(level, x, y, z)` key — and the two are the same information in two
//! alphabets, so the conversion is exact and lives here.
//!
//! Getting the child-index bit convention backwards misaligns traversal
//! against the file's own tree without failing, so it is pinned to the
//! reference implementation rather than re-derived.

use crate::bounds::Bounds;

/// Bit convention confirmed against `demo/potree`'s `createChildAABB`
/// (`src/modules/loader/2.0/OctreeLoader.js`): bit 0 is Z, bit 1 is Y,
/// bit 2 is X.
pub fn child_bounds(box_: &Bounds, child: u8) -> Bounds {
    let size = box_.size();
    let mut min = box_.min;
    let mut max = box_.max;

    if child & 0b001 != 0 {
        min[2] += size[2] * 0.5;
    } else {
        max[2] -= size[2] * 0.5;
    }
    if child & 0b010 != 0 {
        min[1] += size[1] * 0.5;
    } else {
        max[1] -= size[1] * 0.5;
    }
    if child & 0b100 != 0 {
        min[0] += size[0] * 0.5;
    } else {
        max[0] -= size[0] * 0.5;
    }
    Bounds { min, max }
}

/// Which child of its parent a point falls in, under the same convention.
pub fn child_index(center: [f64; 3], p: [f64; 3]) -> u8 {
    let mut index = 0u8;
    if p[0] >= center[0] {
        index |= 0b100;
    }
    if p[1] >= center[1] {
        index |= 0b010;
    }
    if p[2] >= center[2] {
        index |= 0b001;
    }
    index
}

/// A node's address as a level and a grid position, which is how COPC and EPT
/// spell it: `1-0-1-0` in EPT, four `i32`s in a COPC hierarchy entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OctreeKey {
    pub level: u32,
    pub x: u32,
    pub y: u32,
    pub z: u32,
}

impl OctreeKey {
    pub const ROOT: Self = Self {
        level: 0,
        x: 0,
        y: 0,
        z: 0,
    };

    pub fn new(level: u32, x: u32, y: u32, z: u32) -> Self {
        Self { level, x, y, z }
    }

    /// The child at `index`, under the bit convention above.
    pub fn child(self, index: u8) -> Self {
        Self {
            level: self.level + 1,
            x: self.x * 2 + u32::from(index >> 2 & 1),
            y: self.y * 2 + u32::from(index >> 1 & 1),
            z: self.z * 2 + u32::from(index & 1),
        }
    }

    /// Which child of its parent this is, or `None` at the root.
    pub fn child_index(self) -> Option<u8> {
        if self.level == 0 {
            return None;
        }
        Some(((self.x & 1) << 2 | (self.y & 1) << 1 | (self.z & 1)) as u8)
    }

    pub fn parent(self) -> Option<Self> {
        if self.level == 0 {
            return None;
        }
        Some(Self {
            level: self.level - 1,
            x: self.x / 2,
            y: self.y / 2,
            z: self.z / 2,
        })
    }

    /// This node's box inside the cloud's root cube.
    pub fn bounds(self, root: &Bounds) -> Bounds {
        let size = root.size();
        let scale = 1.0 / f64::from(1u32 << self.level.min(31));
        let step = [size[0] * scale, size[1] * scale, size[2] * scale];
        let min = [
            root.min[0] + step[0] * f64::from(self.x),
            root.min[1] + step[1] * f64::from(self.y),
            root.min[2] + step[2] * f64::from(self.z),
        ];
        Bounds {
            min,
            max: [min[0] + step[0], min[1] + step[1], min[2] + step[2]],
        }
    }

    /// The EPT spelling: `level-x-y-z`, which is also its filename stem.
    pub fn ept_name(self) -> String {
        format!("{}-{}-{}-{}", self.level, self.x, self.y, self.z)
    }

    /// The Potree spelling: `"r"` then one digit per level, each the child
    /// index taken from the *top* down.
    ///
    /// This is where the two alphabets meet. The digits are the path, so they
    /// are recovered by walking the key's bits from the most significant end —
    /// the bit at position `level - 1 - depth` of each axis.
    pub fn potree_name(self) -> String {
        let mut name = String::with_capacity(self.level as usize + 1);
        name.push('r');
        for depth in 0..self.level {
            let shift = self.level - 1 - depth;
            let index = ((self.x >> shift & 1) << 2) | ((self.y >> shift & 1) << 1) | (self.z >> shift & 1);
            name.push(char::from(b'0' + index as u8));
        }
        name
    }

    /// The inverse of [`potree_name`](Self::potree_name).
    pub fn from_potree_name(name: &str) -> Option<Self> {
        let mut chars = name.chars();
        if chars.next() != Some('r') {
            return None;
        }
        let mut key = Self::ROOT;
        for c in chars {
            let index = c.to_digit(8)? as u8;
            key = key.child(index);
        }
        Some(key)
    }
}

/// Interleave three 10-bit integers into a 30-bit Morton code.
pub fn morton_encode3(x: u32, y: u32, z: u32) -> u32 {
    spread_bits3(x) | (spread_bits3(y) << 1) | (spread_bits3(z) << 2)
}

pub fn morton_decode3(code: u32) -> [u32; 3] {
    [
        compact_bits3(code),
        compact_bits3(code >> 1),
        compact_bits3(code >> 2),
    ]
}

fn spread_bits3(value: u32) -> u32 {
    let mut x = value & 0x3ff;
    x = (x | (x << 16)) & 0x30000ff;
    x = (x | (x << 8)) & 0x300f00f;
    x = (x | (x << 4)) & 0x30c30c3;
    x = (x | (x << 2)) & 0x9249249;
    x
}

fn compact_bits3(value: u32) -> u32 {
    let mut x = value & 0x9249249;
    x = (x | (x >> 2)) & 0x30c30c3;
    x = (x | (x >> 4)) & 0x300f00f;
    x = (x | (x >> 8)) & 0x30000ff;
    x = (x | (x >> 16)) & 0x3ff;
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_bounds_partition_the_parent() {
        let root = Bounds::new([0.0, 0.0, 0.0], [8.0, 8.0, 8.0]);
        let mut volume = 0.0;
        for child in 0..8u8 {
            let b = child_bounds(&root, child);
            let s = b.size();
            volume += s[0] * s[1] * s[2];
            assert!(root.contains(b.center()));
        }
        assert_eq!(volume, 8.0 * 8.0 * 8.0);
    }

    #[test]
    fn child_index_inverts_child_bounds() {
        let root = Bounds::new([0.0, 0.0, 0.0], [8.0, 8.0, 8.0]);
        for child in 0..8u8 {
            let b = child_bounds(&root, child);
            assert_eq!(child_index(root.center(), b.center()), child);
        }
    }

    #[test]
    fn key_bounds_agree_with_repeated_subdivision() {
        let root = Bounds::new([-10.0, 5.0, 100.0], [6.0, 21.0, 116.0]);
        // r047: down three levels through children 0, 4 and 7.
        let mut expect = root;
        for child in [0u8, 4, 7] {
            expect = child_bounds(&expect, child);
        }
        let key = OctreeKey::from_potree_name("r047").unwrap();
        let got = key.bounds(&root);
        for axis in 0..3 {
            assert!((got.min[axis] - expect.min[axis]).abs() < 1e-9);
            assert!((got.max[axis] - expect.max[axis]).abs() < 1e-9);
        }
    }

    #[test]
    fn potree_and_ept_names_are_the_same_node() {
        let key = OctreeKey::from_potree_name("r047").unwrap();
        assert_eq!(key.level, 3);
        assert_eq!(key.potree_name(), "r047");
        // 0 -> (0,0,0), 4 -> x, 7 -> x,y,z: x = 0b011, y = 0b001, z = 0b001.
        assert_eq!(key.ept_name(), "3-3-1-1");
        assert_eq!(key.child_index(), Some(7));
        assert_eq!(key.parent().unwrap().potree_name(), "r04");
    }

    #[test]
    fn morton_round_trips() {
        for (x, y, z) in [(0, 0, 0), (1, 2, 3), (1023, 1023, 1023), (5, 0, 1023)] {
            assert_eq!(morton_decode3(morton_encode3(x, y, z)), [x, y, z]);
        }
    }
}
