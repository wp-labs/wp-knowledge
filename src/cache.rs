use std::collections::HashMap;
use std::net::IpAddr;
use std::num::NonZeroUsize;

use lru::LruCache;
use wp_model_core::model::{DataField, FValueStr, Value};

#[derive(Debug, Clone)]
pub struct FieldQueryCache {
    str_idx: HashMap<FValueStr, usize>,
    i64_idx: HashMap<i64, usize>,
    ip_idx: HashMap<IpAddr, usize>,
    cache_data: LruCache<LocalCacheKey, Vec<DataField>>,
    idx_num: usize,
    generation: Option<u64>,
}

pub type QueryLocalCache = FieldQueryCache;

impl Default for FieldQueryCache {
    fn default() -> Self {
        Self::with_capacity(100)
    }
}

impl FieldQueryCache {
    pub fn with_capacity(size: usize) -> Self {
        let size = size.max(1);
        Self {
            str_idx: HashMap::new(),
            i64_idx: HashMap::new(),
            ip_idx: HashMap::new(),
            cache_data: LruCache::new(NonZeroUsize::new(size).expect("non-zero cache size")),
            idx_num: 0,
            generation: None,
        }
    }

    fn get_idx(&self, param: &DataField) -> Option<usize> {
        match param.get_value() {
            Value::Chars(v) => self.str_idx.get(v).copied(),
            Value::Digit(v) => self.i64_idx.get(v).copied(),
            Value::IpAddr(v) => self.ip_idx.get(v).copied(),
            _ => None,
        }
    }

    fn try_up_idx(&mut self, param: &DataField) -> Option<usize> {
        match param.get_value() {
            Value::Chars(v) => {
                if let Some(idx) = self.str_idx.get(v) {
                    Some(*idx)
                } else {
                    self.idx_num += 1;
                    self.str_idx.insert(v.clone(), self.idx_num);
                    Some(self.idx_num)
                }
            }
            Value::Digit(v) => {
                if let Some(idx) = self.i64_idx.get(v) {
                    Some(*idx)
                } else {
                    self.idx_num += 1;
                    self.i64_idx.insert(*v, self.idx_num);
                    Some(self.idx_num)
                }
            }
            Value::IpAddr(v) => {
                if let Some(idx) = self.ip_idx.get(v) {
                    Some(*idx)
                } else {
                    self.idx_num += 1;
                    self.ip_idx.insert(*v, self.idx_num);
                    Some(self.idx_num)
                }
            }
            _ => None,
        }
    }

    fn reset(&mut self) {
        self.str_idx.clear();
        self.i64_idx.clear();
        self.ip_idx.clear();
        self.cache_data.clear();
        self.idx_num = 0;
    }
}

#[derive(PartialEq, Eq, Hash, Debug, Clone)]
pub enum EnumSizeIndex {
    Idx1(usize),
    Idx2(usize, usize),
    Idx3(usize, usize, usize),
    Idx4(usize, usize, usize, usize),
    Idx5(usize, usize, usize, usize, usize),
    Idx6(usize, usize, usize, usize, usize, usize),
}

#[derive(PartialEq, Eq, Hash, Debug, Clone)]
struct LocalCacheKey {
    scope_hash: u64,
    idxs: EnumSizeIndex,
}

pub trait CacheAble<P, T, const N: usize> {
    fn prepare_generation(&mut self, _generation: u64) {}
    fn save_scoped(&mut self, _scope_hash: u64, params: &[P; N], result: T) {
        self.save(params, result);
    }
    fn fetch_scoped(&self, _scope_hash: u64, params: &[P; N]) -> Option<&T> {
        self.fetch(params)
    }
    fn save(&mut self, params: &[P; N], result: T);
    fn fetch(&self, params: &[P; N]) -> Option<&T>;
}

impl CacheAble<DataField, Vec<DataField>, 1> for FieldQueryCache {
    fn prepare_generation(&mut self, generation: u64) {
        if self.generation != Some(generation) {
            self.reset();
            self.generation = Some(generation);
        }
    }
    fn save_scoped(&mut self, scope_hash: u64, params: &[DataField; 1], result: Vec<DataField>) {
        if let Some(i0) = self.try_up_idx(&params[0]) {
            self.cache_data.put(
                LocalCacheKey {
                    scope_hash,
                    idxs: EnumSizeIndex::Idx1(i0),
                },
                result,
            );
        }
    }

    fn fetch_scoped(&self, scope_hash: u64, params: &[DataField; 1]) -> Option<&Vec<DataField>> {
        if let Some(i0) = self.get_idx(&params[0]) {
            return self.cache_data.peek(&LocalCacheKey {
                scope_hash,
                idxs: EnumSizeIndex::Idx1(i0),
            });
        }
        None
    }

    fn save(&mut self, params: &[DataField; 1], result: Vec<DataField>) {
        self.save_scoped(0, params, result);
    }

    fn fetch(&self, params: &[DataField; 1]) -> Option<&Vec<DataField>> {
        self.fetch_scoped(0, params)
    }
}

impl CacheAble<DataField, Vec<DataField>, 2> for FieldQueryCache {
    fn prepare_generation(&mut self, generation: u64) {
        if self.generation != Some(generation) {
            self.reset();
            self.generation = Some(generation);
        }
    }

    fn save_scoped(&mut self, scope_hash: u64, params: &[DataField; 2], result: Vec<DataField>) {
        if let (Some(i0), Some(i1)) = (self.try_up_idx(&params[0]), self.try_up_idx(&params[1])) {
            self.cache_data.put(
                LocalCacheKey {
                    scope_hash,
                    idxs: EnumSizeIndex::Idx2(i0, i1),
                },
                result,
            );
        }
    }

    fn fetch_scoped(&self, scope_hash: u64, params: &[DataField; 2]) -> Option<&Vec<DataField>> {
        if let (Some(i0), Some(i1)) = (self.get_idx(&params[0]), self.get_idx(&params[1])) {
            return self.cache_data.peek(&LocalCacheKey {
                scope_hash,
                idxs: EnumSizeIndex::Idx2(i0, i1),
            });
        }
        None
    }

    fn save(&mut self, params: &[DataField; 2], result: Vec<DataField>) {
        self.save_scoped(0, params, result);
    }

    fn fetch(&self, params: &[DataField; 2]) -> Option<&Vec<DataField>> {
        self.fetch_scoped(0, params)
    }
}

impl CacheAble<DataField, Vec<DataField>, 3> for FieldQueryCache {
    fn prepare_generation(&mut self, generation: u64) {
        if self.generation != Some(generation) {
            self.reset();
            self.generation = Some(generation);
        }
    }

    fn save_scoped(&mut self, scope_hash: u64, params: &[DataField; 3], result: Vec<DataField>) {
        if let (Some(i0), Some(i1), Some(i2)) = (
            self.try_up_idx(&params[0]),
            self.try_up_idx(&params[1]),
            self.try_up_idx(&params[2]),
        ) {
            self.cache_data.put(
                LocalCacheKey {
                    scope_hash,
                    idxs: EnumSizeIndex::Idx3(i0, i1, i2),
                },
                result,
            );
        }
    }

    fn fetch_scoped(&self, scope_hash: u64, params: &[DataField; 3]) -> Option<&Vec<DataField>> {
        if let (Some(i0), Some(i1), Some(i2)) = (
            self.get_idx(&params[0]),
            self.get_idx(&params[1]),
            self.get_idx(&params[2]),
        ) {
            return self.cache_data.peek(&LocalCacheKey {
                scope_hash,
                idxs: EnumSizeIndex::Idx3(i0, i1, i2),
            });
        }
        None
    }

    fn save(&mut self, params: &[DataField; 3], result: Vec<DataField>) {
        self.save_scoped(0, params, result);
    }

    fn fetch(&self, params: &[DataField; 3]) -> Option<&Vec<DataField>> {
        self.fetch_scoped(0, params)
    }
}

impl CacheAble<DataField, Vec<DataField>, 4> for FieldQueryCache {
    fn prepare_generation(&mut self, generation: u64) {
        if self.generation != Some(generation) {
            self.reset();
            self.generation = Some(generation);
        }
    }

    fn save_scoped(&mut self, scope_hash: u64, params: &[DataField; 4], result: Vec<DataField>) {
        if let (Some(i0), Some(i1), Some(i2), Some(i3)) = (
            self.try_up_idx(&params[0]),
            self.try_up_idx(&params[1]),
            self.try_up_idx(&params[2]),
            self.try_up_idx(&params[3]),
        ) {
            self.cache_data.put(
                LocalCacheKey {
                    scope_hash,
                    idxs: EnumSizeIndex::Idx4(i0, i1, i2, i3),
                },
                result,
            );
        }
    }

    fn fetch_scoped(&self, scope_hash: u64, params: &[DataField; 4]) -> Option<&Vec<DataField>> {
        if let (Some(i0), Some(i1), Some(i2), Some(i3)) = (
            self.get_idx(&params[0]),
            self.get_idx(&params[1]),
            self.get_idx(&params[2]),
            self.get_idx(&params[3]),
        ) {
            return self.cache_data.peek(&LocalCacheKey {
                scope_hash,
                idxs: EnumSizeIndex::Idx4(i0, i1, i2, i3),
            });
        }
        None
    }

    fn save(&mut self, params: &[DataField; 4], result: Vec<DataField>) {
        self.save_scoped(0, params, result);
    }

    fn fetch(&self, params: &[DataField; 4]) -> Option<&Vec<DataField>> {
        self.fetch_scoped(0, params)
    }
}

impl CacheAble<DataField, Vec<DataField>, 5> for FieldQueryCache {
    fn prepare_generation(&mut self, generation: u64) {
        if self.generation != Some(generation) {
            self.reset();
            self.generation = Some(generation);
        }
    }

    fn save_scoped(&mut self, scope_hash: u64, params: &[DataField; 5], result: Vec<DataField>) {
        if let (Some(i0), Some(i1), Some(i2), Some(i3), Some(i4)) = (
            self.try_up_idx(&params[0]),
            self.try_up_idx(&params[1]),
            self.try_up_idx(&params[2]),
            self.try_up_idx(&params[3]),
            self.try_up_idx(&params[4]),
        ) {
            self.cache_data.put(
                LocalCacheKey {
                    scope_hash,
                    idxs: EnumSizeIndex::Idx5(i0, i1, i2, i3, i4),
                },
                result,
            );
        }
    }

    fn fetch_scoped(&self, scope_hash: u64, params: &[DataField; 5]) -> Option<&Vec<DataField>> {
        if let (Some(i0), Some(i1), Some(i2), Some(i3), Some(i4)) = (
            self.get_idx(&params[0]),
            self.get_idx(&params[1]),
            self.get_idx(&params[2]),
            self.get_idx(&params[3]),
            self.get_idx(&params[4]),
        ) {
            return self.cache_data.peek(&LocalCacheKey {
                scope_hash,
                idxs: EnumSizeIndex::Idx5(i0, i1, i2, i3, i4),
            });
        }
        None
    }

    fn save(&mut self, params: &[DataField; 5], result: Vec<DataField>) {
        self.save_scoped(0, params, result);
    }

    fn fetch(&self, params: &[DataField; 5]) -> Option<&Vec<DataField>> {
        self.fetch_scoped(0, params)
    }
}

impl CacheAble<DataField, Vec<DataField>, 6> for FieldQueryCache {
    fn prepare_generation(&mut self, generation: u64) {
        if self.generation != Some(generation) {
            self.reset();
            self.generation = Some(generation);
        }
    }

    fn save_scoped(&mut self, scope_hash: u64, params: &[DataField; 6], result: Vec<DataField>) {
        if let (Some(i0), Some(i1), Some(i2), Some(i3), Some(i4), Some(i5)) = (
            self.try_up_idx(&params[0]),
            self.try_up_idx(&params[1]),
            self.try_up_idx(&params[2]),
            self.try_up_idx(&params[3]),
            self.try_up_idx(&params[4]),
            self.try_up_idx(&params[5]),
        ) {
            self.cache_data.put(
                LocalCacheKey {
                    scope_hash,
                    idxs: EnumSizeIndex::Idx6(i0, i1, i2, i3, i4, i5),
                },
                result,
            );
        }
    }

    fn fetch_scoped(&self, scope_hash: u64, params: &[DataField; 6]) -> Option<&Vec<DataField>> {
        if let (Some(i0), Some(i1), Some(i2), Some(i3), Some(i4), Some(i5)) = (
            self.get_idx(&params[0]),
            self.get_idx(&params[1]),
            self.get_idx(&params[2]),
            self.get_idx(&params[3]),
            self.get_idx(&params[4]),
            self.get_idx(&params[5]),
        ) {
            return self.cache_data.peek(&LocalCacheKey {
                scope_hash,
                idxs: EnumSizeIndex::Idx6(i0, i1, i2, i3, i4, i5),
            });
        }
        None
    }

    fn save(&mut self, params: &[DataField; 6], result: Vec<DataField>) {
        self.save_scoped(0, params, result);
    }

    fn fetch(&self, params: &[DataField; 6]) -> Option<&Vec<DataField>> {
        self.fetch_scoped(0, params)
    }
}
