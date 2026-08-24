//! An axis-aligned box in the cloud's own coordinates.

/// Minimum and maximum corner, in the file's CRS units.
///
/// Two of these describe a cloud and they are not the same box, which is the
/// distinction the whole project is careful about: the *indexing* volume an
/// octree subdivides is a cube, and on a real survey it can be twenty times
/// taller than the points inside it. Camera framing and elevation ramps want
/// the tight one. See [`CloudInfo`](crate::cloud::CloudInfo).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

impl Bounds {
    /// The box that contains nothing: `min` at `+inf`, `max` at `-inf`.
    ///
    /// Growing this by any point yields that point, which is what makes a
    /// streaming pass over a file need no special case for the first one.
    pub const EMPTY: Self = Self {
        min: [f64::INFINITY; 3],
        max: [f64::NEG_INFINITY; 3],
    };

    pub fn new(min: [f64; 3], max: [f64; 3]) -> Self {
        Self { min, max }
    }

    /// True when no point has been added, or when an axis is inverted.
    pub fn is_empty(&self) -> bool {
        (0..3).any(|i| self.min[i] > self.max[i])
    }

    pub fn size(&self) -> [f64; 3] {
        [
            self.max[0] - self.min[0],
            self.max[1] - self.min[1],
            self.max[2] - self.min[2],
        ]
    }

    pub fn center(&self) -> [f64; 3] {
        [
            (self.min[0] + self.max[0]) * 0.5,
            (self.min[1] + self.max[1]) * 0.5,
            (self.min[2] + self.max[2]) * 0.5,
        ]
    }

    /// The longest edge. The side of the cube in [`cube`](Self::cube).
    pub fn longest_edge(&self) -> f64 {
        let s = self.size();
        s[0].max(s[1]).max(s[2])
    }

    pub fn grow(&mut self, p: [f64; 3]) {
        for (axis, value) in p.into_iter().enumerate() {
            if value < self.min[axis] {
                self.min[axis] = value;
            }
            if value > self.max[axis] {
                self.max[axis] = value;
            }
        }
    }

    pub fn union(&self, other: &Self) -> Self {
        let mut out = *self;
        out.grow(other.min);
        out.grow(other.max);
        out
    }

    pub fn contains(&self, p: [f64; 3]) -> bool {
        (0..3).all(|i| p[i] >= self.min[i] && p[i] <= self.max[i])
    }

    /// The cube this box is indexed in: anchored at this box's minimum, with
    /// every edge equal to the longest.
    ///
    /// Every octree format here subdivides a cube — Potree v2, COPC and EPT
    /// alike — because a non-cubic root would make a child's aspect ratio
    /// depend on its depth, and the spacing-halving-per-level rule the LOD
    /// metric rests on would stop being true.
    ///
    /// **Anchored rather than centred**, which is PotreeConverter's convention
    /// and not an arbitrary match: the origin a file quantizes against is its
    /// own minimum, and a cube that starts there keeps the two grids aligned.
    /// A centred cube shifts the octree by half the slack on every axis, which
    /// changes which node a point falls in without changing anything a reader
    /// could check.
    ///
    /// Not grown by an epsilon: a point exactly on the maximum face lands in
    /// the last cell because the cell index is clamped and the child index
    /// tests `>=`, so the face is inside by construction rather than by a fudge
    /// that would also perturb the spacing.
    pub fn index_cube(&self) -> Self {
        let edge = self.longest_edge();
        Self {
            min: self.min,
            max: [self.min[0] + edge, self.min[1] + edge, self.min[2] + edge],
        }
    }
}
