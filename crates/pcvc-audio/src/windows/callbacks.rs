//! Core Audio notification callbacks and the guards that keep them
//! registered.
//!
//! Core Audio calls these objects on its own threads, possibly concurrently.
//! They only post a message to the COM thread, which never blocks, so they
//! cannot deadlock against the unregister calls made from that thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use ::windows::Win32::Devices::FunctionDiscovery::{
    PKEY_Device_DeviceDesc, PKEY_Device_FriendlyName,
};
use ::windows::Win32::Foundation::PROPERTYKEY;
use ::windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
};
use ::windows::Win32::Media::Audio::{
    AUDIO_VOLUME_NOTIFICATION_DATA, AudioSessionDisconnectReason, AudioSessionState,
    AudioSessionStateExpired, DEVICE_STATE, EDataFlow, ERole, IAudioSessionControl,
    IAudioSessionEvents, IAudioSessionEvents_Impl, IAudioSessionManager2,
    IAudioSessionNotification, IAudioSessionNotification_Impl, IMMDeviceEnumerator,
    IMMNotificationClient, IMMNotificationClient_Impl, eMultimedia, eRender,
};
use ::windows::core::{BOOL, GUID, PCWSTR, Ref, Result, implement};

use super::logic::{self, Change};
use super::worker::Msg;

/// Posts messages to the COM thread.
#[derive(Clone)]
pub(super) struct Poster {
    tx: Sender<Msg>,
    /// Event context of this backend's own volume and mute changes, unique
    /// per backend so that another instance's changes are not mistaken for
    /// its own.
    context: GUID,
}

impl Poster {
    pub fn new(tx: Sender<Msg>, context: GUID) -> Self {
        Self { tx, context }
    }

    pub fn context(&self) -> &GUID {
        &self.context
    }

    fn send(&self, msg: Msg) {
        // Fails only once the COM thread has exited, when nothing is
        // listening any more.
        let _ = self.tx.send(msg);
    }

    fn post(&self, change: Change) {
        self.send(Msg::Changed(change));
    }

    /// Whether `context` marks a change this backend made.
    ///
    /// # Safety
    ///
    /// `context` must be null or point to a GUID valid for the call.
    unsafe fn is_own(&self, context: *const GUID) -> bool {
        // SAFETY: per the contract.
        !context.is_null() && unsafe { *context } == self.context
    }
}

/// A session control handed to `OnSessionCreated`, moved to the COM thread.
pub(super) struct MtaSession(IAudioSessionControl);

// SAFETY: the pointer was obtained in the multithreaded apartment (Core Audio
// calls back on MTA threads, and a thread of a process that has an MTA is
// implicitly in it) and is only used on the backend's COM thread, which is
// also in the MTA. COM allows any MTA thread to use an MTA interface pointer.
unsafe impl Send for MtaSession {}

impl MtaSession {
    pub fn into_inner(self) -> IAudioSessionControl {
        self.0
    }
}

/// A callback registered with a Core Audio object, unregistered on drop.
/// Holding a reference to the callback keeps it alive for as long as it is
/// registered, which Core Audio requires and does not do itself for every
/// interface; holding the source keeps the registration's owner alive.
pub(super) struct Registration<S, C> {
    source: S,
    callback: C,
    unregister: fn(&S, &C) -> Result<()>,
}

impl<S, C> Drop for Registration<S, C> {
    fn drop(&mut self) {
        if let Err(e) = (self.unregister)(&self.source, &self.callback) {
            tracing::debug!("unregistering a Core Audio callback failed: {e}");
        }
    }
}

/// Render device added, removed, state changed or renamed, or default
/// multimedia render device changed.
pub(super) fn register_endpoint_notifications(
    enumerator: &IMMDeviceEnumerator,
    poster: Poster,
) -> Result<Registration<IMMDeviceEnumerator, IMMNotificationClient>> {
    let callback: IMMNotificationClient = DeviceNotifier { poster }.into();
    // SAFETY: `callback` is a live object, kept alive by the returned guard
    // until it is unregistered.
    unsafe { enumerator.RegisterEndpointNotificationCallback(&callback) }?;
    Ok(Registration {
        source: enumerator.clone(),
        callback,
        // SAFETY: unregisters the object registered above, from the COM thread
        // and outside any callback.
        unregister: |source, callback| unsafe {
            source.UnregisterEndpointNotificationCallback(callback)
        },
    })
}

/// Master volume or mute changed by someone other than this backend.
pub(super) fn register_volume_notifications(
    volume: &IAudioEndpointVolume,
    poster: Poster,
) -> Result<Registration<IAudioEndpointVolume, IAudioEndpointVolumeCallback>> {
    let callback: IAudioEndpointVolumeCallback = VolumeNotifier { poster }.into();
    // SAFETY: as for `register_endpoint_notifications`.
    unsafe { volume.RegisterControlChangeNotify(&callback) }?;
    Ok(Registration {
        source: volume.clone(),
        callback,
        // SAFETY: unregisters the object registered above.
        unregister: |source, callback| unsafe { source.UnregisterControlChangeNotify(callback) },
    })
}

/// A session was created on the endpoint `endpoint_id`. The caller must have
/// requested a session enumerator from `manager` first, or Core Audio never
/// delivers these.
pub(super) fn register_session_created(
    manager: &IAudioSessionManager2,
    endpoint_id: String,
    poster: Poster,
) -> Result<Registration<IAudioSessionManager2, IAudioSessionNotification>> {
    let callback: IAudioSessionNotification = SessionCreatedNotifier {
        endpoint_id,
        poster,
    }
    .into();
    // SAFETY: as for `register_endpoint_notifications`.
    unsafe { manager.RegisterSessionNotification(&callback) }?;
    Ok(Registration {
        source: manager.clone(),
        callback,
        // SAFETY: unregisters the object registered above.
        unregister: |source, callback| unsafe { source.UnregisterSessionNotification(callback) },
    })
}

/// A session's volume, mute or display name changed by someone other than
/// this backend, or it expired or was disconnected. The latter also set
/// `gone`.
pub(super) fn register_session_events(
    control: &IAudioSessionControl,
    gone: Arc<AtomicBool>,
    poster: Poster,
) -> Result<Registration<IAudioSessionControl, IAudioSessionEvents>> {
    let callback: IAudioSessionEvents = SessionNotifier { gone, poster }.into();
    // SAFETY: as for `register_endpoint_notifications`.
    unsafe { control.RegisterAudioSessionNotification(&callback) }?;
    Ok(Registration {
        source: control.clone(),
        callback,
        // SAFETY: unregisters the object registered above.
        unregister: |source, callback| unsafe {
            source.UnregisterAudioSessionNotification(callback)
        },
    })
}

#[implement(IMMNotificationClient)]
struct DeviceNotifier {
    poster: Poster,
}

impl DeviceNotifier {
    fn post_if_render(&self, device_id: &PCWSTR) {
        let id = if device_id.is_null() {
            None
        } else {
            // SAFETY: Core Audio passes a nul-terminated string valid for the
            // duration of the callback.
            unsafe { device_id.to_string() }.ok()
        };
        if id.is_none_or(|id| logic::may_be_render_endpoint(&id)) {
            self.poster.post(Change::Devices);
        }
    }
}

impl IMMNotificationClient_Impl for DeviceNotifier_Impl {
    fn OnDeviceStateChanged(&self, device_id: &PCWSTR, _new_state: DEVICE_STATE) -> Result<()> {
        self.post_if_render(device_id);
        Ok(())
    }

    fn OnDeviceAdded(&self, device_id: &PCWSTR) -> Result<()> {
        self.post_if_render(device_id);
        Ok(())
    }

    fn OnDeviceRemoved(&self, device_id: &PCWSTR) -> Result<()> {
        self.post_if_render(device_id);
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        _default_device_id: &PCWSTR,
    ) -> Result<()> {
        // The backend follows the multimedia role only.
        if flow == eRender && role == eMultimedia {
            self.poster.post(Change::Default);
        }
        Ok(())
    }

    fn OnPropertyValueChanged(&self, device_id: &PCWSTR, key: &PROPERTYKEY) -> Result<()> {
        // Fires often and for many properties. Renaming an endpoint changes
        // its description, from which the friendly name is derived.
        let is_name = [PKEY_Device_FriendlyName, PKEY_Device_DeviceDesc]
            .iter()
            .any(|k| k.fmtid == key.fmtid && k.pid == key.pid);
        if is_name {
            self.post_if_render(device_id);
        }
        Ok(())
    }
}

#[implement(IAudioEndpointVolumeCallback)]
struct VolumeNotifier {
    poster: Poster,
}

impl IAudioEndpointVolumeCallback_Impl for VolumeNotifier_Impl {
    fn OnNotify(&self, data: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> Result<()> {
        // SAFETY: Core Audio passes null or a structure valid for the duration
        // of the callback; only its first field is read.
        let own =
            !data.is_null() && unsafe { self.poster.is_own(&raw const (*data).guidEventContext) };
        if !own {
            self.poster.post(Change::Master);
        }
        Ok(())
    }
}

#[implement(IAudioSessionNotification)]
struct SessionCreatedNotifier {
    endpoint_id: String,
    poster: Poster,
}

impl IAudioSessionNotification_Impl for SessionCreatedNotifier_Impl {
    fn OnSessionCreated(&self, new_session: Ref<'_, IAudioSessionControl>) -> Result<()> {
        // The session enumerator may not list a session reported here, so it
        // is handed over directly.
        if let Ok(session) = new_session.ok() {
            self.poster.send(Msg::SessionCreated {
                endpoint_id: self.endpoint_id.clone(),
                session: MtaSession(session.clone()),
            });
        }
        Ok(())
    }
}

#[implement(IAudioSessionEvents)]
struct SessionNotifier {
    gone: Arc<AtomicBool>,
    poster: Poster,
}

impl SessionNotifier {
    fn gone(&self) {
        self.gone.store(true, Ordering::Release);
        self.poster.post(Change::SessionList);
    }
}

impl IAudioSessionEvents_Impl for SessionNotifier_Impl {
    fn OnDisplayNameChanged(&self, _name: &PCWSTR, context: *const GUID) -> Result<()> {
        // SAFETY: Core Audio passes null or a GUID valid for the callback.
        if !unsafe { self.poster.is_own(context) } {
            self.poster.post(Change::SessionValues);
        }
        Ok(())
    }

    fn OnIconPathChanged(&self, _path: &PCWSTR, _context: *const GUID) -> Result<()> {
        Ok(())
    }

    fn OnSimpleVolumeChanged(&self, _volume: f32, _mute: BOOL, context: *const GUID) -> Result<()> {
        // SAFETY: Core Audio passes null or a GUID valid for the callback.
        if !unsafe { self.poster.is_own(context) } {
            self.poster.post(Change::SessionValues);
        }
        Ok(())
    }

    fn OnChannelVolumeChanged(
        &self,
        _channel_count: u32,
        _volumes: *const f32,
        _changed_channel: u32,
        _context: *const GUID,
    ) -> Result<()> {
        Ok(())
    }

    fn OnGroupingParamChanged(&self, _param: *const GUID, _context: *const GUID) -> Result<()> {
        Ok(())
    }

    fn OnStateChanged(&self, state: AudioSessionState) -> Result<()> {
        // Active and inactive sessions are both listed.
        if state == AudioSessionStateExpired {
            self.gone();
        }
        Ok(())
    }

    fn OnSessionDisconnected(&self, _reason: AudioSessionDisconnectReason) -> Result<()> {
        self.gone();
        Ok(())
    }
}
