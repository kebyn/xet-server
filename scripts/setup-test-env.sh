#!/bin/bash
set -e

echo "=== Xet Server Test Environment Setup ==="

# Configuration
XET_HOST="127.0.0.1"
XET_PORT="8080"
HUB_PORT="8081"
TEST_DIR="test-data"
STORAGE_DIR="${TEST_DIR}/storage"
KEY_DIR="${TEST_DIR}/keys"
PRIVATE_KEY="${KEY_DIR}/private.pem"
PUBLIC_KEY="${KEY_DIR}/public.pem"
KID="test-kid"

# Create directories
echo "Creating test directories..."
mkdir -p "${STORAGE_DIR}" "${KEY_DIR}"

# Generate Ed25519 key pair (if not already present)
echo "Generating Ed25519 key pair..."
python3 scripts/generate-ed25519-token.py \
    "${PRIVATE_KEY}" \
    --keys-only \
    --public-key "${PUBLIC_KEY}" \
    --kid "${KID}"

# Generate Ed25519 JWT token
echo "Generating Ed25519 JWT token..."
JWT_TOKEN=$(python3 scripts/generate-ed25519-token.py \
    "${PRIVATE_KEY}" 24 \
    --kid "${KID}")
echo "JWT Token: ${JWT_TOKEN}"

# Save token for later use
echo "${JWT_TOKEN}" > "${TEST_DIR}/jwt-token.txt"

# Build server
echo "Building Xet server..."
/root/.cargo/bin/cargo build --release

echo ""
echo "=== Setup Complete ==="
echo "Storage directory: ${STORAGE_DIR}"
echo "Ed25519 key pair:  ${PRIVATE_KEY} / ${PUBLIC_KEY}"
echo "Key ID (kid):      ${KID}"
echo "JWT token saved to: ${TEST_DIR}/jwt-token.txt"
echo ""
echo "To start the CAS (xet) server:"
echo "  CAS_PUBLIC_KEY_PATH=\$(pwd)/${PUBLIC_KEY} \\"
echo "  CAS_TRUSTED_KIDS=${KID} \\"
echo "  XET_HOST=${XET_HOST} \\"
echo "  XET_PORT=${XET_PORT} \\"
echo "  XET_STORAGE_BACKEND=local \\"
echo "  XET_LOCAL_PATH=./${STORAGE_DIR} \\"
echo "  ./target/release/xet-server"
echo ""
echo "To start the Hub API server:"
echo "  HUB_PRIVATE_KEY_PATH=\$(pwd)/${PRIVATE_KEY} \\"
echo "  HUB_KID=${KID} \\"
echo "  ./target/release/hub-api"
