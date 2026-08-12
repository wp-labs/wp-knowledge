use std::collections::HashMap;
use std::net::IpAddr;
use std::num::NonZeroUsize;

use lru::LruCache;
use wp_model_core::model::{DataField, FValueStr, Value};

/// 本地缓存去重索引表上限：触顶后整体重置，防止运行期无界增长。
const MAX_LOCAL_IDX: usize = 100_000;

#[derive(Debug, Clone)]
pub struct FieldQueryCache {
    str_idx: HashMap<FValueStr, usize>,
    i64_idx: HashMap<i64, usize>,
    ip_idx: HashMap<IpAddr, usize>,
    cache_data: LruCache<LocalCacheKey, Vec<DataField>>,
    idx_num: usize,
    /// `scope -> generation`：本地缓存按 scope（provider 粒度）跟踪代际，
    /// 避免多 provider 切换时整缓存重置。
    generations: HashMap<u64, u64>,
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
            generations: HashMap::new(),
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
        if self.idx_num >= MAX_LOCAL_IDX {
            // 索引表触顶：整体重置，防止无界增长（代价是清空一次本地缓存）
            self.str_idx.clear();
            self.i64_idx.clear();
            self.ip_idx.clear();
            self.cache_data.clear();
            self.generations.clear();
            self.idx_num = 0;
        }
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

    /// 仅当 scope 的 generation 变化时，清理该 scope 的缓存条目；
    /// 其它 provider 的本地缓存保留。
    fn prepare_generation_scoped(&mut self, scope: u64, generation: u64) {
        if self.generations.get(&scope) == Some(&generation) {
            return;
        }
        self.generations.insert(scope, generation);
        let stale: Vec<LocalCacheKey> = self
            .cache_data
            .iter()
            .filter(|(key, _)| key.scope_hash == scope)
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            self.cache_data.pop(&key);
        }
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
    /// 通知缓存某 scope 进入新 generation（通常因 provider 重载）。实现可只清理该 scope。
    fn prepare_generation(&mut self, _scope: u64, _generation: u64) {}
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
    fn prepare_generation(&mut self, scope: u64, generation: u64) {
        self.prepare_generation_scoped(scope, generation);
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
    fn prepare_generation(&mut self, scope: u64, generation: u64) {
        self.prepare_generation_scoped(scope, generation);
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
    fn prepare_generation(&mut self, scope: u64, generation: u64) {
        self.prepare_generation_scoped(scope, generation);
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
    fn prepare_generation(&mut self, scope: u64, generation: u64) {
        self.prepare_generation_scoped(scope, generation);
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
    fn prepare_generation(&mut self, scope: u64, generation: u64) {
        self.prepare_generation_scoped(scope, generation);
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
    fn prepare_generation(&mut self, scope: u64, generation: u64) {
        self.prepare_generation_scoped(scope, generation);
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
