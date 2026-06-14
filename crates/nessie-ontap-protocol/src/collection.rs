//! The HAL collection envelope: `{ records, num_records, _links }`.

use serde::{Deserialize, Serialize};

use crate::links::Links;

/// A HAL-style collection response used by every ONTAP GET-collection endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HalCollection<T> {
    /// The records in this page.
    pub records: Vec<T>,
    /// The number of records (ONTAP reports the count of `records`).
    pub num_records: usize,
    /// The HAL `_links` for the collection itself.
    #[serde(rename = "_links")]
    pub links: Links,
}

impl<T> HalCollection<T> {
    /// Wrap `records` in a collection envelope whose `self.href` is `self_href`.
    /// `num_records` is set to the length of `records`.
    pub fn new(records: Vec<T>, self_href: impl Into<String>) -> Self {
        let num_records = records.len();
        Self {
            records,
            num_records,
            links: Links::to(self_href),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_collection_shape() {
        let c: HalCollection<u8> = HalCollection::new(vec![], "/api/storage/volumes");
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            json!({
                "records": [],
                "num_records": 0,
                "_links": { "self": { "href": "/api/storage/volumes" } }
            })
        );
    }

    #[test]
    fn num_records_tracks_len() {
        let c = HalCollection::new(vec![1, 2, 3], "/x");
        assert_eq!(c.num_records, 3);
    }
}
