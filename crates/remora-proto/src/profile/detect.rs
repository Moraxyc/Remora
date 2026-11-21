//! Identify an official client profile from incoming request headers.

use super::{PROFILES, Profile};

const PLAT_HEADER: &str = "x-param-plat";
const VERSION_HEADER: &str = "x-param-vn";
const BUILD_HEADER: &str = "x-param-vc";

/// Identifies the matching embedded [`Profile`] from `x-param-plat`, `x-param-vn`, and
/// optionally `x-param-vc` headers.
///
/// Header names are compared ASCII case-insensitively. Returns `None` when required headers
/// are missing or no embedded profile matches.
pub fn detect_from_headers<'a>(
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Option<&'static Profile> {
    let mut plat: Option<&str> = None;
    let mut version: Option<&str> = None;
    let mut build: Option<u32> = None;

    for (name, value) in headers {
        if name.eq_ignore_ascii_case(PLAT_HEADER) {
            plat = Some(value.trim());
        } else if name.eq_ignore_ascii_case(VERSION_HEADER) {
            version = Some(value.trim());
        } else if name.eq_ignore_ascii_case(BUILD_HEADER) {
            build = value.trim().parse().ok();
        }
    }

    let (plat, version) = (plat?, version?);
    PROFILES.iter().find(|profile| {
        profile.detect.plat == plat
            && profile.detect.version == version
            && match (profile.detect.build, build) {
                (Some(expected), Some(actual)) => expected == actual,
                (Some(_), None) | (None, _) => true,
            }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_profile_from_mixed_case_headers() {
        let headers = [
            ("X-Param-PLAT", "4"),
            ("X-Param-VN", "4.48.2"),
            ("X-Param-VC", "634"),
        ];
        let detected = detect_from_headers(headers).expect("4.48.2 should match");
        assert_eq!(detected.id, "macos-4.48.2");
    }

    #[test]
    fn detects_without_vc_and_rejects_mismatched_vc() {
        let headers = [("x-param-plat", "4"), ("x-param-vn", "4.48.1")];
        assert_eq!(
            detect_from_headers(headers).map(|p| p.id),
            Some("macos-4.48.1")
        );

        let mismatched = [
            ("x-param-plat", "4"),
            ("x-param-vn", "4.48.1"),
            ("x-param-vc", "634"),
        ];
        assert_eq!(detect_from_headers(mismatched), None);
    }

    #[test]
    fn returns_none_for_unknown_or_incomplete_headers() {
        assert_eq!(
            detect_from_headers([("x-param-plat", "1"), ("x-param-vn", "4.45.0.4318")]),
            None
        );
        assert_eq!(detect_from_headers([("x-param-plat", "4")]), None);
    }
}
