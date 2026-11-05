use crate::protocol::{Message, MouseMode};
use anyhow::{Result, ensure};

const SCALE: i64 = 65535;
pub(crate) const SWITCH_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

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
    pub fn center(self) -> PixelPoint {
        PixelPoint {
            x: self.left.saturating_add(self.width / 2),
            y: self.top.saturating_add(self.height / 2),
        }
    }
    pub fn height_ratio(self, y: i32) -> u16 {
        screen_axis(y, self.top, self.height).clamp(0, 65535) as u16
    }
    pub fn point_at(self, x: i32, y: i32) -> PixelPoint {
        PixelPoint {
            x: decode_axis(x, self.left, self.width),
            y: decode_axis(y, self.top, self.height),
        }
    }
    pub fn entry(self, from_left: bool, y: u16) -> PixelPoint {
        let inset = 4.min(self.width - 1);
        PixelPoint {
            x: if from_left {
                self.left.saturating_add(inset)
            } else {
                self.right().saturating_sub(inset)
            },
            y: decode_axis(i32::from(y), self.top, self.height),
        }
    }
    pub fn entry_ratio(self, from_left: bool) -> u16 {
        screen_axis(self.entry(from_left, 32768).x, self.left, self.width).clamp(0, 65535) as u16
    }
    pub fn absolute(self, point: PixelPoint) -> (i32, i32) {
        (
            normalize_axis(point.x, self.left, self.width),
            normalize_axis(point.y, self.top, self.height),
        )
    }
}
fn screen_axis(coordinate: i32, origin: i32, extent: i32) -> i32 {
    ((i64::from(coordinate) - i64::from(origin)) * SCALE / i64::from(extent))
        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}
fn decode_axis(ratio: i32, origin: i32, extent: i32) -> i32 {
    let offset = (i64::from(ratio.clamp(0, 65535)) * i64::from(extent) + SCALE / 2) / SCALE;
    origin.saturating_add(offset.min(i64::from(extent - 1)) as i32)
}
fn normalize_axis(coordinate: i32, origin: i32, extent: i32) -> i32 {
    let offset = (i64::from(coordinate) - i64::from(origin)).clamp(0, i64::from(extent) - 1);
    (((offset * 2 + 1) * 32768 / i64::from(extent)).min(65535)) as i32
}
fn validate_delta(dx: i32, dy: i32) -> Result<()> {
    ensure!(
        dx.unsigned_abs() <= 32768 && dy.unsigned_abs() <= 32768,
        "鼠标位移无效"
    );
    Ok(())
}

// The controlling PC owns the virtual cursor, in primary-screen pixel units.
// The receiving PC does not accumulate pixel deltas in screen-relative mode.
pub(crate) struct CaptureMotion {
    frame: DesktopBounds,
    x: i64,
    y: i64,
    mode: MouseMode,
}
impl CaptureMotion {
    pub fn enter(primary: DesktopBounds, from_left: bool, y: u16, mode: MouseMode) -> Self {
        Self {
            frame: primary,
            x: (i64::from(primary.entry(from_left, y).x) - i64::from(primary.left)) * SCALE,
            y: i64::from(primary.height) * i64::from(y),
            mode,
        }
    }
    pub fn advance(&mut self, dx: i32, dy: i32) -> Result<Message> {
        validate_delta(dx, dy)?;
        if self.mode == MouseMode::Relative {
            return Ok(Message::Move { dx, dy });
        }
        // Fixed-point source pixels preserve the entry fraction and each small
        // delta. Both PCs start at the same proportion, preventing a first-move jump.
        self.x = (self.x + i64::from(dx) * SCALE)
            .clamp(-SCALE, (i64::from(self.frame.width) + 1) * SCALE);
        self.y = (self.y + i64::from(dy) * SCALE).clamp(0, i64::from(self.frame.height) * SCALE);
        let x = (self.x / i64::from(self.frame.width)) as i32;
        let y = (self.y / i64::from(self.frame.height)) as i32;
        Ok(Message::MoveAbsolute { x, y })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MotionStep {
    MoveTo(PixelPoint),
    ReturnLocal { y: u16 },
}
#[derive(Default)]
pub(crate) struct PointerMotion {
    mode: Option<MouseMode>,
    from_left: bool,
}
impl PointerMotion {
    pub fn is_relative(&self) -> bool {
        self.mode == Some(MouseMode::Relative)
    }
    pub fn enter(
        &mut self,
        bounds: DesktopBounds,
        from_left: bool,
        y: u16,
        mode: MouseMode,
        x: u16,
    ) -> PixelPoint {
        self.mode = Some(mode);
        self.from_left = from_left;
        bounds.point_at(i32::from(x), i32::from(y))
    }
    pub fn absolute(
        &self,
        bounds: DesktopBounds,
        x: i32,
        y: i32,
        can_return: bool,
    ) -> Result<MotionStep> {
        ensure!(
            self.mode == Some(MouseMode::ScreenRelative),
            "鼠标移动模式与当前控制会话不符"
        );
        ensure!(
            (-65535..=131070).contains(&x) && (0..=65535).contains(&y),
            "鼠标比例坐标无效"
        );
        if can_return && ((self.from_left && x < 0) || (!self.from_left && x >= 65535)) {
            return Ok(MotionStep::ReturnLocal { y: y as u16 });
        }
        Ok(MotionStep::MoveTo(bounds.point_at(x, y)))
    }
    pub fn relative(&self, dx: i32, dy: i32) -> Result<()> {
        ensure!(
            self.mode == Some(MouseMode::Relative),
            "鼠标移动模式与当前控制会话不符"
        );
        validate_delta(dx, dy)
    }
    // Read queued relative injection's actual result for edge detection only.
    pub fn observe(&self, bounds: DesktopBounds, point: PixelPoint) -> Option<u16> {
        if self.mode == Some(MouseMode::Relative)
            && ((self.from_left && point.x <= bounds.left)
                || (!self.from_left && point.x >= bounds.right()))
        {
            Some(bounds.height_ratio(point.y))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn apply(target: &PointerMotion, bounds: DesktopBounds, msg: Message) -> MotionStep {
        let Message::MoveAbsolute { x, y } = msg else {
            panic!("expected screen coordinates")
        };
        target.absolute(bounds, x, y, true).unwrap()
    }
    #[test]
    fn normalization_targets_every_physical_pixel_at_common_resolutions() {
        for extent in [
            1, 2, 3, 768, 1080, 1366, 1920, 2560, 3840, 7680, 11520, 32768, 65536,
        ] {
            for pixel in 0..extent {
                assert_eq!(
                    i64::from(normalize_axis(pixel, 0, extent)) * i64::from(extent) / 65536,
                    i64::from(pixel)
                );
            }
        }
    }
    #[test]
    fn screen_mode_scales_both_axes_with_source_and_target_resolution() {
        for (sw, tw, expected) in [(1920, 1920, 100), (1920, 3840, 200), (3840, 1920, 50)] {
            let source = DesktopBounds::new(0, 0, sw, sw);
            let bounds = DesktopBounds::new(0, 0, tw, tw);
            let mut capture = CaptureMotion::enter(source, true, 16384, MouseMode::ScreenRelative);
            let mut target = PointerMotion::default();
            target.enter(
                bounds,
                true,
                16384,
                MouseMode::ScreenRelative,
                source.entry_ratio(true),
            );
            let MotionStep::MoveTo(before) = apply(&target, bounds, capture.advance(0, 0).unwrap())
            else {
                panic!()
            };
            let MotionStep::MoveTo(after) =
                apply(&target, bounds, capture.advance(100, 100).unwrap())
            else {
                panic!()
            };
            assert_eq!(after.x - before.x, expected);
            assert_eq!(after.y - before.y, expected);
        }
    }
    #[test]
    fn entering_different_screens_does_not_jump_on_the_first_move() {
        for (width, height) in [(1920, 1080), (3840, 2160), (1366, 768)] {
            for from_left in [true, false] {
                for y in [0, 16384, 32768, 65535] {
                    let source = DesktopBounds::new(0, 0, width, height);
                    let bounds = DesktopBounds::new(-2560, -720, 7680, 2880);
                    let mut capture =
                        CaptureMotion::enter(source, from_left, y, MouseMode::ScreenRelative);
                    let mut target = PointerMotion::default();
                    let entry = target.enter(
                        bounds,
                        from_left,
                        y,
                        MouseMode::ScreenRelative,
                        source.entry_ratio(from_left),
                    );
                    assert_eq!(
                        apply(&target, bounds, capture.advance(0, 0).unwrap()),
                        MotionStep::MoveTo(entry)
                    );
                }
            }
        }
    }

    #[test]
    fn one_pixel_motion_and_batching_preserve_source_position() {
        let source = DesktopBounds::new(0, 0, 1920, 1080);
        let bounds = DesktopBounds::new(-3840, -1080, 5760, 2160);
        let mut small = CaptureMotion::enter(source, true, 32768, MouseMode::ScreenRelative);
        let mut large = CaptureMotion::enter(source, true, 32768, MouseMode::ScreenRelative);
        let mut target = PointerMotion::default();
        let entry = target.enter(
            bounds,
            true,
            32768,
            MouseMode::ScreenRelative,
            source.entry_ratio(true),
        );
        let mut last = Message::Ping;
        for delta in 1..=500 {
            last = small.advance(1, 0).unwrap();
            let MotionStep::MoveTo(point) = apply(&target, bounds, last.clone()) else {
                panic!()
            };
            assert_eq!(point.x, entry.x + delta * 3);
        }
        assert_eq!(last, large.advance(500, 0).unwrap());
    }
    #[test]
    fn negative_desktop_origins_and_entry_height_are_preserved() {
        let bounds = DesktopBounds::new(-3840, -1080, 5760, 2160);
        let y = bounds.height_ratio(-540);
        assert_eq!(bounds.entry(true, y), PixelPoint { x: -3836, y: -540 });
        assert_eq!(bounds.entry(false, y), PixelPoint { x: 1915, y: -540 });
        for point in [
            PixelPoint { x: -3840, y: -1080 },
            bounds.center(),
            PixelPoint { x: 1919, y: 1079 },
        ] {
            let (x, y) = bounds.absolute(point);
            assert_eq!(
                bounds.left + (i64::from(x) * i64::from(bounds.width) / 65536) as i32,
                point.x
            );
            assert_eq!(
                bounds.top + (i64::from(y) * i64::from(bounds.height) / 65536) as i32,
                point.y
            );
        }
    }
    #[test]
    fn relative_mode_passes_deltas_instead_of_accumulating_absolute_positions() {
        let mut capture = CaptureMotion::enter(
            DesktopBounds::new(0, 0, 1920, 1080),
            true,
            32768,
            MouseMode::Relative,
        );
        let mut target = PointerMotion::default();
        let bounds = DesktopBounds::new(-3840, 0, 7680, 2160);
        target.enter(bounds, true, 32768, MouseMode::Relative, 136);
        for (dx, dy) in [(1, -1), (500, -200), (-32768, 32768)] {
            assert_eq!(capture.advance(dx, dy).unwrap(), Message::Move { dx, dy });
            target.relative(dx, dy).unwrap();
        }
        assert_eq!(
            target.observe(bounds, PixelPoint { x: -3840, y: 540 }),
            Some(16383)
        );
        assert_eq!(target.observe(bounds, PixelPoint { x: 3839, y: 540 }), None);
    }
    #[test]
    fn only_entry_edge_returns_and_cooldown_can_defer_a_switch() {
        let bounds = DesktopBounds::new(0, 0, 1920, 1080);
        for from_left in [true, false] {
            let mut target = PointerMotion::default();
            target.enter(
                bounds,
                from_left,
                16384,
                MouseMode::ScreenRelative,
                bounds.entry_ratio(from_left),
            );
            let (entry, opposite) = if from_left { (-1, 65536) } else { (65536, -1) };
            assert_eq!(
                target.absolute(bounds, entry, 16384, true).unwrap(),
                MotionStep::ReturnLocal { y: 16384 }
            );
            assert!(matches!(
                target.absolute(bounds, entry, 16384, false).unwrap(),
                MotionStep::MoveTo(_)
            ));
            assert!(matches!(
                target.absolute(bounds, opposite, 16384, true).unwrap(),
                MotionStep::MoveTo(_)
            ));
        }
    }
    #[test]
    fn stale_or_mismatched_modes_and_invalid_values_are_rejected() {
        let bounds = DesktopBounds::new(0, 0, 1920, 1080);
        let mut target = PointerMotion::default();
        assert!(target.absolute(bounds, 1, 1, true).is_err());
        assert!(target.relative(1, 1).is_err());
        target.enter(bounds, true, 32768, MouseMode::ScreenRelative, 136);
        assert!(target.relative(1, 1).is_err());
        assert!(target.absolute(bounds, i32::MIN, 1, true).is_err());
        assert!(target.absolute(bounds, 1, -1, true).is_err());
        target.enter(bounds, true, 32768, MouseMode::Relative, 136);
        assert!(target.relative(i32::MIN, 0).is_err());
        assert!(target.absolute(bounds, 1, 1, true).is_err());
    }
}
