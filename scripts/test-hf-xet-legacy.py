#!/usr/bin/env python3
"""
Test using hf_xet legacy upload_files/download_files with xet server.
"""

import os
import sys
import time
import hashlib
import requests

try:
    import hf_xet
except ImportError:
    print("Error: hf-xet not installed", file=sys.stderr)
    sys.exit(1)

# Configuration
SERVER_URL = "http://127.0.0.1:8080"
TEST_DIR = "/data/test-data"
QWEN_4B_DIR = f"{TEST_DIR}/qwen-model"
QWEN_8B_DIR = f"{TEST_DIR}/qwen3-8b-model"
DOWNLOAD_DIR = f"{TEST_DIR}/hf-xet-legacy-test"
JWT_TOKEN_FILE = f"{TEST_DIR}/jwt-token.txt"

def load_jwt_token():
    """Load JWT token from file."""
    if os.path.exists(JWT_TOKEN_FILE):
        with open(JWT_TOKEN_FILE, 'r') as f:
            return f.read().strip()
    return None

def compute_file_hash(filepath):
    """Compute SHA256 hash of a file."""
    sha256_hash = hashlib.sha256()
    with open(filepath, "rb") as f:
        for byte_block in iter(lambda: f.read(4096), b""):
            sha256_hash.update(byte_block)
    return sha256_hash.hexdigest()

def get_small_files(model_dir, max_files=3):
    """Get small files for testing."""
    files = []
    if not os.path.exists(model_dir):
        return files

    for filename in sorted(os.listdir(model_dir)):
        filepath = os.path.join(model_dir, filename)
        if os.path.isfile(filepath) and not filename.startswith('.'):
            size = os.path.getsize(filepath)
            if size < 10 * 1024 * 1024:  # Less than 10MB
                files.append(filepath)
                if len(files) >= max_files:
                    break
    return files

def test_upload_legacy(model_name, model_dir, jwt_token):
    """Test upload using legacy upload_files."""
    print("\n" + "="*70)
    print(f"TEST: Upload {model_name} using hf_xet.upload_files")
    print("="*70)

    files = get_small_files(model_dir, max_files=3)
    if not files:
        print(f"❌ No suitable files found in {model_dir}")
        return False, []

    print(f"\nFiles to upload:")
    file_info = []
    for f in files:
        size = os.path.getsize(f)
        hash_val = compute_file_hash(f)
        print(f"  - {os.path.basename(f)} ({size} bytes, hash={hash_val[:16]}...)")
        file_info.append({
            'path': f,
            'size': size,
            'hash': hash_val
        })

    print(f"\nUploading to: {SERVER_URL}")

    try:
        # token_info format: (token, expiry_timestamp)
        # Use a future timestamp (1 hour from now)
        expiry = int(time.time()) + 3600
        token_info = (jwt_token, expiry)

        start_time = time.time()

        result = hf_xet.upload_files(
            file_paths=files,
            endpoint=SERVER_URL,
            token_info=token_info,
            token_refresher=None,
            progress_updater=None,
            _repo_type="model",
            sha256s=[info['hash'] for info in file_info]
        )

        upload_time = time.time() - start_time
        print(f"\n✅ Upload completed in {upload_time:.2f}s")
        print(f"Result: {result}")

        return True, file_info

    except Exception as e:
        print(f"\n❌ Upload failed: {e}")
        import traceback
        traceback.print_exc()
        return False, []

def test_download_legacy(model_name, file_info, jwt_token):
    """Test download using legacy download_files."""
    print("\n" + "="*70)
    print(f"TEST: Download {model_name} using hf_xet.download_files")
    print("="*70)

    if not file_info:
        print("❌ No file info available for download")
        return False

    os.makedirs(DOWNLOAD_DIR, exist_ok=True)

    # Prepare download list
    files_to_download = []
    for info in file_info:
        filename = os.path.basename(info['path'])
        download_path = os.path.join(DOWNLOAD_DIR, f"{model_name.replace('/', '_')}_{filename}")

        files_to_download.append({
            'hash': info['hash'],
            'file_size': info['size'],
            'download_path': download_path
        })

    print(f"\nFiles to download:")
    for f in files_to_download:
        print(f"  - {os.path.basename(f['download_path'])} (hash={f['hash'][:16]}...)")

    print(f"\nDownloading from: {SERVER_URL}")

    try:
        expiry = int(time.time()) + 3600
        token_info = (jwt_token, expiry)

        start_time = time.time()

        result = hf_xet.download_files(
            files=files_to_download,
            endpoint=SERVER_URL,
            token_info=token_info,
            token_refresher=None,
            progress_updater=None
        )

        download_time = time.time() - start_time
        print(f"\n✅ Download completed in {download_time:.2f}s")
        print(f"Result: {result}")

        # Verify downloaded files
        print("\nVerifying downloaded files...")
        verified = 0
        for orig_info, dl_info in zip(file_info, files_to_download):
            dl_path = dl_info['download_path']
            if os.path.exists(dl_path):
                dl_hash = compute_file_hash(dl_path)
                if dl_hash == orig_info['hash']:
                    print(f"  ✅ {os.path.basename(dl_path)} - Hash matches")
                    verified += 1
                else:
                    print(f"  ❌ {os.path.basename(dl_path)} - Hash mismatch")
            else:
                print(f"  ❌ {os.path.basename(dl_path)} - Not found")

        return verified == len(file_info)

    except Exception as e:
        print(f"\n❌ Download failed: {e}")
        import traceback
        traceback.print_exc()
        return False

def main():
    """Run tests."""
    print("\n" + "="*70)
    print(" XET SERVER HF_XET LEGACY API TEST")
    print("="*70)

    # Check server
    try:
        response = requests.get(f"{SERVER_URL}/health", timeout=5)
        if response.status_code != 200:
            print(f"❌ Server not healthy: {response.status_code}")
            return 1
        print("✅ Server is healthy")
    except Exception as e:
        print(f"❌ Server not accessible: {e}")
        return 1

    jwt_token = load_jwt_token()
    if not jwt_token:
        print("❌ JWT token not found")
        return 1
    print(f"✅ JWT token loaded")

    results = {
        'qwen_4b_upload': False,
        'qwen_4b_download': False,
        'qwen_8b_upload': False,
        'qwen_8b_download': False
    }

    file_info_4b = []
    file_info_8b = []

    # Test Qwen3-4B
    if os.path.exists(QWEN_4B_DIR):
        print("\n" + "="*70)
        print(" TESTING Qwen/Qwen3-4B-Thinking-2507")
        print("="*70)
        results['qwen_4b_upload'], file_info_4b = test_upload_legacy("Qwen3-4B", QWEN_4B_DIR, jwt_token)
        if results['qwen_4b_upload']:
            results['qwen_4b_download'] = test_download_legacy("Qwen3-4B", file_info_4b, jwt_token)

    # Test Qwen3-8B
    if os.path.exists(QWEN_8B_DIR):
        print("\n" + "="*70)
        print(" TESTING Qwen/Qwen3-8B")
        print("="*70)
        results['qwen_8b_upload'], file_info_8b = test_upload_legacy("Qwen3-8B", QWEN_8B_DIR, jwt_token)
        if results['qwen_8b_upload']:
            results['qwen_8b_download'] = test_download_legacy("Qwen3-8B", file_info_8b, jwt_token)

    # Summary
    print("\n" + "="*70)
    print(" TEST SUMMARY")
    print("="*70)
    print(f"\nQwen3-4B:")
    print(f"  Upload:   {'✅ PASS' if results['qwen_4b_upload'] else '❌ FAIL'}")
    print(f"  Download: {'✅ PASS' if results['qwen_4b_download'] else '❌ FAIL'}")
    print(f"\nQwen3-8B:")
    print(f"  Upload:   {'✅ PASS' if results['qwen_8b_upload'] else '❌ FAIL'}")
    print(f"  Download: {'✅ PASS' if results['qwen_8b_download'] else '❌ FAIL'}")

    all_passed = all(results.values())
    print("\n" + "="*70)
    if all_passed:
        print(" ✅ ALL TESTS PASSED")
    else:
        print(" ❌ SOME TESTS FAILED")
    print("="*70 + "\n")

    return 0 if all_passed else 1

if __name__ == "__main__":
    sys.exit(main())
