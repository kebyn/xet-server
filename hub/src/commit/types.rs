use serde::{Deserialize, Serialize};

/// Maximum size for inline file content (10MB).
pub(crate) const MAX_INLINE_SIZE: usize = 10 * 1024 * 1024;

/// NDJSON commit operations.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "key", content = "value")]
#[serde(rename_all = "camelCase")]
pub(crate) enum CommitOperation {
    Header(CommitHeader),
    File(FileOperation),
    LfsFile(LfsFileOperation),
    DeletedEntry(DeletedEntryOperation),
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct CommitHeader {
    pub(crate) summary: String,
    #[serde(default, rename = "parentRevision")]
    pub(crate) parent_revision: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct FileOperation {
    pub(crate) path: String,
    pub(crate) content: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct LfsFileOperation {
    pub(crate) path: String,
    pub(crate) oid: String,
    pub(crate) size: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct DeletedEntryOperation {
    pub(crate) path: String,
}

/// Commit response.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CommitResponse {
    #[serde(rename = "commitOid")]
    pub(crate) commit_oid: String,
    #[serde(rename = "commitUrl")]
    pub(crate) commit_url: String,
    #[serde(rename = "prUrl")]
    pub(crate) pr_url: Option<String>,
    #[serde(rename = "prNum")]
    pub(crate) pr_num: Option<u64>,
}
