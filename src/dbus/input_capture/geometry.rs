//! Logical portal zones and outside-edge barriers. Pixel spans are inclusive;
//! crossing uses the unconfined destination, before normal pointer clipping.
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_ZONES: usize = 64;
pub const MAX_BARRIERS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Zone {
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
}
impl Zone {
    fn bounds(self) -> [i64; 4] {
        [
            self.x.into(),
            self.y.into(),
            i64::from(self.x) + i64::from(self.width),
            i64::from(self.y) + i64::from(self.height),
        ]
    }
    fn contains(self, p: (f64, f64)) -> bool {
        let [x, y, right, bottom] = self.bounds();
        x as f64 <= p.0 && p.0 < right as f64 && y as f64 <= p.1 && p.1 < bottom as f64
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Barrier {
    pub id: u32,
    pub position: (i32, i32, i32, i32),
}
#[derive(Clone, Copy, Debug)]
struct Edge {
    id: u32,
    vertical: bool,
    coordinate: i64,
    start: i64,
    end: i64,
    positive: bool,
}
impl Edge {
    fn candidates(barrier: Barrier, zone: Zone) -> Vec<Self> {
        let (x1, y1, x2, y2) = barrier.position;
        let [left, top, right, bottom] = zone.bounds();
        let mut edges = Vec::with_capacity(2);
        if x1 == x2 {
            let coordinate = i64::from(x1);
            let start = i64::from(y1.min(y2));
            let end = i64::from(y1.max(y2));
            if top <= start && end < bottom && (coordinate == left || coordinate == right) {
                edges.push(Self {
                    id: barrier.id,
                    vertical: true,
                    coordinate,
                    start,
                    end,
                    positive: coordinate == right,
                });
            }
        }
        if y1 == y2 {
            let coordinate = i64::from(y1);
            let start = i64::from(x1.min(x2));
            let end = i64::from(x1.max(x2));
            if left <= start && end < right && (coordinate == top || coordinate == bottom) {
                edges.push(Self {
                    id: barrier.id,
                    vertical: false,
                    coordinate,
                    start,
                    end,
                    positive: coordinate == bottom,
                });
            }
        }
        edges
    }
    fn outside(self, zones: &[Zone]) -> bool {
        zones.iter().all(|zone| {
            let [x, y, right, bottom] = zone.bounds();
            let (low, high, start, end) = if self.vertical {
                (x, right, y, bottom)
            } else {
                (y, bottom, x, right)
            };
            let covered = if self.positive {
                low <= self.coordinate && self.coordinate < high
            } else {
                low < self.coordinate && self.coordinate <= high
            };
            // A touching zone endpoint does not cover the next inclusive pixel.
            !covered || end <= self.start || self.end < start
        })
    }
    fn crossing(self, from: (f64, f64), to: (f64, f64)) -> Option<f64> {
        let (old, new, tangent_old, tangent_new) = if self.vertical {
            (from.0, to.0, from.1, to.1)
        } else {
            (from.1, to.1, from.0, to.0)
        };
        let boundary = self.coordinate as f64;
        // Left/top boundary pixels remain inside their zone. Right/bottom
        // boundaries already lie outside the half-open logical rectangle.
        let crosses = if self.positive {
            old < boundary && new >= boundary
        } else {
            old >= boundary && new < boundary
        };
        if !crosses {
            return None;
        }
        let t = (boundary - old) / (new - old);
        let tangent = tangent_old + t * (tangent_new - tangent_old);
        (self.start as f64 <= tangent && tangent < (self.end + 1) as f64).then_some(t)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    InvalidZones,
    TooManyBarriers,
    StaleZoneSet,
}

pub struct Space {
    zones: Vec<Zone>,
    generation: u32,
    edges: Vec<Edge>,
}
impl Space {
    pub fn new(zones: Vec<Zone>) -> Result<Self, Error> {
        Ok(Self {
            zones: Self::normalize(zones)?,
            generation: 1,
            edges: Vec::new(),
        })
    }
    fn normalize(mut zones: Vec<Zone>) -> Result<Vec<Zone>, Error> {
        if zones.len() > MAX_ZONES || zones.iter().any(|z| z.width == 0 || z.height == 0) {
            return Err(Error::InvalidZones);
        }
        zones.sort_unstable();
        zones.dedup();
        Ok(zones)
    }
    pub fn zones(&self) -> &[Zone] {
        &self.zones
    }
    pub fn generation(&self) -> u32 {
        self.generation
    }
    /// Returns the invalidated generation. Topology changes erase all barriers.
    pub fn refresh(&mut self, zones: Vec<Zone>) -> Result<Option<u32>, Error> {
        let zones = Self::normalize(zones)?;
        if zones == self.zones {
            return Ok(None);
        }
        let previous = self.generation;
        self.generation = self.generation.wrapping_add(1);
        self.zones = zones;
        self.edges.clear();
        Ok(Some(previous))
    }
    pub fn replace(&mut self, generation: u32, barriers: &[Barrier]) -> Result<Vec<u32>, Error> {
        if generation != self.generation {
            return Err(Error::StaleZoneSet);
        }
        if barriers.len() > MAX_BARRIERS {
            return Err(Error::TooManyBarriers);
        }
        let mut counts = BTreeMap::<u32, usize>::new();
        for barrier in barriers {
            *counts.entry(barrier.id).or_default() += 1;
        }
        let mut edges = Vec::new();
        let mut failed = BTreeSet::new();
        for barrier in barriers {
            let edge = (barrier.id != 0 && counts[&barrier.id] == 1)
                .then(|| {
                    self.zones
                        .iter()
                        .flat_map(|zone| Edge::candidates(*barrier, *zone))
                        .find(|edge| edge.outside(&self.zones))
                })
                .flatten();
            if let Some(edge) = edge {
                edges.push(edge);
            } else {
                failed.insert(barrier.id);
            }
        }
        self.edges = edges;
        Ok(failed.into_iter().collect())
    }
    pub fn crossing(&self, from: (f64, f64), to: (f64, f64)) -> Option<u32> {
        if ![from.0, from.1, to.0, to.1].into_iter().all(f64::is_finite)
            || !self.zones.iter().any(|zone| zone.contains(from))
        {
            return None;
        }
        self.edges
            .iter()
            .filter_map(|edge| edge.crossing(from, to).map(|t| (t, edge.id)))
            .min_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))
            .map(|(_, id)| id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn zone(x: i32, y: i32, width: u32, height: u32) -> Zone {
        Zone {
            x,
            y,
            width,
            height,
        }
    }
    fn barrier(id: u32, position: (i32, i32, i32, i32)) -> Barrier {
        Barrier { id, position }
    }
    #[test]
    fn adjacent_outputs_have_no_internal_barrier() {
        let mut s = Space::new(vec![zone(0, 0, 1920, 1080), zone(1920, 0, 1920, 1080)]).unwrap();
        let positions = [
            (0, 0, 1919, 0),
            (0, 1080, 1919, 1080),
            (1920, 0, 3839, 0),
            (1920, 1080, 3839, 1080),
            (0, 0, 0, 1079),
            (3840, 0, 3840, 1079),
            (1920, 0, 1920, 1079),
            (0, 0, 3839, 0),
        ];
        let b: Vec<_> = positions
            .into_iter()
            .enumerate()
            .map(|(i, p)| barrier(i as u32 + 1, p))
            .collect();
        assert_eq!(s.replace(1, &b).unwrap(), vec![7, 8]);
        assert_eq!(s.crossing((1900., 500.), (1950., 500.)), None);
        assert_eq!(s.crossing((3830., 500.), (3890., 500.)), Some(6));
    }
    #[test]
    fn partial_neighbors_cover_only_their_shared_pixels() {
        let mut s = Space::new(vec![zone(-100, -50, 100, 100), zone(0, -25, 50, 50)]).unwrap();
        assert_eq!(
            s.replace(
                1,
                &[
                    barrier(1, (0, -50, 0, -26)),
                    barrier(2, (0, 25, 0, 49)),
                    barrier(3, (0, -26, 0, -25)),
                    barrier(4, (0, 24, 0, 25))
                ]
            )
            .unwrap(),
            vec![3, 4]
        );
        assert_eq!(s.crossing((-1., -25.1), (3., -25.1)), Some(1));
        assert_eq!(s.crossing((-1., -25.), (3., -25.)), None);
    }
    #[test]
    fn one_pixel_spans_are_inclusive_and_allow_reversed_endpoints() {
        let mut s = Space::new(vec![zone(0, 0, 10, 10)]).unwrap();
        assert!(s
            .replace(
                1,
                &[
                    barrier(1, (10, 8, 10, 9)),
                    barrier(2, (0, 2, 0, 2)),
                    barrier(3, (5, 10, 3, 10))
                ]
            )
            .unwrap()
            .is_empty());
        assert_eq!(s.crossing((9., 9.9), (12., 9.9)), Some(1));
        assert_eq!(s.crossing((1., 2.9), (-2., 2.9)), Some(2));
        assert_eq!(s.crossing((1., 3.), (-2., 3.)), None);
        assert_eq!(s.crossing((5.9, 9.), (5.9, 12.)), Some(3));
    }
    #[test]
    fn invalid_and_duplicate_ids_fail_without_discarding_valid_barriers() {
        let mut s = Space::new(vec![zone(0, 0, 10, 10)]).unwrap();
        assert_eq!(
            s.replace(
                1,
                &[
                    barrier(0, (0, 0, 0, 9)),
                    barrier(2, (10, 0, 10, 9)),
                    barrier(2, (0, 0, 0, 9)),
                    barrier(3, (0, 0, 9, 9)),
                    barrier(4, (5, 0, 5, 9)),
                    barrier(5, (0, 0, 9, 0))
                ]
            )
            .unwrap(),
            vec![0, 2, 3, 4]
        );
        assert_eq!(s.crossing((5., 1.), (5., -20.)), Some(5));
        assert_eq!(s.crossing((9., 1.), (30., 1.)), None);
    }
    #[test]
    fn crossing_uses_the_full_motion_before_clipping_and_chooses_first_edge() {
        let mut s = Space::new(vec![zone(0, 0, 10, 10)]).unwrap();
        s.replace(1, &[barrier(9, (10, 0, 10, 9)), barrier(1, (0, 10, 9, 10))])
            .unwrap();
        assert_eq!(s.crossing((8., 8.), (18., 12.)), Some(9));
        assert_eq!(s.crossing((8., 8.), (12., 18.)), Some(1));
        assert_eq!(s.crossing((8., 8.), (10., 10.)), None); // exclusive tangent ends
        assert_eq!(s.crossing((8., 8.), (9., 9.)), None); // clipped point never crosses
        assert_eq!(s.crossing((8., 8.), (f64::INFINITY, 8.)), None);
    }
    #[test]
    fn topology_invalidates_only_when_geometry_changes_and_wraps() {
        let a = zone(-10, 0, 10, 10);
        let b = zone(10, 0, 10, 10);
        let mut s = Space::new(vec![a, b]).unwrap();
        s.replace(1, &[barrier(1, (-10, 0, -10, 9))]).unwrap();
        assert_eq!(s.refresh(vec![b, a, a]).unwrap(), None);
        assert_eq!(s.replace(9, &[]), Err(Error::StaleZoneSet));
        assert_eq!(s.crossing((-9., 2.), (-20., 2.)), Some(1));
        s.generation = u32::MAX;
        assert_eq!(s.refresh(vec![a]).unwrap(), Some(u32::MAX));
        assert_eq!(s.generation(), 0);
        assert_eq!(s.crossing((-9., 2.), (-20., 2.)), None);
        assert_eq!(s.replace(u32::MAX, &[]), Err(Error::StaleZoneSet));
        assert_eq!(s.refresh(Vec::new()).unwrap(), Some(0));
    }
    #[test]
    fn limits_do_not_mutate_the_previous_valid_configuration() {
        assert!(matches!(
            Space::new(vec![zone(0, 0, 0, 1)]),
            Err(Error::InvalidZones)
        ));
        assert!(matches!(
            Space::new(vec![zone(0, 0, 1, 1); MAX_ZONES + 1]),
            Err(Error::InvalidZones)
        ));
        let mut s = Space::new(vec![zone(i32::MAX - 2, i32::MIN, 2, 2)]).unwrap();
        s.replace(
            1,
            &[barrier(1, (i32::MAX, i32::MIN, i32::MAX, i32::MIN + 1))],
        )
        .unwrap();
        assert_eq!(
            s.replace(1, &vec![barrier(2, (0, 0, 0, 0)); MAX_BARRIERS + 1]),
            Err(Error::TooManyBarriers)
        );
        assert!(s.refresh(vec![zone(0, 0, 1, 0)]).is_err());
        assert_eq!(
            s.crossing(
                (f64::from(i32::MAX) - 1., f64::from(i32::MIN) + 1.5),
                (f64::from(i32::MAX) + 10., f64::from(i32::MIN) + 1.5)
            ),
            Some(1)
        );
        assert_eq!(s.zones().len(), 1);
    }
}
