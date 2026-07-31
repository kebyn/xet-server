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

#[test]
fn docs_pin_commit_snapshot_and_blob_size_contracts() {
    let hub_api = repo_file("docs/api/hub-api.md");
    assert!(
        hub_api.contains("且不带 `sha256:` 前缀"),
        "Commit API docs must describe the exact LFS OID wire format"
    );
    assert!(
        !hub_api.contains("\"oid\":\"sha256:"),
        "Commit API JSON examples must not use Git LFS pointer syntax"
    );
    assert!(
        hub_api.contains("第一个非空 operation") && hub_api.contains("必须恰好出现一次"),
        "Commit API docs must require exactly one leading header"
    );
    assert!(
        hub_api.contains("按请求中的原始顺序执行") && hub_api.contains("完整文件树快照"),
        "Commit API docs must pin operation ordering and snapshot semantics"
    );
    assert!(
        hub_api.contains("X-Blob-Size") && hub_api.contains("422 Unprocessable Entity"),
        "Commit API docs must describe CAS size verification"
    );

    let cas_api = repo_file("docs/api/cas-api.md");
    assert!(
        cas_api.contains("必需的 `X-Blob-Size: <u64>`")
            && cas_api.contains("raw_only")
            && cas_api.contains("xet_only"),
        "CAS HEAD docs must describe verified raw and reconstructed sizes"
    );

    let integration_guide = repo_file("HF_XET_INTEGRATION_GUIDE.md");
    assert!(
        !integration_guide.contains("\"oid\":\"sha256:"),
        "integration examples must use the Commit API OID format"
    );
}

#[test]
fn docs_describe_hub_module_boundaries_and_schema_recovery() {
    let architecture = repo_file("docs/architecture.md");
    for module in [
        "services/",
        "commit/",
        "lfs_proxy/",
        "migrations.rs",
        "sqlite_pool.rs",
    ] {
        assert!(
            architecture.contains(module),
            "architecture docs must include current Hub module: {module}"
        );
    }

    let configuration = repo_file("docs/configuration.md");
    for phrase in [
        "停止所有 Hub 实例",
        "HUB_SQLITE_PATH",
        "-wal",
        "-shm",
        "CAS 对象无需重新上传",
        "不要静默删除",
    ] {
        assert!(
            configuration.contains(phrase),
            "configuration docs must preserve schema recovery step: {phrase}"
        );
    }
}

#[test]
fn docs_describe_real_keyring_rotation_and_legacy_mode() {
    let configuration = repo_file("docs/configuration.md");
    for phrase in [
        "CAS_PUBLIC_KEYS=kid1=/path/old-public.pem,kid2=/path/new-public.pem",
        "keyring 的 allowlist",
        "单公钥兼容入口",
        "必须与 signing kid 的映射完全匹配",
        "等待所有旧 token 的最大 TTL 过期",
    ] {
        assert!(
            configuration.contains(phrase),
            "configuration docs must preserve keyring guarantee: {phrase}"
        );
    }

    let authentication = repo_file("docs/api/authentication.md");
    assert!(
        authentication.contains("JWT header")
            && authentication.contains("映射的公钥验签")
            && authentication.contains("不提供真正的多公钥轮换")
            && authentication.contains("CAS_SIGNING_KID=new-key"),
        "authentication docs must distinguish exact key selection from legacy compatibility"
    );
}

#[test]
fn docs_pin_resource_config_and_error_boundaries() {
    let configuration = repo_file("docs/configuration.md");
    assert!(
        configuration.contains("只接受 `true`/`false`/`1`/`0`")
            && configuration.contains("必须是带有效 host 的 HTTP(S) URL")
            && configuration
                .contains("HUB_INLINE_THRESHOLD <= HUB_MAX_UPLOAD_SIZE <= HUB_MAX_DOWNLOAD_SIZE"),
        "configuration docs must describe fail-fast parsing and cross-field validation"
    );
    assert!(
        configuration.contains("唯一 upload ID")
            && configuration.contains("lifecycle rule 是不可省略的最终兜底"),
        "S3 docs must describe cancellation cleanup and lifecycle fallback"
    );
    assert!(
        !configuration.contains("转换过程会加载整个文件到内存"),
        "conversion docs must not describe the removed whole-file buffering"
    );

    let cas_api = repo_file("docs/api/cas-api.md");
    assert!(
        cas_api.contains("64 KiB 哈希缓冲区") && cas_api.contains("最多 10 个 shard 为一批"),
        "CAS docs must describe bounded shard parsing and rebuild concurrency"
    );
    assert!(
        cas_api.contains("{\"error\":\"Internal server error\"}"),
        "CAS docs must pin the sanitized 500 response"
    );

    let hub_api = repo_file("docs/api/hub-api.md");
    assert!(
        hub_api.contains("{\"error\":\"Internal server error\",\"error_type\":\"InternalError\"}")
            && hub_api.contains(
                "{\"error\":\"Upstream CAS request failed\",\"error_type\":\"BadGateway\"}"
            ),
        "Hub docs must pin sanitized 500 and 502 responses"
    );
}
