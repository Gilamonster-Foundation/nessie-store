//! Parsing ByteStream resource names.
//!
//! ByteStream addresses a blob by a slash-delimited *string*, not a message field —
//! a notorious REAPI incompatibility source. A strict validator (exact 64-hex hash,
//! numeric size) avoids the ambiguity. v0 rejects `compressed-blobs/{codec}/...`.

use tonic::Status;

/// A parsed ByteStream resource name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceName {
    /// A read: `{instance}/blobs/{sha256}/{size}`.
    Read {
        /// The instance prefix (may be empty).
        instance: String,
        /// The 64-hex SHA-256.
        sha256: String,
        /// The declared byte size.
        size: u64,
    },
    /// A write: `{instance}/uploads/{uuid}/blobs/{sha256}/{size}[/...]`.
    Write {
        /// The instance prefix (may be empty).
        instance: String,
        /// The client-chosen upload UUID.
        uuid: String,
        /// The 64-hex SHA-256.
        sha256: String,
        /// The declared byte size.
        size: u64,
    },
}

fn bad(msg: impl Into<String>) -> Status {
    Status::invalid_argument(msg.into())
}

fn parse_hash(s: &str) -> Result<String, Status> {
    if s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(s.to_string())
    } else {
        Err(bad(format!(
            "resource hash must be 64 lowercase-hex chars, got {s:?}"
        )))
    }
}

fn parse_size(s: &str) -> Result<u64, Status> {
    s.parse::<u64>().map_err(|_| {
        bad(format!(
            "resource size must be a non-negative integer, got {s:?}"
        ))
    })
}

impl ResourceName {
    /// Parse a ByteStream resource name (read or write form).
    ///
    /// # Errors
    ///
    /// [`tonic::Status::invalid_argument`] on any malformed component, or on a
    /// `compressed-blobs` form (unsupported in v0).
    pub fn parse(name: &str) -> Result<Self, Status> {
        let segs: Vec<&str> = name.split('/').collect();

        if let Some(up) = segs.iter().position(|&s| s == "uploads") {
            let uuid = segs.get(up + 1).ok_or_else(|| bad("missing upload uuid"))?;
            let marker = segs
                .get(up + 2)
                .ok_or_else(|| bad("missing 'blobs' after uploads/{uuid}"))?;
            if *marker == "compressed-blobs" {
                return Err(bad("compressed-blobs are not supported in v0"));
            }
            if *marker != "blobs" {
                return Err(bad("expected 'blobs' after uploads/{uuid}"));
            }
            let hash = segs.get(up + 3).ok_or_else(|| bad("missing hash"))?;
            let size = segs.get(up + 4).ok_or_else(|| bad("missing size"))?;
            return Ok(ResourceName::Write {
                instance: segs[..up].join("/"),
                uuid: (*uuid).to_string(),
                sha256: parse_hash(hash)?,
                size: parse_size(size)?,
            });
        }

        if let Some(bp) = segs.iter().position(|&s| s == "blobs") {
            let hash = segs.get(bp + 1).ok_or_else(|| bad("missing hash"))?;
            let size = segs.get(bp + 2).ok_or_else(|| bad("missing size"))?;
            return Ok(ResourceName::Read {
                instance: segs[..bp].join("/"),
                sha256: parse_hash(hash)?,
                size: parse_size(size)?,
            });
        }

        if segs.contains(&"compressed-blobs") {
            return Err(bad("compressed-blobs are not supported in v0"));
        }
        Err(bad(
            "resource name must contain 'blobs' or 'uploads/{uuid}/blobs'",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn parses_a_read_with_instance() {
        let r = ResourceName::parse(&format!("my-instance/blobs/{HASH}/12")).unwrap();
        assert_eq!(
            r,
            ResourceName::Read {
                instance: "my-instance".to_string(),
                sha256: HASH.to_string(),
                size: 12,
            }
        );
    }

    #[test]
    fn parses_a_read_without_instance() {
        let r = ResourceName::parse(&format!("blobs/{HASH}/0")).unwrap();
        assert_eq!(
            r,
            ResourceName::Read {
                instance: String::new(),
                sha256: HASH.to_string(),
                size: 0,
            }
        );
    }

    #[test]
    fn parses_a_write() {
        let r = ResourceName::parse(&format!("inst/uploads/abc-123/blobs/{HASH}/99")).unwrap();
        assert_eq!(
            r,
            ResourceName::Write {
                instance: "inst".to_string(),
                uuid: "abc-123".to_string(),
                sha256: HASH.to_string(),
                size: 99,
            }
        );
    }

    #[test]
    fn rejects_bad_and_compressed() {
        assert!(ResourceName::parse("no-markers-here").is_err());
        assert!(ResourceName::parse(&format!("blobs/{HASH}/notanumber")).is_err());
        assert!(ResourceName::parse("blobs/tooshort/0").is_err());
        assert!(ResourceName::parse(&format!("i/compressed-blobs/zstd/{HASH}/5")).is_err());
    }
}
