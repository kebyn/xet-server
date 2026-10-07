/// Generate the three per-repo-type handler wrappers around one shared impl.
///
/// Expands to `pub async fn $model`, `pub async fn $dataset`, and
/// `pub async fn $space` with the parameter list `($($params)*)` re-emitted
/// verbatim (actix requires concrete fn items with extractor parameters),
/// each delegating to `$call(crate::metadata::RepoType::<variant>, $($args)*)`.
/// The shared impl fn must therefore take `repo_type: RepoType` as its
/// FIRST parameter.
macro_rules! repo_type_handlers {
    (
        $(#[$doc:meta])*
        [$model:ident, $dataset:ident, $space:ident]
        ($($params:tt)*) -> $ret:ty
        = $call:ident($($args:tt)*)
    ) => {
        $(#[$doc])*
        pub async fn $model($($params)*) -> $ret {
            $call(crate::metadata::RepoType::Model, $($args)*).await
        }

        $(#[$doc])*
        pub async fn $dataset($($params)*) -> $ret {
            $call(crate::metadata::RepoType::Dataset, $($args)*).await
        }

        $(#[$doc])*
        pub async fn $space($($params)*) -> $ret {
            $call(crate::metadata::RepoType::Space, $($args)*).await
        }
    };
}

pub mod commit;
pub mod lfs_proxy;
pub mod preupload;
pub mod repo;
pub mod resolve;
pub mod token_exchange;
pub mod tree;
pub mod whoami;

pub use commit::{commit_dataset, commit_model, commit_space};
pub use lfs_proxy::{lfs_batch, lfs_download, lfs_upload};
pub use preupload::{preupload_dataset, preupload_model, preupload_space};
pub use repo::{
    create_dataset, create_model, create_space, delete_repo_dataset, delete_repo_model,
    delete_repo_space, get_repo_dataset, get_repo_model, get_repo_space,
};
pub use resolve::{resolve_dataset, resolve_model, resolve_space};
pub use token_exchange::{
    exchange_dataset_read, exchange_dataset_write, exchange_model_read, exchange_model_write,
    exchange_space_read, exchange_space_write,
};
pub use tree::{tree_dataset, tree_model, tree_space};
pub use whoami::whoami;

/// Uniform JSON error body for Hub API responses.
///
/// Shared by every api handler module; previously this helper was defined
/// identically in seven places. `error_type` values follow the HuggingFace
/// Hub convention (e.g. `AuthorizationError` for 403).
pub(crate) fn error_json(error: String, error_type: &str) -> serde_json::Value {
    serde_json::json!({
        "error": error,
        "error_type": error_type
    })
}
