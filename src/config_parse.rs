//! Shared line-oriented parsing helpers for the two Nikon SDK config
//! formats (`MaidLayer.config`, `RangeValue.config`) — same tag-embedded
//! XML-ish shape, different payloads. Previously hand-duplicated verbatim
//! in `maid_layer.rs` and `range_value.rs`, deferred from being shared
//! "until a third parser shows up" even though the two existing copies had
//! zero variation between them already.

/// Extract the substring between `open` and `close` on the same line.
/// Returns `None` if either marker is absent or they're out of order.
pub(crate) fn extract_between<'a>(line: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = line.find(open)? + open.len();
    let end = line[start..].find(close)? + start;
    Some(&line[start..end])
}

/// Match an opening tag with embedded data: `<NAME:PAYLOAD>`. On match,
/// return the payload string. Returns `None` if the line isn't this shape.
///
/// The match is strict — we anchor on `<NAME:` to avoid eating
/// `<observercapability:...>` when we asked for `capability`.
///
/// Allocation-free: this runs once per line while walking config files that
/// can run past 300k lines (MaidLayer.config), so the `format!("<{name}:")`
/// this used to build on every call was a real, not just theoretical, cost
/// — `str::strip_prefix` checks the same "<" + name + ":" shape without it.
pub(crate) fn strip_open_tag<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let rest = line.strip_prefix('<')?.strip_prefix(name)?.strip_prefix(':')?;
    // The payload runs up to the next '>'. Some lines have trailing whitespace
    // or self-close on the same line, but the opener itself ends at '>'.
    let end = rest.find('>')?;
    Some(&rest[..end])
}

/// Match `<NAME:VALUE></NAME>` on a single line. Returns VALUE.
pub(crate) fn strip_self_tag<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let payload = strip_open_tag(line, name)?;
    // Deliberately still allocates the closer for a `contains` search
    // (not just an immediately-adjacent check after the opener's '>') —
    // unlike strip_open_tag, this isn't verified to run on every line of a
    // huge file, and loosening the match to "closer must be adjacent"
    // without confirming real config lines never have anything between
    // opener and closer would risk a silent parsing regression for a
    // micro-optimization.
    let closer = format!("</{name}>");
    if line.contains(&closer) {
        Some(payload)
    } else {
        None
    }
}

/// Split "kNkMAIDCapability_Aperture-33285" into ("kNkMAIDCapability_Aperture", 33285).
pub(crate) fn split_name_code(payload: &str) -> Result<(String, u32), String> {
    let dash = payload
        .rfind('-')
        .ok_or_else(|| format!("no '-' in {payload:?}"))?;
    let name = &payload[..dash];
    let code: u32 = payload[dash + 1..]
        .parse()
        .map_err(|_| format!("non-numeric code in {payload:?}"))?;
    Ok((name.to_string(), code))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_between ──────────────────────────────────────────────────

    #[test]
    fn extract_between_works() {
        assert_eq!(extract_between("<a>hello</a>", "<a>", "</a>"), Some("hello"));
    }

    #[test]
    fn extract_between_missing_open_is_none() {
        assert_eq!(extract_between("hello</a>", "<a>", "</a>"), None);
    }

    #[test]
    fn extract_between_missing_close_is_none() {
        assert_eq!(extract_between("<a>hello", "<a>", "</a>"), None);
    }

    // ── strip_open_tag ────────────────────────────────────────────────────

    #[test]
    fn strip_open_tag_matches_exact_name() {
        assert_eq!(strip_open_tag("<capability:kFoo-1>", "capability"), Some("kFoo-1"));
    }

    #[test]
    fn strip_open_tag_does_not_eat_longer_tag_name() {
        // Regression: anchoring on "<capability:" (not just "capability")
        // must not match "<observercapability:...>".
        assert_eq!(strip_open_tag("<observercapability:x>", "capability"), None);
    }

    // ── strip_self_tag ────────────────────────────────────────────────────

    #[test]
    fn strip_self_tag_requires_matching_closer_on_same_line() {
        assert_eq!(strip_self_tag("<version:1.0></version>", "version"), Some("1.0"));
        assert_eq!(strip_self_tag("<version:1.0>", "version"), None);
    }

    // ── split_name_code ───────────────────────────────────────────────────
    // The single canonical implementation — previously hand-duplicated
    // verbatim in maid_layer.rs and range_value.rs.

    #[test]
    fn split_name_code_works() {
        assert_eq!(
            split_name_code("kNkMAIDCapability_Aperture-33285").unwrap(),
            ("kNkMAIDCapability_Aperture".to_string(), 33285u32)
        );
    }

    #[test]
    fn split_name_code_hyphen_in_name() {
        // rfind ensures the LAST '-' is the code separator, so hyphens
        // within the capability name are preserved.
        assert_eq!(
            split_name_code("kNkMAIDCapability_Some-Name-33285").unwrap(),
            ("kNkMAIDCapability_Some-Name".to_string(), 33285u32)
        );
    }

    #[test]
    fn split_name_code_no_dash_is_err() {
        assert!(split_name_code("kNkMAIDCapability_NoDash").is_err());
    }

    #[test]
    fn split_name_code_non_numeric_code_is_err() {
        assert!(split_name_code("kNkMAIDCapability_Foo-BAR").is_err());
    }
}
