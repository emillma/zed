use anyhow::Error;
use etagere::euclid::{Point2D, Vector2D};
use lyon::geom::Angle;
use lyon::math::{Vector, vector};
use lyon::path::traits::SvgPathBuilder;
use lyon::path::{ArcFlags, Polygon};
use lyon::tessellation::{
    BuffersBuilder, FillTessellator, FillVertex, StrokeTessellator, StrokeVertex, VertexBuffers,
};

pub use lyon::math::Transform;
pub use lyon::tessellation::{FillOptions, FillRule, StrokeOptions};

use crate::{Path, Pixels, Point, point, px};

/// Samples per quadratic/cubic centerline segment when encoding stroke
/// distances (keeps chord sagitta small for large-radius bends).
const CENTERLINE_CURVE_SAMPLES: u32 = 32;

/// Style of the PathBuilder
pub enum PathStyle {
    /// Stroke style
    Stroke(StrokeOptions),
    /// Fill style
    Fill(FillOptions),
}

/// A [`Path`] builder.
pub struct PathBuilder {
    raw: lyon::path::builder::WithSvg<lyon::path::BuilderImpl>,
    transform: Option<lyon::math::Transform>,
    /// PathStyle of the PathBuilder
    pub style: PathStyle,
    dash_array: Option<Vec<Pixels>>,
}

impl From<lyon::path::Builder> for PathBuilder {
    fn from(builder: lyon::path::Builder) -> Self {
        Self {
            raw: builder.with_svg(),
            ..Default::default()
        }
    }
}

impl From<lyon::path::builder::WithSvg<lyon::path::BuilderImpl>> for PathBuilder {
    fn from(raw: lyon::path::builder::WithSvg<lyon::path::BuilderImpl>) -> Self {
        Self {
            raw,
            ..Default::default()
        }
    }
}

impl From<lyon::math::Point> for Point<Pixels> {
    fn from(p: lyon::math::Point) -> Self {
        point(px(p.x), px(p.y))
    }
}

impl From<Point<Pixels>> for lyon::math::Point {
    fn from(p: Point<Pixels>) -> Self {
        lyon::math::point(p.x.0, p.y.0)
    }
}

impl From<Point<Pixels>> for Vector {
    fn from(p: Point<Pixels>) -> Self {
        vector(p.x.0, p.y.0)
    }
}

impl From<Point<Pixels>> for Point2D<f32, Pixels> {
    fn from(p: Point<Pixels>) -> Self {
        Point2D::new(p.x.0, p.y.0)
    }
}

impl Default for PathBuilder {
    fn default() -> Self {
        Self {
            raw: lyon::path::Path::builder().with_svg(),
            style: PathStyle::Fill(FillOptions::default()),
            transform: None,
            dash_array: None,
        }
    }
}

impl PathBuilder {
    /// Creates a new [`PathBuilder`] to build a Stroke path.
    pub fn stroke(width: Pixels) -> Self {
        Self {
            style: PathStyle::Stroke(StrokeOptions::default().with_line_width(width.0)),
            ..Self::default()
        }
    }

    /// Creates a new [`PathBuilder`] to build a Fill path.
    pub fn fill() -> Self {
        Self::default()
    }

    /// Sets the style of the [`PathBuilder`].
    pub fn with_style(self, style: PathStyle) -> Self {
        Self { style, ..self }
    }

    /// Sets the dash array of the [`PathBuilder`].
    ///
    /// [MDN](https://developer.mozilla.org/en-US/docs/Web/SVG/Reference/Attribute/stroke-dasharray)
    pub fn dash_array(mut self, dash_array: &[Pixels]) -> Self {
        // If an odd number of values is provided, then the list of values is repeated to yield an even number of values.
        // Thus, 5,3,2 is equivalent to 5,3,2,5,3,2.
        let array = if dash_array.len() % 2 == 1 {
            let mut new_dash_array = dash_array.to_vec();
            new_dash_array.extend_from_slice(dash_array);
            new_dash_array
        } else {
            dash_array.to_vec()
        };

        self.dash_array = Some(array);
        self
    }

    /// Move the current point to the given point.
    #[inline]
    pub fn move_to(&mut self, to: Point<Pixels>) {
        self.raw.move_to(to.into());
    }

    /// Draw a straight line from the current point to the given point.
    #[inline]
    pub fn line_to(&mut self, to: Point<Pixels>) {
        self.raw.line_to(to.into());
    }

    /// Draw a curve from the current point to the given point, using the given control point.
    #[inline]
    pub fn curve_to(&mut self, to: Point<Pixels>, ctrl: Point<Pixels>) {
        self.raw.quadratic_bezier_to(ctrl.into(), to.into());
    }

    /// Adds a cubic Bézier to the [`Path`] given its two control points
    /// and its end point.
    #[inline]
    pub fn cubic_bezier_to(
        &mut self,
        to: Point<Pixels>,
        control_a: Point<Pixels>,
        control_b: Point<Pixels>,
    ) {
        self.raw
            .cubic_bezier_to(control_a.into(), control_b.into(), to.into());
    }

    /// Adds an elliptical arc.
    pub fn arc_to(
        &mut self,
        radii: Point<Pixels>,
        x_rotation: Pixels,
        large_arc: bool,
        sweep: bool,
        to: Point<Pixels>,
    ) {
        self.raw.arc_to(
            radii.into(),
            Angle::degrees(x_rotation.into()),
            ArcFlags { large_arc, sweep },
            to.into(),
        );
    }

    /// Equivalent to `arc_to` in relative coordinates.
    pub fn relative_arc_to(
        &mut self,
        radii: Point<Pixels>,
        x_rotation: Pixels,
        large_arc: bool,
        sweep: bool,
        to: Point<Pixels>,
    ) {
        self.raw.relative_arc_to(
            radii.into(),
            Angle::degrees(x_rotation.into()),
            ArcFlags { large_arc, sweep },
            to.into(),
        );
    }

    /// Adds a polygon.
    pub fn add_polygon(&mut self, points: &[Point<Pixels>], closed: bool) {
        let points = points.iter().copied().map(|p| p.into()).collect::<Vec<_>>();
        self.raw.add_polygon(Polygon {
            points: points.as_ref(),
            closed,
        });
    }

    /// Close the current sub-path.
    #[inline]
    pub fn close(&mut self) {
        self.raw.close();
    }

    /// Applies a transform to the path.
    #[inline]
    pub fn transform(&mut self, transform: Transform) {
        self.transform = Some(transform);
    }

    /// Applies a translation to the path.
    #[inline]
    pub fn translate(&mut self, to: Point<Pixels>) {
        if let Some(transform) = self.transform {
            self.transform = Some(transform.then_translate(Vector2D::new(to.x.0, to.y.0)));
        } else {
            self.transform = Some(Transform::translation(to.x.0, to.y.0))
        }
    }

    /// Applies a scale to the path.
    #[inline]
    pub fn scale(&mut self, scale: f32) {
        if let Some(transform) = self.transform {
            self.transform = Some(transform.then_scale(scale, scale));
        } else {
            self.transform = Some(Transform::scale(scale, scale));
        }
    }

    /// Applies a rotation to the path.
    ///
    /// The `angle` is in degrees value in the range 0.0 to 360.0.
    #[inline]
    pub fn rotate(&mut self, angle: f32) {
        let radians = angle.to_radians();
        if let Some(transform) = self.transform {
            self.transform = Some(transform.then_rotate(Angle::radians(radians)));
        } else {
            self.transform = Some(Transform::rotation(Angle::radians(radians)));
        }
    }

    /// Builds into a [`Path`].
    #[inline]
    pub fn build(self) -> Result<Path<Pixels>, Error> {
        let path = if let Some(transform) = self.transform {
            self.raw.build().transformed(&transform)
        } else {
            self.raw.build()
        };

        match self.style {
            PathStyle::Stroke(options) => Self::tessellate_stroke(self.dash_array, &path, &options),
            PathStyle::Fill(options) => Self::tessellate_fill(&path, &options),
        }
    }

    fn tessellate_fill(
        path: &lyon::path::Path,
        options: &FillOptions,
    ) -> Result<Path<Pixels>, Error> {
        // Will contain the result of the tessellation.
        let mut buf: VertexBuffers<lyon::math::Point, u16> = VertexBuffers::new();
        let mut tessellator = FillTessellator::new();

        // Compute the tessellation.
        tessellator.tessellate_path(
            path,
            options,
            &mut BuffersBuilder::new(&mut buf, |vertex: FillVertex| vertex.position()),
        )?;

        Ok(Self::build_path(buf, None))
    }

    fn tessellate_stroke(
        dash_array: Option<Vec<Pixels>>,
        path: &lyon::path::Path,
        options: &StrokeOptions,
    ) -> Result<Path<Pixels>, Error> {
        let path = if let Some(dash_array) = dash_array {
            let measurements = lyon::algorithms::measure::PathMeasurements::from_path(path, 0.01);
            let mut sampler = measurements
                .create_sampler(path, lyon::algorithms::measure::SampleType::Normalized);
            let mut builder = lyon::path::Path::builder();

            let total_length = sampler.length();
            let dash_array_len = dash_array.len();
            let mut pos = 0.;
            let mut dash_index = 0;
            while pos < total_length {
                let dash_length = dash_array[dash_index % dash_array_len].0;
                let next_pos = (pos + dash_length).min(total_length);
                if dash_index % 2 == 0 {
                    let start = pos / total_length;
                    let end = next_pos / total_length;
                    sampler.split_range(start..end, &mut builder);
                }
                pos = next_pos;
                dash_index += 1;
            }

            &builder.build()
        } else {
            path
        };

        // Will contain the result of the tessellation.
        let mut buf: VertexBuffers<lyon::math::Point, u16> = VertexBuffers::new();
        let mut tessellator = StrokeTessellator::new();

        // Compute the tessellation.
        tessellator.tessellate_path(
            path,
            options,
            &mut BuffersBuilder::new(&mut buf, |vertex: StrokeVertex| vertex.position()),
        )?;

        let st = Self::stroke_st_positions(path, &buf.vertices, options.line_width);
        Ok(Self::build_path(buf, st.as_deref()))
    }

    /// Per-vertex st for stroked tessellation: the signed distance from the
    /// centerline (normalized by half the stroke width) in x, and st.y == 0.0
    /// as the stroke-mode flag read by `fs_path_rasterization`. `None` keeps
    /// the legacy constant st when the width is non-positive or the path has
    /// no non-degenerate centerline geometry.
    fn stroke_st_positions(
        path: &lyon::path::Path,
        vertices: &[lyon::math::Point],
        stroke_width: f32,
    ) -> Option<Vec<Point<f32>>> {
        let half_width = stroke_width / 2.0;
        if half_width <= 0.0 {
            return None;
        }
        let polylines = Self::centerline_polylines(path, half_width);
        if polylines.is_empty() {
            return None;
        }
        Some(
            vertices
                .iter()
                .map(|&p| {
                    let (dist, side) = Self::signed_side_distance(p, &polylines);
                    point(side * dist / half_width, 0.0)
                })
                .collect(),
        )
    }

    /// Per-contour centerline polylines of `path`: straight segments stay
    /// exact, quadratic/cubic curves are sampled at
    /// `CENTERLINE_CURVE_SAMPLES` points, and each open contour is extended
    /// past both ends by `half_width` so round-cap vertices get their true
    /// perpendicular distance. Zero-length contours are skipped.
    fn centerline_polylines(
        path: &lyon::path::Path,
        half_width: f32,
    ) -> Vec<Vec<lyon::math::Point>> {
        let mut polylines: Vec<Vec<lyon::math::Point>> = Vec::new();
        let mut current: Option<Vec<lyon::math::Point>> = None;
        for event in path.iter() {
            match event {
                lyon::path::PathEvent::Begin { at } => current = Some(vec![at]),
                lyon::path::PathEvent::Line { from: _, to } => {
                    if let Some(poly) = current.as_mut() {
                        poly.push(to);
                    }
                }
                lyon::path::PathEvent::Quadratic { from, ctrl, to } => {
                    if let Some(poly) = current.as_mut() {
                        Self::sample_curve(poly, |t| {
                            let u = 1.0 - t;
                            (
                                u * u * from.x + 2.0 * u * t * ctrl.x + t * t * to.x,
                                u * u * from.y + 2.0 * u * t * ctrl.y + t * t * to.y,
                            )
                        });
                    }
                }
                lyon::path::PathEvent::Cubic {
                    from,
                    ctrl1,
                    ctrl2,
                    to,
                } => {
                    if let Some(poly) = current.as_mut() {
                        Self::sample_curve(poly, |t| {
                            let u = 1.0 - t;
                            (
                                u * u * u * from.x
                                    + 3.0 * u * u * t * ctrl1.x
                                    + 3.0 * u * t * t * ctrl2.x
                                    + t * t * t * to.x,
                                u * u * u * from.y
                                    + 3.0 * u * u * t * ctrl1.y
                                    + 3.0 * u * t * t * ctrl2.y
                                    + t * t * t * to.y,
                            )
                        });
                    }
                }
                lyon::path::PathEvent::End {
                    last: _,
                    first,
                    close,
                } => {
                    if let Some(mut poly) = current.take() {
                        if close && poly.last() != Some(&first) {
                            // Include the closing edge in distance queries.
                            poly.push(first);
                        }
                        if poly.len() >= 2 && !poly.windows(2).all(|w| w[0] == w[1]) {
                            Self::extend_ends(&mut poly, half_width, !close);
                            polylines.push(poly);
                        }
                    }
                }
            }
        }
        polylines
    }

    /// Appends `CENTERLINE_CURVE_SAMPLES` samples of the curve `f`, t in
    /// (0, 1]; t = 0 is the current polyline end.
    fn sample_curve(poly: &mut Vec<lyon::math::Point>, f: impl Fn(f32) -> (f32, f32)) {
        for i in 1..=CENTERLINE_CURVE_SAMPLES {
            let (x, y) = f(i as f32 / CENTERLINE_CURVE_SAMPLES as f32);
            poly.push(lyon::math::point(x, y));
        }
    }

    /// Extends an open polyline past both ends by `half_width` along the
    /// first/last non-degenerate segment, so the round caps sit on the
    /// extended centerline.
    fn extend_ends(poly: &mut Vec<lyon::math::Point>, half_width: f32, open: bool) {
        if !open {
            return;
        }
        // Skip coincident leading/trailing points so `start`/`end` bound the
        // first/last real segment.
        let mut start = 0;
        while start + 1 < poly.len() && poly[start] == poly[start + 1] {
            start += 1;
        }
        let mut end = poly.len() - 1;
        while end > 0 && poly[end] == poly[end - 1] {
            end -= 1;
        }
        if start + 1 < poly.len() {
            let d = poly[start + 1] - poly[start];
            if d.length() > 0.0 {
                poly[start] -= d / d.length() * half_width;
            }
        }
        if end > 0 {
            let d = poly[end] - poly[end - 1];
            if d.length() > 0.0 {
                poly[end] += d / d.length() * half_width;
            }
        }
    }

    /// The distance from `p` to the nearest centerline segment over all
    /// contours, plus the side: the sign of the cross product of the segment
    /// direction with (p - nearest point).
    fn signed_side_distance(
        p: lyon::math::Point,
        polylines: &[Vec<lyon::math::Point>],
    ) -> (f32, f32) {
        let mut best: Option<(f32, f32)> = None;
        for poly in polylines {
            for w in poly.windows(2) {
                let (d, side) = Self::segment_distance_side(p, w[0], w[1]);
                match best {
                    None => best = Some((d, side)),
                    Some((bd, _)) if d < bd => best = Some((d, side)),
                    _ => {}
                }
            }
        }
        best.unwrap_or((0.0, 0.0))
    }

    /// The distance from `p` to the segment [a, b], plus the sign of the
    /// cross product of the segment direction with (p - nearest point).
    fn segment_distance_side(
        p: lyon::math::Point,
        a: lyon::math::Point,
        b: lyon::math::Point,
    ) -> (f32, f32) {
        let ab = b - a;
        let len2 = ab.dot(ab);
        if len2 == 0.0 {
            return ((p - a).length(), 0.0);
        }
        let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
        let to_p = p - (a + ab * t);
        (to_p.length(), ab.cross(to_p).signum())
    }

    /// Builds a [`Path`] from a [`lyon::tessellation::VertexBuffers`].
    /// `st` overrides the per-vertex st position; `None` keeps the legacy
    /// constant `(0, 1)`.
    pub fn build_path(
        buf: VertexBuffers<lyon::math::Point, u16>,
        st: Option<&[Point<f32>]>,
    ) -> Path<Pixels> {
        if buf.vertices.is_empty() {
            return Path::new(Point::default());
        }

        let first_point = buf.vertices[0];

        let mut path = Path::new(first_point.into());
        for i in 0..buf.indices.len() / 3 {
            let i0 = buf.indices[i * 3] as usize;
            let i1 = buf.indices[i * 3 + 1] as usize;
            let i2 = buf.indices[i * 3 + 2] as usize;

            let v0 = buf.vertices[i0];
            let v1 = buf.vertices[i1];
            let v2 = buf.vertices[i2];

            let st0 = st.map_or(point(0., 1.), |st| st[i0]);
            let st1 = st.map_or(point(0., 1.), |st| st[i1]);
            let st2 = st.map_or(point(0., 1.), |st| st[i2]);

            path.push_triangle((v0.into(), v1.into(), v2.into()), (st0, st1, st2));
        }

        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dumps the stroke tessellation of the GitGraph "checkout curve" shape:
    /// vertical run -> quadratic bend (horizontal end tangent) -> horizontal run.
    ///
    /// Used to diagnose the near-horizontal blob artifact: verifies that the
    /// tessellated strip stays within ~half the line width of the centerline.
    #[test]
    fn lane_tessellation_width() {
        // Geometry mirrors crates/git_ui/src/git_graph.rs checkout-curve drawing.
        let x0 = 100.0;
        let y_top = 50.0;
        let to_row_y = 200.0;
        let curve_h = 8.0; // row_height / 3
        let curve_w = 16.0 / 3.0; // LANE_WIDTH / 3
        let p_start = (x0, to_row_y - curve_h);
        let p_end = (x0 + curve_w, to_row_y);
        let p_ctrl = (x0, to_row_y);

        let mut builder = PathBuilder::stroke(px(1.5));
        builder.move_to(point(px(x0), px(y_top)));
        builder.line_to(point(px(p_start.0), px(p_start.1)));
        builder.move_to(point(px(p_start.0), px(p_start.1)));
        builder.curve_to(
            point(px(p_end.0), px(p_end.1)),
            point(px(p_ctrl.0), px(p_ctrl.1)),
        );
        builder.move_to(point(px(p_end.0), px(p_end.1)));
        builder.line_to(point(px(p_end.0 + 50.0), px(to_row_y)));
        let path = builder.build().unwrap();

        let curve = |t: f32| {
            let a = (1.0 - t) * (1.0 - t);
            let b = 2.0 * t * (1.0 - t);
            let c = t * t;
            (
                a * p_start.0 + b * p_ctrl.0 + c * p_end.0,
                a * p_start.1 + b * p_ctrl.1 + c * p_end.1,
            )
        };
        let dist_to_centerline = |p: (f32, f32)| {
            let mut best = f32::INFINITY;
            for i in 0..=2000 {
                let t = i as f32 / 2000.0;
                let (cx, cy) = curve(t);
                best = best.min(((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt());
            }
            best.min((p.0 - x0).abs()) // vertical run at x = x0
                .min((p.1 - to_row_y).abs()) // horizontal run at y = to_row_y
        };

        let n = path.vertices.len();
        assert!(n % 3 == 0);
        println!("{} vertices, {} triangles", n, n / 3);
        for i in (0..n).step_by(3) {
            let xs: Vec<f32> = path.vertices[i..i + 3]
                .iter()
                .map(|v| v.xy_position.x.0)
                .collect();
            let ys: Vec<f32> = path.vertices[i..i + 3]
                .iter()
                .map(|v| v.xy_position.y.0)
                .collect();
            let in_bend = xs
                .iter()
                .zip(ys.iter())
                .any(|(x, y)| *x >= 98.0 && *x <= 108.0 && *y >= 189.0 && *y <= 202.0);
            if in_bend {
                let dmax = [0, 1, 2]
                    .map(|j| dist_to_centerline((xs[j], ys[j])))
                    .into_iter()
                    .fold(0.0_f32, f32::max);
                println!(
                    "tri {:3}: [{:7.3},{:7.3}] [{:7.3},{:7.3}] [{:7.3},{:7.3}] max_dist={:6.3}",
                    i / 3,
                    xs[0],
                    ys[0],
                    xs[1],
                    ys[1],
                    xs[2],
                    ys[2],
                    dmax
                );
            }
        }
    }

    /// Verifies the stroke st encoding: st.y == 0.0 (stroke-mode flag) on
    /// every vertex, signed |st.x| reaches 1.0 on both side edges, and no
    /// vertex overshoots the stroke. Fills keep the legacy st == (0, 1).
    #[test]
    fn stroke_st_encoding() {
        let mut builder = PathBuilder::stroke(px(2.0));
        builder.move_to(point(px(0.0), px(0.0)));
        builder.line_to(point(px(10.0), px(0.0)));
        let path = builder.build().unwrap();
        assert!(!path.vertices.is_empty());

        let mut min_abs_st = f32::MAX;
        for v in &path.vertices {
            assert_eq!(v.st_position.y, 0.0);
            assert!(
                v.st_position.x.abs() <= 1.0 + 1e-3,
                "stroke vertex overshoots the edge: {:?}",
                v.st_position
            );
            min_abs_st = min_abs_st.min(v.st_position.x.abs());
        }
        assert!(min_abs_st <= 1.0 + 1e-3);
        assert!(
            path.vertices
                .iter()
                .any(|v| (v.st_position.x - 1.0).abs() <= 1e-3),
            "no vertex at st.x ~= +1.0 (right edge)"
        );
        assert!(
            path.vertices
                .iter()
                .any(|v| (v.st_position.x + 1.0).abs() <= 1e-3),
            "no vertex at st.x ~= -1.0 (left edge)"
        );

        let mut builder = PathBuilder::fill();
        builder.move_to(point(px(0.0), px(0.0)));
        builder.line_to(point(px(10.0), px(0.0)));
        builder.line_to(point(px(5.0), px(5.0)));
        builder.close();
        let filled = builder.build().unwrap();
        assert!(!filled.vertices.is_empty());
        for v in &filled.vertices {
            assert_eq!(v.st_position, point(0.0, 1.0));
        }
    }
}
