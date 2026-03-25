use std::path::{Component, Path, PathBuf};

const WINDOWS_RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6",
    "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7",
    "LPT8", "LPT9",
];

pub(crate) fn sanitize_relative_path(path: &Path) -> Option<PathBuf> {
    let mut sanitized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let part = sanitize_filename_component(&value.to_string_lossy());
                if part.is_empty() {
                    continue;
                }
                sanitized.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    if sanitized.as_os_str().is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

pub(crate) fn parse_content_disposition(header_value: &str) -> Option<String> {
    for part in header_value.split(';').map(str::trim) {
        if let Some(value) = part.strip_prefix("filename*=") {
            if let Some(decoded) = decode_rfc5987_value(value.trim_matches('"')) {
                let sanitized = sanitize_filename_component(&decoded);
                if !sanitized.is_empty() {
                    return Some(sanitized);
                }
            }
        }
    }

    for part in header_value.split(';').map(str::trim) {
        if let Some(value) = part.strip_prefix("filename=") {
            let sanitized = sanitize_filename_component(value.trim_matches('"'));
            if !sanitized.is_empty() {
                return Some(sanitized);
            }
        }
    }

    None
}

pub(crate) fn filename_from_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let segment = parsed
        .path_segments()
        .and_then(|mut segments| segments.rfind(|segment: &&str| !segment.is_empty()))?;
    let decoded = percent_decode(segment)?;
    let sanitized = sanitize_filename_component(&decoded);
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

pub(crate) fn detect_filename(content_disposition: Option<&str>, url: &str) -> String {
    content_disposition
        .and_then(parse_content_disposition)
        .or_else(|| filename_from_url(url))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "download".to_string())
}

fn decode_rfc5987_value(value: &str) -> Option<String> {
    let (charset, _language, encoded) = split_rfc5987_parts(value)?;
    let bytes = percent_decode_bytes(encoded)?;
    if charset.eq_ignore_ascii_case("utf-8") {
        String::from_utf8(bytes).ok()
    } else if charset.eq_ignore_ascii_case("iso-8859-1") || charset.eq_ignore_ascii_case("latin1") {
        Some(bytes.into_iter().map(char::from).collect())
    } else {
        None
    }
}

fn split_rfc5987_parts(value: &str) -> Option<(&str, &str, &str)> {
    let first = value.find('\'')?;
    let second = value[first + 1..].find('\'')? + first + 1;
    Some((&value[..first], &value[first + 1..second], &value[second + 1..]))
}

fn sanitize_filename_component(value: &str) -> String {
    let filtered: String = value
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            ch if ch.is_control() => '_',
            ch => ch,
        })
        .collect();

    let trimmed = filtered.trim().trim_end_matches('.').trim_end_matches(' ');
    if trimmed.is_empty() {
        return String::new();
    }

    let mut sanitized = trimmed.to_string();
    if WINDOWS_RESERVED_NAMES
        .iter()
        .any(|reserved| sanitized.eq_ignore_ascii_case(reserved))
    {
        sanitized.push('_');
    }

    if sanitized.len() > 255 {
        let (stem, extension) = match sanitized.rsplit_once('.') {
            Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
                (stem.to_string(), Some(extension.to_string()))
            }
            _ => (sanitized.clone(), None),
        };
        if let Some(extension) = extension {
            let keep = 255usize.saturating_sub(extension.len() + 1);
            sanitized = format!("{}.{}", trim_to_boundary(&stem, keep), extension);
        } else {
            sanitized = trim_to_boundary(&sanitized, 255);
        }
    }

    sanitized
}

fn trim_to_boundary(value: &str, max_len: usize) -> String {
    value.chars().take(max_len).collect()
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = percent_decode_bytes(value)?;
    String::from_utf8(bytes).ok()
}

fn percent_decode_bytes(value: &str) -> Option<Vec<u8>> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0usize;

    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
            let byte = u8::from_str_radix(hex, 16).ok()?;
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }

    Some(decoded)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{detect_filename, filename_from_url, parse_content_disposition, sanitize_relative_path};

    #[test]
    fn test_parse_content_disposition_prefers_filename_star() {
        let header = "attachment; filename=plain.txt; filename*=UTF-8''hello%20world.txt";
        assert_eq!(
            parse_content_disposition(header).as_deref(),
            Some("hello world.txt")
        );
    }

    #[test]
    fn test_parse_content_disposition_sanitizes_filename() {
        let header = "attachment; filename=..\\bad:name?.txt";
        assert_eq!(parse_content_disposition(header).as_deref(), Some(".._bad_name_.txt"));
    }

    #[test]
    fn test_parse_content_disposition_supports_latin1_filename_star() {
        let header = "attachment; filename*=ISO-8859-1''caf%E9.txt";
        assert_eq!(parse_content_disposition(header).as_deref(), Some("café.txt"));
    }

    #[test]
    fn test_parse_content_disposition_falls_back_when_filename_star_is_invalid() {
        let header = "attachment; filename*=UTF-8''bad%ZZname.txt; filename=fallback.txt";
        assert_eq!(parse_content_disposition(header).as_deref(), Some("fallback.txt"));
    }

    #[test]
    fn test_parse_content_disposition_rejects_invalid_only_filename_star() {
        let header = "attachment; filename*=UTF-8''bad%ZZname.txt";
        assert_eq!(parse_content_disposition(header), None);
    }

    #[test]
    fn test_parse_content_disposition_trims_reserved_and_long_names() {
        let long_name = format!("{}.txt", "a".repeat(400));
        let header = format!("attachment; filename=\"{long_name}\"");
        let parsed = parse_content_disposition(&header).unwrap();
        assert_eq!(parsed.len(), 255);
        assert!(parsed.ends_with(".txt"));

        let reserved = parse_content_disposition("attachment; filename=CON ").unwrap();
        assert_eq!(reserved, "CON_");
    }

    #[test]
    fn test_filename_from_url_decodes_last_segment() {
        assert_eq!(
            filename_from_url("https://example.com/files/report%20final.zip").as_deref(),
            Some("report final.zip")
        );
    }

    #[test]
    fn test_filename_from_url_rejects_invalid_inputs() {
        assert_eq!(filename_from_url("not a url"), None);
        assert_eq!(filename_from_url("https://example.com/files/bad%ZZname.zip"), None);
    }

    #[test]
    fn test_detect_filename_falls_back_to_url_when_header_is_invalid() {
        assert_eq!(
            detect_filename(
                Some("attachment; filename*=UTF-8''bad%ZZname.txt"),
                "https://example.com/files/url-name.bin"
            ),
            "url-name.bin"
        );
    }

    #[test]
    fn test_detect_filename_falls_back_to_default() {
        assert_eq!(detect_filename(None, "https://example.com/"), "download");
    }

    #[test]
    fn test_sanitize_relative_path_rejects_parent_dir() {
        assert_eq!(sanitize_relative_path(Path::new("../bad.txt")), None);
    }

    #[test]
    fn test_sanitize_relative_path_rejects_absolute_and_empty_paths() {
        assert_eq!(sanitize_relative_path(Path::new("/bad.txt")), None);
        assert_eq!(sanitize_relative_path(Path::new(".")), None);
    }

    #[test]
    fn test_sanitize_relative_path_allows_nested_relative_paths() {
        assert_eq!(
            sanitize_relative_path(Path::new("nested/output.txt")),
            Some(PathBuf::from("nested/output.txt"))
        );
    }

    #[test]
    fn test_sanitize_relative_path_normalizes_components() {
        assert_eq!(
            sanitize_relative_path(Path::new("./CON ./child?.txt")),
            Some(PathBuf::from("CON_/child_.txt"))
        );
    }

    #[test]
    fn test_sanitize_relative_path_drops_empty_components() {
        assert_eq!(
            sanitize_relative_path(Path::new("nested/ /file.txt")),
            Some(PathBuf::from("nested/file.txt"))
        );
    }
}