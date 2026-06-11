#!/bin/bash
set -e

echo "=== Xet Server Upload Test ==="

# Configuration
TEST_DIR="test-data"
REPO_DIR="${TEST_DIR}/test-repo"
STORAGE_DIR="${TEST_DIR}/storage"
XET_PORT="8080"

cd "${REPO_DIR}"

# Check if test files exist
if [ ! -f "test-model.bin" ] && [ ! -f "config.json" ]; then
    echo "Error: No test files found in ${REPO_DIR}"
    echo "Please copy test files (e.g., Qwen 8B subset) to ${REPO_DIR}"
    echo ""
    echo "Or create a small test file:"
    echo "  dd if=/dev/urandom of=${REPO_DIR}/test-model.bin bs=1M count=5"
    exit 1
fi

# Record original file hashes
echo "Recording original file hashes..."
find . -type f \( -name "*.safetensors" -o -name "*.bin" -o -name "config.json" \) \
    -exec sha256sum {} \; > ../original-hashes.txt
cat ../original-hashes.txt

# Add files to Git
echo ""
echo "Adding files to Git..."
git add *.safetensors *.bin config.json 2>/dev/null || true
git commit -m "Add model files" || echo "No changes to commit"

# Push to trigger upload
echo ""
echo "Pushing to trigger upload..."
git push -u origin main || git push -u origin master

# Verify upload on server
echo ""
echo "=== Verifying Upload ==="
echo "Storage contents:"
find "../../${STORAGE_DIR}" -type f | head -20

echo ""
echo "Upload metrics:"
curl -s http://127.0.0.1:${XET_PORT}/metrics | grep -E "upload|requests_total" || echo "No metrics found"

echo ""
echo "=== Upload Test Complete ==="
echo ""
echo "Next steps:"
echo "  1. Verify files were uploaded to storage"
echo "  2. Run: ./scripts/run-download-test.sh"
