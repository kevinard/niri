use std::collections::HashMap;
use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::zvariant::{DeserializeDict, OwnedObjectPath, Type, Value};
use zbus::{fdo, interface, ObjectServer};

use super::{request_name, Start};
use crate::screencasting::sources::{self, Message as SourceMessage};
use crate::utils::{CastSessionId, CastStreamId};

#[derive(Clone)]
pub struct ScreenCast {
    to_sources: calloop::channel::SyncSender<SourceMessage>,
    to_niri: calloop::channel::Sender<ScreenCastToNiri>,
    #[allow(clippy::type_complexity)]
    sessions: Arc<Mutex<Vec<(Session, InterfaceRef<Session>)>>>,
}

#[derive(Clone)]
pub struct Session {
    id: CastSessionId,
    to_sources: calloop::channel::SyncSender<SourceMessage>,
    to_niri: calloop::channel::Sender<ScreenCastToNiri>,
    #[allow(clippy::type_complexity)]
    streams: Arc<Mutex<Vec<(Stream, InterfaceRef<Stream>)>>>,
    stopped: Arc<AtomicBool>,
}

#[derive(Debug, Default, Deserialize, Type, Clone, Copy, PartialEq, Eq)]
pub enum CursorMode {
    #[default]
    Hidden = 0,
    Embedded = 1,
    Metadata = 2,
}

#[derive(Debug, DeserializeDict, Type)]
#[zvariant(signature = "dict")]
struct RecordMonitorProperties {
    #[zvariant(rename = "cursor-mode")]
    cursor_mode: Option<CursorMode>,
    #[zvariant(rename = "is-recording")]
    _is_recording: Option<bool>,
    #[zvariant(rename = "canopa-source-id")]
    source_id: Option<u64>,
}

#[derive(Debug, DeserializeDict, Type)]
#[zvariant(signature = "dict")]
struct RecordWindowProperties {
    #[zvariant(rename = "window-id")]
    window_id: u64,
    #[zvariant(rename = "cursor-mode")]
    cursor_mode: Option<CursorMode>,
    #[zvariant(rename = "is-recording")]
    _is_recording: Option<bool>,
    #[zvariant(rename = "canopa-source-id")]
    source_id: Option<u64>,
}

#[derive(Clone)]
pub struct Stream {
    id: CastStreamId,
    session_id: CastSessionId,
    to_sources: calloop::channel::SyncSender<SourceMessage>,
    cursor_mode: CursorMode,
    was_started: Arc<AtomicBool>,
    to_niri: calloop::channel::Sender<ScreenCastToNiri>,
}

pub enum ScreenCastToNiri {
    StartCast {
        session_id: CastSessionId,
        stream_id: CastStreamId,
        cursor_mode: CursorMode,
        signal_ctx: SignalEmitter<'static>,
    },
    StopCast {
        session_id: CastSessionId,
    },
}

#[interface(name = "org.gnome.Mutter.ScreenCast")]
impl ScreenCast {
    async fn create_session(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        properties: HashMap<&str, Value<'_>>,
    ) -> fdo::Result<OwnedObjectPath> {
        if properties.contains_key("remote-desktop-session-id") {
            return Err(fdo::Error::Failed(
                "there are no remote desktop sessions".to_owned(),
            ));
        }

        let session_id = CastSessionId::next();
        let path = format!("/org/gnome/Mutter/ScreenCast/Session/u{}", session_id.get());
        let path = OwnedObjectPath::try_from(path).unwrap();

        let session = Session::new(session_id, self.to_sources.clone(), self.to_niri.clone());
        match server.at(&path, session.clone()).await {
            Ok(true) => {
                let iface = server.interface(&path).await.unwrap();
                self.sessions.lock().unwrap().push((session, iface));
            }
            Ok(false) => return Err(fdo::Error::Failed("session path already exists".to_owned())),
            Err(err) => {
                return Err(fdo::Error::Failed(format!(
                    "error creating session object: {err:?}"
                )))
            }
        }

        Ok(path)
    }

    /// Stable live monitor identity for Canopa's source selection. Callers can
    /// require this identity in RecordMonitor's canopa-source-id property.
    async fn monitor_info(&self, connector: &str) -> fdo::Result<(u64, (i32, i32), (i32, i32))> {
        let (reply, receive) = async_channel::bounded(1);
        self.to_sources
            .try_send(SourceMessage::MonitorInfo {
                name: connector.into(),
                reply,
            })
            .map_err(|_| fdo::Error::LimitsExceeded("capture query queue unavailable".into()))?;
        let metadata = receive
            .recv()
            .await
            .map_err(|_| fdo::Error::Failed("compositor stopped".into()))??;
        Ok((
            metadata.source_id.unwrap(),
            metadata.position,
            metadata.size,
        ))
    }

    #[zbus(property)]
    fn canopa_source_version(&self) -> u32 {
        1
    }

    #[zbus(property)]
    async fn version(&self) -> i32 {
        4
    }
}

#[interface(name = "org.gnome.Mutter.ScreenCast.Session")]
impl Session {
    async fn start(&self) {
        debug!("start");

        for (stream, iface) in &*self.streams.lock().unwrap() {
            stream.start(iface.signal_emitter().clone());
        }
    }

    pub async fn stop(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(signal_context)] ctxt: SignalEmitter<'_>,
    ) {
        debug!("stop");

        if self.stopped.swap(true, Ordering::SeqCst) {
            // Already stopped.
            return;
        }

        Session::closed(&ctxt).await.unwrap();

        if let Err(err) = self.to_niri.send(ScreenCastToNiri::StopCast {
            session_id: self.id,
        }) {
            warn!("error sending StopCast to niri: {err:?}");
        }

        let streams = mem::take(&mut *self.streams.lock().unwrap());
        for (_, iface) in streams.iter() {
            server
                .remove::<Stream, _>(iface.signal_emitter().path())
                .await
                .unwrap();
        }

        server.remove::<Session, _>(ctxt.path()).await.unwrap();
    }

    async fn record_monitor(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(connection)] connection: &zbus::Connection,
        connector: &str,
        properties: RecordMonitorProperties,
    ) -> fdo::Result<OwnedObjectPath> {
        self.record(
            server,
            connection,
            sources::Request::Monitor(connector.into()),
            properties.source_id,
            properties.cursor_mode.unwrap_or_default(),
        )
        .await
    }

    async fn record_window(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(connection)] connection: &zbus::Connection,
        properties: RecordWindowProperties,
    ) -> fdo::Result<OwnedObjectPath> {
        self.record(
            server,
            connection,
            sources::Request::Window(properties.window_id),
            properties.source_id,
            properties.cursor_mode.unwrap_or_default(),
        )
        .await
    }

    #[zbus(signal)]
    async fn closed(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;
}

#[interface(name = "org.gnome.Mutter.ScreenCast.Stream")]
impl Stream {
    #[zbus(signal)]
    pub async fn pipe_wire_stream_added(ctxt: &SignalEmitter<'_>, node_id: u32)
        -> zbus::Result<()>;

    /// This is live on-demand metadata; it is not a cached output projection.
    #[zbus(property(emits_changed_signal = "false"))]
    async fn parameters(&self) -> fdo::Result<HashMap<&'static str, Value<'static>>> {
        let (reply, receive) = async_channel::bounded(1);
        self.to_sources
            .try_send(SourceMessage::Metadata {
                session: self.session_id,
                stream: self.id,
                reply,
            })
            .map_err(|_| fdo::Error::LimitsExceeded("capture query queue unavailable".into()))?;
        let metadata = receive
            .recv()
            .await
            .map_err(|_| fdo::Error::Failed("compositor stopped".into()))??;
        let mut values = HashMap::from([
            ("position", Value::from(metadata.position)),
            ("size", Value::from(metadata.size)),
        ]);
        if let Some(id) = metadata.source_id {
            values.insert("canopa-source-id", Value::from(id));
        }
        if let Some(id) = metadata.mapping_id {
            values.insert("mapping-id", Value::from(id));
        }
        if let Some(scale) = metadata.physical_scale {
            values.insert("canopa-physical-scale", Value::from(scale));
        }
        Ok(values)
    }
}

impl ScreenCast {
    pub fn new(
        to_sources: calloop::channel::SyncSender<SourceMessage>,
        to_niri: calloop::channel::Sender<ScreenCastToNiri>,
    ) -> Self {
        Self {
            to_sources,
            to_niri,
            sessions: Arc::new(Mutex::new(vec![])),
        }
    }
}

impl Start for ScreenCast {
    fn start(self, monitor: bool) -> anyhow::Result<zbus::blocking::Connection> {
        let conn = zbus::blocking::Connection::session()?;
        conn.object_server()
            .at("/org/gnome/Mutter/ScreenCast", self)?;
        request_name(&conn, "org.gnome.Mutter.ScreenCast", monitor)?;

        Ok(conn)
    }
}

impl Session {
    async fn record(
        &self,
        server: &ObjectServer,
        connection: &zbus::Connection,
        target: sources::Request,
        expected: Option<u64>,
        cursor_mode: CursorMode,
    ) -> fdo::Result<OwnedObjectPath> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(fdo::Error::Failed("capture session stopped".into()));
        }
        let stream_id = CastStreamId::next();
        let path = OwnedObjectPath::try_from(format!(
            "/org/gnome/Mutter/ScreenCast/Stream/u{}",
            stream_id.get()
        ))
        .unwrap();
        let owner = connection
            .unique_name()
            .ok_or_else(|| fdo::Error::Failed("capture is not on the session bus".into()))?;
        let mapping_id = format!("canopa-niri:{}:u{}", owner, stream_id.get());
        let (reply, receive) = async_channel::bounded(1);
        self.to_sources
            .try_send(SourceMessage::Pin {
                session: self.id,
                stream: stream_id,
                target,
                expected,
                mapping_id,
                stopped: self.stopped.clone(),
                reply,
            })
            .map_err(|_| fdo::Error::LimitsExceeded("capture query queue unavailable".into()))?;
        receive
            .recv()
            .await
            .map_err(|_| fdo::Error::Failed("compositor stopped".into()))??;
        let stream = Stream::new(
            stream_id,
            self.id,
            self.to_sources.clone(),
            cursor_mode,
            self.to_niri.clone(),
        );
        match server.at(&path, stream.clone()).await {
            Ok(true) => {
                let iface = server
                    .interface(&path)
                    .await
                    .map_err(|e| fdo::Error::Failed(e.to_string()))?;
                // Do not publish a new stream after concurrent Session::stop
                // drained its list. The read-only D-Bus method avoids holding an
                // exclusive interface lock while waiting on the compositor.
                let stopped = {
                    let mut streams = self.streams.lock().unwrap();
                    if self.stopped.load(Ordering::Acquire) {
                        true
                    } else {
                        streams.push((stream, iface));
                        false
                    }
                };
                if stopped {
                    let _ = server.remove::<Stream, _>(&path).await;
                    return Err(fdo::Error::Failed("capture session stopped".into()));
                }
                Ok(path)
            }
            Ok(false) => Err(fdo::Error::Failed("stream path already exists".into())),
            Err(error) => Err(fdo::Error::Failed(format!(
                "error creating stream object: {error:?}"
            ))),
        }
    }

    pub fn new(
        id: CastSessionId,
        to_sources: calloop::channel::SyncSender<SourceMessage>,
        to_niri: calloop::channel::Sender<ScreenCastToNiri>,
    ) -> Self {
        Self {
            id,
            to_sources,
            streams: Arc::new(Mutex::new(vec![])),
            to_niri,
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.to_niri.send(ScreenCastToNiri::StopCast {
            session_id: self.id,
        });
    }
}

impl Stream {
    fn new(
        id: CastStreamId,
        session_id: CastSessionId,
        to_sources: calloop::channel::SyncSender<SourceMessage>,
        cursor_mode: CursorMode,
        to_niri: calloop::channel::Sender<ScreenCastToNiri>,
    ) -> Self {
        Self {
            id,
            session_id,
            to_sources,
            cursor_mode,
            was_started: Arc::new(AtomicBool::new(false)),
            to_niri,
        }
    }

    fn start(&self, ctxt: SignalEmitter<'static>) {
        if self.was_started.swap(true, Ordering::SeqCst) {
            return;
        }

        let msg = ScreenCastToNiri::StartCast {
            session_id: self.session_id,
            stream_id: self.id,
            cursor_mode: self.cursor_mode,
            signal_ctx: ctxt,
        };

        if let Err(err) = self.to_niri.send(msg) {
            warn!("error sending StartCast to niri: {err:?}");
        }
    }
}
