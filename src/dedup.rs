use anyhow::{Context, Result};
use fastbloom::AtomicBloomFilter;

pub struct Dedup {
    filter: AtomicBloomFilter,
}

impl Dedup {
    pub fn new(expected_items: usize, fp_rate: f64) -> Result<Self> {
        let filter = AtomicBloomFilter::with_false_pos(fp_rate)
            .expected_items(expected_items);
        Ok(Self { filter })
    }

    /// Returns true if already seen (or false positive). Inserts if new.
    pub fn check_and_insert(&self, url: &str) -> bool {
        if self.filter.contains(url) {
            true
        } else {
            self.filter.insert(url);
            false
        }
    }

    /// Batch check: for each (url, domain), returns None if URL already seen,
    /// or Some(domain_is_new) if URL is new. Lock-free, no contention.
    pub fn check_urls_with_domains(&self, items: &[(String, String)]) -> Vec<Option<bool>> {
        let mut domain_key = String::with_capacity(128);
        items
            .iter()
            .map(|(url, domain)| {
                if self.filter.contains(url.as_str()) {
                    return None;
                }
                self.filter.insert(url.as_str());
                // Check domain newness with reusable buffer
                domain_key.clear();
                domain_key.push_str("__domain__");
                domain_key.push_str(domain);
                let is_new = !self.filter.contains(domain_key.as_str());
                if is_new {
                    self.filter.insert(domain_key.as_str());
                }
                Some(is_new)
            })
            .collect()
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        bincode::serialize(&self.filter).context("serialize bloom filter")
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let filter: AtomicBloomFilter =
            bincode::deserialize(data).context("deserialize bloom filter")?;
        Ok(Self { filter })
    }
}
