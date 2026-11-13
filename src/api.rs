use crate::failure::Failure;
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct Response<T> {
    pub schema_version: u32,
    pub data: T,
}
impl<T> Response<T> {
    pub fn new(data: T) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            data,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub schema_version: u32,
    pub error: Failure,
}
impl ErrorResponse {
    pub fn new(error: Failure) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            error,
        }
    }
}
