//! A very small WMI client, for the few questions only WMI can answer.
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
    VariantClear, VARIANT, VT_BOOL, VT_BSTR, VT_I1, VT_I2, VT_I4, VT_I8, VT_INT, VT_UI1, VT_UI2,
    VT_UI4, VT_UI8, VT_UINT,
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
    // Held for the whole query: every interface obtained below belongs to
    // this apartment and must be released before it is left.
    let _apartment = Apartment::enter();

    // The namespace is absent on editions without BitLocker and inside Windows
    // PE, which is an answer rather than a failure.
    let Some(services) = connect(BITLOCKER_NAMESPACE)? else {
        return Ok(Vec::new());
    };

    let mut out = Vec::new();
    for object in query(
        &services,
        "SELECT DeviceID, ProtectionStatus, ConversionStatus FROM Win32_EncryptableVolume",
        "the list of encryptable volumes",
    )? {
        let device_id = property_string(&object, "DeviceID");
        let protection_status = property_u32(&object, "ProtectionStatus");
        if let (Some(device_id), Some(protection_status)) = (device_id, protection_status) {
            out.push(EncryptableVolume {
                device_id,
                protection_status,
                conversion_status: property_u32(&object, "ConversionStatus"),
            });
        }
    }
    Ok(out)
}

/// How much room a volume gives its shadow copies.
///
/// From `Win32_ShadowStorage`, which Microsoft documents, and which is the same
/// information `vssadmin list shadowstorage` prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowStorage {
    /// The volume whose shadow copies these figures describe, as a GUID path.
    pub volume: String,
    /// The volume the differential data is kept on, which is usually the same
    /// one.
    pub diff_volume: String,
    /// Bytes currently holding shadow copy data.
    pub used_bytes: u64,
    /// Bytes reserved for shadow copy data.
    pub allocated_bytes: u64,
    /// The ceiling, or `None` when there is none.
    ///
    /// Windows reports an unbounded limit as `u64::MAX`, which is a limit only
    /// in the sense that arithmetic has to stop somewhere.
    pub max_bytes: Option<u64>,
}

impl ShadowStorage {
    /// Room left before the ceiling, when there is one.
    pub fn headroom_bytes(&self) -> Option<u64> {
        self.max_bytes
            .map(|m| m.saturating_sub(self.allocated_bytes))
    }

    /// Whether the ceiling leaves less than `wanted` bytes of room.
    ///
    /// An unbounded limit is never constrained: the disk runs out first, and
    /// that is a different problem with a different message.
    pub fn is_constrained(&self, wanted: u64) -> bool {
        self.headroom_bytes().is_some_and(|room| room < wanted)
    }
}

/// One shadow copy Windows is keeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowCopy {
    /// The shadow copy's identifier.
    pub id: String,
    /// The volume it was taken of, as a GUID path.
    pub volume: String,
    /// Whether it survives a reboot, which is what a restore point is.
    pub persistent: bool,
}

/// Asks Windows how much room each volume gives its shadow copies.
///
/// An empty list means the provider had nothing to say, which happens on a
/// machine where no volume has ever had a shadow copy.
pub fn shadow_storage() -> Result<Vec<ShadowStorage>> {
    // Held for the whole query: every interface obtained below belongs to
    // this apartment and must be released before it is left.
    let _apartment = Apartment::enter();

    // Unlike the BitLocker namespace, this one exists on every Windows, so
    // failing to reach it is a failure rather than an answer. An empty list
    // from a namespace that answered means the machine has no shadow copy
    // storage, which is a real and common state.
    let Some(services) = connect(CIMV2_NAMESPACE)? else {
        return Err(unreachable_namespace(CIMV2_NAMESPACE));
    };

    let mut out = Vec::new();
    for object in query(
        &services,
        "SELECT Volume, DiffVolume, UsedSpace, AllocatedSpace, MaxSpace FROM Win32_ShadowStorage",
        "the shadow copy storage settings",
    )? {
        // Volume and DiffVolume are object references, printed as
        // `Win32_Volume.DeviceID="\\?\Volume{...}\"`. The GUID path is what
        // everything else in MjolnirVSS identifies a volume by.
        let volume = property_string(&object, "Volume")
            .as_deref()
            .and_then(device_id_from_reference);
        let diff_volume = property_string(&object, "DiffVolume")
            .as_deref()
            .and_then(device_id_from_reference);

        let Some(volume) = volume else { continue };
        let max_raw = property_u64(&object, "MaxSpace").unwrap_or(u64::MAX);
        out.push(ShadowStorage {
            diff_volume: diff_volume.unwrap_or_else(|| volume.clone()),
            volume,
            used_bytes: property_u64(&object, "UsedSpace").unwrap_or(0),
            allocated_bytes: property_u64(&object, "AllocatedSpace").unwrap_or(0),
            // Windows spells "no limit" as the largest value it can hold.
            max_bytes: if max_raw == u64::MAX {
                None
            } else {
                Some(max_raw)
            },
        });
    }
    Ok(out)
}

/// Lists the shadow copies Windows is currently keeping.
pub fn shadow_copies() -> Result<Vec<ShadowCopy>> {
    // Held for the whole query: every interface obtained below belongs to
    // this apartment and must be released before it is left.
    let _apartment = Apartment::enter();

    let Some(services) = connect(CIMV2_NAMESPACE)? else {
        return Err(unreachable_namespace(CIMV2_NAMESPACE));
    };

    let mut out = Vec::new();
    for object in query(
        &services,
        "SELECT ID, VolumeName, Persistent FROM Win32_ShadowCopy",
        "the list of shadow copies",
    )? {
        let Some(id) = property_string(&object, "ID") else {
            continue;
        };
        out.push(ShadowCopy {
            id,
            volume: property_string(&object, "VolumeName").unwrap_or_default(),
            persistent: property_bool(&object, "Persistent").unwrap_or(false),
        });
    }
    Ok(out)
}

/// Prefix every volume GUID path starts with.
const VOLUME_PREFIX: &str = r"\\?\Volume{";

/// Pulls the volume path out of a WMI object reference.
///
/// A reference is `Win32_Volume.DeviceID="..."`, and WMI doubles the
/// backslashes inside the quotes when it renders one. Some paths arrive already
/// unescaped, though, so rather than unescaping blindly, the result is required
/// to look like a volume GUID path either way. Blind unescaping would turn a
/// path that was not doubled into `\?\Volume{...}`, which names nothing and
/// would quietly match no volume.
///
/// Anything that does not have that shape is discarded rather than guessed at.
pub fn device_id_from_reference(reference: &str) -> Option<String> {
    let quoted = reference.split_once('"')?.1;
    let inner = quoted.rsplit_once('"')?.0;

    if inner.starts_with(VOLUME_PREFIX) {
        return Some(inner.to_owned());
    }
    let unescaped = inner.replace(r"\\", r"\");
    if unescaped.starts_with(VOLUME_PREFIX) {
        return Some(unescaped);
    }
    None
}

/// The WMI namespace holding `Win32_EncryptableVolume`.
const BITLOCKER_NAMESPACE: &str = "ROOT\\CIMV2\\Security\\MicrosoftVolumeEncryption";
/// The namespace holding almost everything else.
const CIMV2_NAMESPACE: &str = "ROOT\\CIMV2";

/// Connects to a WMI namespace, or reports that it is not there.
///
/// `Ok(None)` means the namespace does not exist on this machine, which is a
/// fact about the machine rather than a failure.
fn connect(namespace: &str) -> Result<Option<IWbemServices>> {
    // SAFETY: creates the documented WMI locator class in process. No pointer
    // is passed in, and the returned interface is reference counted by the
    // wrapper, which releases it when it drops.
    let locator: IWbemLocator =
        unsafe { CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER) }
            .map_err(|e| wmi_error("the Windows management service could not be reached", e))?;

    let namespace = BSTR::from(namespace);
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
        Err(_) => return Ok(None),
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

    Ok(Some(services))
}

/// Runs a WQL query and collects the objects it returns.
fn query(services: &IWbemServices, wql: &str, what: &'static str) -> Result<Vec<IWbemClassObject>> {
    let language = BSTR::from("WQL");
    let text = BSTR::from(wql);

    // SAFETY: `services` is alive and both BSTR locals outlive the call. The
    // two flags are documented constants that ask for a forward only enumerator
    // that does not block.
    let enumerator = unsafe {
        services.ExecQuery(
            &language,
            &text,
            WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
            None,
        )
    }
    .map_err(|e| wmi_error(what, e))?;

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
        match objects[0].take() {
            Some(object) => out.push(object),
            None => break,
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

    /// The wide number inside, when the variant holds one.
    ///
    /// WMI reports a `uint64` as a string, because the scripting types it was
    /// designed around have no 64 bit integer. That is documented behaviour
    /// rather than a quirk, so a string is the expected case here, not a
    /// fallback.
    fn as_u64(&self) -> Option<u64> {
        if let Some(text) = self.as_string() {
            return text.trim().parse::<u64>().ok();
        }
        // SAFETY: the tag establishes which member was written, and every
        // member read here is a plain integer.
        unsafe {
            let raw = &self.0.Anonymous.Anonymous.Anonymous;
            match self.kind() {
                k if k == VT_UI1.0 => Some(u64::from(raw.bVal)),
                k if k == VT_UI2.0 => Some(u64::from(raw.uiVal)),
                k if k == VT_UI4.0 || k == VT_UINT.0 => Some(u64::from(raw.ulVal)),
                k if k == VT_UI8.0 => Some(raw.ullVal),
                k if k == VT_I2.0 => u64::try_from(raw.iVal).ok(),
                k if k == VT_I4.0 || k == VT_INT.0 => u64::try_from(raw.lVal).ok(),
                k if k == VT_I8.0 => u64::try_from(raw.llVal).ok(),
                _ => None,
            }
        }
    }

    /// The boolean inside, when the variant holds one.
    fn as_bool(&self) -> Option<bool> {
        if self.kind() != VT_BOOL.0 {
            return self.as_u64().map(|v| v != 0);
        }
        // SAFETY: the tag says VT_BOOL, so `boolVal` is the member that was
        // written. A VARIANT_BOOL is a plain 16 bit integer where zero is false.
        Some(unsafe { self.0.Anonymous.Anonymous.Anonymous.boolVal.0 != 0 })
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

/// Reads a wide numeric property from a WMI object.
fn property_u64(object: &IWbemClassObject, name: &str) -> Option<u64> {
    property(object, name)?.as_u64()
}

/// Reads a boolean property from a WMI object.
fn property_bool(object: &IWbemClassObject, name: &str) -> Option<bool> {
    property(object, name)?.as_bool()
}

/// A namespace that should exist and did not.
fn unreachable_namespace(namespace: &str) -> Error {
    Error::new(
        ExitCode::Failure,
        "the Windows management service could not be reached",
        format!("the {namespace} namespace did not answer"),
        "this is usually a temporary failure of the management service; the operation carries on without what it would have said",
    )
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

    /// The volume path this machine actually reports, used by both forms of
    /// the reference test.
    const VOLUME: &str = "\\\\?\\Volume{12c957bd-eeab-4a25-a25a-21f437bf5179}\\";

    /// WMI doubles the backslashes when it renders an object reference.
    #[test]
    fn a_quoted_reference_yields_its_volume_path() {
        let reference = format!("Win32_Volume.DeviceID=\"{}\"", VOLUME.replace('\\', "\\\\"));
        assert_eq!(
            device_id_from_reference(&reference).as_deref(),
            Some(VOLUME)
        );
    }

    /// Some paths arrive already unescaped. Unescaping one of those again would
    /// produce `\?\Volume{...}`, which names nothing, so the shape is checked
    /// rather than assumed.
    #[test]
    fn a_reference_that_is_not_doubled_is_left_alone() {
        let reference = format!("Win32_Volume.DeviceID=\"{VOLUME}\"");
        assert_eq!(
            device_id_from_reference(&reference).as_deref(),
            Some(VOLUME)
        );
    }

    #[test]
    fn something_that_is_not_a_volume_reference_yields_nothing() {
        assert_eq!(device_id_from_reference("Win32_Volume"), None);
        assert_eq!(device_id_from_reference(""), None);
        assert_eq!(
            device_id_from_reference(r#"Win32_Volume.DeviceID="""#),
            None
        );
        assert_eq!(
            device_id_from_reference(r#"Win32_Directory.Name="C:\\Windows""#),
            None
        );
    }

    /// An unbounded limit is not a small number and must never be treated as
    /// one, because that would make every machine look constrained.
    #[test]
    fn an_unbounded_limit_is_never_constrained() {
        let storage = ShadowStorage {
            volume: "v".to_owned(),
            diff_volume: "v".to_owned(),
            used_bytes: 1 << 30,
            allocated_bytes: 2 << 30,
            max_bytes: None,
        };
        assert_eq!(storage.headroom_bytes(), None);
        assert!(!storage.is_constrained(u64::MAX));
    }

    #[test]
    fn a_bounded_limit_reports_its_headroom() {
        let storage = ShadowStorage {
            volume: "v".to_owned(),
            diff_volume: "v".to_owned(),
            used_bytes: 0,
            allocated_bytes: 3 << 30,
            max_bytes: Some(4 << 30),
        };
        assert_eq!(storage.headroom_bytes(), Some(1 << 30));
        assert!(storage.is_constrained(2 << 30));
        assert!(!storage.is_constrained(1 << 29));

        // Already over the limit leaves no room rather than a negative amount.
        let over = ShadowStorage {
            allocated_bytes: 8 << 30,
            ..storage
        };
        assert_eq!(over.headroom_bytes(), Some(0));
        assert!(over.is_constrained(1));
    }

    /// Both queries must be safe to run on any machine, and must never fail
    /// merely because there is nothing to report.
    #[test]
    fn the_shadow_copy_queries_are_safe_to_run_anywhere() {
        let storage = shadow_storage().expect("the query should not fail");
        for s in &storage {
            assert!(!s.volume.is_empty());
            assert!(!s.diff_volume.is_empty());
        }
        let copies = shadow_copies().expect("the query should not fail");
        for c in &copies {
            assert!(!c.id.is_empty());
        }
    }

    #[test]
    fn looking_up_a_volume_that_does_not_exist_is_not_an_error() {
        let found = encryptable_volume("\\\\?\\Volume{00000000-0000-0000-0000-000000000000}\\")
            .expect("the lookup should not fail");
        assert!(found.is_none());
    }
}
