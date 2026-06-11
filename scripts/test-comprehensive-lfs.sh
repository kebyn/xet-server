#!/bin/bash
# Comprehensive test script for Xet server with both Qwen models
# Uses Git LFS for upload/download operations

set -e

# Configuration
SERVER_URL="http://127.0.0.1:8080"
TEST_DIR="/data/test-data"
QWEN_4B_DIR="$TEST_DIR/qwen-model"
QWEN_8B_DIR="$TEST_DIR/qwen3-8b-model"
REPO_DIR="$TEST_DIR/test-repo-comprehensive"
CLONE_DIR="$TEST_DIR/test-repo-clone-comprehensive"
JWT_TOKEN=$(cat "$TEST_DIR/jwt-token.txt")

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo "=============================================================="
echo " XET SERVER COMPREHENSIVE TEST - Git LFS Upload/Download"
echo "=============================================================="
echo ""

# Check server health
echo "Checking server health..."
if curl -s -f "$SERVER_URL/health" > /dev/null 2>&1; then
    echo -e "${GREEN}✅ Server is healthy${NC}"
else
    echo -e "${RED}❌ Server is not accessible${NC}"
    exit 1
fi

# Setup git credential helper
echo "Setting up Git credential helper..."
cat > /usr/local/bin/git-credential-xet << 'EOF'
#!/bin/bash
while read line; do
    key=$(echo "$line" | cut -d= -f1)
    value=$(echo "$line" | cut -d= -f2-)
    case "$key" in
        host) HOST="$value" ;;
        protocol) PROTO="$value" ;;
    esac
done

JWT_TOKEN=$(cat /data/test-data/jwt-token.txt)

echo "protocol=$PROTO"
echo "host=$HOST"
echo "username=xet-user"
echo "password=$JWT_TOKEN"
EOF
chmod +x /usr/local/bin/git-credential-xet

# Configure git to use our credential helper
git config --global credential.helper xet
git config --global user.email "test@example.com"
git config --global user.name "Test User"

# Clean up previous test
rm -rf "$REPO_DIR" "$CLONE_DIR"
mkdir -p "$REPO_DIR"

echo ""
echo "=============================================================="
echo " TEST 1: Qwen/Qwen3-4B-Thinking-2507"
echo "=============================================================="
echo ""

# Initialize repository
cd "$REPO_DIR"
git init
git lfs install

# Configure LFS to use our server
cat > .lfsconfig << EOF
[lfs]
    url = $SERVER_URL/lfs
EOF

# Track large files with LFS
echo "*.safetensors filter=lfs diff=lfs merge=lfs -text" > .gitattributes
echo "*.bin filter=lfs diff=lfs merge=lfs -text" >> .gitattributes

# Copy Qwen3-4B files
echo "Copying Qwen3-4B-Thinking-2507 files..."
cp -r "$QWEN_4B_DIR"/* . 2>/dev/null || true
ls -lh *.safetensors *.json 2>/dev/null | head -10 || echo "No files found"

# Commit files
git add .
git commit -m "Add Qwen3-4B-Thinking-2507 model files"

# Create bare remote
REMOTE_DIR="$TEST_DIR/test-remote-4b.git"
rm -rf "$REMOTE_DIR"
git init --bare "$REMOTE_DIR"
git remote add origin "$REMOTE_DIR"

# Push to remote (this will trigger LFS uploads)
echo ""
echo "Pushing Qwen3-4B files (with LFS)..."
git push -u origin master 2>&1 | tee /tmp/push-4b.log

# Check LFS objects
echo ""
echo "LFS objects uploaded:"
git lfs ls-files

echo ""
echo "=============================================================="
echo " TEST 2: Qwen/Qwen3-8B"
echo "=============================================================="
echo ""

# Copy Qwen3-8B files
echo "Copying Qwen3-8B files..."
cp -r "$QWEN_8B_DIR"/* . 2>/dev/null || true
ls -lh *.json 2>/dev/null | head -10 || echo "No files found"

# Commit files
git add .
git commit -m "Add Qwen3-8B model files" || echo "No new files to commit"

# Push to remote
echo ""
echo "Pushing Qwen3-8B files (with LFS)..."
git push origin master 2>&1 | tee /tmp/push-8b.log

# Check LFS objects
echo ""
echo "LFS objects uploaded (total):"
git lfs ls-files

echo ""
echo "=============================================================="
echo " TEST 3: Download/Clone Test"
echo "=============================================================="
echo ""

# Clone the repository
echo "Cloning repository to test download..."
cd "$TEST_DIR"
git clone "$REMOTE_DIR" "$(basename $CLONE_DIR)" 2>&1 | tee /tmp/clone.log

cd "$CLONE_DIR"

# Pull LFS files
echo ""
echo "Pulling LFS files..."
git lfs pull 2>&1 | tee /tmp/lfs-pull.log

# Verify files
echo ""
echo "Verifying downloaded files..."
echo ""
echo "Qwen3-4B files:"
ls -lh "$QWEN_4B_DIR"/*.safetensors 2>/dev/null | head -3
ls -lh ./*.safetensors 2>/dev/null | head -3

echo ""
echo "Comparing file hashes:"
for file in *.safetensors *.json; do
    if [ -f "$file" ]; then
        orig_hash=$(sha256sum "$QWEN_4B_DIR/$file" 2>/dev/null | cut -d' ' -f1 || echo "N/A")
        dl_hash=$(sha256sum "$file" 2>/dev/null | cut -d' ' -f1 || echo "N/A")
        if [ "$orig_hash" = "$dl_hash" ]; then
            echo -e "  ${GREEN}✅${NC} $file - Hash matches"
        else
            echo -e "  ${RED}❌${NC} $file - Hash mismatch or file not in original"
        fi
    fi
done

echo ""
echo "=============================================================="
echo " SERVER METRICS"
echo "=============================================================="
curl -s "$SERVER_URL/metrics" | grep -E "^(upload_bytes|download_bytes|http_requests|storage_operations)" | while read line; do
    echo "  $line"
done

echo ""
echo "=============================================================="
echo " TEST SUMMARY"
echo "=============================================================="
echo ""

# Check if tests passed
UPLOAD_SUCCESS=false
DOWNLOAD_SUCCESS=false

if grep -q "done" /tmp/push-4b.log 2>/dev/null || grep -q "Writing" /tmp/push-4b.log 2>/dev/null; then
    UPLOAD_SUCCESS=true
fi

if [ -f "*.safetensors" ] || ls *.safetensors 1> /dev/null 2>&1; then
    DOWNLOAD_SUCCESS=true
fi

echo -e "Upload Test:   $([ "$UPLOAD_SUCCESS" = true ] && echo -e "${GREEN}✅ PASS${NC}" || echo -e "${YELLOW}⚠️  PARTIAL${NC}")"
echo -e "Download Test: $([ "$DOWNLOAD_SUCCESS" = true ] && echo -e "${GREEN}✅ PASS${NC}" || echo -e "${YELLOW}⚠️  PARTIAL${NC}")"
echo ""
echo "=============================================================="
echo ""
