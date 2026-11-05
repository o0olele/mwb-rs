use anyhow::{Context, Result, ensure};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PixelPoint {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DesktopBounds {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

impl DesktopBounds {
    pub fn new(left: i32, top: i32, width: i32, height: i32) -> Self {
        Self {
            left,
            top,
            width: width.max(1),
            height: height.max(1),
        }
    }

    pub fn right(self) -> i32 {
        self.left.saturating_add(self.width - 1)
    }
    pub fn bottom(self) -> i32 {
        self.top.saturating_add(self.height - 1)
    }

    fn clamp(self, point: PixelPoint) -> PixelPoint {
        PixelPoint {
            x: point.x.clamp(self.left, self.right()),
            y: point.y.clamp(self.top, self.bottom()),
        }
    }

    pub fn center(self) -> PixelPoint {
        PixelPoint {
            x: self.left.saturating_add(self.width / 2),
            y: self.top.saturating_add(self.height / 2),
        }
    }

    pub fn absolute(self, point: PixelPoint) -> (i32, i32) {
        (
            normalize_axis(point.x, self.left, self.width),
            normalize_axis(point.y, self.top, self.height),
        )
    }
}

// Target the center of a physical pixel's normalized interval. Rounding at its
// boundary can land in the preceding pixel and swallow one-pixel movements.
// See dotnet/wpf UIAutomationClientSideProviders/.../AutomationProxies/Input.cs.
fn normalize_axis(coordinate: i32, origin: i32, extent: i32) -> i32 {
    let offset = (i64::from(coordinate) - i64::from(origin)).clamp(0, i64::from(extent) - 1);
    (((offset * 2 + 1) * 32768 / i64::from(extent)).min(65535)) as i32
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MotionStep {
    MoveTo(PixelPoint),
    ReturnLocal,
}

#[derive(Default)]
pub(crate) struct PointerMotion {
    position: Option<PixelPoint>,
    from_left: bool,
}

impl PointerMotion {
    pub fn enter(&mut self, bounds: DesktopBounds, from_left: bool) -> PixelPoint {
        self.from_left = from_left;
        let inset = 4.min(bounds.width - 1);
        let point = PixelPoint {
            x: if from_left {
                bounds.left.saturating_add(inset)
            } else {
                bounds.right().saturating_sub(inset)
            },
            y: bounds.center().y,
        };
        self.position = Some(point);
        point
    }

    pub fn reset(&mut self) {
        self.position = None;
    }

    pub fn advance(&mut self, bounds: DesktopBounds, dx: i32, dy: i32) -> Result<MotionStep> {
        ensure!(
            dx.unsigned_abs() <= 32768 && dy.unsigned_abs() <= 32768,
            "鼠标位移无效"
        );
        // SendInput queues events. GetCursorPos may still report the position
        // before earlier events in the same network batch have been processed.
        // Accumulate from the last commanded position instead of reading it back.
        let point = bounds.clamp(self.position.context("鼠标尚未进入远端控制状态")?);
        let next = PixelPoint {
            x: point.x.saturating_add(dx),
            y: point.y.saturating_add(dy),
        };
        if (self.from_left && next.x < bounds.left) || (!self.from_left && next.x > bounds.right())
        {
            self.reset();
            return Ok(MotionStep::ReturnLocal);
        }
        let next = bounds.clamp(next);
        self.position = Some(next);
        Ok(MotionStep::MoveTo(next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Windows maps normalized absolute input into 65536 bins across the axis.
    fn decoded_pixel(value: i32, extent: i32) -> i32 {
        (i64::from(value) * i64::from(extent) / 65536) as i32
    }

    #[test]
    fn normalization_targets_every_physical_pixel_at_common_resolutions() {
        for extent in [
            1, 2, 3, 768, 1080, 1366, 1920, 2560, 3840, 7680, 11520, 32768, 65536,
        ] {
            for pixel in 0..extent {
                assert_eq!(
                    decoded_pixel(normalize_axis(pixel, 0, extent), extent),
                    pixel,
                    "extent={extent}, pixel={pixel}"
                );
            }
        }
    }

    #[test]
    fn negative_desktop_origins_and_outside_points_are_handled() {
        let bounds = DesktopBounds::new(-3840, -1080, 5760, 2160);
        for point in [
            PixelPoint { x: -3840, y: -1080 },
            bounds.center(),
            PixelPoint { x: 1919, y: 1079 },
        ] {
            let (x, y) = bounds.absolute(point);
            assert_eq!(bounds.left + decoded_pixel(x, bounds.width), point.x);
            assert_eq!(bounds.top + decoded_pixel(y, bounds.height), point.y);
        }
        assert_eq!(
            bounds.absolute(PixelPoint {
                x: i32::MIN,
                y: i32::MAX
            }),
            bounds.absolute(PixelPoint {
                x: bounds.left,
                y: bounds.bottom()
            })
        );
    }

    #[test]
    fn slow_one_pixel_motion_is_preserved_without_os_cursor_readback() {
        let bounds = DesktopBounds::new(0, 0, 3840, 2160);
        let mut motion = PointerMotion::default();
        let start = motion.enter(bounds, true);
        for delta in 1..=1000 {
            let expected = PixelPoint {
                x: start.x + delta,
                y: start.y - delta,
            };
            assert_eq!(
                motion.advance(bounds, 1, -1).unwrap(),
                MotionStep::MoveTo(expected)
            );
            let (x, y) = bounds.absolute(expected);
            assert_eq!(decoded_pixel(x, bounds.width), expected.x);
            assert_eq!(decoded_pixel(y, bounds.height), expected.y);
        }
    }

    #[test]
    fn packet_batching_does_not_change_total_distance() {
        let bounds = DesktopBounds::new(-1920, 0, 7680, 4320);
        let mut small = PointerMotion::default();
        let mut large = PointerMotion::default();
        small.enter(bounds, true);
        large.enter(bounds, true);
        let mut last = MotionStep::ReturnLocal;
        for _ in 0..500 {
            last = small.advance(bounds, 3, -2).unwrap();
        }
        assert_eq!(last, large.advance(bounds, 1500, -1000).unwrap());
    }

    #[test]
    fn movement_is_symmetric_and_only_exits_through_the_entry_edge() {
        let bounds = DesktopBounds::new(-1920, 0, 3840, 1080);
        for from_left in [true, false] {
            let mut motion = PointerMotion::default();
            let start = motion.enter(bounds, from_left);
            let direction = if from_left { 1 } else { -1 };
            assert!(matches!(
                motion.advance(bounds, direction * 20, 0).unwrap(),
                MotionStep::MoveTo(_)
            ));
            assert_eq!(
                motion.advance(bounds, -direction * 20, 0).unwrap(),
                MotionStep::MoveTo(start)
            );
            assert_eq!(
                motion.advance(bounds, -direction * 5, 0).unwrap(),
                MotionStep::ReturnLocal
            );
            assert!(motion.advance(bounds, 1, 0).is_err());
        }
        let mut motion = PointerMotion::default();
        motion.enter(bounds, true);
        assert_eq!(
            motion.advance(bounds, 32768, -32768).unwrap(),
            MotionStep::MoveTo(PixelPoint {
                x: bounds.right(),
                y: bounds.top
            })
        );
        assert_eq!(
            motion.advance(bounds, -1, 1).unwrap(),
            MotionStep::MoveTo(PixelPoint {
                x: bounds.right() - 1,
                y: bounds.top + 1
            })
        );
    }

    #[test]
    fn reset_and_invalid_deltas_do_not_allow_stale_motion() {
        let bounds = DesktopBounds::new(0, 0, 1920, 1080);
        let mut motion = PointerMotion::default();
        let start = motion.enter(bounds, true);
        assert!(motion.advance(bounds, i32::MIN, 0).is_err());
        assert_eq!(
            motion.advance(bounds, 0, 0).unwrap(),
            MotionStep::MoveTo(start)
        );
        motion.reset();
        assert!(motion.advance(bounds, 1, 1).is_err());
    }
}
