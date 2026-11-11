//! Coordinate frames and clamping (task 2.2, item 4).
//!
//! Callers give x,y in one of four frames and every action needs device pixels on a display:
//!
//! * `Device`: real screen pixels (the `@(x,y)` in the tree).
//! * `Image`: pixels of the latest screenshot, scaled by `device_width / image_width`.
//! * `Window`: pixels relative to a window's top-left corner; clamped to the window and the screen.
//! * `VirtualDisplay`: pixels of a virtual display (its own size, display id carried on the result).
//!
//! A point outside the frame's region is clamped to its edge and the result says so; nothing is
//! ever silently moved. The 17 Oct bug (a 720-wide-frame y read against a 480-wide screenshot)
//! is the reason: the reply always names the frame and the scale used.

use crate::tree::Bounds;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Space {
    Image,
    Device,
    Window,
    VirtualDisplay,
}

/// Geometry of the latest screenshot: device size and the size of the image the caller was shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shot {
    pub dev_w: i32,
    pub dev_h: i32,
    pub img_w: i32,
    pub img_h: i32,
}

impl Shot {
    pub fn scale(&self) -> f64 {
        f64::from(self.dev_w) / f64::from(self.img_w)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// `space='image'` with no screenshot and no `image_width`.
    NeedShot,
    BadSpace(String),
    NotFinite,
    /// Zero or negative size, or a window that does not intersect the screen.
    EmptyRegion(String),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::NeedShot => write!(f, "space='image' needs a screenshot in this session, or image_width. Use space='device'."),
            FrameError::BadSpace(s) => write!(f, "space must be 'image' (pixels of a screenshot) or 'device' (real screen pixels), not {s:?}."),
            FrameError::NotFinite => write!(f, "coordinates must be finite numbers"),
            FrameError::EmptyRegion(s) => write!(f, "empty region: {s}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// A point in device pixels of one display, with how it got there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapped {
    pub x: i32,
    pub y: i32,
    pub display: u32,
    pub clamped: bool,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub space: Space,
    scale: f64,
    /// Offset added after scaling (a window's top-left, in device pixels).
    origin: (i64, i64),
    /// Region the result must land in, in the target display's pixels; the last pixel is `r - 1`.
    region: Bounds,
    region_name: String,
    pub display: u32,
}

impl Frame {
    /// Real screen pixels on display 0.
    pub fn device(w: i32, h: i32) -> Result<Frame, FrameError> {
        Frame::checked(
            Space::Device,
            1.0,
            (0, 0),
            Bounds {
                l: 0,
                t: 0,
                r: w,
                b: h,
            },
            format!("{w}x{h} screen"),
            0,
        )
    }

    /// Pixels of a screenshot: `image_width` overrides the latest shot's scale.
    pub fn image(
        dev_w: i32,
        dev_h: i32,
        shot: Option<Shot>,
        image_width: Option<i32>,
    ) -> Result<Frame, FrameError> {
        let scale = match (image_width, shot) {
            (Some(w), _) if w > 0 => f64::from(dev_w) / f64::from(w),
            (Some(_), _) => {
                return Err(FrameError::EmptyRegion(
                    "image_width must be positive".into(),
                ))
            }
            (None, Some(s)) => s.scale(),
            (None, None) => return Err(FrameError::NeedShot),
        };
        Frame::checked(
            Space::Image,
            scale,
            (0, 0),
            Bounds {
                l: 0,
                t: 0,
                r: dev_w,
                b: dev_h,
            },
            format!("{dev_w}x{dev_h} screen"),
            0,
        )
    }

    /// Pixels relative to a window's top-left. `window` is in screen pixels; the result must be inside
    /// both the window and the screen.
    pub fn window(screen_w: i32, screen_h: i32, window: Bounds) -> Result<Frame, FrameError> {
        let region = Bounds {
            l: window.l.max(0),
            t: window.t.max(0),
            r: window.r.min(screen_w),
            b: window.b.min(screen_h),
        };
        let name = format!(
            "window [{},{}][{},{}]",
            window.l, window.t, window.r, window.b
        );
        Frame::checked(
            Space::Window,
            1.0,
            (i64::from(window.l), i64::from(window.t)),
            region,
            name,
            0,
        )
    }

    /// Pixels of virtual display `id` of size `w` x `h`.
    pub fn virtual_display(id: u32, w: i32, h: i32) -> Result<Frame, FrameError> {
        Frame::checked(
            Space::VirtualDisplay,
            1.0,
            (0, 0),
            Bounds {
                l: 0,
                t: 0,
                r: w,
                b: h,
            },
            format!("{w}x{h} virtual display {id}"),
            id,
        )
    }

    /// phone-hands' `_frame`: `space` is `None` (image if a screenshot or width exists, else device),
    /// `"image"` or `"device"`.
    pub fn resolve(
        space: Option<&str>,
        dev_w: i32,
        dev_h: i32,
        shot: Option<Shot>,
        image_width: Option<i32>,
    ) -> Result<Frame, FrameError> {
        let space = space.unwrap_or(if shot.is_some() || image_width.is_some() {
            "image"
        } else {
            "device"
        });
        match space {
            "device" => Frame::device(dev_w, dev_h),
            "image" => Frame::image(dev_w, dev_h, shot, image_width),
            other => Err(FrameError::BadSpace(other.to_string())),
        }
    }

    fn checked(
        space: Space,
        scale: f64,
        origin: (i64, i64),
        region: Bounds,
        name: String,
        display: u32,
    ) -> Result<Frame, FrameError> {
        if !scale.is_finite() || scale <= 0.0 {
            return Err(FrameError::EmptyRegion(format!("scale {scale}")));
        }
        if region.r <= region.l || region.b <= region.t {
            return Err(FrameError::EmptyRegion(name));
        }
        Ok(Frame {
            space,
            scale,
            origin,
            region,
            region_name: name,
            display,
        })
    }

    /// The scale from frame units to device pixels.
    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// "device px", "image px (x2.542, ...)", and so on, for the reply.
    pub fn describe(&self) -> String {
        match self.space {
            Space::Device => "device px".into(),
            Space::Image => format!(
                "image px (x{:.3}, a {} px wide image)",
                self.scale,
                (f64::from(self.region.width()) / self.scale).round()
            ),
            Space::Window => format!("px relative to {}", self.region_name),
            Space::VirtualDisplay => format!("px of {}", self.region_name),
        }
    }

    /// Map `(x, y)` in this frame to display pixels, clamping to the region.
    pub fn map(&self, x: f64, y: f64) -> Result<Mapped, FrameError> {
        if !x.is_finite() || !y.is_finite() {
            return Err(FrameError::NotFinite);
        }
        // `as i64` saturates, so absurd inputs still clamp.
        let dx = (x * self.scale).round() as i64 + self.origin.0;
        let dy = (y * self.scale).round() as i64 + self.origin.1;
        let cx = dx.clamp(i64::from(self.region.l), i64::from(self.region.r - 1));
        let cy = dy.clamp(i64::from(self.region.t), i64::from(self.region.b - 1));
        let clamped = (cx, cy) != (dx, dy);
        let mut notes = Vec::new();
        if clamped {
            notes.push(format!(
                "CLAMPED ({dx},{dy}) to ({cx},{cy}): it was outside the {}",
                self.region_name
            ));
        }
        Ok(Mapped {
            x: cx as i32,
            y: cy as i32,
            display: self.display,
            clamped,
            notes,
        })
    }

    /// A relative point (`fx`, `fy` in 0..=1 of the region) to display pixels. 0 is the first pixel and
    /// 1 the last, so it never needs clamping.
    pub fn map_rel(&self, fx: f64, fy: f64) -> Result<Mapped, FrameError> {
        if !fx.is_finite() || !fy.is_finite() {
            return Err(FrameError::NotFinite);
        }
        let (fx, fy) = (fx.clamp(0.0, 1.0), fy.clamp(0.0, 1.0));
        let x = f64::from(self.region.l) + fx * f64::from(self.region.width() - 1);
        let y = f64::from(self.region.t) + fy * f64::from(self.region.height() - 1);
        Ok(Mapped {
            x: x.round() as i32,
            y: y.round() as i32,
            display: self.display,
            clamped: false,
            notes: vec![],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHOT: Shot = Shot {
        dev_w: 1220,
        dev_h: 2712,
        img_w: 480,
        img_h: 1067,
    };

    fn xy(m: &Mapped) -> (i32, i32) {
        (m.x, m.y)
    }

    #[test]
    fn image_to_device_scale() {
        let f = Frame::resolve(None, 1220, 2712, Some(SHOT), None).unwrap();
        assert_eq!(f.space, Space::Image);
        let m = f.map(100.0, 200.0).unwrap();
        assert_eq!(xy(&m), (254, 508));
        assert!(!m.clamped && m.notes.is_empty());
        assert!(f.describe().starts_with("image px"));
        let f = Frame::resolve(None, 1220, 2712, Some(SHOT), Some(720)).unwrap();
        assert_eq!(xy(&f.map(100.0, 200.0).unwrap()), (169, 339));
        let f = Frame::resolve(Some("device"), 1220, 2712, Some(SHOT), None).unwrap();
        assert_eq!(xy(&f.map(610.0, 1400.0).unwrap()), (610, 1400));
        assert_eq!(f.describe(), "device px");
        // no shot and no space: device
        assert_eq!(
            Frame::resolve(None, 1220, 2712, None, None).unwrap().space,
            Space::Device
        );
    }

    #[test]
    fn seventeenth_october_bug_is_clamped_and_reported() {
        let f = Frame::resolve(None, 1220, 2712, Some(SHOT), None).unwrap();
        let m = f.map(360.0, 1400.0).unwrap();
        assert_eq!(xy(&m), (915, 2711));
        assert!(m.clamped && m.notes[0].contains("CLAMPED"));
    }

    #[test]
    fn clamping_at_every_edge_of_the_screen() {
        let f = Frame::device(1220, 2712).unwrap();
        let cases = [
            // (input, expected, clamped)
            ((0.0, 0.0), (0, 0), false),
            ((1219.0, 2711.0), (1219, 2711), false),
            ((-1.0, 100.0), (0, 100), true),        // left
            ((1220.0, 100.0), (1219, 100), true),   // right: one past the last pixel
            ((100.0, -1.0), (100, 0), true),        // top
            ((100.0, 2712.0), (100, 2711), true),   // bottom
            ((-5.0, -5.0), (0, 0), true),           // top-left corner
            ((5000.0, 5000.0), (1219, 2711), true), // bottom-right corner
            ((-5.0, 5000.0), (0, 2711), true),      // bottom-left corner
            ((5000.0, -5.0), (1219, 0), true),      // top-right corner
            ((1e300, -1e300), (1219, 0), true),     // absurd values still clamp
        ];
        for ((x, y), want, clamped) in cases {
            let m = f.map(x, y).unwrap();
            assert_eq!((xy(&m), m.clamped), (want, clamped), "input ({x},{y})");
            assert_eq!(m.notes.is_empty(), !clamped);
        }
        assert_eq!(f.map(f64::NAN, 0.0), Err(FrameError::NotFinite));
        assert_eq!(f.map(0.0, f64::INFINITY), Err(FrameError::NotFinite));
    }

    #[test]
    fn window_frame_offsets_and_clamps_to_window_and_screen() {
        let w = Frame::window(
            1220,
            2712,
            Bounds {
                l: 100,
                t: 200,
                r: 600,
                b: 800,
            },
        )
        .unwrap();
        assert_eq!(xy(&w.map(10.0, 20.0).unwrap()), (110, 220));
        assert_eq!(xy(&w.map(0.0, 0.0).unwrap()), (100, 200));
        assert_eq!(xy(&w.map(499.0, 599.0).unwrap()), (599, 799));
        // each edge of the window
        assert_eq!(xy(&w.map(-1.0, 10.0).unwrap()), (100, 210));
        assert_eq!(xy(&w.map(500.0, 10.0).unwrap()), (599, 210));
        assert_eq!(xy(&w.map(10.0, -1.0).unwrap()), (110, 200));
        assert_eq!(xy(&w.map(10.0, 600.0).unwrap()), (110, 799));
        assert!(w.map(500.0, 10.0).unwrap().clamped);
        // a window hanging off the screen is clamped to the screen as well
        let off = Frame::window(
            1220,
            2712,
            Bounds {
                l: 1000,
                t: 2500,
                r: 1500,
                b: 3000,
            },
        )
        .unwrap();
        assert_eq!(xy(&off.map(400.0, 400.0).unwrap()), (1219, 2711));
        let m = off.map(0.0, 0.0).unwrap();
        assert_eq!(xy(&m), (1000, 2500));
        assert!(!m.clamped);
        // a window wholly off screen is refused
        assert!(matches!(
            Frame::window(
                1220,
                2712,
                Bounds {
                    l: 2000,
                    t: 0,
                    r: 2100,
                    b: 10
                }
            ),
            Err(FrameError::EmptyRegion(_))
        ));
    }

    #[test]
    fn virtual_display_carries_its_id_and_size() {
        let v = Frame::virtual_display(7, 720, 1280).unwrap();
        let m = v.map(719.0, 1279.0).unwrap();
        assert_eq!((xy(&m), m.display, m.clamped), ((719, 1279), 7, false));
        let m = v.map(720.0, -3.0).unwrap();
        assert_eq!((xy(&m), m.display, m.clamped), ((719, 0), 7, true));
        assert!(m.notes[0].contains("virtual display 7"));
        // the same point on the screen is not clamped: frames do not leak into each other
        assert!(
            !Frame::device(1220, 2712)
                .unwrap()
                .map(720.0, 1280.0)
                .unwrap()
                .clamped
        );
        assert!(Frame::virtual_display(1, 0, 10).is_err());
    }

    #[test]
    fn relative_points_hit_the_corners_exactly() {
        let f = Frame::device(1220, 2712).unwrap();
        assert_eq!(xy(&f.map_rel(0.0, 0.0).unwrap()), (0, 0));
        assert_eq!(xy(&f.map_rel(1.0, 1.0).unwrap()), (1219, 2711));
        assert_eq!(xy(&f.map_rel(0.5, 0.5).unwrap()), (610, 1356));
        let w = Frame::window(
            1220,
            2712,
            Bounds {
                l: 100,
                t: 200,
                r: 600,
                b: 800,
            },
        )
        .unwrap();
        assert_eq!(xy(&w.map_rel(1.0, 1.0).unwrap()), (599, 799));
        assert!(f.map_rel(f64::NAN, 0.0).is_err());
    }

    #[test]
    fn errors() {
        assert_eq!(
            Frame::resolve(Some("image"), 1220, 2712, None, None),
            Err(FrameError::NeedShot)
        );
        assert!(matches!(
            Frame::resolve(Some("moon"), 1220, 2712, None, None),
            Err(FrameError::BadSpace(_))
        ));
        assert!(Frame::image(1220, 2712, None, Some(0)).is_err());
    }
}
