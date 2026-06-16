use std::collections::HashMap;
use std::sync::OnceLock;

use crate::error::{KnowReason, KnowledgeResult};
use crate::loader::FunCall;
use orion_error::conversion::ToStructError;

// ---------------------------------------------------------------------------
// Global registry
// ---------------------------------------------------------------------------

static FUN_MAP: OnceLock<HashMap<String, CachedFunSpec>> = OnceLock::new();

#[derive(Debug, Clone)]
pub(crate) struct CachedFunSpec {
    call: FunCall,
    pub key: Option<String>,
    cache: bool,
    pub ttl_ms: Option<u64>,
}

impl CachedFunSpec {
    pub fn enabled(&self) -> bool {
        self.cache
    }
}

pub(crate) fn register_fun_map(map: HashMap<String, crate::loader::FunSpec>) {
    let cached: HashMap<String, CachedFunSpec> = map
        .into_iter()
        .map(|(name, spec)| {
            (
                name.clone(),
                CachedFunSpec {
                    call: spec.call,
                    key: spec.key,
                    cache: spec.cache,
                    ttl_ms: spec.ttl_ms,
                },
            )
        })
        .collect();
    FUN_MAP.set(cached).ok();
}

pub(crate) fn resolve_spec(
    service: &str,
    returns_bool: bool,
) -> KnowledgeResult<&'static CachedFunSpec> {
    resolve(service, returns_bool)
}

fn resolve(service: &str, returns_bool: bool) -> KnowledgeResult<&'static CachedFunSpec> {
    let map = FUN_MAP.get().ok_or_else(|| {
        KnowReason::from_logic()
            .to_err()
            .with_detail("external: no [fun] definitions registered")
    })?;
    let spec = map.get(service).ok_or_else(|| {
        KnowReason::from_logic()
            .to_err()
            .with_detail(format!("external service '{service}' not found"))
    })?;
    let expected = if returns_bool { "bool" } else { "value" };
    let actual = if spec.call.returns_bool() {
        "bool"
    } else {
        "value"
    };
    if expected != actual {
        return Err(KnowReason::from_logic().to_err().with_detail(format!(
            "external service '{service}' returns {actual}, not {expected}",
        )));
    }
    Ok(spec)
}

impl FunCall {
    fn returns_bool(&self) -> bool {
        matches!(self, FunCall::BfExists | FunCall::Sismember)
    }
}

// ---------------------------------------------------------------------------
// Public (crate-internal) API
// ---------------------------------------------------------------------------

pub(crate) fn external_exists(service: &str, arg: &str) -> KnowledgeResult<bool> {
    let spec = resolve(service, true)?;
    let redis_key = spec.key.as_deref().unwrap_or(arg);
    match spec.call {
        FunCall::BfExists => crate::redis::bf_exists("knowdb", redis_key, arg),
        FunCall::Sismember => crate::redis::set_exists("knowdb", redis_key, arg),
        _ => unreachable!(),
    }
}

pub(crate) fn external_value(service: &str, arg: &str) -> KnowledgeResult<Option<String>> {
    let spec = resolve(service, false)?;
    let redis_key = spec.key.as_deref().unwrap_or(arg);
    match spec.call {
        FunCall::Hget => crate::redis::hget("knowdb", redis_key, arg),
        FunCall::Get => crate::redis::get("knowdb", redis_key),
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::FunSpec;

    fn register_test_fun() {
        let mut map = HashMap::new();
        map.insert(
            "test_bf".to_string(),
            FunSpec {
                call: FunCall::BfExists,
                key: Some("bf_key".to_string()),
                cache: true,
                ttl_ms: None,
            },
        );
        map.insert(
            "test_set".to_string(),
            FunSpec {
                call: FunCall::Sismember,
                key: Some("set_key".to_string()),
                cache: true,
                ttl_ms: None,
            },
        );
        map.insert(
            "test_hash".to_string(),
            FunSpec {
                call: FunCall::Hget,
                key: Some("hash_key".to_string()),
                cache: true,
                ttl_ms: None,
            },
        );
        map.insert(
            "test_kv".to_string(),
            FunSpec {
                call: FunCall::Get,
                key: Some("kv_key".to_string()),
                cache: true,
                ttl_ms: None,
            },
        );
        map.insert(
            "test_no_key".to_string(),
            FunSpec {
                call: FunCall::Get,
                key: None,
                cache: true,
                ttl_ms: None,
            },
        );
        register_fun_map(map);
    }

    #[test]
    fn resolve_service_not_found() {
        register_test_fun();
        let err = resolve("nonexistent", true).expect_err("should fail");
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn resolve_type_mismatch() {
        register_test_fun();
        // test_hash returns value, but we ask for bool
        let err = resolve("test_hash", true).expect_err("should fail");
        assert!(err.to_string().contains("returns value"));
    }

    #[test]
    fn resolve_bool_services() {
        register_test_fun();
        let spec = resolve("test_bf", true).expect("test_bf");
        assert_eq!(spec.call, FunCall::BfExists);
        assert!(spec.call.returns_bool());

        let spec = resolve("test_set", true).expect("test_set");
        assert_eq!(spec.call, FunCall::Sismember);
        assert!(spec.call.returns_bool());
    }

    #[test]
    fn resolve_value_services() {
        register_test_fun();
        let spec = resolve("test_hash", false).expect("test_hash");
        assert_eq!(spec.call, FunCall::Hget);
        assert!(!spec.call.returns_bool());

        let spec = resolve("test_kv", false).expect("test_kv");
        assert_eq!(spec.call, FunCall::Get);
        assert!(!spec.call.returns_bool());
    }

    #[test]
    fn no_key_uses_arg() {
        register_test_fun();
        let spec = resolve("test_no_key", false).expect("test_no_key");
        assert!(spec.key.is_none());
    }

    // -----------------------------------------------------------------------
    // external_exists / external_value error paths (CI-safe, no Redis)
    // -----------------------------------------------------------------------

    #[test]
    fn external_exists_service_not_found() {
        register_test_fun();
        let err = external_exists("nonexistent", "arg").expect_err("should fail");
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn external_exists_type_mismatch() {
        register_test_fun();
        // test_hash returns value, calling external_exists should fail
        let err = external_exists("test_hash", "arg").expect_err("should fail");
        assert!(err.to_string().contains("returns value"));
    }

    #[test]
    fn external_value_service_not_found() {
        register_test_fun();
        let err = external_value("nonexistent", "arg").expect_err("should fail");
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn external_value_type_mismatch() {
        register_test_fun();
        // test_bf returns bool, calling external_value should fail
        let err = external_value("test_bf", "arg").expect_err("should fail");
        assert!(err.to_string().contains("returns bool"));
    }
}
