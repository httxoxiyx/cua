//! Display geometry is independent of AppKit objects and cursor ownership.
//! AX/CG points are global and top-left based; AppKit window frames are kept
//! separately so the primary display's height is never applied twice.

#[derive(Clone, Debug, PartialEq)]
pub(super) struct DisplayGeometry {
    pub id: u32,
    pub appkit_frame: [f64; 4],
    pub bounds: [f64; 4],
    pub scale: f64,
}

impl DisplayGeometry {
    pub fn new(id: u32, appkit_frame: [f64; 4], bounds: [f64; 4], scale: f64) -> Option<Self> {
        let display = Self {
            id,
            appkit_frame,
            bounds,
            scale,
        };
        if id == 0
            || !scale.is_finite()
            || scale < 1.0
            || !appkit_frame
                .iter()
                .chain(bounds.iter())
                .all(|v| v.is_finite())
            || appkit_frame[2] <= 0.0
            || appkit_frame[3] <= 0.0
            || bounds[2] <= 0.0
            || bounds[3] <= 0.0
            || !(bounds[0] + bounds[2]).is_finite()
            || !(bounds[1] + bounds[3]).is_finite()
            || display.pixel_size().is_none()
        {
            return None;
        }
        Some(display)
    }

    pub fn contains(&self, point: (f64, f64)) -> bool {
        let [x, y, w, h] = self.bounds;
        point.0 >= x && point.0 < x + w && point.1 >= y && point.1 < y + h
    }

    pub fn pixel_size(&self) -> Option<(u32, u32)> {
        // Bound each allocation even if a display API returns corrupt geometry.
        const MAX_PIXELS: f64 = 64.0 * 1024.0 * 1024.0;
        let w = (self.bounds[2] * self.scale).ceil();
        let h = (self.bounds[3] * self.scale).ceil();
        (w.is_finite()
            && h.is_finite()
            && w >= 1.0
            && h >= 1.0
            && w <= u32::MAX as f64
            && h <= u32::MAX as f64
            && w * h <= MAX_PIXELS)
            .then_some((w as u32, h as u32))
    }

    fn clamp(&self, point: (f64, f64)) -> (f64, f64) {
        let [x, y, w, h] = self.bounds;
        let mx = 2.0_f64.min(w / 2.0);
        let my = 2.0_f64.min(h / 2.0);
        (
            point.0.clamp(x + mx, x + w - mx),
            point.1.clamp(y + my, y + h - my),
        )
    }

    fn distance_squared(&self, point: (f64, f64)) -> f64 {
        let nearest = self.clamp(point);
        (point.0 - nearest.0).powi(2) + (point.1 - nearest.1).powi(2)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct DisplayLayout {
    pub generation: u64,
    pub displays: Vec<DisplayGeometry>,
}

impl DisplayLayout {
    pub fn replace(&mut self, mut displays: Vec<DisplayGeometry>) -> bool {
        displays.sort_by_key(|display| display.id);
        if displays.windows(2).any(|pair| pair[0].id == pair[1].id) || self.displays == displays {
            return false;
        }
        self.generation = self
            .generation
            .checked_add(1)
            .expect("display generation exhausted");
        self.displays = displays;
        true
    }

    pub fn contains(&self, point: (f64, f64)) -> bool {
        self.displays.iter().any(|display| display.contains(point))
    }

    pub fn accepts_frame(&self, generation: u64, display_id: u32) -> bool {
        self.generation == generation
            && self.displays.iter().any(|display| display.id == display_id)
    }

    /// Prefer the target display, not a union rectangle that includes desktop
    /// gaps. A cursor on an unplugged display can be reseeded by its next action.
    pub fn seed_near(&self, target: (f64, f64)) -> Option<(f64, f64)> {
        if !target.0.is_finite() || !target.1.is_finite() {
            return None;
        }
        let display = self
            .displays
            .iter()
            .find(|display| display.contains(target))
            .or_else(|| {
                self.displays.iter().min_by(|a, b| {
                    a.distance_squared(target)
                        .total_cmp(&b.distance_squared(target))
                })
            })?;
        const OFFSET: f64 = 140.0;
        let mut seed = display.clamp((target.0 - OFFSET, target.1 - OFFSET));
        if (seed.0 - target.0).abs() < 8.0 && (seed.1 - target.1).abs() < 8.0 {
            seed = display.clamp((target.0 + OFFSET, target.1 + OFFSET));
        }
        Some(seed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn primary() -> DisplayGeometry {
        DisplayGeometry::new(
            1,
            [0.0, 0.0, 1728.0, 1117.0],
            [0.0, 0.0, 1728.0, 1117.0],
            2.0,
        )
        .unwrap()
    }

    fn above() -> DisplayGeometry {
        DisplayGeometry::new(
            3,
            [-192.0, 1117.0, 1920.0, 1080.0],
            [-192.0, -1080.0, 1920.0, 1080.0],
            2.0,
        )
        .unwrap()
    }

    #[test]
    fn reported_above_primary_layout_keeps_global_and_appkit_origins_distinct() {
        let mut layout = DisplayLayout::default();
        assert!(layout.replace(vec![primary(), above()]));
        assert!(layout.contains((768.0, -540.0)));
        assert!(layout.contains((-180.0, -900.0)));
        assert!(!primary().contains((768.0, -540.0)));
        let external = &layout.displays[1];
        assert_eq!(external.pixel_size(), Some((3840, 2160)));
        assert_eq!(external.appkit_frame[1], 1117.0);
        assert_eq!(
            (768.0 - external.bounds[0], -540.0 - external.bounds[1]),
            (960.0, 540.0)
        );
        assert!(external.contains(layout.seed_near((768.0, -540.0)).unwrap()));
    }

    #[test]
    fn left_right_below_and_mixed_scale_displays_seed_on_the_target_display() {
        for bounds in [
            [-1920.0, 0.0, 1920.0, 1080.0],
            [1728.0, 0.0, 1920.0, 1080.0],
            [0.0, 1117.0, 1920.0, 1080.0],
            [-2000.0, -1200.0, 1920.0, 1080.0],
        ] {
            for scale in [1.0, 2.0] {
                let display = DisplayGeometry::new(3, bounds, bounds, scale).unwrap();
                let mut layout = DisplayLayout::default();
                layout.replace(vec![primary(), display.clone()]);
                for target in [
                    (bounds[0] + 1.0, bounds[1] + 1.0),
                    (bounds[0] + 900.0, bounds[1] + 400.0),
                ] {
                    assert!(display.contains(layout.seed_near(target).unwrap()));
                }
                assert_eq!(
                    display.pixel_size(),
                    Some(((1920.0 * scale) as u32, (1080.0 * scale) as u32))
                );
            }
        }
    }

    #[test]
    fn gaps_are_not_displays_and_empty_layouts_do_not_invent_a_screen() {
        let mut layout = DisplayLayout::default();
        assert_eq!(layout.seed_near((10.0, 10.0)), None);
        let external = DisplayGeometry::new(
            3,
            [2000.0, 0.0, 1000.0, 800.0],
            [2000.0, 0.0, 1000.0, 800.0],
            1.0,
        )
        .unwrap();
        layout.replace(vec![primary(), external]);
        assert!(!layout.contains((1900.0, 500.0)));
        assert!(layout.contains(layout.seed_near((1900.0, 500.0)).unwrap()));
        assert_eq!(layout.seed_near((f64::NAN, 0.0)), None);
    }

    #[test]
    fn hotplug_layout_and_scale_changes_invalidate_old_frames_even_if_id_is_reused() {
        let mut layout = DisplayLayout::default();
        layout.replace(vec![primary(), above()]);
        let original = layout.generation;
        assert!(
            !layout.replace(vec![above(), primary()]),
            "enumeration order is not a layout change"
        );
        assert!(layout.accepts_frame(original, 3));
        layout.replace(vec![primary()]);
        assert!(!layout.accepts_frame(original, 3));
        layout.replace(vec![primary(), above()]);
        assert!(
            !layout.accepts_frame(original, 3),
            "reconnected display must not accept an old frame"
        );
        let mut changed = above();
        changed.scale = 1.0;
        let before_scale = layout.generation;
        assert!(layout.replace(vec![primary(), changed]));
        assert!(!layout.accepts_frame(before_scale, 3));
        let mut moved = above();
        moved.bounds[0] -= 100.0;
        assert!(layout.replace(vec![primary(), moved]));
    }

    #[test]
    fn invalid_geometry_and_unbounded_allocations_are_rejected() {
        for bounds in [
            [0.0, 0.0, 0.0, 20.0],
            [0.0, 0.0, -10.0, 20.0],
            [f64::NAN, 0.0, 20.0, 20.0],
            [0.0, 0.0, 1e20, 1e20],
        ] {
            assert!(DisplayGeometry::new(1, bounds, bounds, 1.0).is_none());
        }
        assert!(DisplayGeometry::new(
            1,
            [0.0, 0.0, 20.0, 20.0],
            [0.0, 0.0, 20.0, 20.0],
            f64::INFINITY
        )
        .is_none());
    }
}
