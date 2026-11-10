//! Absolute input bindings to selected, live producer streams. Region dimensions and scale
//! remain fixed for one lease, matching EI's immutable device region contract.

use std::collections::HashSet;

use smithay::utils::{Logical, Point};
use zbus::fdo;

use super::{unavailable, Metadata, Target};
use crate::niri::{CastTarget, Niri};
use crate::utils::{CastSessionId, CastStreamId};

mod path;

struct Region {
    stream: CastStreamId,
    metadata: Metadata,
}
pub struct Bindings {
    session: CastSessionId,
    regions: Vec<Region>,
}
impl Bindings {
    pub fn new(niri: &Niri, session: &str, streams: &[String]) -> fdo::Result<Self> {
        if streams.is_empty() || streams.len() > 16 {
            return Err(fdo::Error::InvalidArgs(
                "expected 1 to 16 capture streams".into(),
            ));
        }
        let session = path::id(session, "Session").ok_or_else(unavailable)?;
        let session = CastSessionId::from(session);
        let mut seen = HashSet::new();
        let mut regions = Vec::with_capacity(streams.len());
        for name in streams {
            let raw = path::id(name, "Stream").ok_or_else(unavailable)?;
            if !seen.insert(raw) {
                return Err(fdo::Error::InvalidArgs("duplicate capture stream".into()));
            }
            let stream = *niri
                .casting
                .sources
                .0
                .keys()
                .find(|s| s.get() == raw)
                .ok_or_else(unavailable)?;
            let metadata = active_metadata(niri, session, stream)?;
            regions.push(Region { stream, metadata });
        }
        Ok(Self { session, regions })
    }

    pub fn descriptors(&self) -> Vec<(u64, String, (i32, i32), f64)> {
        self.regions
            .iter()
            .map(|r| {
                (
                    r.stream.get(),
                    r.metadata.mapping_id.clone().unwrap(),
                    r.metadata.size,
                    r.metadata.physical_scale.expect("active scaled binding"),
                )
            })
            .collect()
    }

    pub fn valid(&self, niri: &Niri) -> bool {
        self.regions.iter().all(|r| {
            active_metadata(niri, self.session, r.stream).is_ok_and(|m| {
                m.source_id == r.metadata.source_id
                    && m.mapping_id == r.metadata.mapping_id
                    && m.size == r.metadata.size
                    && m.physical_scale == r.metadata.physical_scale
            })
        })
    }

    pub fn point(&self, niri: &Niri, stream: u64, x: f64, y: f64) -> Option<Point<f64, Logical>> {
        if !self.valid(niri) {
            return None;
        }
        let region = self.regions.iter().find(|r| r.stream.get() == stream)?;
        let (width, height) = region.metadata.size;
        if !x.is_finite()
            || !y.is_finite()
            || x < 0.
            || y < 0.
            || x >= f64::from(width)
            || y >= f64::from(height)
        {
            return None;
        }
        let Target::Fixed { target, .. } =
            niri.casting.sources.target(self.session, region.stream)?
        else {
            return None;
        };
        match target {
            CastTarget::Output { output, .. } => {
                let output = output.upgrade().filter(|o| niri.output_exists(o))?;
                let geo = niri.global_space.output_geometry(&output)?;
                // These are already the captured output's logical coordinates,
                // including its transform. Do not apply hardware transforms again.
                Some(geo.loc.to_f64() + Point::from((x, y)))
            }
            CastTarget::Window { id } => {
                let (_, mapped) = niri.layout.windows().find(|(_, m)| m.id().get() == id)?;
                let (output, surface_origin) = niri.layout.window_surface_origin(&mapped.window)?;
                let geo = niri.global_space.output_geometry(output)?;
                let cast_output = niri.casting.mapped_cast_output.get(&mapped.window)?;
                let scale = cast_output.current_scale().fractional_scale();
                // Match render_for_screen_cast's physically rounded popup bbox,
                // then convert the stream's logical region into those video pixels.
                let bbox = mapped
                    .window
                    .bbox_with_popups()
                    .to_physical_precise_up::<_, i32>(scale);
                let within_surface = bbox.loc.to_f64().to_logical(scale)
                    + Point::from((
                        x * f64::from(bbox.size.w) / (scale * f64::from(width)),
                        y * f64::from(bbox.size.h) / (scale * f64::from(height)),
                    ));
                let point = geo.loc.to_f64() + surface_origin + within_surface;
                // Offscreen, obscured, overview and input-region holes cannot
                // silently redirect a window stream's absolute point elsewhere.
                let under = niri.contents_under(point);
                if under.surface.is_none()
                    || !under
                        .window
                        .is_some_and(|(window, _)| window == mapped.window)
                {
                    return None;
                }
                Some(point)
            }
            CastTarget::Nothing => None,
        }
    }
}

fn active_metadata(
    niri: &Niri,
    session: CastSessionId,
    stream: CastStreamId,
) -> fdo::Result<Metadata> {
    let Some(Target::Fixed { target, .. }) = niri.casting.sources.target(session, stream) else {
        return Err(unavailable());
    };
    // A selected pin or a recycled PipeWire node number is insufficient. This
    // exact session/stream must still own a producer and its selected target.
    if !niri.casting.casts.iter().any(|cast| {
        cast.session_id == session
            && cast.stream_id == stream
            && cast.target == target
            && cast.node_id().is_some()
    }) {
        return Err(unavailable());
    }
    let metadata = niri.casting.sources.metadata(niri, session, stream)?;
    if metadata.mapping_id.is_none()
        || metadata.source_id.is_none()
        || !metadata
            .physical_scale
            .is_some_and(|scale| scale.is_finite() && scale > 0. && scale <= 64.)
    {
        return Err(unavailable());
    }
    Ok(metadata)
}
