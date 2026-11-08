//! Pixel-space geometry; device placement preserves real monitor offsets.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
pub const MAX_DEVICES: usize = 16;
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}
impl Rect {
    pub fn right(self) -> f64 {
        self.x as f64 + self.width as f64 - 1.0
    }
    pub fn bottom(self) -> f64 {
        self.y as f64 + self.height as f64 - 1.0
    }
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.x as f64 && p.y >= self.y as f64 && p.x <= self.right() && p.y <= self.bottom()
    }
    pub fn clamp(self, p: Point) -> Point {
        Point {
            x: p.x.clamp(self.x as f64, self.right()),
            y: p.y.clamp(self.y as f64, self.bottom()),
        }
    }
    pub fn center(self) -> Point {
        Point {
            x: self.x as f64 + (self.width - 1) as f64 / 2.0,
            y: self.y as f64 + (self.height - 1) as f64 / 2.0,
        }
    }
    fn overlaps(self, b: Self) -> bool {
        (self.x as i64) < b.x as i64 + b.width as i64
            && (b.x as i64) < self.x as i64 + self.width as i64
            && (self.y as i64) < b.y as i64 + b.height as i64
            && (b.y as i64) < self.y as i64 + self.height as i64
    }
    fn valid(self) -> bool {
        (1..=65536).contains(&self.width)
            && (1..=65536).contains(&self.height)
            && self.x.unsigned_abs() <= 1_000_000
            && self.y.unsigned_abs() <= 1_000_000
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Screen {
    pub name: String,
    pub rect: Rect,
    pub primary: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub bounds: Rect,
    pub screens: Vec<Screen>,
}
impl DeviceInfo {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.id.len() == 32 && self.id.bytes().all(|c| c.is_ascii_hexdigit()),
            "设备标识无效"
        );
        ensure!(
            !self.name.is_empty() && self.name.len() <= 256,
            "设备名称无效"
        );
        ensure!(
            self.bounds.valid() && !self.screens.is_empty() && self.screens.len() <= 16,
            "屏幕信息无效"
        );
        for s in &self.screens {
            ensure!(
                s.name.len() <= 128
                    && s.rect.valid()
                    && self.bounds.contains(Point {
                        x: s.rect.x as f64,
                        y: s.rect.y as f64
                    })
                    && self.bounds.contains(Point {
                        x: s.rect.right(),
                        y: s.rect.bottom()
                    }),
                "屏幕范围无效"
            );
        }
        Ok(())
    }
    pub fn primary(&self) -> Rect {
        self.screens
            .iter()
            .find(|s| s.primary)
            .unwrap_or(&self.screens[0])
            .rect
    }
    pub fn clamp(&self, p: Point) -> Point {
        self.screens
            .iter()
            .map(|s| s.rect.clamp(p))
            .min_by(|a, b| distance(*a, p).total_cmp(&distance(*b, p)))
            .unwrap_or_else(|| self.bounds.clamp(p))
    }
}
fn distance(a: Point, b: Point) -> f64 {
    (a.x - b.x).powi(2) + (a.y - b.y).powi(2)
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Device {
    pub info: DeviceInfo,
    pub x: i32,
    pub y: i32,
    pub online: bool,
}
impl Device {
    pub fn world(&self, p: Point) -> Point {
        Point {
            x: self.x as f64 + p.x - self.info.bounds.x as f64,
            y: self.y as f64 + p.y - self.info.bounds.y as f64,
        }
    }
    pub fn local(&self, p: Point) -> Point {
        Point {
            x: p.x - self.x as f64 + self.info.bounds.x as f64,
            y: p.y - self.y as f64 + self.info.bounds.y as f64,
        }
    }
    pub fn screen_rect(&self, s: &Screen) -> Rect {
        Rect {
            x: self.x + s.rect.x - self.info.bounds.x,
            y: self.y + s.rect.y - self.info.bounds.y,
            ..s.rect
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Layout {
    pub devices: Vec<Device>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Placement {
    pub id: String,
    pub x: i32,
    pub y: i32,
}
impl Layout {
    pub fn get(&self, id: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.info.id == id)
    }
    pub fn placements(&self) -> Vec<Placement> {
        self.devices
            .iter()
            .map(|d| Placement {
                id: d.info.id.clone(),
                x: d.x,
                y: d.y,
            })
            .collect()
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.devices.is_empty() && self.devices.len() <= MAX_DEVICES,
            "设备数量须为 1–16"
        );
        let mut ids = HashSet::new();
        for (i, d) in self.devices.iter().enumerate() {
            d.info.validate()?;
            ensure!(ids.insert(&d.info.id), "设备标识重复");
            ensure!(
                d.x.unsigned_abs() <= 1_000_000 && d.y.unsigned_abs() <= 1_000_000,
                "设备位置超出画布范围"
            );
            for other in &self.devices[..i] {
                ensure!(
                    !d.info.screens.iter().any(|a| other
                        .info
                        .screens
                        .iter()
                        .any(|b| d.screen_rect(a).overlaps(other.screen_rect(b)))),
                    "设备屏幕重叠，请拖开后再保存"
                );
            }
        }
        Ok(())
    }
    pub fn positioned(&self, positions: &[Placement]) -> Result<Self> {
        ensure!(
            positions.len() == self.devices.len(),
            "设备列表已变化，请刷新布局"
        );
        let mut result = self.clone();
        let mut ids = HashSet::new();
        for p in positions {
            ensure!(ids.insert(&p.id), "布局包含重复设备");
            let d = result
                .devices
                .iter_mut()
                .find(|d| d.info.id == p.id)
                .ok_or_else(|| anyhow::anyhow!("布局包含未知设备"))?;
            d.x = p.x;
            d.y = p.y;
        }
        result.validate()?;
        Ok(result)
    }
    pub fn without_offline(&self, id: &str) -> Result<Self> {
        let device = self.get(id).ok_or_else(|| anyhow::anyhow!("设备不存在"))?;
        ensure!(!device.online, "设备已在线，请先在该设备停止连接");
        let mut result = self.clone();
        result.devices.retain(|d| d.info.id != id);
        result.validate()?;
        Ok(result)
    }
    pub fn upsert(&mut self, info: DeviceInfo, online: bool) -> Result<()> {
        info.validate()?;
        let mut next = self.clone();
        let index = if let Some(i) = next.devices.iter().position(|d| d.info.id == info.id) {
            next.devices[i].info = info;
            next.devices[i].online = online;
            i
        } else {
            ensure!(next.devices.len() < MAX_DEVICES, "工作组最多 16 台设备");
            next.devices.push(Device {
                info,
                x: 0,
                y: 0,
                online,
            });
            next.devices.len() - 1
        };
        if next.validate().is_err() {
            next.devices[index].x = next
                .devices
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != index)
                .map(|(_, d)| d.x + d.info.bounds.width + 200)
                .max()
                .unwrap_or(0);
            next.devices[index].y = 0;
        }
        next.validate()?;
        *self = next;
        Ok(())
    }
    pub fn extent(&self) -> Rect {
        let x = self.devices.iter().map(|d| d.x).min().unwrap_or(0);
        let y = self.devices.iter().map(|d| d.y).min().unwrap_or(0);
        let right = self
            .devices
            .iter()
            .map(|d| d.x + d.info.bounds.width)
            .max()
            .unwrap_or(x + 1920);
        let bottom = self
            .devices
            .iter()
            .map(|d| d.y + d.info.bounds.height)
            .max()
            .unwrap_or(y + 1080);
        Rect {
            x,
            y,
            width: (right - x).max(1),
            height: (bottom - y).max(1),
        }
    }
    /// Gaps are skipped, but a destination must intersect the motion ray.
    pub fn crossing(
        &self,
        source: &str,
        p: Point,
        delta: Point,
        block_corners: bool,
    ) -> Option<(String, Point)> {
        let device = self.get(source)?;
        let p = device.info.clamp(p);
        let screen = device
            .info
            .screens
            .iter()
            .find(|s| s.rect.contains(p))?
            .rect;
        let left = p.x <= screen.x as f64 + 0.5 && delta.x < 0.0;
        let right = p.x >= screen.right() - 0.5 && delta.x > 0.0;
        let top = p.y <= screen.y as f64 + 0.5 && delta.y < 0.0;
        let bottom = p.y >= screen.bottom() - 0.5 && delta.y > 0.0;
        if !(left || right || top || bottom) {
            return None;
        }
        if block_corners
            && (((left || right)
                && (p.y - (screen.y as f64) < 100.0 || screen.bottom() - p.y < 100.0))
                || ((top || bottom)
                    && (p.x - (screen.x as f64) < 100.0 || screen.right() - p.x < 100.0)))
        {
            return None;
        }
        let length = delta.x.hypot(delta.y);
        if !length.is_finite() || length < 0.001 {
            return None;
        }
        let direction = Point {
            x: delta.x / length,
            y: delta.y / length,
        };
        let origin = device.world(p);
        let mut best: Option<(f64, &Device, Rect)> = None;
        for d in self.devices.iter().filter(|d| d.online) {
            for s in &d.info.screens {
                let rect = d.screen_rect(s);
                if rect.contains(origin) {
                    continue;
                }
                if let Some(t) = ray_entry(rect, origin, direction)
                    && t >= 0.0
                    && best.as_ref().is_none_or(|(old, _, _)| t < *old)
                {
                    best = Some((t, d, rect));
                }
            }
        }
        let (t, target, rect) = best?;
        if target.info.id == source {
            return None;
        }
        let at = rect.clamp(Point {
            x: origin.x + direction.x * (t + 4.0),
            y: origin.y + direction.y * (t + 4.0),
        });
        Some((target.info.id.clone(), target.info.clamp(target.local(at))))
    }
}
fn ray_entry(r: Rect, p: Point, d: Point) -> Option<f64> {
    let mut near: f64 = 0.0;
    let mut far = f64::INFINITY;
    for (lo, hi, origin, direction) in [
        (r.x as f64, r.right(), p.x, d.x),
        (r.y as f64, r.bottom(), p.y, d.y),
    ] {
        if direction.abs() < 1e-9 {
            if origin < lo || origin > hi {
                return None;
            }
        } else {
            let a = (lo - origin) / direction;
            let b = (hi - origin) / direction;
            near = near.max(a.min(b));
            far = far.min(a.max(b));
        }
    }
    (far >= near && far >= 0.0).then_some(near)
}
#[cfg(test)]
pub(crate) fn test_device(id: u8, width: i32, height: i32) -> DeviceInfo {
    let rect = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    DeviceInfo {
        id: format!("{id:032x}"),
        name: format!("PC-{id}"),
        bounds: rect,
        screens: vec![Screen {
            name: "screen".into(),
            rect,
            primary: true,
        }],
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn layout() -> Layout {
        Layout {
            devices: vec![
                Device {
                    info: test_device(1, 1920, 1080),
                    x: 0,
                    y: 0,
                    online: true,
                },
                Device {
                    info: test_device(2, 3840, 2160),
                    x: 2100,
                    y: 500,
                    online: true,
                },
                Device {
                    info: test_device(3, 1080, 1920),
                    x: -800,
                    y: -2100,
                    online: true,
                },
            ],
        }
    }
    #[test]
    fn removing_an_offline_device_frees_capacity_and_preserves_other_positions() {
        let mut l = Layout::default();
        for id in 1..=MAX_DEVICES as u8 {
            l.upsert(test_device(id, 1920, 1080), id == 1).unwrap();
        }
        let extra = test_device(17, 1920, 1080);
        assert!(l.upsert(extra.clone(), true).is_err());
        let original = l.clone();
        assert!(l.without_offline(&l.devices[0].info.id).is_err());
        let removed = l.devices[1].info.id.clone();
        l = l.without_offline(&removed).unwrap();
        assert_eq!(l.devices[1..], original.devices[2..]);
        l.upsert(extra, true).unwrap();
        assert_eq!(l.devices.len(), MAX_DEVICES);
        l.validate().unwrap();
    }
    #[test]
    fn arbitrary_offset_and_vertical_gaps_route_to_actual_intersections() {
        let l = layout();
        l.validate().unwrap();
        let a = &l.devices[0].info.id;
        assert!(
            l.crossing(
                a,
                Point {
                    x: 1919.0,
                    y: 200.0
                },
                Point { x: 1.0, y: 0.0 },
                false
            )
            .is_none()
        );
        let (id, p) = l
            .crossing(
                a,
                Point {
                    x: 1919.0,
                    y: 700.0,
                },
                Point { x: 1.0, y: 0.0 },
                false,
            )
            .unwrap();
        assert_eq!(id, l.devices[1].info.id);
        assert_eq!(p.y, 200.0);
        let (id, _) = l
            .crossing(
                a,
                Point { x: 100.0, y: 0.0 },
                Point { x: 0.0, y: -1.0 },
                false,
            )
            .unwrap();
        assert_eq!(id, l.devices[2].info.id);
    }
    #[test]
    fn overlap_and_unknown_ids_are_rejected_without_mutation() {
        let l = layout();
        let mut p = l.placements();
        p[1].x = 0;
        p[1].y = 0;
        assert!(l.positioned(&p).is_err());
        p = l.placements();
        p[1].id = p[0].id.clone();
        assert!(l.positioned(&p).is_err());
        assert_eq!(l.devices[1].x, 2100);
    }
    #[test]
    fn offline_screens_and_corner_guard_do_not_switch() {
        let mut l = layout();
        let a = l.devices[0].info.id.clone();
        l.devices[1].online = false;
        assert!(
            l.crossing(
                &a,
                Point {
                    x: 1919.0,
                    y: 700.0
                },
                Point { x: 1.0, y: 0.0 },
                false
            )
            .is_none()
        );
        assert!(
            l.crossing(
                &a,
                Point { x: 10.0, y: 0.0 },
                Point { x: 0.0, y: -1.0 },
                true
            )
            .is_none()
        );
    }
    #[test]
    fn saved_geometry_survives_reconnect_and_size_change_cannot_overlap() {
        let mut l = layout();
        let info = l.devices[1].info.clone();
        l.upsert(info, false).unwrap();
        assert_eq!((l.devices[1].x, l.devices[1].y), (2100, 500));
        let mut info = l.devices[0].info.clone();
        info.bounds.width = 3000;
        info.screens[0].rect.width = 3000;
        l.upsert(info, true).unwrap();
        l.validate().unwrap();
    }
}
