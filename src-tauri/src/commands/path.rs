use std::fmt;
use std::ops::Deref;

use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{FlipperError, Result};

/// Roots accepted by the Flipper firmware's storage namespace.
const VALID_ROOTS: [&str; 3] = ["ext", "int", "any"];

/// A validated, canonical path in the Flipper storage namespace.
///
/// Construction is intentionally private: command arguments deserialize
/// through [`TryFrom<String>`], and device-provided child names extend an
/// existing path only through [`DevicePath::join_child`]. Once this type has
/// been constructed, callers can safely pass [`DevicePath::as_str`] to the
/// existing protocol helpers without repeating string validation.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct DevicePath(String);

impl DevicePath {
    /// Return the canonical firmware path.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the validated path while retaining its canonical spelling.
    pub fn into_string(self) -> String {
        self.0
    }

    /// Whether this path names a storage root rather than an entry below it.
    pub fn is_root(&self) -> bool {
        matches!(self.0.as_str(), "/ext" | "/int" | "/any")
    }

    /// Join one plain child name returned by the device.
    ///
    /// Device directory listings must return a basename, never a path. This
    /// rejects separators and dot segments before constructing the new path,
    /// so a malformed response cannot escape its validated parent.
    pub fn join_child(&self, child: &str) -> Result<Self> {
        validate_child_name(child)?;
        Ok(Self(format!("{}/{child}", self.0)))
    }
}

impl TryFrom<String> for DevicePath {
    type Error = FlipperError;

    fn try_from(path: String) -> Result<Self> {
        if path.contains('\\') {
            return Err(invalid_path(path, "backslash separators are not allowed"));
        }
        if path.chars().any(char::is_control) {
            return Err(invalid_path(path, "control characters are not allowed"));
        }

        let Some(body) = path.strip_prefix('/') else {
            return Err(invalid_path(path, "path must be absolute"));
        };
        let mut components = body.split('/');
        let root = components.next().unwrap_or_default();
        if !VALID_ROOTS.contains(&root) {
            return Err(invalid_path(
                path,
                "path must start with /ext, /int, or /any",
            ));
        }

        let mut normalized = String::with_capacity(path.len().max(root.len() + 1));
        normalized.push('/');
        normalized.push_str(root);
        for component in components {
            match component {
                "" | "." => continue,
                ".." => {
                    return Err(invalid_path(path, "path traversal (..) is not allowed"));
                }
                component => {
                    normalized.push('/');
                    normalized.push_str(component);
                }
            }
        }
        Ok(Self(normalized))
    }
}

impl TryFrom<&str> for DevicePath {
    type Error = FlipperError;

    fn try_from(path: &str) -> Result<Self> {
        Self::try_from(path.to_owned())
    }
}

impl<'de> Deserialize<'de> for DevicePath {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// Serde adapter for compatibility DTOs that still expose paths as strings.
///
/// Cache entry structs are shared with the frontend and many parser helpers,
/// so changing all of their public fields at once would be unnecessarily
/// disruptive. Applying this adapter to those input fields preserves their
/// JSON shape while guaranteeing that deserialized values are canonical.
pub fn deserialize_device_path_string<'de, D>(
    deserializer: D,
) -> std::result::Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    DevicePath::deserialize(deserializer).map(DevicePath::into_string)
}

impl AsRef<str> for DevicePath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Deref for DevicePath {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for DevicePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<DevicePath> for String {
    fn from(path: DevicePath) -> Self {
        path.into_string()
    }
}

fn invalid_path(path: String, reason: impl Into<String>) -> FlipperError {
    FlipperError::InvalidDevicePath {
        path,
        reason: reason.into(),
    }
}

/// Validate a device-provided directory child before joining it to a path.
pub fn validate_child_name(child: &str) -> Result<()> {
    if child.is_empty() {
        return Err(invalid_path(child.into(), "device child name is empty"));
    }
    if matches!(child, "." | "..") {
        return Err(invalid_path(
            child.into(),
            "device child name cannot be a dot segment",
        ));
    }
    if child.contains('/') || child.contains('\\') {
        return Err(invalid_path(
            child.into(),
            "device child name cannot contain a path separator",
        ));
    }
    if child.chars().any(char::is_control) {
        return Err(invalid_path(
            child.into(),
            "device child name cannot contain control characters",
        ));
    }
    Ok(())
}

/// Compatibility validator for call sites that have not yet adopted the
/// typed boundary. New command arguments should use [`DevicePath`] directly.
pub fn validate_path(path: &str) -> Result<()> {
    DevicePath::try_from(path).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(raw: &str) -> DevicePath {
        DevicePath::try_from(raw).unwrap()
    }

    #[test]
    fn accepts_only_known_roots_and_absolute_subpaths() {
        for raw in [
            "/ext",
            "/int",
            "/any",
            "/ext/foo",
            "/int/a/b/c.txt",
            "/any/x",
        ] {
            assert_eq!(path(raw).as_str(), raw);
        }
        for raw in ["/extABC", "/intra", "/anywhere", "/ex", "/", "", "ext/a"] {
            assert!(DevicePath::try_from(raw).is_err(), "should reject {raw}");
        }
    }

    #[test]
    fn rejects_traversal_and_non_posix_separators() {
        for raw in [
            "/ext/../foo",
            "/ext/foo/..",
            "/ext/..",
            "/../ext",
            "/ext\\foo",
            "/ext/a\\b",
            "/ext/a\0b",
        ] {
            assert!(DevicePath::try_from(raw).is_err(), "should reject {raw:?}");
        }
        assert_eq!(path("/ext/foo..bar").as_str(), "/ext/foo..bar");
        assert_eq!(path("/ext/.../baz").as_str(), "/ext/.../baz");
    }

    #[test]
    fn normalizes_harmless_empty_and_dot_components() {
        for (raw, expected) in [
            ("/ext/", "/ext"),
            ("/ext//folder///file", "/ext/folder/file"),
            ("/int/./folder/./file", "/int/folder/file"),
            ("/any///./", "/any"),
        ] {
            assert_eq!(path(raw).as_str(), expected);
        }
    }

    #[test]
    fn serde_boundary_is_string_compatible_and_validated() {
        let decoded: DevicePath = serde_json::from_str(r#""/ext//folder""#).unwrap();
        assert_eq!(decoded.as_str(), "/ext/folder");
        assert_eq!(serde_json::to_string(&decoded).unwrap(), r#""/ext/folder""#);
        assert!(serde_json::from_str::<DevicePath>(r#""/ext/../secret""#).is_err());
    }

    #[test]
    fn compatibility_dto_adapter_canonicalizes_and_rejects_paths() {
        #[derive(Deserialize)]
        struct CachedEntry {
            #[serde(deserialize_with = "deserialize_device_path_string")]
            path: String,
        }

        let entry: CachedEntry = serde_json::from_str(r#"{"path":"/ext//nfc/card.nfc"}"#).unwrap();
        assert_eq!(entry.path, "/ext/nfc/card.nfc");
        assert!(serde_json::from_str::<CachedEntry>(r#"{"path":"/ext/../card.nfc"}"#).is_err());
    }

    #[test]
    fn child_join_validates_before_building_a_canonical_path() {
        let parent = path("/ext/apps/");
        assert_eq!(
            parent.join_child("tool.fap").unwrap().as_str(),
            "/ext/apps/tool.fap"
        );
        assert_eq!(
            parent.join_child("foo..bar").unwrap().as_str(),
            "/ext/apps/foo..bar"
        );
        for child in ["", ".", "..", "../x", "a/b", "a\\b", "bad\0name"] {
            assert!(
                parent.join_child(child).is_err(),
                "should reject child {child:?}"
            );
        }
    }

    #[test]
    fn rejected_paths_preserve_typed_safe_metadata() {
        let error = DevicePath::try_from("/ext/../secret").unwrap_err();
        let public = error.command_error();
        assert_eq!(public.code, "invalid_path");
        assert_eq!(public.operation, "storage");
        assert_eq!(public.path.as_deref(), Some("/ext/../secret"));
    }
}
