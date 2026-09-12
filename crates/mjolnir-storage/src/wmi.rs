//! A very small WMI client, for asking Windows about BitLocker.
//!
//! `Win32_EncryptableVolume` is the only interface Microsoft documents for
//! finding out whether a volume is encrypted and whether it is locked, and it
//! is reachable only through WMI: there is no Win32 C function for it. This
//! module is the smallest thing that can ask that question.
//!
//! It reads instance properties with a single query rather than invoking
//! methods, because the two properties that matter are exposed both ways and
//! the query form avoids the whole `ExecMethod` and in/out-parameter dance for
//! no loss of information.
//!
//! Nothing here needs, requests or handles any key material. Administrator
//! rights are required, which the documentation states for every method on the
//! class.

use mjolnir_core::error::{Error, Result};
use mjolnir_core::exit::ExitCode;
use windows::core::{BSTR, PCWSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoSetProxyBlanket, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::Variant::{
    VariantClear, VARIANT, VT_BSTR, VT_I1, VT_I2, VT_I4, VT_I8, VT_INT, VT_UI1, VT_UI2, VT_UI4,
    VT_UI8, VT_UINT,
};
use windows::Win32::System::Wmi::{
    IWbemClassObject, IWbemLocator, IWbemServices, WbemLocator, WBEM_FLAG_FORWARD_ONLY,
    WBEM_FLAG_RETURN_IMMEDIATELY,
};

/// What Windows reports about one encryptable volume.
///
/// The numbers are the documented enumerations of `Win32_EncryptableVolume`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptableVolume {
    /// The volume, as a GUID path with a trailing backslash.
    pub device_id: String,
    /// 0 unprotected, 1 protected, 2 unknown.
    ///
    /// The documentation notes that 2 "can be caused by the volume being in a
    /// locked state", which is how a locked volume shows up here.
    pub protection_status: u32,
    /// 0 fully decrypted, 1 fully encrypted, 2 encrypting, 3 decrypting,
    /// 4 encryption paused, 5 decryption paused.
    ///
    /// Absent when the query could not read it, which happens for a locked
    /// volume: the underlying call reports `FVE_E_LOCKED_VOLUME`.
    pub conversion_status: Option<u32>,
}

impl EncryptableVolume {
    /// Whether BitLocker is switched on for this volume.
    pub fn is_protected(&self) -> bool {
        self.protection_status == 1
    }

    /// Whether the volume appears locked.
    ///
    /// A protection status of 2 is the documented signal, and a missing
    /// conversion status corroborates it.
    pub fn is_locked(&self) -> bool {
        self.protection_status == 2 || self.conversion_status.is_none()
    }

    /// Whether any encryption is present, in progress or paused.
    pub fn has_encryption(&self) -> bool {
        matches!(self.conversion_status, Some(1..=5)) || self.protection_status == 1
    }

    /// The conversion status in words.
    pub fn describe_conversion(&self) -> &'static str {
        match self.conversion_status {
            Some(0) => "fully decrypted",
            Some(1) => "fully encrypted",
            Some(2) => "encrypting",
            Some(3) => "decrypting",
            Some(4) => "encryption paused",
            Some(5) => "decryption paused",
            Some(_) => "an unrecognised state",
            None => "not readable, which usually means the volume is locked",
        }
    }
}

/// Guards a COM apartment this module entered.
struct Apartment {
    owned: bool,
}

impl Apartment {
    fn enter() -> Self {
        // SAFETY: takes no pointers. S_FALSE means this thread was already in a
        // compatible apartment, and still requires a matching CoUninitialize;
        // an error means it is in an incompatible one, in which case this
        // module must not uninitialise what it did not initialise.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self { owned: hr.is_ok() }
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: balances the CoInitializeEx in enter, on the same thread,
            // and only when that call was the one that entered the apartment.
            // Every interface obtained inside has been dropped by now.
            unsafe { CoUninitialize() };
        }
    }
}

/// Asks Windows about every encryptable volume on this computer.
///
/// Returns an empty list when the BitLocker WMI provider is not present, which
/// is the case on Windows editions without BitLocker and inside Windows PE.
/// That is an answer, not a failure: a machine without the provider has no
/// BitLocker volumes.
pub fn encryptable_volumes() -> Result<Vec<EncryptableVolume>> {
    let _apartment = Apartment::enter();

    // SAFETY: creates the documented WMI locator class in process. No pointer
    // is passed in, and the returned interface is reference counted by the
    // wrapper, which releases it when it drops.
    let locator: IWbemLocator =
        unsafe { CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER) }
            .map_err(|e| wmi_error("the Windows management service could not be reached", e))?;

    let namespace = BSTR::from("ROOT\\CIMV2\\Security\\MicrosoftVolumeEncryption");
    // SAFETY: `locator` is alive for this call. `namespace` is a BSTR local
    // that outlives it, and every other argument is empty, which the
    // documentation defines as "use the current user and the local machine".
    let services: IWbemServices = match unsafe {
        locator.ConnectServer(
            &namespace,
            &BSTR::new(),
            &BSTR::new(),
            &BSTR::new(),
            0,
            &BSTR::new(),
            None,
        )
    } {
        Ok(services) => services,
        Err(_) => {
            // The namespace is absent on editions without BitLocker. A machine
            // that cannot have BitLocker volumes has none.
            return Ok(Vec::new());
        }
    };

    // WMI calls go through a proxy, and the proxy has to be told to impersonate
    // the caller or every query comes back access denied.
    //
    // SAFETY: `services` is alive. Passing None for the principal and the
    // identity means "the defaults for the current user", which is what the
    // documentation prescribes for a local connection.
    unsafe {
        CoSetProxyBlanket(
            &services,
            RPC_C_AUTHN_WINNT,
            RPC_C_AUTHZ_NONE,
            None,
            RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        )
    }
    .map_err(|e| wmi_error("the Windows management service refused the connection", e))?;

    let language = BSTR::from("WQL");
    let query = BSTR::from(
        "SELECT DeviceID, ProtectionStatus, ConversionStatus FROM Win32_EncryptableVolume",
    );

    // SAFETY: `services` is alive and both BSTR locals outlive the call. The
    // two flags are documented constants that ask for a forward only enumerator
    // that does not block.
    let enumerator = unsafe {
        services.ExecQuery(
            &language,
            &query,
            WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
            None,
        )
    }
    .map_err(|e| wmi_error("the list of encryptable volumes could not be read", e))?;

    let mut out = Vec::new();
    loop {
        let mut objects: [Option<IWbemClassObject>; 1] = [None];
        let mut returned = 0u32;

        // SAFETY: `enumerator` is alive. The array is a live local of exactly
        // the length the call is told, and `returned` bounds how much of it was
        // filled. A timeout of -1 means wait indefinitely, which is correct for
        // a local query.
        let hr = unsafe { enumerator.Next(-1, &mut objects, &mut returned) };
        if hr.is_err() || returned == 0 {
            break;
        }
        let Some(object) = objects[0].take() else {
            break;
        };

        let device_id = property_string(&object, "DeviceID");
        let protection_status = property_u32(&object, "ProtectionStatus");
        let conversion_status = property_u32(&object, "ConversionStatus");

        if let (Some(device_id), Some(protection_status)) = (device_id, protection_status) {
            out.push(EncryptableVolume {
                device_id,
                protection_status,
                conversion_status,
            });
        }
    }

    Ok(out)
}

/// Looks up one volume by its GUID path.
///
/// The comparison ignores a trailing backslash, because WMI and the volume
/// enumeration do not always agree about whether one is present.
pub fn encryptable_volume(guid_path: &str) -> Result<Option<EncryptableVolume>> {
    let wanted = guid_path.trim_end_matches('\\').to_ascii_lowercase();
    Ok(encryptable_volumes()?
        .into_iter()
        .find(|v| v.device_id.trim_end_matches('\\').to_ascii_lowercase() == wanted))
}

/// A VARIANT that clears itself.
///
/// `IWbemClassObject::Get` fills a caller owned VARIANT, and a string property
/// arrives as a BSTR the caller is responsible for releasing. `VariantClear` is
/// the documented way to release whatever a variant happens to hold, including
/// nothing.
struct OwnedVariant(VARIANT);

impl OwnedVariant {
    fn empty() -> Self {
        Self(VARIANT::default())
    }

    /// The variant's type tag.
    fn kind(&self) -> u16 {
        // SAFETY: a VARIANT is a union whose first arm is the tagged structure,
        // which is how the type is defined in oaidl.h and how every consumer
        // reads it. The variant is initialised: it was zeroed at construction,
        // which is a valid VT_EMPTY, and is only ever written by WMI.
        unsafe { self.0.Anonymous.Anonymous.vt.0 }
    }

    /// The string inside, when the variant holds one.
    fn as_string(&self) -> Option<String> {
        if self.kind() != VT_BSTR.0 {
            return None;
        }
        // SAFETY: the tag says VT_BSTR, so `bstrVal` is the arm that was
        // written. The BSTR is borrowed rather than taken: `to_string` copies
        // the characters out, and the original is still owned by this variant
        // and released by its Drop.
        let text = unsafe { (*self.0.Anonymous.Anonymous.Anonymous.bstrVal).to_string() };
        Some(text)
    }

    /// The number inside, when the variant holds one.
    ///
    /// WMI providers are inconsistent about which integer arm they use for a
    /// `uint32` property, so every arm that can hold one is accepted, and a
    /// string is parsed. A negative value is rejected rather than wrapped.
    fn as_u32(&self) -> Option<u32> {
        // SAFETY: in each arm the tag has already established which member was
        // written, and every member read here is a plain integer, so the read
        // is of an initialised value of the right type.
        let value = unsafe {
            let raw = &self.0.Anonymous.Anonymous.Anonymous;
            match self.kind() {
                k if k == VT_I1.0 => i64::from(raw.bVal as i8),
                k if k == VT_UI1.0 => i64::from(raw.bVal),
                k if k == VT_I2.0 => i64::from(raw.iVal),
                k if k == VT_UI2.0 => i64::from(raw.uiVal),
                k if k == VT_I4.0 || k == VT_INT.0 => i64::from(raw.lVal),
                k if k == VT_UI4.0 || k == VT_UINT.0 => i64::from(raw.ulVal),
                k if k == VT_I8.0 => raw.llVal,
                k if k == VT_UI8.0 => i64::try_from(raw.ullVal).ok()?,
                _ => return self.as_string()?.trim().parse::<u32>().ok(),
            }
        };
        u32::try_from(value).ok()
    }
}

impl Drop for OwnedVariant {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a live variant owned solely by this value, and
        // VariantClear accepts any initialised variant including VT_EMPTY.
        // Nothing borrowed from it outlives this point, because the accessors
        // copy their results into owned Rust types.
        unsafe {
            let _ = VariantClear(&mut self.0);
        }
    }
}

/// Reads one property from a WMI object.
fn property(object: &IWbemClassObject, name: &str) -> Option<OwnedVariant> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut value = OwnedVariant::empty();

    // SAFETY: `object` is alive for the call, `wide` is a null terminated local
    // that outlives it, and `value` is a live variant the call fills. The two
    // optional out parameters are not wanted and are passed as null, which the
    // documentation permits.
    let hr = unsafe { object.Get(PCWSTR(wide.as_ptr()), 0, &mut value.0, None, None) };
    if hr.is_err() {
        return None;
    }
    Some(value)
}

/// Reads a string property from a WMI object.
fn property_string(object: &IWbemClassObject, name: &str) -> Option<String> {
    let text = property(object, name)?.as_string()?;
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Reads a numeric property from a WMI object.
fn property_u32(object: &IWbemClassObject, name: &str) -> Option<u32> {
    property(object, name)?.as_u32()
}

fn wmi_error(what: &str, e: windows::core::Error) -> Error {
    Error::new(
        ExitCode::Failure,
        what.to_owned(),
        format!("Windows reported: {e}"),
        "MjolnirVSS falls back to reading the partition header directly, which answers the same question",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(protection: u32, conversion: Option<u32>) -> EncryptableVolume {
        EncryptableVolume {
            device_id: "\\\\?\\Volume{test}\\".to_owned(),
            protection_status: protection,
            conversion_status: conversion,
        }
    }

    #[test]
    fn the_documented_enumerations_are_interpreted_correctly() {
        // Protection status 1 is the documented value for "on".
        assert!(volume(1, Some(1)).is_protected());
        assert!(!volume(0, Some(0)).is_protected());

        // 2 is documented as unknown, caused by a locked volume.
        assert!(volume(2, None).is_locked());
        assert!(!volume(1, Some(1)).is_locked());

        // A conversion status that could not be read means locked too, because
        // the underlying call reports FVE_E_LOCKED_VOLUME.
        assert!(volume(1, None).is_locked());
    }

    #[test]
    fn a_volume_part_way_through_encryption_still_counts_as_encrypted() {
        for status in 1..=5 {
            assert!(
                volume(1, Some(status)).has_encryption(),
                "conversion status {status} should count as encrypted"
            );
        }
        assert!(!volume(0, Some(0)).has_encryption());
    }

    #[test]
    fn every_conversion_status_is_described() {
        for status in 0..=5 {
            let v = volume(1, Some(status));
            assert!(!v.describe_conversion().is_empty());
            assert!(!v.describe_conversion().contains("unrecognised"));
        }
        assert!(volume(1, None).describe_conversion().contains("locked"));
        assert!(volume(1, Some(99))
            .describe_conversion()
            .contains("unrecognised"));
    }

    /// The query must be safe to run on any machine, including one with no
    /// BitLocker provider at all, and must never fail merely because there is
    /// nothing to report.
    #[test]
    fn querying_is_safe_to_run_anywhere() {
        let volumes = encryptable_volumes().expect("the query should not fail");
        for v in &volumes {
            assert!(!v.device_id.is_empty());
            assert!(v.protection_status <= 2, "{v:?}");
        }
    }

    #[test]
    fn looking_up_a_volume_that_does_not_exist_is_not_an_error() {
        let found = encryptable_volume("\\\\?\\Volume{00000000-0000-0000-0000-000000000000}\\")
            .expect("the lookup should not fail");
        assert!(found.is_none());
    }
}
