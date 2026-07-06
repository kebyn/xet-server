use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signer, SigningKey};
use hub_api::auth::xet_signer::XetSigner;
use rand::rngs::OsRng;
use std::time::{SystemTime, UNIX_EPOCH};
use xet_auth_types::{TokenKind, TokenWireError, verify_token};
use xet_server::api::auth::{
    AuthError, AuthVerifier, KeyPair, XetClaims, sign_internal_token, sign_proxy_claims_token,
    sign_xet_token, verify_xet_token,
};
use xet_server::config::AuthConfig;

fn verifier_for_signing_key(
    signing_key: &SigningKey,
    kid: &str,
) -> (AuthVerifier, tempfile::TempDir) {
    let temp_dir = tempfile::tempdir().unwrap();
    let public_key_pem = KeyPair::public_key_to_pem(&signing_key.verifying_key()).unwrap();
    let public_key_path = temp_dir.path().join("public.pem");
    std::fs::write(&public_key_path, public_key_pem).unwrap();

    let verifier = AuthVerifier::from_config(&AuthConfig {
        public_key_path: public_key_path.to_str().unwrap().to_string(),
        trusted_kids: vec![kid.to_string()],
        private_key_path: None,
        signing_kid: None,
    })
    .unwrap();

    (verifier, temp_dir)
}

fn keypair_from_signing_key(signing_key: &SigningKey) -> KeyPair {
    use ed25519_dalek::pkcs8::EncodePrivateKey;

    let pem = signing_key.to_pkcs8_pem(pkcs8::LineEnding::LF).unwrap();
    KeyPair::private_key_from_pem(pem.as_str()).unwrap()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn claims(kind: TokenKind, kid: &str, scope: &str) -> XetClaims {
    let now = now_secs();
    XetClaims {
        sub: match kind {
            TokenKind::Internal => "hub-service".to_string(),
            _ => "alice".to_string(),
        },
        scope: scope.to_string(),
        repo_id: "alice/model".to_string(),
        repo_type: "model".to_string(),
        revision: "main".to_string(),
        exp: now + 3600,
        iat: now,
        kid: kid.to_string(),
        token_type: kind.token_type().to_string(),
        oid: (kind == TokenKind::Proxy).then(|| "a".repeat(64)),
        operation: (kind == TokenKind::Proxy).then(|| "download".to_string()),
    }
}

fn sign_raw_token(
    signing_key: &SigningKey,
    claims: &XetClaims,
    prefix: &str,
    alg: &str,
    typ: &str,
    header_kid: &str,
) -> String {
    let header = serde_json::json!({
        "alg": alg,
        "typ": typ,
        "kid": header_kid,
    });
    let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
    let claims_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());
    let input = format!("{}.{}", header_b64, claims_b64);
    let signature = signing_key.sign(input.as_bytes());

    format!(
        "{}{}.{}",
        prefix,
        input,
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )
}

#[test]
fn hub_signed_user_proxy_and_internal_tokens_verify_in_cas() {
    let signing_key = SigningKey::generate(&mut OsRng);
    let kid = "shared-kid";
    let signer = XetSigner::new_with_internal_ttl(signing_key.clone(), kid, 3600, 300, 3600);
    let (verifier, _temp_dir) = verifier_for_signing_key(&signing_key, kid);

    let (user_token, _) = signer
        .sign("alice", "read", "alice/model", "model", "main")
        .unwrap();
    let user_claims = verifier.verify_token(&user_token).unwrap();
    assert_eq!(user_claims.token_type, "user");
    assert_eq!(user_claims.sub, "alice");

    let oid = "a".repeat(64);
    let (proxy_token, _) = signer
        .sign_proxy("alice", &oid, "download", "alice/model", "model")
        .unwrap();
    let proxy_claims = verifier.verify_token(&proxy_token).unwrap();
    assert_eq!(proxy_claims.token_type, "proxy");
    assert_eq!(proxy_claims.oid.as_deref(), Some(oid.as_str()));
    assert_eq!(proxy_claims.operation.as_deref(), Some("download"));

    let (internal_token, _) = signer.sign_internal().unwrap();
    let internal_claims = verifier.verify_token(&internal_token).unwrap();
    assert_eq!(internal_claims.token_type, "internal");
    assert_eq!(internal_claims.scope, "internal");
}

#[test]
fn cas_signed_user_proxy_and_internal_tokens_verify_in_hub() {
    let signing_key = SigningKey::generate(&mut OsRng);
    let kid = "shared-kid";
    let keypair = keypair_from_signing_key(&signing_key);
    let signer = XetSigner::new_with_internal_ttl(signing_key, kid, 3600, 300, 3600);

    let user_token = sign_xet_token(&claims(TokenKind::User, kid, "read"), &keypair).unwrap();
    let user_claims = signer.verify_xet_token(&user_token).unwrap();
    assert_eq!(user_claims.token_type, "user");
    assert_eq!(user_claims.sub, "alice");

    let proxy_claims = claims(TokenKind::Proxy, kid, "lfs-download");
    let proxy_token = sign_proxy_claims_token(&proxy_claims, &keypair).unwrap();
    let verified_proxy = signer.verify_proxy_token(&proxy_token).unwrap();
    assert_eq!(verified_proxy.token_type, "proxy");
    assert_eq!(verified_proxy.scope, "lfs-download");
    assert_eq!(verified_proxy.oid, proxy_claims.oid);

    let internal_token =
        sign_internal_token(&claims(TokenKind::Internal, kid, "internal"), &keypair).unwrap();
    let internal_claims = signer.verify_internal_token(&internal_token).unwrap();
    assert_eq!(internal_claims.token_type, "internal");
    assert_eq!(internal_claims.scope, "internal");
}

#[test]
fn token_typ_header_mismatch_is_rejected_across_hub_and_cas() {
    let signing_key = SigningKey::generate(&mut OsRng);
    let kid = "shared-kid";
    let signer = XetSigner::new(signing_key.clone(), kid, 3600, 300);
    let claims = claims(TokenKind::User, kid, "read");
    let token = sign_raw_token(&signing_key, &claims, "xet_", "EdDSA", "not-jwt", kid);

    assert_eq!(
        verify_token(&token, &signing_key.verifying_key(), kid, TokenKind::User),
        Err(TokenWireError::InvalidToken)
    );
    assert_eq!(
        verify_xet_token(&token, &signing_key.verifying_key(), kid),
        Err(AuthError::InvalidToken)
    );
    assert!(signer.verify_xet_token(&token).is_none());
}

#[test]
fn token_prefix_and_claim_type_mismatch_is_rejected_across_hub_and_cas() {
    let signing_key = SigningKey::generate(&mut OsRng);
    let kid = "shared-kid";
    let signer = XetSigner::new(signing_key.clone(), kid, 3600, 300);

    let proxy_claims = claims(TokenKind::Proxy, kid, "lfs-download");
    let proxy_claims_with_xet_prefix =
        sign_raw_token(&signing_key, &proxy_claims, "xet_", "EdDSA", "JWT", kid);
    assert_eq!(
        verify_xet_token(
            &proxy_claims_with_xet_prefix,
            &signing_key.verifying_key(),
            kid
        ),
        Err(AuthError::InvalidToken)
    );
    assert!(
        signer
            .verify_xet_token(&proxy_claims_with_xet_prefix)
            .is_none()
    );

    let user_claims = claims(TokenKind::User, kid, "read");
    let user_claims_with_proxy_prefix =
        sign_raw_token(&signing_key, &user_claims, "proxy_", "EdDSA", "JWT", kid);
    assert_eq!(
        verify_xet_token(
            &user_claims_with_proxy_prefix,
            &signing_key.verifying_key(),
            kid
        ),
        Err(AuthError::InvalidToken)
    );
    assert!(
        signer
            .verify_proxy_token(&user_claims_with_proxy_prefix)
            .is_none()
    );

    let user_claims_with_internal_prefix =
        sign_raw_token(&signing_key, &user_claims, "internal_", "EdDSA", "JWT", kid);
    assert_eq!(
        verify_xet_token(
            &user_claims_with_internal_prefix,
            &signing_key.verifying_key(),
            kid
        ),
        Err(AuthError::InvalidToken)
    );
    assert!(
        signer
            .verify_internal_token(&user_claims_with_internal_prefix)
            .is_none()
    );
}

#[test]
fn proxy_tokens_with_non_lfs_scopes_are_rejected_across_hub_and_cas() {
    let signing_key = SigningKey::generate(&mut OsRng);
    let kid = "shared-kid";
    let keypair = keypair_from_signing_key(&signing_key);
    let signer = XetSigner::new(signing_key.clone(), kid, 3600, 300);

    for invalid_scope in ["read", "read lfs-download"] {
        let proxy_claims = claims(TokenKind::Proxy, kid, invalid_scope);
        let proxy_token = sign_proxy_claims_token(&proxy_claims, &keypair).unwrap();

        assert_eq!(
            verify_token(
                &proxy_token,
                &signing_key.verifying_key(),
                kid,
                TokenKind::Proxy
            ),
            Err(TokenWireError::InvalidToken)
        );
        assert_eq!(
            verify_xet_token(&proxy_token, &signing_key.verifying_key(), kid),
            Err(AuthError::InvalidToken)
        );
        assert!(signer.verify_proxy_token(&proxy_token).is_none());
    }
}
