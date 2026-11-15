//! Core Audio notification callbacks and the guards that keep them
//! registered.
//!
//! Core Audio calls these objects on its own threads, possibly concurrently.
//! They only post a [`Change`] to the COM thread, which never blocks, so they
//! cannot deadlock against the unregister calls made from that thread.

use std::sync::mpsc::Sender;

use ::windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use ::windows::Win32::Foundation::PROPERTYKEY;
use ::windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
};
use ::windows::Win32::Media::Audio::{
    AUDIO_VOLUME_NOTIFICATION_DATA, AudioSessionDisconnectReason, AudioSessionState,
    AudioSessionStateExpired, DEVICE_STATE, EDataFlow, ERole, IAudioSessionControl,
    IAudioSessionEvents, IAudioSessionEvents_Impl, IAudioSessionManager2,
    IAudioSessionNotification, IAudioSessionNotification_Impl, IMMDeviceEnumerator,
    IMMNotificationClient, IMMNotificationClient_Impl, eRender,
};
use ::windows::core::{BOOL, GUID, PCWSTR, Ref, Result, implement};

use super::logic::Change;
use super::worker::Msg;

/// Posts changes to the COM thread.
#[derive(Clone)]
pub(super) struct Poster(Sender<Msg>);

impl Poster {
    pub fn new(tx: Sender<Msg>) -> Self {
        Self(tx)
    }

    fn post(&self, change: Change) {
        // Fails only once the COM thread has exited, when nothing is
        // listening any more.
        let _ = self.0.send(Msg::Changed(change));
    }
}

/// A callback registered with a Core Audio object, unregistered on drop.
/// Holding a reference to the callback keeps it alive for as long as it is
/// registered, which Core Audio requires and does not do itself for every
/// interface.
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

/// Device added, removed, state changed, renamed, or default changed.
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

/// Master volume or mute changed.
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

/// A session was created on the endpoint. The caller must have called
/// `GetSessionEnumerator` on `manager` first, or Core Audio never delivers
/// these, and must keep `manager` alive.
pub(super) fn register_session_created(
    manager: &IAudioSessionManager2,
    poster: Poster,
) -> Result<Registration<IAudioSessionManager2, IAudioSessionNotification>> {
    let callback: IAudioSessionNotification = SessionCreatedNotifier { poster }.into();
    // SAFETY: as for `register_endpoint_notifications`.
    unsafe { manager.RegisterSessionNotification(&callback) }?;
    Ok(Registration {
        source: manager.clone(),
        callback,
        // SAFETY: unregisters the object registered above.
        unregister: |source, callback| unsafe { source.UnregisterSessionNotification(callback) },
    })
}

/// A session's volume, mute, display name or state changed, or it was
/// disconnected.
pub(super) fn register_session_events(
    control: &IAudioSessionControl,
    poster: Poster,
) -> Result<Registration<IAudioSessionControl, IAudioSessionEvents>> {
    let callback: IAudioSessionEvents = SessionNotifier { poster }.into();
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

impl IMMNotificationClient_Impl for DeviceNotifier_Impl {
    fn OnDeviceStateChanged(&self, _device_id: &PCWSTR, _new_state: DEVICE_STATE) -> Result<()> {
        self.poster.post(Change::Devices);
        Ok(())
    }

    fn OnDeviceAdded(&self, _device_id: &PCWSTR) -> Result<()> {
        self.poster.post(Change::Devices);
        Ok(())
    }

    fn OnDeviceRemoved(&self, _device_id: &PCWSTR) -> Result<()> {
        self.poster.post(Change::Devices);
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        _role: ERole,
        _default_device_id: &PCWSTR,
    ) -> Result<()> {
        if flow == eRender {
            self.poster.post(Change::Default);
        }
        Ok(())
    }

    fn OnPropertyValueChanged(&self, _device_id: &PCWSTR, key: &PROPERTYKEY) -> Result<()> {
        // Fires often and for many properties; only the name is shown.
        if key.fmtid == PKEY_Device_FriendlyName.fmtid && key.pid == PKEY_Device_FriendlyName.pid {
            self.poster.post(Change::Devices);
        }
        Ok(())
    }
}

#[implement(IAudioEndpointVolumeCallback)]
struct VolumeNotifier {
    poster: Poster,
}

impl IAudioEndpointVolumeCallback_Impl for VolumeNotifier_Impl {
    fn OnNotify(&self, _data: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> Result<()> {
        self.poster.post(Change::Master);
        Ok(())
    }
}

#[implement(IAudioSessionNotification)]
struct SessionCreatedNotifier {
    poster: Poster,
}

impl IAudioSessionNotification_Impl for SessionCreatedNotifier_Impl {
    fn OnSessionCreated(&self, _new_session: Ref<'_, IAudioSessionControl>) -> Result<()> {
        self.poster.post(Change::SessionList);
        Ok(())
    }
}

#[implement(IAudioSessionEvents)]
struct SessionNotifier {
    poster: Poster,
}

impl IAudioSessionEvents_Impl for SessionNotifier_Impl {
    fn OnDisplayNameChanged(&self, _name: &PCWSTR, _context: *const GUID) -> Result<()> {
        self.poster.post(Change::SessionValues);
        Ok(())
    }

    fn OnIconPathChanged(&self, _path: &PCWSTR, _context: *const GUID) -> Result<()> {
        Ok(())
    }

    fn OnSimpleVolumeChanged(
        &self,
        _volume: f32,
        _mute: BOOL,
        _context: *const GUID,
    ) -> Result<()> {
        self.poster.post(Change::SessionValues);
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
            self.poster.post(Change::SessionList);
        }
        Ok(())
    }

    fn OnSessionDisconnected(&self, _reason: AudioSessionDisconnectReason) -> Result<()> {
        self.poster.post(Change::SessionList);
        Ok(())
    }
}
