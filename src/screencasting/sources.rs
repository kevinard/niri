//! Capture selection identities and live metadata, owned by the compositor loop.
//! IPC display names and cached output projections are not object identities.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use smithay::output::Output;
use zbus::fdo;

use crate::niri::{CastTarget, Niri, State};
use crate::utils::{CastSessionId, CastStreamId};

mod identity;
pub mod mapping;
const MAX_PINS: usize = 64;

pub enum Request {
    Monitor(String),
    Window(u64),
}

pub enum Message {
    Pin {
        session: CastSessionId,
        stream: CastStreamId,
        target: Request,
        expected: Option<u64>,
        mapping_id: String,
        stopped: Arc<AtomicBool>,
        reply: async_channel::Sender<fdo::Result<()>>,
    },
    Metadata {
        session: CastSessionId,
        stream: CastStreamId,
        reply: async_channel::Sender<fdo::Result<Metadata>>,
    },
    MonitorInfo {
        name: String,
        reply: async_channel::Sender<fdo::Result<Metadata>>,
    },
}

#[derive(Debug, Clone)]
pub struct Metadata {
    pub source_id: Option<u64>,
    pub position: (i32, i32),
    pub size: (i32, i32),
    pub mapping_id: Option<String>,
    pub physical_scale: Option<f64>,
}

#[derive(Clone)]
pub enum Target {
    Fixed { target: CastTarget, source_id: u64 },
    Dynamic,
}
struct Pin {
    session: CastSessionId,
    target: Target,
    mapping_id: String,
    stopped: Arc<AtomicBool>,
}
#[derive(Default)]
pub struct Sources(HashMap<CastStreamId, Pin>);
impl Sources {
    pub fn target(&self, session: CastSessionId, stream: CastStreamId) -> Option<Target> {
        self.0
            .get(&stream)
            .filter(|pin| pin.session == session && !pin.stopped.load(Ordering::Acquire))
            .map(|pin| pin.target.clone())
    }
    pub fn stop(&mut self, session: CastSessionId) {
        self.0.retain(|_, pin| pin.session != session);
    }
    fn metadata(
        &self,
        niri: &Niri,
        session: CastSessionId,
        stream: CastStreamId,
    ) -> fdo::Result<Metadata> {
        let pin = self
            .0
            .get(&stream)
            .filter(|pin| pin.session == session && !pin.stopped.load(Ordering::Acquire))
            .ok_or_else(unavailable)?;
        match &pin.target {
            Target::Fixed { target, source_id } => {
                let mut metadata = target_metadata(niri, target)?;
                metadata.source_id = Some(*source_id);
                metadata.mapping_id = Some(pin.mapping_id.clone());
                Ok(metadata)
            }
            // Keep the existing Mutter dynamic-picker placeholder before it has
            // selected a target. It is never an approved absolute-input mapping.
            Target::Dynamic => Ok(Metadata {
                source_id: None,
                position: (0, 0),
                size: (1, 1),
                mapping_id: None,
                physical_scale: None,
            }),
        }
    }
}

fn unavailable() -> fdo::Error {
    fdo::Error::Failed("capture source is unavailable".into())
}

/// Lives on the actual Output object. A new Output gets a different identity,
/// even when its connector name or backend CRTC identifier was reused.
fn output_identity(output: &Output) -> fdo::Result<u64> {
    identity::identity(output)
        .ok_or_else(|| fdo::Error::LimitsExceeded("capture identities exhausted".into()))
}
fn target_metadata(niri: &Niri, target: &CastTarget) -> fdo::Result<Metadata> {
    let (position, size, physical_scale) = match target {
        CastTarget::Output { output, .. } => {
            let output = output
                .upgrade()
                .filter(|output| niri.output_exists(output))
                .ok_or_else(unavailable)?;
            let geo = niri
                .global_space
                .output_geometry(&output)
                .ok_or_else(unavailable)?;
            (
                (geo.loc.x, geo.loc.y),
                (geo.size.w, geo.size.h),
                Some(output.current_scale().fractional_scale()),
            )
        }
        CastTarget::Window { id } => {
            let (_, mapped) = niri
                .layout
                .windows()
                .find(|(_, mapped)| mapped.id().get() == *id)
                .ok_or_else(unavailable)?;
            let bbox = mapped.window.bbox_with_popups();
            // The cast output determines the pixels in this recording, even
            // when logical window dimensions stay fixed across output scaling.
            let scale = niri
                .casting
                .mapped_cast_output
                .get(&mapped.window)
                .map(|output| output.current_scale().fractional_scale());
            ((0, 0), (bbox.size.w, bbox.size.h), scale)
        }
        CastTarget::Nothing => return Err(unavailable()),
    };
    if !(1..=32768).contains(&size.0) || !(1..=32768).contains(&size.1) {
        return Err(unavailable());
    }
    Ok(Metadata {
        source_id: None,
        position,
        size,
        mapping_id: None,
        physical_scale,
    })
}
fn resolve(niri: &Niri, request: Request, expected: Option<u64>) -> fdo::Result<Target> {
    let (target, id) = match request {
        Request::Monitor(name) => {
            if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
                return Err(fdo::Error::InvalidArgs("invalid connector name".into()));
            }
            let output = niri
                .global_space
                .outputs()
                .find(|output| output.name() == name)
                .ok_or_else(unavailable)?;
            (CastTarget::output(output), output_identity(output)?)
        }
        Request::Window(id) if id == niri.casting.dynamic_cast_id_for_portal.get() => {
            if expected.is_some() {
                return Err(fdo::Error::InvalidArgs(
                    "dynamic source has no fixed identity".into(),
                ));
            }
            return Ok(Target::Dynamic);
        }
        Request::Window(id) => {
            let target = CastTarget::Window { id };
            target_metadata(niri, &target)?;
            (target, id)
        }
    };
    if expected.is_some_and(|value| value != id) {
        return Err(fdo::Error::Failed("capture source identity changed".into()));
    }
    target_metadata(niri, &target)?;
    Ok(Target::Fixed {
        target,
        source_id: id,
    })
}

pub fn handle(state: &mut State, message: Message) {
    match message {
        Message::Pin {
            session,
            stream,
            target,
            expected,
            mapping_id,
            stopped,
            reply,
        } => {
            let result = if stopped.load(Ordering::Acquire) {
                Err(unavailable())
            } else if state.niri.casting.sources.0.len() >= MAX_PINS {
                Err(fdo::Error::LimitsExceeded(
                    "too many capture source pins".into(),
                ))
            } else if state.niri.casting.sources.0.contains_key(&stream) {
                Err(fdo::Error::Failed(
                    "capture stream identity already exists".into(),
                ))
            } else {
                resolve(&state.niri, target, expected).map(|target| {
                    state.niri.casting.sources.0.insert(
                        stream,
                        Pin {
                            session,
                            target,
                            mapping_id,
                            stopped,
                        },
                    );
                })
            };
            let inserted = result.is_ok();
            if reply.try_send(result).is_err() && inserted {
                state.niri.casting.sources.0.remove(&stream);
            }
        }
        Message::Metadata {
            session,
            stream,
            reply,
        } => {
            let _ = reply.try_send(state.niri.casting.sources.metadata(
                &state.niri,
                session,
                stream,
            ));
        }
        Message::MonitorInfo { name, reply } => {
            let result =
                resolve(&state.niri, Request::Monitor(name), None).and_then(
                    |target| match target {
                        Target::Fixed { target, source_id } => {
                            let mut metadata = target_metadata(&state.niri, &target)?;
                            metadata.source_id = Some(source_id);
                            Ok(metadata)
                        }
                        Target::Dynamic => unreachable!(),
                    },
                );
            let _ = reply.try_send(result);
        }
    }
}
