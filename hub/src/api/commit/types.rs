use serde::{Deserialize, Serialize};

/// Maximum size for inline file content (10MB).
pub(super) const MAX_INLINE_SIZE: usize = 10 * 1024 * 1024;

/// NDJSON commit operations.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "key", content = "value")]
#[serde(rename_all = "camelCase")]
pub enum CommitOperation {
    Header(CommitHeader),
    File(FileOperation),
    LfsFile(LfsFileOperation),
    DeletedEntry(DeletedEntryOperation),
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CommitHeader {
    pub summary: String,
    #[serde(default, rename = "parentRevision")]
    pub parent_revision: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct FileOperation {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct LfsFileOperation {
    pub path: String,
    pub oid: String,
    pub size: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct DeletedEntryOperation {
    pub path: String,
}

/// Commit response.
#[derive(Debug, Serialize, Deserialize)]
pub struct CommitResponse {
    #[serde(rename = "commitOid")]
    pub commit_oid: String,
    #[serde(rename = "commitUrl")]
    pub commit_url: String,
    #[serde(rename = "prUrl")]
    pub pr_url: Option<String>,
    #[serde(rename = "prNum")]
    pub pr_num: Option<u64>,
}
