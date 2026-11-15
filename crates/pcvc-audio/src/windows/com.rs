//! Safe wrappers over the Core Audio and Win32 calls the backend makes.
//!
//! Every function here must run on the backend's COM thread, after
//! [`Apartment::init`]. The interface types are `!Send`, which keeps them on
//! that thread. The `unsafe` in this module is the `windows` crate marking
//! every COM method and Win32 function as unsafe; each call's pointer
//! arguments are covered by its SAFETY comment.

use std::ffi::c_void;
use std::iter;
use std::marker::PhantomData;
use std::ptr;

use ::windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use ::windows::Win32::Foundation::ERROR_NOT_FOUND;
use ::windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use ::windows::Win32::Media::Audio::{
    AudioSessionStateExpired, DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2,
    IMMDevice, IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, eMultimedia, eRender,
};
use ::windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PropVariantClear, PropVariantToStringAlloc,
};
use ::windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};
use ::windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use ::windows::Win32::UI::Shell::SHLoadIndirectString;
use ::windows::core::{Interface, Owned, PCWSTR, PWSTR, Result};

use super::logic;

/// Membership of the calling thread in the multithreaded apartment, left on
/// drop. Every COM object the thread holds must be released first.
pub(super) struct Apartment {
    _not_send: PhantomData<*const ()>,
}

impl Apartment {
    pub fn init() -> Result<Self> {
        // SAFETY: no reserved pointer. S_FALSE (already initialised) is a
        // success that still needs a balancing CoUninitialize, so both make an
        // Apartment; RPC_E_CHANGED_MODE is an error and makes none.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        Ok(Self {
            _not_send: PhantomData,
        })
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful CoInitializeEx in `init`, on the same
        // thread since `Apartment` is `!Send`.
        unsafe { CoUninitialize() }
    }
}

/// Copies a string allocated by COM's task allocator and frees it.
///
/// # Safety
///
/// `p` must be null or a nul-terminated string the caller owns that was
/// allocated with `CoTaskMemAlloc`, and must not be used afterwards.
unsafe fn take_co_string(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: per the contract, `p` is a valid nul-terminated string.
    let s = String::from_utf16_lossy(unsafe { p.as_wide() });
    // SAFETY: per the contract, the caller owns `p` and it came from
    // CoTaskMemAlloc; it is not touched again.
    unsafe { CoTaskMemFree(Some(p.0 as *const c_void)) };
    s
}

/// A UTF-16, nul-terminated copy of `s` for passing as a `PCWSTR`.
pub(super) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(iter::once(0)).collect()
}

pub(super) fn create_enumerator() -> Result<IMMDeviceEnumerator> {
    // SAFETY: valid CLSID pointer and no aggregation.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

/// Active render endpoints.
pub(super) fn active_render_endpoints(enumerator: &IMMDeviceEnumerator) -> Result<Vec<IMMDevice>> {
    // SAFETY: no pointer arguments.
    let collection = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }?;
    // SAFETY: no pointer arguments.
    let count = unsafe { collection.GetCount() }?;
    // SAFETY: indexes are below the reported count.
    (0..count).map(|i| unsafe { collection.Item(i) }).collect()
}

/// The default multimedia render endpoint, or `None` if there is no render
/// endpoint at all.
pub(super) fn default_render_endpoint(
    enumerator: &IMMDeviceEnumerator,
) -> Result<Option<IMMDevice>> {
    // SAFETY: no pointer arguments.
    match unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia) } {
        Ok(device) => Ok(Some(device)),
        Err(e) if e.code() == ERROR_NOT_FOUND.to_hresult() => Ok(None),
        Err(e) => Err(e),
    }
}

/// The endpoint id string, `{0.0.0.00000000}.{GUID}`.
pub(super) fn endpoint_id(device: &IMMDevice) -> Result<String> {
    // SAFETY: no pointer arguments. GetId hands ownership of a CoTaskMemAlloc
    // string to the caller, which `take_co_string` frees.
    unsafe { device.GetId().map(|p| take_co_string(p)) }
}

/// Clears a `PROPVARIANT` on drop.
struct PropVariant(PROPVARIANT);

impl Drop for PropVariant {
    fn drop(&mut self) {
        // SAFETY: the value was initialised by IPropertyStore::GetValue and is
        // cleared exactly once.
        let _ = unsafe { PropVariantClear(&mut self.0) };
    }
}

/// `PKEY_Device_FriendlyName`, such as "Speakers (Realtek High Definition
/// Audio)", or `None` if the property is missing or blank.
pub(super) fn friendly_name(device: &IMMDevice) -> Result<Option<String>> {
    // SAFETY: no pointer arguments.
    let store = unsafe { device.OpenPropertyStore(STGM_READ) }?;
    // SAFETY: the key points to a static PROPERTYKEY. The returned value is
    // owned by `PropVariant`, which clears it.
    let value = PropVariant(unsafe { store.GetValue(&PKEY_Device_FriendlyName) }?);
    // SAFETY: `value.0` is an initialised PROPVARIANT that outlives the call.
    // The result is a CoTaskMemAlloc string owned by the caller.
    let text = unsafe { PropVariantToStringAlloc(&value.0) }.map(|p| unsafe { take_co_string(p) });
    match text {
        Ok(text) => Ok(logic::non_blank(&text)),
        // VT_EMPTY: the property is not set.
        Err(_) => Ok(None),
    }
}

/// Activates an interface on an endpoint, such as `IAudioEndpointVolume` or
/// `IAudioSessionManager2`.
pub(super) fn activate<T: Interface>(device: &IMMDevice) -> Result<T> {
    // SAFETY: no activation parameters.
    unsafe { device.Activate(CLSCTX_ALL, None) }
}

/// Master volume as a percentage, and mute.
pub(super) fn master(volume: &IAudioEndpointVolume) -> Result<(f32, bool)> {
    // SAFETY: no pointer arguments.
    let scalar = unsafe { volume.GetMasterVolumeLevelScalar() }?;
    // SAFETY: no pointer arguments.
    let muted = unsafe { volume.GetMute() }?;
    Ok((logic::scalar_to_percent(scalar), muted.as_bool()))
}

/// Sets master mute and/or volume (a percentage), mute first as the C# host
/// did.
pub(super) fn set_master(
    volume: &IAudioEndpointVolume,
    percent: Option<f32>,
    muted: Option<bool>,
) -> Result<()> {
    if let Some(muted) = muted {
        // SAFETY: a null event context is allowed.
        unsafe { volume.SetMute(muted, ptr::null()) }?;
    }
    if let Some(percent) = percent {
        // SAFETY: a null event context is allowed.
        unsafe {
            volume.SetMasterVolumeLevelScalar(logic::percent_to_scalar(percent), ptr::null())
        }?;
    }
    Ok(())
}

/// Every session control on the endpoint, including expired and system-sound
/// sessions.
pub(super) fn session_controls(
    manager: &IAudioSessionManager2,
) -> Result<Vec<IAudioSessionControl2>> {
    // SAFETY: no pointer arguments.
    let sessions = unsafe { manager.GetSessionEnumerator() }?;
    // SAFETY: no pointer arguments.
    let count = unsafe { sessions.GetCount() }?;
    let mut controls = Vec::new();
    for i in 0..count {
        // SAFETY: the index is below the reported count.
        match unsafe { sessions.GetSession(i) }.and_then(|c| c.cast::<IAudioSessionControl2>()) {
            Ok(control) => controls.push(control),
            Err(e) => tracing::debug!("skipping audio session {i}: {e}"),
        }
    }
    Ok(controls)
}

/// Whether a session should be listed: not the system-sounds session and not
/// expired.
pub(super) fn is_listed(control: &IAudioSessionControl2) -> Result<bool> {
    // SAFETY: no pointer arguments.
    let system = unsafe { control.IsSystemSoundsSession() };
    system.ok()?;
    let system = logic::is_s_ok(system.0);
    // SAFETY: no pointer arguments.
    let state = unsafe { control.GetState() }?;
    Ok(!system && state != AudioSessionStateExpired)
}

/// The session instance identifier, which is the protocol session id.
pub(super) fn session_instance_id(control: &IAudioSessionControl2) -> Result<String> {
    // SAFETY: no pointer arguments. The result is a CoTaskMemAlloc string
    // owned by the caller.
    unsafe {
        control
            .GetSessionInstanceIdentifier()
            .map(|p| take_co_string(p))
    }
}

pub(super) fn display_name(control: &IAudioSessionControl2) -> Result<String> {
    // SAFETY: no pointer arguments. The result is a CoTaskMemAlloc string
    // owned by the caller.
    unsafe { control.GetDisplayName().map(|p| take_co_string(p)) }
}

pub(super) fn process_id(control: &IAudioSessionControl2) -> Result<u32> {
    // SAFETY: no pointer arguments.
    unsafe { control.GetProcessId() }
}

/// Session volume as a percentage, and mute.
pub(super) fn session_volume(volume: &ISimpleAudioVolume) -> Result<(f32, bool)> {
    // SAFETY: no pointer arguments.
    let scalar = unsafe { volume.GetMasterVolume() }?;
    // SAFETY: no pointer arguments.
    let muted = unsafe { volume.GetMute() }?;
    Ok((logic::scalar_to_percent(scalar), muted.as_bool()))
}

/// Sets a session's volume (a percentage) and mute.
pub(super) fn set_session_volume(
    volume: &ISimpleAudioVolume,
    percent: f32,
    muted: bool,
) -> Result<()> {
    // SAFETY: a null event context is allowed.
    unsafe { volume.SetMasterVolume(logic::percent_to_scalar(percent), ptr::null()) }?;
    // SAFETY: a null event context is allowed.
    unsafe { volume.SetMute(muted, ptr::null()) }
}

/// Resolves an indirect string such as `@%SystemRoot%\System32\x.dll,-202`.
pub(super) fn load_indirect_string(source: &str) -> Option<String> {
    let source = wide(source);
    let mut buf = vec![0u16; 1024];
    // SAFETY: `source` is nul-terminated and outlives the call; the output
    // slice is writable for its whole length.
    unsafe { SHLoadIndirectString(PCWSTR(source.as_ptr()), &mut buf, None) }.ok()?;
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// The image file stem of a process (`chrome` for `...\chrome.exe`).
pub(super) fn process_image_stem(pid: u32) -> Option<String> {
    // SAFETY: plain Win32 call with no pointer arguments.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    // SAFETY: OpenProcess returned a fresh handle that nothing else closes.
    let handle = unsafe { Owned::new(handle) };
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` is writable for `len` u16s; on success `len` holds the
    // number of characters written, excluding the terminator.
    unsafe {
        QueryFullProcessImageNameW(
            *handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .ok()?;
    let path = String::from_utf16_lossy(buf.get(..len as usize)?);
    logic::process_stem(&path)
}
