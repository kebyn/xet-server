#!/usr/bin/env python3
"""
Test actual file upload/download using hf-xet library.
This performs real file transfers to validate the Xet server.
"""

import os
import sys
import time
import hashlib
import requests
from pathlib import Path

try:
    import hf_xet
except ImportError:
    print("Error: hf-xet not installed. Run: pip3 install hf-xet", file=sys.stderr)
    sys.exit(1)

# Configuration
SERVER_URL = "http://127.0.0.1:8080"
TEST_DIR = "test-data"
UPLOAD_FILE = f"{TEST_DIR}/test-repo/test-model.bin"
DOWNLOADED_FILE = f"{TEST_DIR}/downloaded-model.bin"

def load_jwt_token():
    """Load JWT token from file."""
    token_file = f"{TEST_DIR}/jwt-token.txt"
    if os.path.exists(token_file):
        with open(token_file, 'r') as f:
            return f.read().strip()
    else:
        print(f"Error: JWT token file not found: {token_file}", file=sys.stderr)
        sys.exit(1)

def compute_file_hash(filepath):
    """Compute SHA256 hash of a file."""
    sha256_hash = hashlib.sha256()
    with open(filepath, "rb") as f:
        for byte_block in iter(lambda: f.read(4096), b""):
            sha256_hash.update(byte_block)
    return sha256_hash.hexdigest()

def test_upload_with_hf_xet():
    """Test file upload using hf-xet library."""
    print("\n" + "="*60)
    print("Test: File Upload with hf-xet")
    print("="*60)

    if not os.path.exists(UPLOAD_FILE):
        print(f"❌ Upload file not found: {UPLOAD_FILE}")
        return False

    # Load JWT token
    jwt_token = load_jwt_token()

    # Compute file hash
    file_hash = compute_file_hash(UPLOAD_FILE)
    file_size = os.path.getsize(UPLOAD_FILE)

    print(f"File: {UPLOAD_FILE}")
    print(f"Size: {file_size} bytes")
    print(f"SHA256: {file_hash}")

    # Prepare upload parameters
    file_paths = [UPLOAD_FILE]
    endpoint = SERVER_URL
    token_info = jwt_token
    token_refresher = None
    progress_updater = None

    print(f"\nUploading to: {endpoint}")
    print(f"Using JWT token: {jwt_token[:20]}...")

    try:
        start_time = time.time()

        # Use hf-xet to upload file
        hf_xet.upload_files(
            file_paths=file_paths,
            endpoint=endpoint,
            token_info=token_info,
            token_refresher=token_refresher,
            progress_updater=progress_updater,
            _repo_type="default"
        )

        upload_time = time.time() - start_time
        print(f"\n✅ Upload completed in {upload_time:.2f}s")

        # Check server metrics
        response = requests.get(f"{SERVER_URL}/metrics",
                              proxies={'http': None, 'https': None},
                              timeout=5)
        if response.status_code == 200:
            metrics = response.text
            for line in metrics.split('\n'):
                if 'upload' in line.lower() and not line.startswith('#'):
                    print(f"   {line}")

        return True

    except Exception as e:
        print(f"\n❌ Upload failed: {e}")
        import traceback
        traceback.print_exc()
        return False

def test_download_with_hf_xet():
    """Test file download using hf-xet library."""
    print("\n" + "="*60)
    print("Test: File Download with hf-xet")
    print("="*60)

    # Load JWT token
    jwt_token = load_jwt_token()

    # For download, we need the file hash/ID
    # This is a simplified test - in reality, we'd need the file metadata
    file_hash = compute_file_hash(UPLOAD_FILE)

    print(f"Attempting to download file with hash: {file_hash}")
    print(f"Download path: {DOWNLOADED_FILE}")

    try:
        # Prepare download parameters
        # Note: This is a simplified approach. Real usage requires proper file metadata
        files = [{
            'file_id': file_hash,
            'download_path': DOWNLOADED_FILE
        }]

        endpoint = SERVER_URL
        token_info = jwt_token
        token_refresher = None
        progress_updater = None

        start_time = time.time()

        # Use hf-xet to download file
        hf_xet.download_files(
            files=files,
            endpoint=endpoint,
            token_info=token_info,
            token_refresher=token_refresher,
            progress_updater=progress_updater
        )

        download_time = time.time() - start_time
        print(f"\n✅ Download completed in {download_time:.2f}s")

        # Verify downloaded file
        if os.path.exists(DOWNLOADED_FILE):
            downloaded_hash = compute_file_hash(DOWNLOADED_FILE)
            downloaded_size = os.path.getsize(DOWNLOADED_FILE)

            print(f"Downloaded size: {downloaded_size} bytes")
            print(f"Downloaded hash: {downloaded_hash}")

            if downloaded_hash == file_hash:
                print(f"\n✅ Data integrity verified: Hashes match!")
                return True
            else:
                print(f"\n❌ Data integrity check failed: Hashes do not match")
                return False
        else:
            print(f"\n❌ Downloaded file not found: {DOWNLOADED_FILE}")
            return False

    except Exception as e:
        print(f"\n❌ Download failed: {e}")
        import traceback
        traceback.print_exc()
        return False

def main():
    """Run upload/download tests."""
    print("\n" + "="*60)
    print("XET SERVER HF-XET UPLOAD/DOWNLOAD TEST")
    print("="*60)

    # Check server health
    print("\nChecking server health...")
    try:
        response = requests.get(f"{SERVER_URL}/health",
                              proxies={'http': None, 'https': None},
                              timeout=5)
        if response.status_code != 200:
            print(f"❌ Server not healthy: {response.status_code}")
            return 1
        print("✅ Server is healthy")
    except Exception as e:
        print(f"❌ Server not accessible: {e}")
        return 1

    # Run tests
    results = {
        'upload': test_upload_with_hf_xet(),
        'download': test_download_with_hf_xet() if os.path.exists(UPLOAD_FILE) else False
    }

    # Print summary
    print("\n" + "="*60)
    print("TEST SUMMARY")
    print("="*60)
    print(f"Upload: {'✅ PASS' if results['upload'] else '❌ FAIL'}")
    print(f"Download: {'✅ PASS' if results['download'] else '❌ FAIL'}")

    print("\n" + "="*60)
    if results['upload'] and results['download']:
        print("✅ ALL TESTS PASSED")
        print("Upload/download functionality verified with hf-xet!")
        return 0
    else:
        print("❌ SOME TESTS FAILED")
        print("\nNote: hf-xet library may require specific server endpoints")
        print("or file metadata that our test server doesn't provide.")
        return 1

if __name__ == "__main__":
    sys.exit(main())
