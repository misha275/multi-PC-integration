//! Screen layout: where every machine's monitors sit in one shared "global" plane,
//! and how a cursor movement on one machine turns into a jump to another.
//!
//! Each machine reports its monitors in its own virtual-desktop coordinates
//! (on Windows these can be negative). The user arranges machines in the UI;
//! a [`Placement`] is the global position of the top-left corner of a machine's
//! bounding box. Converting between local and global coordinates is then a shift.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }

    /// Nearest point inside the rectangle.
    pub fn clamp(&self, p: Point) -> Point {
        Point::new(p.x.clamp(self.x, self.right() - 1), p.y.clamp(self.y, self.bottom() - 1))
    }

    pub fn center(&self) -> Point {
        Point::new(self.x + self.w / 2, self.y + self.h / 2)
    }

    pub fn union(&self, o: &Rect) -> Rect {
        let x = self.x.min(o.x);
        let y = self.y.min(o.y);
        Rect::new(x, y, self.right().max(o.right()) - x, self.bottom().max(o.bottom()) - y)
    }
}

/// Global position of the top-left corner of a machine's monitor bounding box.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
}

/// Bounding box of a set of monitors.
pub fn bounding_box(monitors: &[Rect]) -> Option<Rect> {
    let mut it = monitors.iter();
    let first = *it.next()?;
    Some(it.fold(first, |acc, r| acc.union(r)))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineGeom {
    /// Monitors in the machine's own coordinates.
    pub monitors: Vec<Rect>,
    pub placement: Placement,
    origin: Point,
}

impl MachineGeom {
    pub fn new(monitors: Vec<Rect>, placement: Placement) -> Self {
        let bb = bounding_box(&monitors).unwrap_or_default();
        Self { monitors, placement, origin: Point::new(bb.x, bb.y) }
    }

    pub fn to_global(&self, p: Point) -> Point {
        Point::new(p.x - self.origin.x + self.placement.x, p.y - self.origin.y + self.placement.y)
    }

    pub fn to_local(&self, g: Point) -> Point {
        Point::new(g.x - self.placement.x + self.origin.x, g.y - self.placement.y + self.origin.y)
    }

    pub fn contains_local(&self, p: Point) -> bool {
        self.monitors.iter().any(|m| m.contains(p))
    }

    /// Nearest point that lies on one of this machine's monitors.
    pub fn clamp_local(&self, p: Point) -> Point {
        self.monitors
            .iter()
            .map(|m| m.clamp(p))
            .min_by_key(|c| {
                let dx = (c.x - p.x) as i64;
                let dy = (c.y - p.y) as i64;
                dx * dx + dy * dy
            })
            .unwrap_or(p)
    }

    pub fn global_monitors(&self) -> impl Iterator<Item = Rect> + '_ {
        self.monitors.iter().map(|m| {
            let g = self.to_global(Point::new(m.x, m.y));
            Rect::new(g.x, g.y, m.w, m.h)
        })
    }
}

/// What happens when the cursor of a machine moves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// The cursor stays on the same machine, at this local position.
    Stay(Point),
    /// The cursor leaves for another machine and appears at `at` (target-local coordinates).
    Cross { machine: String, at: Point },
}

#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub machines: BTreeMap<String, MachineGeom>,
}

impl Layout {
    pub fn insert(&mut self, name: impl Into<String>, monitors: Vec<Rect>, placement: Placement) {
        self.machines.insert(name.into(), MachineGeom::new(monitors, placement));
    }

    /// Which machine owns a global point (and the local coordinates there).
    pub fn machine_at(&self, g: Point) -> Option<(&str, Point)> {
        self.machines.iter().find_map(|(name, m)| {
            let local = m.to_local(g);
            m.contains_local(local).then_some((name.as_str(), local))
        })
    }

    /// Move the cursor of `machine` from local point `from` by `(dx, dy)`.
    ///
    /// Own monitors always win, so overlapping placements never steal the cursor.
    /// When the target point is on no monitor at all, the cursor is clamped to the
    /// nearest point on its own machine (a screen edge with no neighbour behind it).
    pub fn step(&self, machine: &str, from: Point, dx: i32, dy: i32) -> Step {
        let target = Point::new(from.x.saturating_add(dx), from.y.saturating_add(dy));
        let Some(me) = self.machines.get(machine) else {
            return Step::Stay(target);
        };
        if me.contains_local(target) {
            return Step::Stay(target);
        }
        let g = me.to_global(target);
        for (name, other) in &self.machines {
            if name == machine {
                continue;
            }
            let local = other.to_local(g);
            if other.contains_local(local) {
                return Step::Cross { machine: name.clone(), at: local };
            }
        }
        Step::Stay(me.clamp_local(target))
    }

    /// Global position right of everything already placed, aligned to the top.
    pub fn next_free_placement(&self) -> Placement {
        let right = self.machines.values().flat_map(|m| m.global_monitors().collect::<Vec<_>>()).map(|r| r.right()).max();
        let top = self.machines.values().flat_map(|m| m.global_monitors().collect::<Vec<_>>()).map(|r| r.y).min();
        Placement { x: right.unwrap_or(0), y: top.unwrap_or(0) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_side_by_side() -> Layout {
        let mut l = Layout::default();
        // "a" has two monitors, the second one to the left at negative coordinates.
        l.insert("a", vec![Rect::new(0, 0, 1920, 1080), Rect::new(-1280, 0, 1280, 1024)], Placement { x: 0, y: 0 });
        // "b" sits right of "a"'s bounding box (3200 wide), shifted 100px down.
        l.insert("b", vec![Rect::new(0, 0, 2560, 1440)], Placement { x: 3200, y: 100 });
        l
    }

    #[test]
    fn local_global_roundtrip() {
        let l = two_side_by_side();
        let a = &l.machines["a"];
        assert_eq!(a.to_global(Point::new(-1280, 0)), Point::new(0, 0));
        assert_eq!(a.to_global(Point::new(0, 0)), Point::new(1280, 0));
        assert_eq!(a.to_local(Point::new(1280, 0)), Point::new(0, 0));
    }

    #[test]
    fn stays_inside_own_monitors() {
        let l = two_side_by_side();
        assert_eq!(l.step("a", Point::new(100, 100), 10, 5), Step::Stay(Point::new(110, 105)));
        // Moving between a's own two monitors is not a crossing.
        assert_eq!(l.step("a", Point::new(0, 10), -1, 0), Step::Stay(Point::new(-1, 10)));
    }

    #[test]
    fn crosses_right_edge() {
        let l = two_side_by_side();
        let step = l.step("a", Point::new(1919, 500), 1, 0);
        assert_eq!(step, Step::Cross { machine: "b".into(), at: Point::new(0, 400) });
    }

    #[test]
    fn crosses_back_left() {
        let l = two_side_by_side();
        let step = l.step("b", Point::new(0, 400), -3, 0);
        assert_eq!(step, Step::Cross { machine: "a".into(), at: Point::new(1917, 500) });
    }

    #[test]
    fn edge_without_neighbour_clamps() {
        let l = two_side_by_side();
        // b is 100px lower, so at y < 100 there is nothing to the right of a.
        assert_eq!(l.step("a", Point::new(1919, 50), 5, 0), Step::Stay(Point::new(1919, 50)));
        assert_eq!(l.step("a", Point::new(500, 0), 0, -10), Step::Stay(Point::new(500, 0)));
    }

    #[test]
    fn machine_at_global() {
        let l = two_side_by_side();
        assert_eq!(l.machine_at(Point::new(10, 10)), Some(("a", Point::new(-1270, 10))));
        assert_eq!(l.machine_at(Point::new(3300, 200)), Some(("b", Point::new(100, 100))));
        assert_eq!(l.machine_at(Point::new(3300, 50)), None);
    }

    #[test]
    fn vertical_arrangement() {
        let mut l = Layout::default();
        l.insert("top", vec![Rect::new(0, 0, 1920, 1080)], Placement { x: 0, y: 0 });
        l.insert("bottom", vec![Rect::new(0, 0, 1920, 1080)], Placement { x: 0, y: 1080 });
        assert_eq!(l.step("top", Point::new(960, 1079), 0, 4), Step::Cross { machine: "bottom".into(), at: Point::new(960, 3) });
    }

    #[test]
    fn next_free_placement_goes_right() {
        let l = two_side_by_side();
        assert_eq!(l.next_free_placement(), Placement { x: 5760, y: 0 });
        assert_eq!(Layout::default().next_free_placement(), Placement { x: 0, y: 0 });
    }
}
