use std::path::PathBuf;

fn repo_file(path: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

#[test]
fn current_docs_do_not_describe_internal_as_xet_or_wildcard() {
    let current_docs = [
        "README.md",
        "docs/api/authentication.md",
        "docs/api/cas-api.md",
        "docs/api/hub-api.md",
        "docs/architecture.md",
        "docs/configuration.md",
        "docs/superpowers/specs/2026-06-10-hf-hub-api-design.md",
    ];
    let forbidden = [
        "Authorization: Bearer xet_xxx (需要 internal token",
        "Scope \"internal\" supersedes",
        "`internal` 自动包含 `read` 和 `write`",
        "internal scope supersedes",
        "使用 `internal_xxx` token 调用 CAS batch API",
        "Hub API 使用 `internal_xxx` token 代理请求到 CAS Server",
        "Hub 再用内部 token 代理到 CAS",
    ];

    for path in current_docs {
        let text = repo_file(path);
        for phrase in forbidden {
            assert!(
                !text.contains(phrase),
                "{path} still contains outdated auth contract phrase: {phrase}"
            );
        }
    }
}

#[test]
fn docs_state_lfs_object_authorization_boundary() {
    let hub_api = repo_file("docs/api/hub-api.md");
    assert!(
        hub_api.contains("content-hash capability"),
        "docs/api/hub-api.md must name the current LFS object authorization model"
    );
    assert!(
        hub_api.contains("不校验 OID 是否属于 URL 中的 repo"),
        "docs/api/hub-api.md must document the current repo/OID boundary"
    );

    let architecture = repo_file("docs/architecture.md");
    assert!(
        architecture.contains("Authorization Boundaries"),
        "docs/architecture.md must include an authorization boundary section"
    );
    assert!(
        architecture.contains("CAS content-capability authorization"),
        "docs/architecture.md must distinguish CAS object authorization from Hub repo authorization"
    );
}

#[test]
fn docs_state_hub_lfs_proxy_to_cas_token_contract() {
    let authentication = repo_file("docs/api/authentication.md");
    assert!(
        authentication.contains("使用短期 `xet_xxx` user token 调用 CAS batch API"),
        "docs/api/authentication.md must state Hub uses xet user tokens for CAS batch"
    );
    assert!(
        authentication.contains("将同一个 `proxy_xxx` token 转发给 CAS Server"),
        "docs/api/authentication.md must state Hub forwards the validated proxy token to CAS object endpoints"
    );

    let hub_api = repo_file("docs/api/hub-api.md");
    assert!(
        hub_api.contains("使用短期 `xet_xxx` user token 调用 CAS batch API"),
        "docs/api/hub-api.md must state Hub uses xet user tokens for CAS batch"
    );
    assert!(
        hub_api.contains("将同一个 `proxy_xxx` token 转发给 CAS"),
        "docs/api/hub-api.md must state Hub forwards proxy tokens to CAS object endpoints"
    );
    assert!(
        hub_api.contains("小文件直读 CAS 时，Hub 使用短期 `xet_xxx` user token"),
        "docs/api/hub-api.md must state resolve inline CAS reads use xet user tokens"
    );

    assert!(
        authentication.contains("commit inline 上传和 resolve inline 直读"),
        "docs/api/authentication.md must document public CAS object calls from commit/resolve"
    );
}

#[test]
fn docs_limit_internal_tokens_to_internal_endpoints() {
    let configuration = repo_file("docs/configuration.md");
    assert!(
        configuration.contains("Hub→CAS internal endpoints"),
        "docs/configuration.md must scope HUB_INTERNAL_TOKEN_TTL_SECONDS to internal endpoints"
    );
    assert!(
        configuration
            .contains("不用于 CAS batch、public LFS object 或 inline resolve/commit 对象读写"),
        "docs/configuration.md must state internal tokens are not used for public CAS object calls"
    );

    let architecture = repo_file("docs/architecture.md");
    assert!(
        architecture.contains("签发 CAS user token（xet_xxx）、LFS proxy token（proxy_xxx）和 internal service token（internal_xxx）"),
        "docs/architecture.md must describe the layered token issuance model"
    );
}

#[test]
fn historical_internal_scope_plan_is_marked_superseded() {
    let cas_plan = repo_file("docs/superpowers/plans/2026-06-10-cas-modifications.md");
    assert!(
        cas_plan.contains("Superseded auth note"),
        "historical CAS modification plan must warn readers that old internal-scope examples are superseded"
    );

    let hub_plan = repo_file("docs/superpowers/plans/2026-06-10-hub-api-service.md");
    assert!(
        hub_plan.contains("Superseded auth note"),
        "historical Hub API plan must warn readers that old Hub->CAS public endpoint token examples are superseded"
    );

    let hub_spec = repo_file("docs/superpowers/specs/2026-06-10-hf-hub-api-design.md");
    assert!(
        hub_spec.contains("Current-state auth note"),
        "historical Hub API spec must summarize the current token boundary"
    );
    assert!(
        !hub_spec.contains("Requested resource must belong to token's repo_id"),
        "historical Hub API spec must not preserve obsolete repository-scoped CAS object wording without correction"
    );
}
