//! The HAL `_links` object. Universally `{ "self": { "href": "<path>" } }`.

use serde::{Deserialize, Serialize};

/// A HAL self-link: `{ "href": "<path>" }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfLink {
    /// The path this resource is reachable at.
    pub href: String,
}

/// A HAL `_links` object carrying a `self` link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Links {
    /// The `self` link (serialized as the JSON key `self`).
    #[serde(rename = "self")]
    pub self_link: SelfLink,
}

impl Links {
    /// Build a `_links` object whose `self.href` is `href`.
    #[must_use]
    pub fn to(href: impl Into<String>) -> Self {
        Self {
            self_link: SelfLink { href: href.into() },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn links_render_self_key() {
        let links = Links::to("/api/storage/volumes/abc");
        assert_eq!(
            serde_json::to_value(&links).unwrap(),
            json!({ "self": { "href": "/api/storage/volumes/abc" } })
        );
    }
}
