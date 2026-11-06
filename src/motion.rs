use crate::{
    layout::{DeviceInfo, Point, Rect},
    protocol::MouseMode,
};
pub(crate) const SWITCH_DELAY: std::time::Duration = std::time::Duration::from_millis(100);
/// Win32 absolute coordinates refer to pixel centres, not rectangle boundaries.
pub(crate) fn absolute(bounds: Rect, p: Point) -> (i32, i32) {
    fn axis(v: f64, start: i32, size: i32) -> i32 {
        let offset = (v.round() as i64 - i64::from(start)).clamp(0, i64::from(size) - 1);
        (((offset * 2 + 1) * 32768 / i64::from(size)).min(65535)) as i32
    }
    (
        axis(p.x, bounds.x, bounds.width),
        axis(p.y, bounds.y, bounds.height),
    )
}
pub(crate) struct RemoteMotion {
    pub point: Point,
    pub direction: Point,
    source: Rect,
    target: DeviceInfo,
    pub mode: MouseMode,
}
impl RemoteMotion {
    pub fn new(source: Rect, target: DeviceInfo, point: Point, mode: MouseMode) -> Self {
        Self {
            point,
            direction: Point::default(),
            source,
            target,
            mode,
        }
    }
    pub fn advance(&mut self, dx: i32, dy: i32) -> Point {
        self.direction = Point {
            x: dx as f64,
            y: dy as f64,
        };
        if self.mode == MouseMode::ScreenRelative {
            self.point = self.target.clamp(Point {
                x: self.point.x
                    + dx as f64 * self.target.bounds.width as f64 / self.source.width as f64,
                y: self.point.y
                    + dy as f64 * self.target.bounds.height as f64 / self.source.height as f64,
            });
        }
        self.point
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::test_device;
    #[test]
    fn normalization_targets_every_pixel_with_negative_origins() {
        for width in [1366, 1920, 2560, 3840, 7680] {
            let b = Rect {
                x: -width,
                y: -1080,
                width,
                height: 2160,
            };
            for x in 0..width {
                let (nx, _) = absolute(
                    b,
                    Point {
                        x: (b.x + x) as f64,
                        y: 0.0,
                    },
                );
                assert_eq!((nx as i64 * width as i64 / 65536) as i32, x);
            }
        }
    }
    #[test]
    fn source_fraction_and_small_deltas_survive_resolution_scaling() {
        for (from, to) in [(1920, 3840), (3840, 1920)] {
            let target = test_device(2, to, 2160);
            let source = Rect {
                x: 0,
                y: 0,
                width: from,
                height: 1080,
            };
            let mut a = RemoteMotion::new(
                source,
                target.clone(),
                Point { x: 100.0, y: 200.0 },
                MouseMode::ScreenRelative,
            );
            let mut b = RemoteMotion::new(
                source,
                target,
                Point { x: 100.0, y: 200.0 },
                MouseMode::ScreenRelative,
            );
            for _ in 0..100 {
                a.advance(1, 1);
            }
            b.advance(100, 100);
            assert_eq!(a.point, b.point);
            assert_eq!(a.point.x, 100.0 + 100.0 * to as f64 / from as f64);
            assert_eq!(a.point.y, 400.0);
        }
    }
    #[test]
    fn relative_mode_uses_observed_position_not_delta_accumulation() {
        let d = test_device(1, 1920, 1080);
        let mut m = RemoteMotion::new(d.bounds, d, Point { x: 10.0, y: 20.0 }, MouseMode::Relative);
        assert_eq!(m.advance(100, 100), Point { x: 10.0, y: 20.0 });
    }
}
