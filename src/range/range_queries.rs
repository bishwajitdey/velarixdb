use std::{cmp::Ordering, collections::HashMap};

// NOTE: STCS can handle range queries but scans within identified SSTables might be neccessary.
// Data for your range might be spread across multiple SSTables. Even with a successful bloom filter check,
// each identified SSTable might still contain data outside your desired range. For heavily range query-focused workloads, LCS or TWSC should be considered
// Although this stratedy is not available for now, It will be implmented in the future

use crate::{
    err::StorageEngineError, memtable::Entry, sparse_index::SparseIndex, sstable::SSTable,
    storage_engine::StorageEngine,
};

pub(crate) type Key = Vec<u8>;
pub(crate) type ValOffset = usize;

pub struct FetchedEntry {
    pub key: Vec<u8>,
    pub val: Vec<u8>,
}
pub struct RangeIterator<'a> {
    pub start: &'a [u8],
    pub current: u32,
    pub end: &'a [u8],
    pub allow_prefetch: bool,
    pub prefetch_entries_size: usize,
    pub prefetch_entries: Vec<FetchedEntry>,
    pub keys: Vec<Entry<Key, ValOffset>>,
}

impl<'a> RangeIterator<'a> {
    fn new(
        start: &'a [u8],
        end: &'a [u8],
        allow_prefetch: bool,
        prefetch_entries_size: usize,
        keys: Vec<Entry<Key, ValOffset>>,
    ) -> Self {
        Self {
            start,
            current: 0,
            end,
            allow_prefetch,
            prefetch_entries_size,
            prefetch_entries: Vec::new(),
            keys,
        }
    }

    pub fn next(&mut self) -> Option<FetchedEntry> {
        None
    }
    pub fn prev(&mut self) -> Option<FetchedEntry> {
        None
    }
    pub fn key<K>(&mut self) -> Option<K> {
        None
    }

    pub fn value<V>(&mut self) -> Option<V> {
        None
    }

    // Move the iterator to the end of the collection.
    pub fn end(&mut self) -> Option<FetchedEntry> {
        None
    }
}

impl<'a> Iterator for RangeIterator<'a> {
    type Item = FetchedEntry;

    fn next(&mut self) -> Option<Self::Item> {
        None
    }
}

impl<'a> DoubleEndedIterator for RangeIterator<'a> {
    fn next_back(&mut self) -> Option<Self::Item> {
        None
        // ... (implementation to return next element from back)
    }
}

impl StorageEngine<Vec<u8>> {
    // Start if the range query
    pub async fn seek<'a>(
        &self,
        start: &'a [u8],
        end: &'a [u8],
    ) -> Result<Box<dyn Iterator<Item = FetchedEntry> + 'a>, StorageEngineError> {
        let mut merger = Merger::new();
        // check entries within active memtable
        if !self.active_memtable.index.is_empty() {
            if self
                .active_memtable
                .index
                .lower_bound(std::ops::Bound::Included(start))
                .is_some()
                || self
                    .active_memtable
                    .index
                    .upper_bound(std::ops::Bound::Included(end))
                    .is_some()
            {
                merger.merge_entries(
                    self.active_memtable
                        .clone()
                        .index
                        .iter()
                        .map(|e| {
                            Entry::new(e.key().to_vec(), e.value().0, e.value().1, e.value().2)
                        })
                        .collect::<Vec<Entry<Key, ValOffset>>>(),
                );
            }
        }
        // check inactive memtable
        if !self.read_only_memtables.read().await.is_empty() {
            let read_only_memtables = self.read_only_memtables.read().await.clone();

            for (_, memtable) in read_only_memtables {
                let memtable_ref = memtable.read().await;
                if memtable_ref
                    .index
                    .lower_bound(std::ops::Bound::Included(start))
                    .is_some()
                    || memtable_ref
                        .index
                        .upper_bound(std::ops::Bound::Included(end))
                        .is_some()
                {
                    merger.merge_entries(
                        memtable_ref
                            .clone()
                            .index
                            .iter()
                            .map(|e| {
                                Entry::new(e.key().to_vec(), e.value().0, e.value().1, e.value().2)
                            })
                            .collect::<Vec<Entry<Key, ValOffset>>>(),
                    );
                }
            }
        }

        let sstables_within_range = {
            let mut sstable_path = HashMap::new();
            let bf = self.bloom_filters.read().await.clone();
            for b in bf.into_iter() {
                let bf_inner = b.to_owned();
                let bf_sstable = bf_inner.sstable_path.to_owned().unwrap();
                let data_path = bf_sstable.data_file_path.to_str().unwrap();
                if bf_inner.contains(&start.to_vec()) || bf_inner.contains(&end.to_vec()) {
                    sstable_path.insert(data_path.to_owned(), bf_sstable.to_owned());
                }
            }

            let key_range = self.key_range.read().await;
            let paths_from_key_range = key_range.range_scan(&start.to_vec(), &end.to_vec());
            if !paths_from_key_range.is_empty() {
                for range in paths_from_key_range.iter() {
                    if !sstable_path
                        .contains_key(range.full_sst_path.data_file_path.to_str().unwrap())
                    {
                        sstable_path.insert(
                            range
                                .full_sst_path
                                .data_file_path
                                .to_str()
                                .unwrap()
                                .to_owned(),
                            range.full_sst_path.to_owned(),
                        );
                    }
                }
            }
            sstable_path
        };

        for (_, sst) in sstables_within_range {
            let sparse_index = SparseIndex::new(sst.index_file_path.clone()).await;

            match sparse_index.get_block_offset_range(&start, &end).await {
                Ok(range_offset) => {
                    let sst = SSTable::new_with_exisiting_file_path(
                        sst.dir.to_owned(),
                        sst.data_file_path.to_owned(),
                        sst.index_file_path.to_owned(),
                    );
                    match sst.range(range_offset).await {
                        Ok(sstable_entries) => merger.merge_entries(sstable_entries),
                        Err(err) => return Err(err),
                    }
                }
                Err(err) => return Err(StorageEngineError::RangeScanError(Box::new(err))),
            }
        }

        Ok(Box::new(RangeIterator::new(
            start,
            end,
            self.config.allow_prefetch,
            self.config.prefetch_size,
            merger.entries,
        )))
    }
}

pub struct Merger {
    entries: Vec<Entry<Key, ValOffset>>,
}

impl Merger {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn merge_entries(&mut self, entries_to_merge: Vec<Entry<Key, ValOffset>>) {
        let mut merged_indexes = Vec::new();
        let e1 = &self.entries;
        let e2 = entries_to_merge;

        let (mut i, mut j) = (0, 0);
        // Compare elements from both arrays and merge them
        while i < e1.len() && j < e2.len() {
            match e1[i].key.cmp(&e2[j].key) {
                Ordering::Less => {
                    merged_indexes.push(e1[i].to_owned());
                    i += 1;
                }
                Ordering::Equal => {
                    if e1[i].created_at > e2[j].created_at {
                        merged_indexes.push(e1[i].to_owned());
                    } else {
                        merged_indexes.push(e2[j].to_owned());
                    }
                    i += 1;
                    j += 1;
                }
                Ordering::Greater => {
                    merged_indexes.push(e2[j].to_owned());
                    j += 1;
                }
            }
        }

        // If there are any remaining entries in e1, append them
        while i < e1.len() {
            merged_indexes.push(e1[i].to_owned());
            i += 1;
        }

        // If there are any remaining entries in e2, append them
        while j < e2.len() {
            merged_indexes.push(e2[j].to_owned());
            j += 1;
        }
    }
}
