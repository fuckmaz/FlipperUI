//! Path / extension / exclusion helpers shared by every per-library scanner.
//!
//! Each `flipper/<lib>.rs` (nfc, rfid, infrared, subghz, badusb, apps) used to
//! carry near-identical copies of these. Centralising them here means a fix to
//! the path-rebuild or exclusion semantics lands in one place — and adding the
//! next library doesn't grow the duplication count.

use crate::commands::path::DevicePath;
use crate::error::Result;

/// Progress callback fired after each parsed file. `scanned` ≤ `total`.
/// Re-exported under the same alias each scanner already used so call sites
/// don't need to change.
pub type ScanProgress<'a> = &'a mut dyn FnMut(u32, u32, &str);

/// True if `path` is `excluded` itself or sits underneath an `excluded`
/// entry. Trailing slashes on entries are tolerated.
pub fn is_excluded(path: &str, excluded: &[String]) -> bool {
    excluded.iter().any(|ex| {
        let ex = ex.trim_end_matches('/');
        path == ex || path.starts_with(&format!("{ex}/"))
    })
}

/// Case-insensitive extension match. `ext_with_dot` must include the leading
/// dot (e.g. `".nfc"`, `".rfid"`).
pub fn has_extension_ci(name: &str, ext_with_dot: &str) -> bool {
    name.len() >= ext_with_dot.len()
        && name[name.len() - ext_with_dot.len()..].eq_ignore_ascii_case(ext_with_dot)
}

/// Validate a child name returned by StorageList before rebuilding a path.
///
/// The device should return plain names, not paths. Rejecting separators and
/// dot segments keeps a malformed response from escaping the scanned root or
/// writing outside the selected local download directory.
pub fn validate_child_name(child: &str) -> Result<()> {
    crate::commands::path::validate_child_name(child)
}

/// Concatenate `parent` and `child` with exactly one `/` between them.
pub fn join_path(parent: &str, child: &str) -> Result<String> {
    DevicePath::try_from(parent)?
        .join_child(child)
        .map(DevicePath::into_string)
}

/// Extend an already-validated device path with one device-provided basename.
pub fn join_device_path(parent: &DevicePath, child: &str) -> Result<DevicePath> {
    parent.join_child(child)
}

/// Last path segment, mirroring POSIX `basename`. Falls back to the whole
/// string when there is no `/`.
pub fn file_basename(path: &str) -> &str {
    path.rsplit_once('/').map(|(_, b)| b).unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excluded_self_and_descendants() {
        let excluded = vec!["/ext/foo".to_string()];
        assert!(is_excluded("/ext/foo", &excluded));
        assert!(is_excluded("/ext/foo/bar", &excluded));
        assert!(!is_excluded("/ext/foobar", &excluded));
        assert!(!is_excluded("/ext/other", &excluded));
    }

    #[test]
    fn extension_match_is_case_insensitive() {
        assert!(has_extension_ci("a.nfc", ".nfc"));
        assert!(has_extension_ci("A.NFC", ".nfc"));
        assert!(has_extension_ci("file.RFID", ".rfid"));
        assert!(!has_extension_ci("a.nf", ".nfc"));
        assert!(!has_extension_ci("nfc", ".nfc"));
    }

    #[test]
    fn join_path_handles_trailing_slash() {
        assert_eq!(join_path("/ext/nfc", "x.nfc").unwrap(), "/ext/nfc/x.nfc");
        assert_eq!(join_path("/ext/nfc/", "x.nfc").unwrap(), "/ext/nfc/x.nfc");
    }

    #[test]
    fn typed_join_keeps_the_parent_validated() {
        let parent = DevicePath::try_from("/ext//nfc").unwrap();
        assert_eq!(
            join_device_path(&parent, "x.nfc").unwrap().as_str(),
            "/ext/nfc/x.nfc"
        );
    }

    #[test]
    fn child_name_rejects_path_segments() {
        for name in ["", ".", "..", "../x", "a/b", "a\\b"] {
            assert!(validate_child_name(name).is_err(), "should reject {name:?}");
        }
        assert!(validate_child_name("foo..bar").is_ok());
    }

    #[test]
    fn basename_strips_directory() {
        assert_eq!(file_basename("/ext/nfc/x.nfc"), "x.nfc");
        assert_eq!(file_basename("x.nfc"), "x.nfc");
    }
}
