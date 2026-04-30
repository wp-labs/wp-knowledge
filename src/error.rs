use derive_more::From;
use orion_error::{OrionError, StructError, UvsReason};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize, From, OrionError)]
pub enum Reason {
    #[orion_error(identity = "biz.not_data")]
    NotData,
    #[orion_error(transparent)]
    Uvs(UvsReason),
}

impl std::error::Error for Reason {}

pub type KnowledgeError = StructError<Reason>;
pub type KnowledgeResult<T> = Result<T, KnowledgeError>;
