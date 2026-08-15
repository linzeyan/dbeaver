//! `geometry`, `geography` and `hierarchyid`, turned into the text a person
//! reads.
//!
//! These three arrive as CLR user-defined types: one TDS type byte, one opaque
//! byte string per value, and a type name in `COLMETADATA` that is the only
//! thing saying which of them a column holds. tiberius hands the bytes over
//! (see `third_party/tiberius`); deciding what a cell should show is this
//! driver's job, and it is decided here.
//!
//! **What a cell shows, and why it is not the bytes.** A `geography` renders as
//! its WKT — `POINT (-122.35 47.62)` — and a `hierarchyid` as its path,
//! `/1/2/3/`. Those are the forms SQL Server itself produces from `.ToString()`,
//! which means the grid agrees with what somebody would see in SSMS, in a query
//! they wrote by hand, and in every piece of documentation about these types.
//! The serialized bytes are none of those things: they are a private structure
//! with a version byte, a figure table and a shape tree, and rendering them as
//! hex would be showing a person the envelope instead of the letter. The one
//! thing hex has going for it is that it cannot be wrong, which is why anything
//! this cannot decode becomes an error naming the type rather than a plausible
//! string.
//!
//! The formats are Microsoft's, published as [MS-SSCLRT]: section 2.1 for the
//! spatial structure and 2.2 for `hierarchyid`. Both are read strictly —
//! offsets are bounds-checked and antiambiguity bits are verified — because
//! these bytes come off a network and this decoder is the reason a panic in one
//! no longer ends the process.

use std::fmt::Write;

use crate::MsSqlError;

/// Which CLR type a `Udt` column holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdtKind {
    Geometry,
    Geography,
    HierarchyId,
}

impl UdtKind {
    /// The kind a CLR type name names, or `None` for a type somebody registered
    /// themselves, whose bytes mean nothing here.
    pub fn from_type_name(name: &str) -> Option<Self> {
        // Case-insensitively, because the name is written in whatever collation
        // the database has rather than in a spelling this side chose.
        if name.eq_ignore_ascii_case("geometry") {
            Some(Self::Geometry)
        } else if name.eq_ignore_ascii_case("geography") {
            Some(Self::Geography)
        } else if name.eq_ignore_ascii_case("hierarchyid") {
            Some(Self::HierarchyId)
        } else {
            None
        }
    }

    fn sql_type(self) -> &'static str {
        match self {
            Self::Geometry => "geometry",
            Self::Geography => "geography",
            Self::HierarchyId => "hierarchyid",
        }
    }
}

/// The text for one UDT value, or `None` where the value says it is null.
pub fn to_text(kind: UdtKind, bytes: &[u8]) -> Result<Option<String>, MsSqlError> {
    let outcome = match kind {
        UdtKind::Geometry => spatial_text(bytes, false),
        UdtKind::Geography => spatial_text(bytes, true),
        UdtKind::HierarchyId => hierarchy_path(bytes).map(Some),
    };
    outcome.map_err(|reason| MsSqlError::UndecodableValue {
        sql_type: kind.sql_type(),
        reason,
    })
}

// --- geometry and geography -------------------------------------------------

// Serialization Properties, MS-SSCLRT 2.1.1 and 2.1.2.
const HAS_Z: u8 = 0x01;
const HAS_M: u8 = 0x02;
const SINGLE_POINT: u8 = 0x08;
const SINGLE_LINE: u8 = 0x10;

// OpenGIS types a shape can be, MS-SSCLRT 2.1.4.
const POINT: u8 = 1;
const LINESTRING: u8 = 2;
const POLYGON: u8 = 3;
const MULTIPOINT: u8 = 4;
const MULTILINESTRING: u8 = 5;
const MULTIPOLYGON: u8 = 6;
const GEOMETRYCOLLECTION: u8 = 7;
const CIRCULARSTRING: u8 = 8;
const COMPOUNDCURVE: u8 = 9;
const CURVEPOLYGON: u8 = 10;
const FULLGLOBE: u8 = 11;

/// A `GEOMETRYCOLLECTION` nests, and the depth is decided by bytes off a
/// network. Deep enough nesting would overflow the stack, which is the same
/// death by another name, so it is capped: nothing legitimate comes close, and
/// a value that does gets an error instead of a crash.
const MAX_NESTING: usize = 64;

fn spatial_text(bytes: &[u8], geography: bool) -> Result<Option<String>, String> {
    match Spatial::parse(bytes, geography)? {
        Some(shape) => shape.wkt().map(Some),
        None => Ok(None),
    }
}

struct Figure {
    point_offset: usize,
}

struct Shape {
    parent_offset: i32,
    figure_offset: i32,
    ogc_type: u8,
}

struct Spatial {
    /// Coordinate pairs exactly as stored: X then Y for a geometry, latitude
    /// then longitude for a geography.
    points: Vec<[f64; 2]>,
    z: Vec<f64>,
    m: Vec<f64>,
    figures: Vec<Figure>,
    shapes: Vec<Shape>,
    geography: bool,
}

impl Spatial {
    fn parse(bytes: &[u8], geography: bool) -> Result<Option<Self>, String> {
        let mut r = Reader::new(bytes);

        // SRID -1 is the format's own way of writing a null value. It is not how
        // a null column arrives — that is a null at the TDS level — but it is
        // reachable, and when it appears no other field is present.
        if r.i32()? == -1 {
            return Ok(None);
        }
        let _version = r.u8()?;
        let properties = r.u8()?;

        let single_point = properties & SINGLE_POINT != 0;
        let single_line = properties & SINGLE_LINE != 0;
        let point_count = if single_point {
            1
        } else if single_line {
            2
        } else {
            r.count("points")?
        };

        let mut points = Vec::with_capacity(point_count.min(r.remaining() / 16));
        for _ in 0..point_count {
            points.push([r.f64()?, r.f64()?]);
        }
        let z = r.doubles(point_count, properties & HAS_Z != 0)?;
        let m = r.doubles(point_count, properties & HAS_M != 0)?;

        // A single point and a single line segment are the format's two
        // shorthands: they omit the figure and shape tables entirely and mean a
        // fixed one of each. Filling them in here is what lets everything below
        // read one shape of code.
        let (figures, shapes) = if single_point || single_line {
            (
                vec![Figure { point_offset: 0 }],
                vec![Shape {
                    parent_offset: -1,
                    figure_offset: 0,
                    ogc_type: if single_point { POINT } else { LINESTRING },
                }],
            )
        } else {
            let figure_count = r.count("figures")?;
            let mut figures = Vec::with_capacity(figure_count.min(r.remaining() / 5));
            for _ in 0..figure_count {
                let _attribute = r.u8()?;
                figures.push(Figure {
                    point_offset: r.offset("figure point offset", points.len())?,
                });
            }

            let shape_count = r.count("shapes")?;
            let mut shapes = Vec::with_capacity(shape_count.min(r.remaining() / 9));
            for _ in 0..shape_count {
                shapes.push(Shape {
                    parent_offset: r.i32()?,
                    figure_offset: r.i32()?,
                    ogc_type: r.u8()?,
                });
            }
            (figures, shapes)
        };

        // Segments are read by nothing here. They describe which parts of a
        // compound curve are arcs, and a compound curve is one of the two shapes
        // this reports rather than renders, so there is no use for them.
        Ok(Some(Self {
            points,
            z,
            m,
            figures,
            shapes,
            geography,
        }))
    }

    fn wkt(&self) -> Result<String, String> {
        if self.shapes.is_empty() {
            return Err("no shapes".to_string());
        }
        let mut out = String::new();
        self.write_shape(0, 0, &mut out)?;
        Ok(out)
    }

    /// One shape with its type in front: `POINT (1 2)`.
    fn write_shape(&self, index: usize, depth: usize, out: &mut String) -> Result<(), String> {
        let shape = self
            .shapes
            .get(index)
            .ok_or_else(|| format!("shape {index} is past the end of the shape table"))?;
        out.push_str(keyword(shape.ogc_type)?);
        // The whole sphere has no coordinates, so `FULLGLOBE` stands alone with
        // no parenthesised body after it.
        if shape.ogc_type == FULLGLOBE {
            return Ok(());
        }
        out.push(' ');
        self.write_body(index, depth, out)
    }

    /// The parenthesised part of a shape, without its type: `(1 2)`.
    ///
    /// Separate from `write_shape` because that is exactly the difference
    /// between the members of a `MULTIPOINT`, which are written bare, and the
    /// members of a `GEOMETRYCOLLECTION`, which each keep their own type.
    fn write_body(&self, index: usize, depth: usize, out: &mut String) -> Result<(), String> {
        if depth > MAX_NESTING {
            return Err(format!("shapes nested more than {MAX_NESTING} deep"));
        }
        let shape = self
            .shapes
            .get(index)
            .ok_or_else(|| format!("shape {index} is past the end of the shape table"))?;
        let figures = self.figures_of(index)?;

        // Neither figures of its own nor shapes inside it is how the format
        // spells an empty geometry, and every OGC type has an empty form.
        if figures.is_empty() && self.children_of(index).next().is_none() {
            out.push_str("EMPTY");
            return Ok(());
        }

        match shape.ogc_type {
            POINT | LINESTRING | CIRCULARSTRING => {
                out.push('(');
                let mut first = true;
                for figure in figures.clone() {
                    for point in self.points_of(figure)? {
                        if !first {
                            out.push_str(", ");
                        }
                        first = false;
                        self.write_point(point, out);
                    }
                }
                out.push(')');
            }
            // Each figure is one ring: the exterior first, then the holes.
            POLYGON => {
                out.push('(');
                for (n, figure) in figures.clone().enumerate() {
                    if n > 0 {
                        out.push_str(", ");
                    }
                    out.push('(');
                    for (k, point) in self.points_of(figure)?.enumerate() {
                        if k > 0 {
                            out.push_str(", ");
                        }
                        self.write_point(point, out);
                    }
                    out.push(')');
                }
                out.push(')');
            }
            MULTIPOINT | MULTILINESTRING | MULTIPOLYGON => {
                out.push('(');
                for (n, child) in self.children_of(index).enumerate() {
                    if n > 0 {
                        out.push_str(", ");
                    }
                    self.write_body(child, depth + 1, out)?;
                }
                out.push(')');
            }
            // The one collection whose members keep their own type names, which
            // is what makes it a collection rather than a multi-anything.
            GEOMETRYCOLLECTION => {
                out.push('(');
                for (n, child) in self.children_of(index).enumerate() {
                    if n > 0 {
                        out.push_str(", ");
                    }
                    self.write_shape(child, depth + 1, out)?;
                }
                out.push(')');
            }
            other => return Err(format!("shape type {other} has no text form here")),
        }
        Ok(())
    }

    /// The figures belonging to one shape.
    ///
    /// A shape's figures run from its own offset to the offset of the next shape
    /// that has one; shapes with no figures of their own — the containers — are
    /// skipped over rather than treated as the end.
    fn figures_of(&self, index: usize) -> Result<std::ops::Range<usize>, String> {
        let start = self.shapes[index].figure_offset;
        if start < 0 {
            return Ok(0..0);
        }
        let start = start as usize;
        let end = self.shapes[index + 1..]
            .iter()
            .map(|s| s.figure_offset)
            .find(|offset| *offset >= 0)
            .map(|offset| offset as usize)
            .unwrap_or(self.figures.len());
        if start > end || end > self.figures.len() {
            return Err(format!(
                "shape {index} claims figures {start}..{end} of {}",
                self.figures.len()
            ));
        }
        Ok(start..end)
    }

    /// The points belonging to one figure, which run to wherever the next figure
    /// starts.
    fn points_of(&self, figure: usize) -> Result<std::ops::Range<usize>, String> {
        let start = self.figures[figure].point_offset;
        let end = self
            .figures
            .get(figure + 1)
            .map(|f| f.point_offset)
            .unwrap_or(self.points.len());
        if start > end || end > self.points.len() {
            return Err(format!(
                "figure {figure} claims points {start}..{end} of {}",
                self.points.len()
            ));
        }
        Ok(start..end)
    }

    /// The shapes this one contains.
    ///
    /// Always at a higher index than their parent, which is what makes the
    /// recursion in `write_body` terminate however the shape table is arranged.
    fn children_of(&self, index: usize) -> impl Iterator<Item = usize> + '_ {
        let parent = index as i32;
        (index + 1..self.shapes.len()).filter(move |i| self.shapes[*i].parent_offset == parent)
    }

    fn write_point(&self, index: usize, out: &mut String) {
        let [first, second] = self.points[index];
        // A geography stores latitude before longitude, and WKT writes longitude
        // before latitude. Getting this backwards puts every point in the wrong
        // hemisphere without ever looking wrong.
        let (x, y) = if self.geography {
            (second, first)
        } else {
            (first, second)
        };
        let _ = write!(out, "{x} {y}");

        // A point with an M but no Z still has to write a placeholder, or the M
        // would be read back as a Z. This is what SQL Server's own text does.
        let has_z = !self.z.is_empty();
        let has_m = !self.m.is_empty();
        if has_z || has_m {
            out.push(' ');
            out.push_str(&measure(has_z.then(|| self.z[index])));
        }
        if has_m {
            out.push(' ');
            out.push_str(&measure(Some(self.m[index])));
        }
    }
}

/// A Z or M value, where the format writes an absent one as a quiet NaN.
fn measure(value: Option<f64>) -> String {
    match value {
        Some(v) if !v.is_nan() => v.to_string(),
        _ => "NULL".to_string(),
    }
}

fn keyword(ogc_type: u8) -> Result<&'static str, String> {
    match ogc_type {
        POINT => Ok("POINT"),
        LINESTRING => Ok("LINESTRING"),
        POLYGON => Ok("POLYGON"),
        MULTIPOINT => Ok("MULTIPOINT"),
        MULTILINESTRING => Ok("MULTILINESTRING"),
        MULTIPOLYGON => Ok("MULTIPOLYGON"),
        GEOMETRYCOLLECTION => Ok("GEOMETRYCOLLECTION"),
        CIRCULARSTRING => Ok("CIRCULARSTRING"),
        FULLGLOBE => Ok("FULLGLOBE"),
        // Both need the segment table to say which stretches of a figure are
        // arcs and which are lines, and getting that wrong would draw a curve as
        // a straight line with no sign that anything was lost. Naming the shape
        // is the honest answer until somebody needs it enough to read segments.
        COMPOUNDCURVE => Err("a COMPOUNDCURVE cannot be rendered as text here".to_string()),
        CURVEPOLYGON => Err("a CURVEPOLYGON cannot be rendered as text here".to_string()),
        other => Err(format!("unknown OpenGIS shape type {other}")),
    }
}

/// Little-endian fields, read with the end of the buffer always in mind.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let end = self.at + N;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| format!("value ends after {} bytes", self.bytes.len()))?;
        self.at = end;
        Ok(slice.try_into().expect("the slice is N bytes long"))
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take::<1>()?[0])
    }

    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_le_bytes(self.take()?))
    }

    fn f64(&mut self) -> Result<f64, String> {
        Ok(f64::from_le_bytes(self.take()?))
    }

    /// A count of things, refused when it is negative.
    fn count(&mut self, what: &str) -> Result<usize, String> {
        let n = self.i32()?;
        usize::try_from(n).map_err(|_| format!("a negative number of {what} ({n})"))
    }

    /// An index into an array of `limit` entries.
    fn offset(&mut self, what: &str, limit: usize) -> Result<usize, String> {
        let n = self.i32()?;
        match usize::try_from(n) {
            Ok(offset) if offset <= limit => Ok(offset),
            _ => Err(format!("{what} {n} is outside 0..={limit}")),
        }
    }

    fn doubles(&mut self, count: usize, present: bool) -> Result<Vec<f64>, String> {
        if !present {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(count.min(self.remaining() / 8));
        for _ in 0..count {
            out.push(self.f64()?);
        }
        Ok(out)
    }
}

// --- hierarchyid ------------------------------------------------------------

/// The encoding of one integer in a `hierarchyid`, straight out of MS-SSCLRT
/// 2.2.2.
///
/// Written as the specification's own table so it can be checked against it
/// without translation. The first column is the L prefix; the second is the O
/// field that follows it, where `.` is a bit of the value and `0` or `1` is an
/// antiambiguity bit whose value is fixed — those exist so the encoding can be
/// parsed backwards, and a value that has them wrong is not a `hierarchyid`. The
/// third column is the bottom of the range the O field counts up from.
///
/// The prefixes form a prefix-free code, so at most one of them can match at any
/// position and the order of the rows does not matter.
const LEVELS: [(&str, &str, i64); 13] = [
    (
        "000100",
        "..............0.....................0......0...0.1...",
        -281_479_271_682_120,
    ),
    (
        "000101",
        "...................0......0...0.1...",
        -4_294_971_464,
    ),
    ("000110", ".....0...0.1...", -4_168),
    ("0010", "..0.1...", -72),
    ("00111", "...", -8),
    ("01", "..", 0),
    ("100", "..", 4),
    ("101", "...", 8),
    ("110", "..0.1...", 16),
    ("1110", "...0...0.1...", 80),
    ("11110", ".....0...0.1...", 1_104),
    ("111110", "...................0......0...0.1...", 5_200),
    (
        "111111",
        "..............0.....................0......0...0.1...",
        4_294_972_496,
    ),
];

/// A `hierarchyid` as its path: `/`, `/1/`, `/1/-2.18/`.
///
/// The path is what every part of SQL Server calls this value — it is what
/// `.ToString()` returns, what `hierarchyid::Parse` accepts back, and the only
/// form in which the position in the tree is legible at all.
fn hierarchy_path(bytes: &[u8]) -> Result<String, String> {
    let mut bits = Bits::new(bytes);
    let mut path = String::from("/");

    loop {
        // W: nought to seven zero bits, padding to a byte boundary. Every L
        // prefix in the table contains a 1, so a run of zeros to the end of the
        // value can only be that padding — which is the test, rather than a
        // count of bits, because the shortest level is five bits and would
        // otherwise be mistaken for it. A whole zero byte is refused: W is
        // shorter than that by definition, so a byte of it means this reader has
        // lost its place.
        let left = bits.remaining();
        if (0..left).all(|n| bits.peek(n) == Some(false)) {
            if left < 8 {
                return Ok(path);
            }
            return Err(format!("{left} zero bits, which is more padding than W is"));
        }

        let (prefix, layout, low) = bits.level()?;
        bits.skip(prefix.len());

        let mut value: i64 = 0;
        for marker in layout.chars() {
            let bit = bits
                .next()
                .ok_or_else(|| "a level ends before its value does".to_string())?;
            match marker {
                '.' => value = value * 2 + i64::from(bit),
                '0' if bit => return Err("an antiambiguity bit is 1, not 0".to_string()),
                '1' if !bit => return Err("an antiambiguity bit is 0, not 1".to_string()),
                _ => {}
            }
        }

        // A real level is followed by a slash and a fake one by a dot; a fake
        // level also encodes one more than the integer it stands for, so that
        // fake levels sort above real ones in a plain byte comparison.
        let real = bits
            .next()
            .ok_or_else(|| "a level ends before saying whether it is real".to_string())?;
        let label = low + value - i64::from(!real);
        let _ = write!(path, "{label}");
        path.push(if real { '/' } else { '.' });
    }
}

/// The bits of a `hierarchyid`, most significant first within each byte.
struct Bits<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Bits<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() * 8 - self.at
    }

    fn peek(&self, ahead: usize) -> Option<bool> {
        let index = self.at + ahead;
        let byte = self.bytes.get(index / 8)?;
        Some(byte >> (7 - index % 8) & 1 == 1)
    }

    fn skip(&mut self, count: usize) {
        self.at += count;
    }

    fn next(&mut self) -> Option<bool> {
        let bit = self.peek(0)?;
        self.at += 1;
        Some(bit)
    }

    /// The row of `LEVELS` whose prefix starts here.
    fn level(&self) -> Result<(&'static str, &'static str, i64), String> {
        LEVELS
            .into_iter()
            .find(|(prefix, _, _)| {
                prefix
                    .chars()
                    .enumerate()
                    .all(|(n, c)| self.peek(n) == Some(c == '1'))
            })
            .ok_or_else(|| "no level starts with these bits".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two worked examples in MS-SSCLRT 2.2, byte for byte.
    #[test]
    fn the_specifications_own_hierarchyid_examples_decode() {
        // 01011000: L1 01 (range 0..3), O1 01 (offset 1), F1 1 (real), W 000.
        assert_eq!(hierarchy_path(&[0b0101_1000]).unwrap(), "/1/");
        // 01011001 11111011 00000101 01000000, which the specification walks
        // through level by level. The middle level is fake, so its encoded -1
        // stands for -2, and the last one carries antiambiguity bits.
        assert_eq!(
            hierarchy_path(&[0b0101_1001, 0b1111_1011, 0b0000_0101, 0b0100_0000]).unwrap(),
            "/1/-2.18/"
        );
    }

    #[test]
    fn the_root_is_the_empty_encoding() {
        // The root is a path of no levels, so there is nothing to encode and
        // nothing to pad. A reader that demanded at least one level would report
        // every root row as broken.
        assert_eq!(hierarchy_path(&[]).unwrap(), "/");
    }

    #[test]
    fn a_level_that_runs_off_the_end_is_refused() {
        // 01011 is a complete level and 000 is valid padding; dropping the last
        // byte's worth leaves a prefix with no value behind it.
        assert!(hierarchy_path(&[0b0111_1111, 0b1111_1111]).is_err());
    }

    #[test]
    fn padding_that_is_not_zero_is_refused() {
        // /1/ is 5 bits; the remaining 3 must be zero. A one there means the
        // reader has lost its place, and guessing would invent a level.
        assert!(hierarchy_path(&[0b0101_1001]).is_err());
    }

    #[test]
    fn a_level_can_be_shorter_than_a_byte() {
        // /1/2/3/ is three five-bit levels and one bit of padding, which is what
        // the encoding is for. A reader that stopped whenever fewer than eight
        // bits were left would silently drop the last level of a great many
        // ordinary values — this one would read back as /1/2/.
        assert_eq!(
            hierarchy_path(&[0b0101_1011, 0b0101_1110]).unwrap(),
            "/1/2/3/"
        );
    }

    #[test]
    fn a_wrong_antiambiguity_bit_is_refused() {
        // The 110 range writes its O field as `..0.1...`. Flipping the fixed 1
        // to a 0 gives a number that would still parse into something plausible,
        // which is exactly why it has to be checked rather than skipped.
        // 110 00001010 1: prefix, the O field the specification works through
        // for 18, then the real-level bit.
        let good = [0b1100_0001, 0b0101_0000];
        assert_eq!(hierarchy_path(&good).unwrap(), "/18/");
        let bad = [0b1100_0000, 0b0101_0000];
        assert!(hierarchy_path(&bad).is_err());
    }

    /// Bytes read off a live SQL Server 2022, from
    /// `SELECT CAST(geography::Point(47.62, -122.35, 4326) AS varbinary(max))`.
    #[test]
    fn a_geography_point_reads_longitude_first() {
        let bytes = point_bytes(4326, 47.62, -122.35);
        // Latitude is stored first and written second. The swap is the whole
        // difference between Seattle and a point in the Indian Ocean.
        assert_eq!(
            to_text(UdtKind::Geography, &bytes).unwrap().unwrap(),
            "POINT (-122.35 47.62)"
        );
        // A geometry stores and writes x then y, so the same bytes mean
        // something else entirely and must not be swapped.
        assert_eq!(
            to_text(UdtKind::Geometry, &bytes).unwrap().unwrap(),
            "POINT (47.62 -122.35)"
        );
    }

    #[test]
    fn a_null_geography_is_a_null_cell_and_not_an_error() {
        // SRID -1 with nothing after it. Rendering it as text would put the
        // word "null" in a cell that is not null.
        assert_eq!(
            to_text(UdtKind::Geography, &(-1i32).to_le_bytes()).unwrap(),
            None
        );
    }

    #[test]
    fn a_truncated_value_is_an_error_and_not_a_panic() {
        // Every length here stops partway through a field. The point of the test
        // is the absence of a panic: this decoder exists because one in the
        // layer below took the whole process with it.
        let full = point_bytes(4326, 1.0, 2.0);
        for len in 0..full.len() {
            assert!(
                to_text(UdtKind::Geography, &full[..len]).is_err(),
                "{len} bytes should be refused"
            );
        }
    }

    #[test]
    fn a_shape_type_with_no_text_form_names_itself() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0i32.to_le_bytes()); // SRID
        bytes.push(2); // version
        bytes.push(0); // no flags: the long form
        bytes.extend_from_slice(&0i32.to_le_bytes()); // no points
        bytes.extend_from_slice(&0i32.to_le_bytes()); // no figures
        bytes.extend_from_slice(&1i32.to_le_bytes()); // one shape
        bytes.extend_from_slice(&(-1i32).to_le_bytes()); // no parent
        bytes.extend_from_slice(&(-1i32).to_le_bytes()); // no figure
        bytes.push(COMPOUNDCURVE);

        let err = to_text(UdtKind::Geometry, &bytes).unwrap_err();
        assert!(
            err.to_string().contains("COMPOUNDCURVE"),
            "the failure has to name the shape, got: {err}"
        );
        assert!(err.to_string().contains("geometry"), "got: {err}");
    }

    #[test]
    fn only_the_three_known_type_names_are_claimed() {
        assert_eq!(
            UdtKind::from_type_name("geography"),
            Some(UdtKind::Geography)
        );
        assert_eq!(
            UdtKind::from_type_name("HierarchyId"),
            Some(UdtKind::HierarchyId)
        );
        // A CLR type somebody registered. Its bytes are whatever its assembly
        // says they are, so guessing at them is the one thing not to do.
        assert_eq!(UdtKind::from_type_name("Point3D"), None);
    }

    /// A single-point geography, in the shorthand the format uses for one.
    fn point_bytes(srid: i32, latitude: f64, longitude: f64) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&srid.to_le_bytes());
        bytes.push(1); // version
        bytes.push(SINGLE_POINT | 0x04); // single point, valid
        bytes.extend_from_slice(&latitude.to_le_bytes());
        bytes.extend_from_slice(&longitude.to_le_bytes());
        bytes
    }
}
