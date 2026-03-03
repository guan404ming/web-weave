use std::sync::Mutex;

use anyhow::{Context, Result};
use bloomfilter::Bloom;
use serde::{Deserialize, Serialize};

/// Serializable bloom filter state.
#[derive(Serialize, Deserialize)]
struct BloomState {
    bitmap: Vec<u8>,
    num_bits: u64,
    num_hashes: u32,
    sip_keys: [(u64, u64); 2],
}

pub struct Dedup {
    filter: Mutex<Bloom<str>>,
}

impl Dedup {
    pub fn new(expected_items: usize, fp_rate: f64) -> Result<Self> {
        let filter = Bloom::new_for_fp_rate(expected_items, fp_rate);
        Ok(Self {
            filter: Mutex::new(filter),
        })
    }

    /// Returns true if already seen (or false positive). Inserts if new.
    pub fn check_and_insert(&self, url: &str) -> bool {
        let mut f = self.filter.lock().unwrap();
        if f.check(url) {
            true
        } else {
            f.set(url);
            false
        }
    }

    /// Batch check: for each (url, domain), returns None if URL already seen,
    /// or Some(domain_is_new) if URL is new. Single lock acquisition for all items.
    pub fn check_urls_with_domains(&self, items: &[(String, String)]) -> Vec<Option<bool>> {
        let mut f = self.filter.lock().unwrap();
        let mut domain_key = String::with_capacity(128);
        items
            .iter()
            .map(|(url, domain)| {
                if f.check(url.as_str()) {
                    return None; // already seen
                }
                f.set(url.as_str());
                // Check domain newness with reusable buffer
                domain_key.clear();
                domain_key.push_str("__domain__");
                domain_key.push_str(domain);
                let is_new = !f.check(&domain_key);
                if is_new {
                    f.set(&domain_key);
                }
                Some(is_new)
            })
            .collect()
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let f = self.filter.lock().unwrap();
        let state = BloomState {
            bitmap: f.bitmap(),
            num_bits: f.number_of_bits(),
            num_hashes: f.number_of_hash_functions(),
            sip_keys: f.sip_keys(),
        };
        bincode::serialize(&state).context("serialize bloom filter")
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let state: BloomState =
            bincode::deserialize(data).context("deserialize bloom filter")?;
        let filter = Bloom::from_existing(
            &state.bitmap,
            state.num_bits,
            state.num_hashes,
            state.sip_keys,
        );
        Ok(Self {
            filter: Mutex::new(filter),
        })
    }
}
