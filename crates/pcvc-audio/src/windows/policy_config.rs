//! `IPolicyConfig`, the undocumented interface behind the Sound control
//! panel's "Set as default device".
//!
//! Only `SetDefaultEndpoint` is called, but every method is declared so that
//! it lands in the right vtable slot (the 11th after `IUnknown`). The IID,
//! CLSID and method order match these independent declarations:
//!
//! - AudioDeviceCmdlets, `SOURCE/IPolicyConfig.cs`
//!   (<https://github.com/frgnca/AudioDeviceCmdlets>)
//! - AudioEndPointController, `EndPointController/PolicyConfig.h`
//!   (<https://github.com/DanStevens/AudioEndPointController>)
//! - com-policy-config 0.6.0, `src/lib.rs`
//!   (<https://github.com/sidit77/com-policy-config>)
//!
//! Pointer parameters of the unused methods are typed `*mut c_void`; only the
//! slot count matters for them.

#![allow(non_snake_case)]

use std::ffi::c_void;

use ::windows::Win32::Media::Audio::{ERole, eCommunications, eConsole, eMultimedia};
use ::windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use ::windows::core::{BOOL, GUID, HRESULT, IUnknown, IUnknown_Vtbl, PCWSTR, Result, interface};

use super::com::wide;

const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

#[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
unsafe trait IPolicyConfig: IUnknown {
    fn GetMixFormat(&self, device_id: PCWSTR, format: *mut *mut c_void) -> HRESULT;
    fn GetDeviceFormat(
        &self,
        device_id: PCWSTR,
        default: BOOL,
        format: *mut *mut c_void,
    ) -> HRESULT;
    fn ResetDeviceFormat(&self, device_id: PCWSTR) -> HRESULT;
    fn SetDeviceFormat(
        &self,
        device_id: PCWSTR,
        endpoint_format: *mut c_void,
        mix_format: *mut c_void,
    ) -> HRESULT;
    fn GetProcessingPeriod(
        &self,
        device_id: PCWSTR,
        default: BOOL,
        default_period: *mut i64,
        minimum_period: *mut i64,
    ) -> HRESULT;
    fn SetProcessingPeriod(&self, device_id: PCWSTR, period: *mut i64) -> HRESULT;
    fn GetShareMode(&self, device_id: PCWSTR, mode: *mut c_void) -> HRESULT;
    fn SetShareMode(&self, device_id: PCWSTR, mode: *mut c_void) -> HRESULT;
    fn GetPropertyValue(
        &self,
        device_id: PCWSTR,
        fx_store: BOOL,
        key: *const c_void,
        value: *mut c_void,
    ) -> HRESULT;
    fn SetPropertyValue(
        &self,
        device_id: PCWSTR,
        fx_store: BOOL,
        key: *const c_void,
        value: *mut c_void,
    ) -> HRESULT;
    fn SetDefaultEndpoint(&self, device_id: PCWSTR, role: ERole) -> HRESULT;
    fn SetEndpointVisibility(&self, device_id: PCWSTR, visible: BOOL) -> HRESULT;
}

/// Makes `endpoint_id` (an `IMMDevice` id) the default endpoint for the
/// console, multimedia and communications roles. Must run on a COM thread.
pub(super) fn set_default_endpoint(endpoint_id: &str) -> Result<()> {
    // SAFETY: valid CLSID pointer and no aggregation.
    let policy: IPolicyConfig =
        unsafe { CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL) }?;
    let id = wide(endpoint_id);
    for role in [eConsole, eMultimedia, eCommunications] {
        // SAFETY: `id` is nul-terminated and outlives the call, and the slot
        // matches the published declarations listed above.
        unsafe { policy.SetDefaultEndpoint(PCWSTR(id.as_ptr()), role) }.ok()?;
    }
    Ok(())
}
