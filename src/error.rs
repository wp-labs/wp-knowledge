use derive_more::From;
use orion_error::{OrionError, StructError, UnifiedReason};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize, From, OrionError)]
pub enum KnowReason {
    #[orion_error(identity = "biz.not_data")]
    NotData,
    #[orion_error(transparent)]
    General(UnifiedReason),
}

impl KnowReason {
    pub fn from_conf() -> Self {
        Self::core_conf()
    }

    pub fn from_logic() -> Self {
        Self::logic_error()
    }

    pub fn from_res() -> Self {
        Self::system_error()
    }

    pub fn from_rule() -> Self {
        Self::rule_error()
    }
}

impl std::error::Error for KnowReason {}

pub type KnowledgeError = StructError<KnowReason>;
pub type KnowledgeResult<T> = Result<T, KnowledgeError>;

#[deprecated(since = "0.12.2", note = "use KnowReason instead")]
pub type Reason = KnowReason;
