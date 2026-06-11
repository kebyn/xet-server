#!/usr/bin/env python3
"""
Generate Ed25519 JWT token for Xet server testing.

Usage:
    python3 scripts/generate-ed25519-token.py [private_key_pem] [hours]

Example:
    python3 scripts/generate-ed25519-token.py test-data/xet-private.pem 24
"""

import sys
import time
import json
import base64

try:
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
except ImportError:
    print("Error: cryptography not installed. Run: pip3 install cryptography", file=sys.stderr)
    sys.exit(1)

def b64url_encode(data):
    """Base64url encode without padding."""
    return base64.urlsafe_b64encode(data).rstrip(b'=').decode('ascii')

def generate_token(private_key_path, hours=24):
    """Generate an Ed25519 JWT token with xet claims."""
    # Load private key
    with open(private_key_path, 'rb') as f:
        private_key = serialization.load_pem_private_key(f.read(), password=None)

    # Get public key for kid
    public_key = private_key.public_key()
    public_bytes = public_key.public_bytes(
        encoding=serialization.Encoding.Raw,
        format=serialization.PublicFormat.Raw
    )
    kid = public_bytes[:8].hex()

    # Create claims
    now = int(time.time())
    claims = {
        "sub": "test-user",
        "scope": "read write",
        "repo_id": "test/repo",
        "repo_type": "model",
        "revision": "main",
        "exp": now + (hours * 3600),
        "iat": now,
        "kid": kid
    }

    # Create header
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

if __name__ == "__main__":
    private_key_path = sys.argv[1] if len(sys.argv) > 1 else "test-data/xet-private.pem"
    hours = int(sys.argv[2]) if len(sys.argv) > 2 else 24

    token = generate_token(private_key_path, hours)
    print(token)
