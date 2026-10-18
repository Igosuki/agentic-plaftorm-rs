use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct RequestTags {
    pub team: String,
    pub application: String,
    pub use_case: String,
}
