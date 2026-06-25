//! Small helpers shared by the VCS backends.

/// Redact any `user:pass@` userinfo from a URL so it is safe to log.
///
/// `https://user:tok@host/path` → `https://host/path`. URLs without userinfo
/// (including `file://…`) are returned unchanged.
pub(crate) fn redact_url(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let after = scheme_end + 3;
        let rest = &url[after..];
        // Userinfo is the part before '@', but only when '@' precedes the path.
        if let Some(at) = rest.find('@') {
            let slash = rest.find('/').unwrap_or(rest.len());
            if at < slash {
                return format!("{}{}", &url[..after], &rest[at + 1..]);
            }
        }
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_userinfo() {
        assert_eq!(redact_url("https://u:p@host/x.git"), "https://host/x.git");
        assert_eq!(redact_url("https://token@host/x"), "https://host/x");
    }

    #[test]
    fn leaves_clean_urls_alone() {
        assert_eq!(redact_url("https://host/x.git"), "https://host/x.git");
        assert_eq!(redact_url("file:///srv/repo"), "file:///srv/repo");
        // an '@' in the path (not userinfo) must not be touched
        assert_eq!(redact_url("https://host/a@b"), "https://host/a@b");
    }
}
