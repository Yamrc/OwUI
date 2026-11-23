//! Logical geometry and exact half-open clipping endpoints.

/// A position or displacement in logical coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const ZERO: Self = Self::new(0.0, 0.0);

    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    pub(crate) fn add(self, other: Self) -> Self {
        Self::new(self.x + other.x, self.y + other.y)
    }
}

/// A nonnegative extent in logical coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

impl Size {
    pub const ZERO: Self = Self::new(0.0, 0.0);

    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }

    pub fn is_valid(self) -> bool {
        self.width.is_finite() && self.height.is_finite() && self.width >= 0.0 && self.height >= 0.0
    }
}

/// An axis-aligned origin-and-size rectangle in logical coordinates.
///
/// Membership is half-open at the upper endpoints computed from origin plus size.
/// This representation cannot encode every pair of f32 endpoints. Clipping covers
/// can therefore be wider than exact membership; use [`ClipBounds`] for endpoint
/// intersection and clipping decisions that must not include an exclusive boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

impl Rect {
    pub const fn new(origin: Point, size: Size) -> Self {
        Self { origin, size }
    }

    pub fn contains(self, point: Point) -> bool {
        point.x >= self.origin.x
            && point.y >= self.origin.y
            && point.x < self.origin.x + self.size.width
            && point.y < self.origin.y + self.size.height
    }

    /// Returns a conservative representable cover of the intersection.
    ///
    /// An origin and an f32 extent cannot encode every pair of f32 endpoints.
    /// Use [`ClipBounds::intersect`] when exact half-open membership is required.
    pub fn intersect(self, other: Self) -> Self {
        ClipBounds::from(self)
            .intersect(ClipBounds::from(other))
            .to_rect()
            .unwrap_or(self)
    }

    pub(crate) fn is_valid(self) -> bool {
        self.origin.x.is_finite()
            && self.origin.y.is_finite()
            && self.size.is_valid()
            && (self.origin.x + self.size.width).is_finite()
            && (self.origin.y + self.size.height).is_finite()
    }
}

/// Half-open clipping endpoints kept without subtract-and-reconstruct rounding.
///
/// `min` is inclusive and `max` is exclusive. Exact endpoint intersection and
/// membership do not depend on converting the interval back to an f32 extent.
/// [`Geometry::clip`] and other rectangle clipping views are conservative covers;
/// a cover can include points excluded by these endpoints.
///
/// Conversion from [`Rect`] computes the same f32 upper endpoints as that rectangle.
/// Construction/conversion does not validate arbitrary inputs; logical geometry
/// requires finite, ordered endpoints. [`to_rect`](Self::to_rect) reports when a
/// finite origin-and-size cover cannot be represented.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ClipBounds {
    pub min: Point,
    pub max: Point,
}

impl From<Rect> for ClipBounds {
    fn from(rect: Rect) -> Self {
        Self {
            min: rect.origin,
            max: Point::new(
                rect.origin.x + rect.size.width,
                rect.origin.y + rect.size.height,
            ),
        }
    }
}

impl ClipBounds {
    /// Tests membership against inclusive lower and exclusive upper endpoints.
    pub fn contains(self, point: Point) -> bool {
        point.x >= self.min.x
            && point.y >= self.min.y
            && point.x < self.max.x
            && point.y < self.max.y
    }

    pub fn is_empty(self) -> bool {
        self.max.x <= self.min.x || self.max.y <= self.min.y
    }

    /// Intersects endpoint intervals without an intermediate origin/extent conversion.
    ///
    /// Disjoint axes become empty at the resulting lower endpoint.
    pub fn intersect(self, other: Self) -> Self {
        let min = Point::new(self.min.x.max(other.min.x), self.min.y.max(other.min.y));
        let max = Point::new(
            self.max.x.min(other.max.x).max(min.x),
            self.max.y.min(other.max.y).max(min.y),
        );
        Self { min, max }
    }

    /// Converts to a conservative rectangle with the same lower endpoints.
    ///
    /// Returns None for invalid endpoints or when the required cover has no finite
    /// f32 extent. A successful cover may include the exclusive upper endpoint;
    /// exact clipping and hit testing use these endpoints rather than that cover.
    pub fn to_rect(self) -> Option<Rect> {
        if !self.is_valid() {
            return None;
        }

        let extent = |min: f32, max: f32| {
            let mut extent = max - min;
            if min + extent < max {
                extent = extent.next_up();
            }
            (extent.is_finite() && (min + extent).is_finite() && min + extent >= max)
                .then_some(extent)
        };

        Some(Rect::new(
            self.min,
            Size::new(
                extent(self.min.x, self.max.x)?,
                extent(self.min.y, self.max.y)?,
            ),
        ))
    }

    pub(crate) fn is_valid(self) -> bool {
        self.min.x.is_finite()
            && self.min.y.is_finite()
            && self.max.x.is_finite()
            && self.max.y.is_finite()
            && self.max.x >= self.min.x
            && self.max.y >= self.min.y
    }
}

/// Minimum and maximum sizes accepted by a parent layout.
///
/// Minimum extents must be finite and nonnegative. Maximum extents may be positive
/// infinity to leave an axis unbounded, but not NaN or smaller than the minimum.
/// Layout results themselves must be finite. Constructors and `constrain` do not
/// replace validation at the measurement boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Constraints {
    pub min: Size,
    pub max: Size,
}

impl Constraints {
    pub const fn new(min: Size, max: Size) -> Self {
        Self { min, max }
    }

    pub const fn tight(size: Size) -> Self {
        Self::new(size, size)
    }

    pub const fn loose(max: Size) -> Self {
        Self::new(Size::ZERO, max)
    }

    pub fn is_valid(self) -> bool {
        self.min.is_valid()
            && !self.max.width.is_nan()
            && !self.max.height.is_nan()
            && self.max.width >= self.min.width
            && self.max.height >= self.min.height
    }

    pub fn constrain(self, size: Size) -> Size {
        Size::new(
            size.width.max(self.min.width).min(self.max.width),
            size.height.max(self.min.height).min(self.max.height),
        )
    }
}

/// Local layout data and its world-space attachment.
///
/// `origin` is relative to the parent; `bounds` and `clip` are world-space views.
/// `clip` is a conservative rectangle cover, not an exact endpoint intersection.
/// Use the window's [`clip_bounds`](crate::application::App::clip_bounds) read for
/// exact clipping membership. `content_size` is the local content extent and can
/// exceed the allocated layout `size`. This value alone is not a Ready-epoch proof.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Geometry {
    pub origin: Point,
    pub size: Size,
    pub bounds: Rect,
    pub clip: Rect,
    pub content_size: Size,
}
