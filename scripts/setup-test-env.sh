#!/bin/bash
set -e

echo "=== Xet Server Test Environment Setup ==="

# Configuration
XET_HOST="127.0.0.1"
XET_PORT="8080"
XET_JWT_SECRET="test-secret-key"
TEST_DIR="test-data"
STORAGE_DIR="${TEST_DIR}/storage"

# Create directories
echo "Creating test directories..."
mkdir -p "${STORAGE_DIR}"

# Generate JWT token
echo "Generating JWT token..."
JWT_TOKEN=$(python3 scripts/generate-jwt-token.py "${XET_JWT_SECRET}" 24)
echo "JWT Token: ${JWT_TOKEN}"

# Save token for later use
echo "${JWT_TOKEN}" > "${TEST_DIR}/jwt-token.txt"

# Build server
echo "Building Xet server..."
/root/.cargo/bin/cargo build --release

echo ""
echo "=== Setup Complete ==="
echo "Storage directory: ${STORAGE_DIR}"
echo "JWT token saved to: ${TEST_DIR}/jwt-token.txt"
echo ""
echo "To start the server:"
echo "  XET_HOST=${XET_HOST} \\"
echo "  XET_PORT=${XET_PORT} \\"
echo "  XET_STORAGE_BACKEND=local \\"
echo "  XET_LOCAL_PATH=./${STORAGE_DIR} \\"
echo "  XET_JWT_SECRET=${XET_JWT_SECRET} \\"
echo "  ./target/release/xet-server"
