#!/usr/bin/env python3
"""
Generate JWT token for Xet server testing.

Usage:
    python3 scripts/generate-jwt-token.py [secret] [hours]

Example:
    python3 scripts/generate-jwt-token.py test-secret-key 24
"""

import sys
import time

try:
    import jwt
except ImportError:
    print("Error: PyJWT not installed. Run: pip3 install PyJWT", file=sys.stderr)
    sys.exit(1)

def generate_token(secret="test-secret-key", hours=24):
    """Generate a JWT token with read/write scope."""
    payload = {
        "sub": "test-user",
        "scope": "read write",
        "exp": int(time.time()) + (hours * 3600)
    }

    token = jwt.encode(payload, secret, algorithm="HS256")
    return token

if __name__ == "__main__":
    secret = sys.argv[1] if len(sys.argv) > 1 else "test-secret-key"
    hours = int(sys.argv[2]) if len(sys.argv) > 2 else 24

    token = generate_token(secret, hours)
    print(token)
