#!/bin/bash
set -e

echo "=== Git Repository Setup for Xet Testing ==="

# Configuration
TEST_DIR="test-data"
REPO_DIR="${TEST_DIR}/test-repo"
BARE_REPO_DIR="${TEST_DIR}/test-repo.git"
JWT_TOKEN=$(cat "${TEST_DIR}/jwt-token.txt")

# Create bare repository (remote)
echo "Creating bare repository..."
mkdir -p "${BARE_REPO_DIR}"
cd "${BARE_REPO_DIR}"
git init --bare
cd - > /dev/null

# Create working repository
echo "Creating working repository..."
mkdir -p "${REPO_DIR}"
cd "${REPO_DIR}"

git init
git config user.email "test@example.com"
git config user.name "Test User"

# Install Git LFS
echo "Installing Git LFS..."
git lfs install

# Create .lfsconfig
cat > .lfsconfig << EOF
[lfs]
    url = http://127.0.0.1:8080
EOF

# Create credential helper
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

# Configure Git to use credential helper
git config credential.helper "$(pwd)/git-credential-xet"
git config lfs.access basic
git config lfs.url http://127.0.0.1:8080

# Track large files with LFS
git lfs track "*.safetensors"
git lfs track "*.bin"
git add .gitattributes
git commit -m "Configure LFS tracking"

# Add remote
git remote add origin "../test-repo.git"

echo ""
echo "=== Git Repository Setup Complete ==="
echo "Repository: ${REPO_DIR}"
echo "Remote: ${BARE_REPO_DIR}"
echo ""
echo "Next steps:"
echo "  1. Start the Xet server"
echo "  2. Copy test files to ${REPO_DIR}"
echo "  3. Run: ./scripts/run-upload-test.sh"
