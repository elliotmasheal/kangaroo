//! Document rendering primitives: TOML front matter delimited by `+++` plus
//! a Markdown body, and the content fingerprint used for the in-process
//! compare-and-swap on writes.

use crate::domain::Fingerprint;
use anyhow::Result;

/// Splits `+++\n<toml>\n+++\n<body>` into its parts.
pub(crate) fn split_document(raw: &str) -> Result<(String, String)> {
    let rest = raw.strip_prefix("+++\n").ok_or_else(|| {
        anyhow::Error::msg("document must start with the '+++' front matter delimiter")
    })?;
    let (front_matter, body) = rest
        .split_once("\n+++")
        .ok_or_else(|| anyhow::Error::msg("front matter is not closed with '+++'"))?;
    let body = body.strip_prefix('\n').unwrap_or(body);
    Ok((front_matter.to_owned(), body.to_owned()))
}

pub(crate) fn join_document(front_matter: &str, body: &str) -> String {
    format!("+++\n{front_matter}\n+++\n{body}")
}

pub(crate) fn fingerprint(bytes: &str) -> Fingerprint {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Fingerprint(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_join_round_trip() {
        let raw = "+++\nschema = 1\n+++\n# hello\n\nbody\n";
        let (front_matter, body) = split_document(raw).expect("parses");
        assert_eq!(front_matter, "schema = 1");
        assert_eq!(body, "# hello\n\nbody\n");
        assert_eq!(join_document(&front_matter, &body), raw);
    }

    #[test]
    fn split_rejects_malformed_documents() {
        assert!(split_document("no front matter").is_err());
        assert!(split_document("+++\nunclosed").is_err());
    }

    #[test]
    fn fingerprints_differ_for_different_content() {
        assert_ne!(fingerprint("a").0, fingerprint("b").0);
        assert_eq!(fingerprint("same").0, fingerprint("same").0);
    }
}
