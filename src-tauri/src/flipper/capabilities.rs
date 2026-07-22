use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::{FlipperError, Result};
use crate::flipper::transport::TransportKind;

const MAX_HANDSHAKE_VALUE_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    pub state: CapabilityState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Capability {
    fn supported() -> Self {
        Self {
            state: CapabilityState::Supported,
            reason: None,
        }
    }

    fn unsupported(reason: impl Into<String>) -> Self {
        Self {
            state: CapabilityState::Unsupported,
            reason: Some(reason.into()),
        }
    }

    fn unknown(reason: impl Into<String>) -> Self {
        Self {
            state: CapabilityState::Unknown,
            reason: Some(reason.into()),
        }
    }
}

/// Capability facts obtained from transport constraints and the successful
/// system.device_info handshake. No field depends on firmware display names or
/// version-string comparisons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCapabilities {
    pub rpc: Capability,
    pub storage: Capability,
    pub screen_stream: Capability,
    pub cli: Capability,
    pub firmware_update: Capability,
    pub gpio: Capability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedDeviceHandshake {
    pub hardware_name: Option<String>,
    pub hardware_version: Option<String>,
    pub hardware_uid: String,
    pub firmware_version: String,
    pub firmware_build_date: Option<String>,
    pub capabilities: DeviceCapabilities,
}

/// Normalize the raw system.device_info map into the connection contract.
///
/// A successful RPC response with no stable device identity or firmware
/// identity is not a usable connection: publishing it would poison per-device
/// caches and make later feature decisions non-deterministic.
pub fn normalize_device_handshake(
    info: &HashMap<String, String>,
    transport: TransportKind,
) -> Result<NormalizedDeviceHandshake> {
    let hardware_uid = required(info, &["hardware_uid"], "hardware_uid")?;
    let firmware_version = required(
        info,
        &["software_version", "firmware_version"],
        "software_version",
    )?;
    let hardware_name = optional(info, &["hardware_name", "hardware_model"])?;
    let hardware_version = optional(info, &["hardware_ver"])?;
    let firmware_build_date = optional(info, &["software_build_date", "firmware_build_date"])?;

    let serial_only_reason = "This feature requires a USB serial connection";
    let cli = match transport {
        TransportKind::Serial => Capability::supported(),
        TransportKind::Ble => Capability::unsupported(serial_only_reason),
    };
    let firmware_update = match transport {
        TransportKind::Serial => Capability::supported(),
        TransportKind::Ble => Capability::unsupported(serial_only_reason),
    };

    Ok(NormalizedDeviceHandshake {
        hardware_name,
        hardware_version,
        hardware_uid,
        firmware_version,
        firmware_build_date,
        capabilities: DeviceCapabilities {
            // Reaching this parser proves the RPC device-info exchange worked.
            rpc: Capability::supported(),
            storage: advertised_capability(info, "capability_storage")
                .unwrap_or_else(Capability::supported),
            screen_stream: advertised_capability(info, "capability_screen_stream")
                .unwrap_or_else(Capability::supported),
            cli,
            firmware_update,
            // GPIO availability is not part of the baseline device-info
            // contract, so absence must remain unknown instead of being
            // guessed from a firmware name/version.
            gpio: advertised_capability(info, "capability_gpio").unwrap_or_else(|| {
                Capability::unknown("The device handshake did not advertise GPIO support")
            }),
        },
    })
}

fn required(info: &HashMap<String, String>, aliases: &[&str], canonical: &str) -> Result<String> {
    let Some((key, value)) = aliases
        .iter()
        .find_map(|key| info.get(*key).map(|value| (*key, value)))
    else {
        return Err(FlipperError::InvalidHandshake {
            key: canonical.into(),
            reason: "required key is missing".into(),
        });
    };
    validate_value(key, value).map(str::to_owned)
}

fn optional(info: &HashMap<String, String>, aliases: &[&str]) -> Result<Option<String>> {
    let Some((key, value)) = aliases
        .iter()
        .find_map(|key| info.get(*key).map(|value| (*key, value)))
    else {
        return Ok(None);
    };
    validate_value(key, value).map(|value| Some(value.to_owned()))
}

fn validate_value<'a>(key: &str, value: &'a str) -> Result<&'a str> {
    let value = value.trim();
    let invalid_reason = if value.is_empty() {
        Some("value is empty")
    } else if value.len() > MAX_HANDSHAKE_VALUE_BYTES {
        Some("value is too long")
    } else if value.chars().any(char::is_control) {
        Some("value contains control characters")
    } else {
        None
    };
    match invalid_reason {
        Some(reason) => Err(FlipperError::InvalidHandshake {
            key: key.into(),
            reason: reason.into(),
        }),
        None => Ok(value),
    }
}

fn advertised_capability(info: &HashMap<String, String>, key: &str) -> Option<Capability> {
    let value = info.get(key)?.trim().to_ascii_lowercase();
    Some(match value.as_str() {
        "1" | "true" | "supported" => Capability::supported(),
        "0" | "false" | "unsupported" => {
            Capability::unsupported("The device reported this feature as unsupported")
        }
        "unknown" => Capability::unknown("The device reported an unknown capability state"),
        _ => Capability::unknown("The device reported an unrecognized capability value"),
    })
}

#[cfg(test)]
mod tests {
    use super::{normalize_device_handshake, CapabilityState};
    use crate::error::FlipperError;
    use crate::flipper::transport::TransportKind;
    use std::collections::HashMap;

    fn complete_fixture() -> HashMap<String, String> {
        HashMap::from([
            ("hardware_name".into(), "Flipper Zero".into()),
            ("hardware_ver".into(), "12".into()),
            ("hardware_uid".into(), "A1B2C3D4E5F6".into()),
            ("software_version".into(), "1.4.2".into()),
            ("software_build_date".into(), "2026-07-01".into()),
            ("capability_gpio".into(), "supported".into()),
        ])
    }

    #[test]
    fn complete_usb_fixture_normalizes_required_identity_and_capabilities() {
        let parsed = normalize_device_handshake(&complete_fixture(), TransportKind::Serial)
            .expect("complete USB fixture");
        assert_eq!(parsed.hardware_uid, "A1B2C3D4E5F6");
        assert_eq!(parsed.firmware_version, "1.4.2");
        assert_eq!(parsed.capabilities.rpc.state, CapabilityState::Supported);
        assert_eq!(parsed.capabilities.cli.state, CapabilityState::Supported);
        assert_eq!(
            parsed.capabilities.firmware_update.state,
            CapabilityState::Supported
        );
        assert_eq!(parsed.capabilities.gpio.state, CapabilityState::Supported);
    }

    #[test]
    fn complete_ble_fixture_preserves_rpc_but_rejects_serial_only_features() {
        let parsed = normalize_device_handshake(&complete_fixture(), TransportKind::Ble)
            .expect("complete BLE fixture");
        assert_eq!(parsed.capabilities.rpc.state, CapabilityState::Supported);
        assert_eq!(parsed.capabilities.cli.state, CapabilityState::Unsupported);
        assert_eq!(
            parsed.capabilities.firmware_update.state,
            CapabilityState::Unsupported
        );
        assert!(parsed.capabilities.cli.reason.unwrap().contains("USB"));
    }

    #[test]
    fn every_missing_required_identity_key_fails_the_handshake() {
        for required_key in ["hardware_uid", "software_version"] {
            let mut fixture = complete_fixture();
            fixture.remove(required_key);
            assert!(matches!(
                normalize_device_handshake(&fixture, TransportKind::Serial),
                Err(FlipperError::InvalidHandshake { key, .. }) if key == required_key
            ));
        }
    }

    #[test]
    fn malformed_known_key_fails_without_echoing_unsafe_data() {
        let mut fixture = complete_fixture();
        fixture.insert("software_version".into(), "bad\0version".into());
        let error = normalize_device_handshake(&fixture, TransportKind::Serial).unwrap_err();
        let public = error.command_error();
        assert_eq!(public.code, "invalid_handshake");
        assert_eq!(public.details.unwrap()["key"], "software_version");
        assert!(!public.message.contains("bad\0version"));
    }

    #[test]
    fn missing_malformed_and_unknown_capability_keys_are_deterministic() {
        let mut missing = complete_fixture();
        missing.remove("capability_gpio");
        assert_eq!(
            normalize_device_handshake(&missing, TransportKind::Serial)
                .unwrap()
                .capabilities
                .gpio
                .state,
            CapabilityState::Unknown
        );
        let baseline = normalize_device_handshake(&missing, TransportKind::Serial).unwrap();
        assert_eq!(
            baseline.capabilities.storage.state,
            CapabilityState::Supported
        );
        assert_eq!(
            baseline.capabilities.screen_stream.state,
            CapabilityState::Supported
        );

        let mut malformed = complete_fixture();
        malformed.insert("capability_gpio".into(), "maybe-v2".into());
        assert_eq!(
            normalize_device_handshake(&malformed, TransportKind::Serial)
                .unwrap()
                .capabilities
                .gpio
                .state,
            CapabilityState::Unknown
        );
        assert_eq!(
            normalize_device_handshake(&malformed, TransportKind::Serial)
                .unwrap()
                .capabilities
                .gpio
                .reason
                .as_deref(),
            Some("The device reported an unrecognized capability value")
        );

        let mut explicitly_unknown = complete_fixture();
        explicitly_unknown.insert("capability_gpio".into(), "unknown".into());
        let explicitly_unknown =
            normalize_device_handshake(&explicitly_unknown, TransportKind::Serial).unwrap();
        assert_eq!(
            explicitly_unknown.capabilities.gpio.state,
            CapabilityState::Unknown
        );
        assert_eq!(
            explicitly_unknown.capabilities.gpio.reason.as_deref(),
            Some("The device reported an unknown capability state")
        );

        let mut unknown = complete_fixture();
        unknown.insert("capability_quantum_radio".into(), "supported".into());
        assert_eq!(
            normalize_device_handshake(&unknown, TransportKind::Serial)
                .unwrap()
                .capabilities
                .gpio
                .state,
            CapabilityState::Supported
        );
    }

    #[test]
    fn firmware_aliases_are_normalized_without_version_heuristics() {
        let mut fixture = complete_fixture();
        fixture.remove("software_version");
        fixture.insert("firmware_version".into(), "custom-channel".into());
        let parsed = normalize_device_handshake(&fixture, TransportKind::Serial).unwrap();
        assert_eq!(parsed.firmware_version, "custom-channel");
    }
}
