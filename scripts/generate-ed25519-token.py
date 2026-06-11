#!/usr/bin/env python3
"""
Generate Ed25519 key pair and JWT tokens for Xet server testing.

Usage:
    # Generate key pair + token (auto-creates keys if missing):
    python3 scripts/generate-ed25519-token.py

    # Use existing key:
    python3 scripts/generate-ed25519-token.py private_key.pem

    # Custom kid and output paths:
    python3 scripts/generate-ed25519-token.py --kid test-kid \
        --private-key test-data/keys/private.pem \
        --public-key test-data/keys/public.pem

    # Just generate keys (no token):
    python3 scripts/generate-ed25519-token.py --keys-only

    # Generate token with specific hours:
    python3 scripts/generate-ed25519-token.py private_key.pem 48
"""

import sys
import os
import time
import json
import base64
import argparse

try:
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
except ImportError:
    print("Error: cryptography not installed. Run: pip3 install cryptography", file=sys.stderr)
    sys.exit(1)

def b64url_encode(data):
    """Base64url encode without padding."""
    return base64.urlsafe_b64encode(data).rstrip(b'=').decode('ascii')

def generate_keypair(private_key_path, public_key_path):
    """Generate a new Ed25519 key pair and save as PEM files."""
    private_key = Ed25519PrivateKey.generate()

    # Serialize private key as PKCS8 PEM
    private_pem = private_key.private_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PrivateFormat.PKCS8,
        encryption_algorithm=serialization.NoEncryption()
    )

    # Serialize public key as SPKI PEM
    public_pem = private_key.public_key().public_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PublicFormat.SubjectPublicKeyInfo
    )

    # Ensure parent directories exist
    for path in [private_key_path, public_key_path]:
        parent = os.path.dirname(path)
        if parent:
            os.makedirs(parent, exist_ok=True)

    with open(private_key_path, 'wb') as f:
        f.write(private_pem)
    os.chmod(private_key_path, 0o600)

    with open(public_key_path, 'wb') as f:
        f.write(public_pem)

    print("Generated key pair", file=sys.stderr)
    print(f"  Private: {private_key_path}", file=sys.stderr)
    print(f"  Public:  {public_key_path}", file=sys.stderr)

    return private_key

def load_private_key(private_key_path):
    """Load an Ed25519 private key from PEM file."""
    with open(private_key_path, 'rb') as f:
        return serialization.load_pem_private_key(f.read(), password=None)

def generate_token(private_key, kid, hours=24, repo_id="test/repo", repo_type="model", revision="main"):
    """Generate an Ed25519 JWT token with xet claims."""
    # Create claims (matching hub XetSigner format)
    now = int(time.time())
    claims = {
        "sub": "test-user",
        "scope": "read write",
        "repo_id": repo_id,
        "repo_type": repo_type,
        "revision": revision,
        "exp": now + (hours * 3600),
        "iat": now,
        "kid": kid,
        "token_type": "user"
    }

    # Create header (kid must match hub's configured kid)
    header = {
        "alg": "EdDSA",
        "typ": "JWT",
        "kid": kid
    }

    # Encode header and payload
    header_json = json.dumps(header, separators=(',', ':'))
    payload_json = json.dumps(claims, separators=(',', ':'))

    header_b64 = b64url_encode(header_json.encode('utf-8'))
    payload_b64 = b64url_encode(payload_json.encode('utf-8'))

    # Sign
    message = f"{header_b64}.{payload_b64}".encode('utf-8')
    signature = private_key.sign(message)
    sig_b64 = b64url_encode(signature)

    # Final token with "xet_" prefix
    token = f"xet_{header_b64}.{payload_b64}.{sig_b64}"
    return token

def main():
    parser = argparse.ArgumentParser(description="Generate Ed25519 key pair and JWT tokens")
    parser.add_argument('private_key', nargs='?', default='private_key.pem',
                        help='Path to private key PEM file (default: private_key.pem)')
    parser.add_argument('hours', nargs='?', type=int, default=24,
                        help='Token validity in hours (default: 24)')
    parser.add_argument('--kid', default='test-kid',
                        help='Key ID (must match CAS trusted_kids, default: test-kid)')
    parser.add_argument('--public-key', dest='public_key_path', default=None,
                        help='Path to public key PEM (default: <private_key_dir>/public_key.pem)')
    parser.add_argument('--keys-only', action='store_true',
                        help='Only generate key pair, no token')
    parser.add_argument('--repo-id', default='test/repo',
                        help='Repository ID for token claims')
    parser.add_argument('--repo-type', default='model',
                        help='Repository type for token claims (default: model)')
    parser.add_argument('--revision', default='main',
                        help='Revision for token claims (default: main)')

    args = parser.parse_args()

    # Determine paths
    private_key_path = args.private_key
    if args.public_key_path:
        public_key_path = args.public_key_path
    else:
        # Default: same directory as private key, named public_key.pem
        dirname = os.path.dirname(private_key_path) or '.'
        public_key_path = os.path.join(dirname, 'public_key.pem')

    # Generate keys if either key file is missing (orphan key = broken state)
    if not os.path.exists(private_key_path) or not os.path.exists(public_key_path):
        missing = []
        if not os.path.exists(private_key_path):
            missing.append(f"private: {private_key_path}")
        if not os.path.exists(public_key_path):
            missing.append(f"public: {public_key_path}")
        print(f"Key pair incomplete. Missing: {', '.join(missing)}", file=sys.stderr)
        print("Generating new Ed25519 key pair...", file=sys.stderr)
        private_key = generate_keypair(private_key_path, public_key_path)
    else:
        private_key = load_private_key(private_key_path)
        if not isinstance(private_key, Ed25519PrivateKey):
            print(f"Error: {private_key_path} is not an Ed25519 key", file=sys.stderr)
            sys.exit(1)

    if args.keys_only:
        return

    # Generate token
    token = generate_token(
        private_key,
        kid=args.kid,
        hours=args.hours,
        repo_id=args.repo_id,
        repo_type=args.repo_type,
        revision=args.revision
    )
    print(token)

if __name__ == "__main__":
    main()
