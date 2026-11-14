//! `org.canopa.Niri.WindowPreview1`: window thumbnails for a desktop shell.
//!
//! The service renders one window's current contents as a small PNG, for example for taskbar
//! hover previews. It is served on the session bus at `/org/canopa/Niri/WindowPreview1` under the
//! well-known name `org.canopa.Niri.WindowPreview1`.
//!
//! Like niri's other session-bus services (`org.gnome.Shell.Screenshot`,
//! `org.gnome.Mutter.ScreenCast`), it trusts every client on the user's session bus: any process
//! of the same user can already take screenshots and screencasts through those. It can be turned
//! off with `window-previews { off; }` in the config.
//!
//! Captures are bounded: each caller (unique bus name) may have one capture in flight, and at
//! most [`MAX_IN_FLIGHT`] captures run at a time. Further calls fail with
//! `org.freedesktop.DBus.Error.LimitsExceeded`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use zbus::message::Header;
use zbus::{fdo, interface};

use super::{request_name, Start};
use crate::window_preview::{clamp_max_size, CaptureError, PreviewPng};

/// Interface version, exposed as the `Version` property.
pub const VERSION: u32 = 1;

/// Maximum number of captures in flight across all callers.
pub const MAX_IN_FLIGHT: usize = 4;

pub struct WindowPreview {
    to_niri: calloop::channel::Sender<WindowPreviewToNiri>,
    in_flight: Arc<Mutex<InFlight>>,
}

pub enum WindowPreviewToNiri {
    Capture {
        window_id: u64,
        /// Already clamped to the supported range.
        max_size: u32,
        reply: async_channel::Sender<Result<PreviewPng, CaptureError>>,
    },
}

impl From<CaptureError> for fdo::Error {
    fn from(err: CaptureError) -> Self {
        match err {
            CaptureError::Disabled => {
                fdo::Error::AccessDenied(String::from("window previews are turned off"))
            }
            CaptureError::UnknownWindow(id) => {
                fdo::Error::InvalidArgs(format!("no mapped window with id {id}"))
            }
            CaptureError::Failed(msg) => fdo::Error::Failed(msg),
        }
    }
}

/// Why a capture could not start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    /// The caller already has a capture in flight.
    Caller,
    /// Too many captures are in flight overall.
    Global,
}

/// Tracks the callers with a capture in flight.
#[derive(Debug, Default)]
pub struct InFlight {
    callers: HashSet<String>,
}

impl InFlight {
    /// Records a capture for `caller`, unless that would exceed the limits.
    pub fn try_begin(&mut self, caller: &str) -> Result<(), Busy> {
        if self.callers.contains(caller) {
            return Err(Busy::Caller);
        }
        if self.callers.len() >= MAX_IN_FLIGHT {
            return Err(Busy::Global);
        }
        self.callers.insert(caller.to_owned());
        Ok(())
    }

    /// Records that the capture for `caller` finished.
    pub fn end(&mut self, caller: &str) {
        self.callers.remove(caller);
    }

    pub fn len(&self) -> usize {
        self.callers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.callers.is_empty()
    }
}

/// Ends the caller's capture when dropped, including when the method call is cancelled.
struct InFlightGuard {
    in_flight: Arc<Mutex<InFlight>>,
    caller: String,
}

impl InFlightGuard {
    fn new(in_flight: &Arc<Mutex<InFlight>>, caller: String) -> Result<Self, Busy> {
        in_flight.lock().unwrap().try_begin(&caller)?;
        Ok(Self {
            in_flight: in_flight.clone(),
            caller,
        })
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.in_flight.lock().unwrap().end(&self.caller);
    }
}

#[interface(name = "org.canopa.Niri.WindowPreview1")]
impl WindowPreview {
    /// Renders the window with the given id, scaled down to fit a `max_size`×`max_size` square.
    ///
    /// `window_id` is the niri IPC window id. `max_size` is clamped to 16..=512. The window is
    /// never upscaled. Returns the PNG and its size.
    #[zbus(out_args("width", "height", "png"))]
    async fn capture(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        window_id: u64,
        max_size: u32,
    ) -> fdo::Result<(u32, u32, Vec<u8>)> {
        // Peer-to-peer connections have no sender; they all share one slot.
        let caller = hdr.sender().map(|s| s.to_string()).unwrap_or_default();
        let _guard = match InFlightGuard::new(&self.in_flight, caller) {
            Ok(guard) => guard,
            Err(Busy::Caller) => {
                return Err(fdo::Error::LimitsExceeded(String::from(
                    "a capture from this caller is already in progress",
                )));
            }
            Err(Busy::Global) => {
                return Err(fdo::Error::LimitsExceeded(String::from(
                    "too many captures in progress",
                )));
            }
        };

        let (tx, rx) = async_channel::bounded(1);
        let msg = WindowPreviewToNiri::Capture {
            window_id,
            max_size: clamp_max_size(max_size),
            reply: tx,
        };
        if let Err(err) = self.to_niri.send(msg) {
            warn!("error sending message to niri: {err:?}");
            return Err(fdo::Error::Failed(String::from("internal error")));
        }

        match rx.recv().await {
            Ok(Ok(preview)) => Ok((preview.width, preview.height, preview.png)),
            Ok(Err(err)) => Err(err.into()),
            Err(err) => {
                warn!("error receiving message from niri: {err:?}");
                Err(fdo::Error::Failed(String::from("internal error")))
            }
        }
    }

    #[zbus(property)]
    async fn version(&self) -> u32 {
        VERSION
    }
}

impl WindowPreview {
    pub fn new(to_niri: calloop::channel::Sender<WindowPreviewToNiri>) -> Self {
        Self {
            to_niri,
            in_flight: Arc::new(Mutex::new(InFlight::default())),
        }
    }
}

impl Start for WindowPreview {
    fn start(self, monitor: bool) -> anyhow::Result<zbus::blocking::Connection> {
        let conn = zbus::blocking::Connection::session()?;
        conn.object_server()
            .at("/org/canopa/Niri/WindowPreview1", self)?;
        request_name(&conn, "org.canopa.Niri.WindowPreview1", monitor)?;

        Ok(conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_capture_per_caller() {
        let mut in_flight = InFlight::default();
        assert_eq!(in_flight.try_begin(":1.1"), Ok(()));
        assert_eq!(in_flight.try_begin(":1.1"), Err(Busy::Caller));
        assert_eq!(in_flight.try_begin(":1.2"), Ok(()));
        in_flight.end(":1.1");
        assert_eq!(in_flight.try_begin(":1.1"), Ok(()));
        assert_eq!(in_flight.len(), 2);
    }

    #[test]
    fn global_limit() {
        let mut in_flight = InFlight::default();
        for i in 0..MAX_IN_FLIGHT {
            assert_eq!(in_flight.try_begin(&format!(":1.{i}")), Ok(()));
        }
        assert_eq!(in_flight.try_begin(":1.99"), Err(Busy::Global));
        // A caller that is already in flight is reported as such first.
        assert_eq!(in_flight.try_begin(":1.0"), Err(Busy::Caller));

        in_flight.end(":1.2");
        assert_eq!(in_flight.try_begin(":1.99"), Ok(()));
        assert_eq!(in_flight.len(), MAX_IN_FLIGHT);
    }

    #[test]
    fn guard_releases_on_drop() {
        let in_flight = Arc::new(Mutex::new(InFlight::default()));
        let guard = InFlightGuard::new(&in_flight, String::from(":1.1")).unwrap();
        assert!(matches!(
            InFlightGuard::new(&in_flight, String::from(":1.1")),
            Err(Busy::Caller)
        ));
        drop(guard);
        assert!(in_flight.lock().unwrap().is_empty());
        let _guard = InFlightGuard::new(&in_flight, String::from(":1.1")).unwrap();
    }

    #[test]
    fn errors_map_to_dbus_names() {
        use zbus::DBusError as _;

        let name = |err: CaptureError| fdo::Error::from(err).name().to_string();
        assert_eq!(
            name(CaptureError::UnknownWindow(7)),
            "org.freedesktop.DBus.Error.InvalidArgs"
        );
        assert_eq!(
            name(CaptureError::Disabled),
            "org.freedesktop.DBus.Error.AccessDenied"
        );
        assert_eq!(
            name(CaptureError::Failed(String::new())),
            "org.freedesktop.DBus.Error.Failed"
        );
    }
}
