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

    pub fn contains(&self, url: &str) -> bool {
        self.filter.lock().unwrap().check(url)
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
