#!/bin/bash
set -e

echo "=== Xet Server Download Test ==="

# Configuration
TEST_DIR="test-data"
REPO_DIR="${TEST_DIR}/test-repo"
BARE_REPO_DIR="${TEST_DIR}/test-repo.git"
CLONE_DIR="${TEST_DIR}/test-repo-clone"
XET_PORT="8080"

# Clean up previous clone if exists
rm -rf "${CLONE_DIR}"

# Clone repository to new location
echo "Cloning repository to new location..."
cd "${TEST_DIR}"
git clone test-repo.git test-repo-clone
cd test-repo-clone

# Configure LFS for the cloned repo
JWT_TOKEN=$(cat ../jwt-token.txt)
cat > .lfsconfig << EOF
[lfs]
    url = http://127.0.0.1:8080
EOF

cat > git-credential-xet << EOF
#!/bin/bash
case "\$1" in
  get)
    echo "username=xet-user"
    echo "password=${JWT_TOKEN}"
    ;;
esac
EOF
chmod +x git-credential-xet
git config credential.helper "$(pwd)/git-credential-xet"
git config lfs.access basic
git config lfs.url http://127.0.0.1:8080

# Verify files are downloaded
echo ""
echo "=== Verifying Download ==="
echo "Downloaded files:"
ls -lh

# Verify data integrity
echo ""
echo "Verifying data integrity..."
find . -type f \( -name "*.safetensors" -o -name "*.bin" -o -name "config.json" \) \
    -exec sha256sum {} \; > ../downloaded-hashes.txt

echo "Comparing hashes..."
# Sort both files for comparison
sort ../original-hashes.txt > ../original-hashes-sorted.txt
sort ../downloaded-hashes.txt > ../downloaded-hashes-sorted.txt

# Remove directory prefix for comparison
sed 's|\./||g' ../original-hashes-sorted.txt > ../original-hashes-clean.txt
sed 's|\./||g' ../downloaded-hashes-sorted.txt > ../downloaded-hashes-clean.txt

if diff -q ../original-hashes-clean.txt ../downloaded-hashes-clean.txt > /dev/null; then
    echo "✅ Data integrity verified: All hashes match!"
else
    echo "❌ Data integrity check failed: Hashes do not match"
    echo ""
    echo "Differences:"
    diff ../original-hashes-clean.txt ../downloaded-hashes-clean.txt
    exit 1
fi

# Check download metrics
echo ""
echo "Download metrics:"
curl -s http://127.0.0.1:${XET_PORT}/metrics | grep -E "download|requests_total" || echo "No metrics found"

echo ""
echo "=== Download Test Complete ==="
echo ""
echo "Next steps:"
echo "  1. Run: ./scripts/validate-metrics.sh"
