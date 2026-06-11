#!/usr/bin/env python3
"""
Test upload/download using hf_xet library with proper XetSession API.
Tests with both Qwen/Qwen3-4B-Thinking-2507 and Qwen/Qwen3-8B models.
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
TEST_DATA_DIR = "/data/test-data"
QWEN_4B_DIR = f"{TEST_DATA_DIR}/qwen-model"
QWEN_8B_DIR = f"{TEST_DATA_DIR}/qwen3-8b-model"
DOWNLOAD_DIR = f"{TEST_DATA_DIR}/hf-xet-download-test"
JWT_TOKEN_FILE = f"{TEST_DATA_DIR}/jwt-token.txt"

def load_jwt_token():
    """Load JWT token from file."""
    if os.path.exists(JWT_TOKEN_FILE):
        with open(JWT_TOKEN_FILE, 'r') as f:
            return f.read().strip()
    else:
        print(f"Error: JWT token file not found: {JWT_TOKEN_FILE}", file=sys.stderr)
        sys.exit(1)

def compute_file_hash(filepath):
    """Compute SHA256 hash of a file."""
    sha256_hash = hashlib.sha256()
    with open(filepath, "rb") as f:
        for byte_block in iter(lambda: f.read(4096), b""):
            sha256_hash.update(byte_block)
    return sha256_hash.hexdigest()

def get_model_files(model_dir, max_files=10):
    """Get list of model files to upload."""
    files = []
    if not os.path.exists(model_dir):
        return files

    for root, dirs, filenames in os.walk(model_dir):
        # Skip cache directory
        if '.cache' in root:
            continue
        for filename in sorted(filenames):
            filepath = os.path.join(root, filename)
            if os.path.isfile(filepath):
                files.append(filepath)
                if len(files) >= max_files:
                    return files
    return files

def test_upload_with_hf_xet(model_name, model_dir, jwt_token):
    """Test uploading model files using hf_xet XetSession API."""
    print("\n" + "="*70)
    print(f"TEST: Upload {model_name} using hf_xet")
    print("="*70)

    # Get model files
    model_files = get_model_files(model_dir, max_files=5)
    if not model_files:
        print(f"❌ No files found in {model_dir}")
        return False

    print(f"\nFound {len(model_files)} files to upload")
    total_size = sum(os.path.getsize(f) for f in model_files)
    print(f"Total size: {total_size / (1024*1024):.2f} MB")

    # Print files
    print("\nFiles to upload:")
    for f in model_files:
        size = os.path.getsize(f)
        rel_path = os.path.relpath(f, model_dir)
        if size > 1024*1024:
            print(f"  - {rel_path} ({size / (1024*1024):.2f} MB)")
        else:
            print(f"  - {rel_path} ({size} bytes)")

    # Create XetSession
    print(f"\nCreating XetSession...")
    print(f"Endpoint: {SERVER_URL}")

    try:
        session = hf_xet.XetSession()

        # Create upload commit
        print("Creating upload commit...")
        with session.new_upload_commit(
            endpoint=SERVER_URL,
            token=jwt_token
        ) as commit:
            print("Starting file uploads...")
            start_time = time.time()

            # Upload each file
            upload_handles = []
            for filepath in model_files:
                print(f"  Queuing: {os.path.basename(filepath)}")
                handle = commit.start_upload_file(filepath)
                upload_handles.append((filepath, handle))

            # Wait for all uploads to complete
            print("\nWaiting for uploads to complete...")
            report = commit.wait_to_finish()

            upload_time = time.time() - start_time
            print(f"\n✅ Upload completed in {upload_time:.2f}s")
            if upload_time > 0:
                print(f"Upload speed: {total_size / (1024*1024*upload_time):.2f} MB/s")

            # Print upload results
            print("\nUpload results:")
            for filepath, handle in upload_handles:
                try:
                    result = handle.result()
                    print(f"  ✅ {os.path.basename(filepath)}: hash={result.hash[:16]}...")
                except Exception as e:
                    print(f"  ❌ {os.path.basename(filepath)}: {e}")
                    return False

            return True

    except Exception as e:
        print(f"\n❌ Upload failed: {e}")
        import traceback
        traceback.print_exc()
        return False

def test_download_with_hf_xet(model_name, model_dir, jwt_token):
    """Test downloading model files using hf_xet XetSession API."""
    print("\n" + "="*70)
    print(f"TEST: Download {model_name} using hf_xet")
    print("="*70)

    # Get original files for reference
    original_files = get_model_files(model_dir, max_files=5)
    if not original_files:
        print(f"❌ No original files found in {model_dir}")
        return False

    # Create download directory
    model_download_dir = os.path.join(DOWNLOAD_DIR, model_name.replace("/", "_"))
    os.makedirs(model_download_dir, exist_ok=True)

    print(f"\nAttempting to download {len(original_files)} files")
    print(f"Download directory: {model_download_dir}")

    try:
        session = hf_xet.XetSession()

        # Create download group
        print("Creating download group...")
        with session.new_file_download_group(
            endpoint=SERVER_URL,
            token=jwt_token
        ) as group:
            print("Starting file downloads...")
            start_time = time.time()

            # Queue downloads
            download_handles = []
            for filepath in original_files:
                filename = os.path.basename(filepath)
                file_hash = compute_file_hash(filepath)
                file_size = os.path.getsize(filepath)
                download_path = os.path.join(model_download_dir, filename)

                print(f"  Queuing: {filename} (hash={file_hash[:16]}...)")

                # Create XetFileInfo
                file_info = hf_xet.XetFileInfo(hash=file_hash, file_size=file_size)

                # Start download
                handle = group.start_download_file(file_info, download_path)
                download_handles.append((filepath, download_path, handle))

            # Wait for all downloads to complete
            print("\nWaiting for downloads to complete...")
            report = group.wait_to_finish()

            download_time = time.time() - start_time
            print(f"\n✅ Download completed in {download_time:.2f}s")

            # Verify downloaded files
            print("\nVerifying downloaded files...")
            verified_count = 0
            total_size = 0

            for orig_file, dl_file, handle in download_handles:
                if os.path.exists(dl_file):
                    orig_hash = compute_file_hash(orig_file)
                    dl_hash = compute_file_hash(dl_file)
                    file_size = os.path.getsize(dl_file)
                    total_size += file_size

                    if orig_hash == dl_hash:
                        verified_count += 1
                        if file_size > 1024*1024:
                            print(f"  ✅ {os.path.basename(dl_file)} ({file_size / (1024*1024):.2f} MB)")
                        else:
                            print(f"  ✅ {os.path.basename(dl_file)} ({file_size} bytes)")
                    else:
                        print(f"  ❌ {os.path.basename(dl_file)} - Hash mismatch!")
                else:
                    print(f"  ❌ {os.path.basename(dl_file)} - Not found")

            print(f"\nDownloaded {total_size / (1024*1024):.2f} MB in {download_time:.2f}s")
            if download_time > 0:
                print(f"Download speed: {total_size / (1024*1024*download_time):.2f} MB/s")

            if verified_count == len(original_files):
                print(f"\n✅ All {verified_count} files verified successfully!")
                return True
            else:
                print(f"\n❌ Only {verified_count}/{len(original_files)} files verified")
                return False

    except Exception as e:
        print(f"\n❌ Download failed: {e}")
        import traceback
        traceback.print_exc()
        return False

def check_server_health():
    """Check if server is healthy."""
    print("\nChecking server health...")
    try:
        response = requests.get(f"{SERVER_URL}/health",
                              proxies={'http': None, 'https': None},
                              timeout=5)
        if response.status_code == 200:
            print("✅ Server is healthy")
            return True
        else:
            print(f"❌ Server not healthy: {response.status_code}")
            return False
    except Exception as e:
        print(f"❌ Server not accessible: {e}")
        return False

def get_server_metrics():
    """Get server metrics."""
    try:
        response = requests.get(f"{SERVER_URL}/metrics",
                              proxies={'http': None, 'https': None},
                              timeout=5)
        if response.status_code == 200:
            return response.text
        return None
    except:
        return None

def main():
    """Run comprehensive upload/download tests using hf_xet."""
    print("\n" + "="*70)
    print(" XET SERVER HF_XET TEST - Qwen Models Upload/Download")
    print(" Using hf_xet.XetSession API")
    print("="*70)

    # Check server health
    if not check_server_health():
        return 1

    # Load JWT token
    jwt_token = load_jwt_token()
    print(f"✅ JWT token loaded: {jwt_token[:30]}...")

    # Test results
    results = {
        'qwen_4b_upload': False,
        'qwen_4b_download': False,
        'qwen_8b_upload': False,
        'qwen_8b_download': False
    }

    # Test Qwen3-4B-Thinking-2507
    if os.path.exists(QWEN_4B_DIR):
        print("\n" + "="*70)
        print(" TESTING Qwen/Qwen3-4B-Thinking-2507")
        print("="*70)
        results['qwen_4b_upload'] = test_upload_with_hf_xet("Qwen3-4B-Thinking-2507", QWEN_4B_DIR, jwt_token)
        if results['qwen_4b_upload']:
            results['qwen_4b_download'] = test_download_with_hf_xet("Qwen3-4B-Thinking-2507", QWEN_4B_DIR, jwt_token)
    else:
        print(f"\n⚠️  Qwen3-4B model directory not found: {QWEN_4B_DIR}")

    # Test Qwen3-8B
    if os.path.exists(QWEN_8B_DIR):
        print("\n" + "="*70)
        print(" TESTING Qwen/Qwen3-8B")
        print("="*70)
        results['qwen_8b_upload'] = test_upload_with_hf_xet("Qwen3-8B", QWEN_8B_DIR, jwt_token)
        if results['qwen_8b_upload']:
            results['qwen_8b_download'] = test_download_with_hf_xet("Qwen3-8B", QWEN_8B_DIR, jwt_token)
    else:
        print(f"\n⚠️  Qwen3-8B model directory not found: {QWEN_8B_DIR}")

    # Get final metrics
    print("\n" + "="*70)
    print(" SERVER METRICS")
    print("="*70)
    metrics = get_server_metrics()
    if metrics:
        for line in metrics.split('\n'):
            if not line.startswith('#') and line.strip():
                if any(keyword in line.lower() for keyword in ['upload', 'download', 'request', 'storage']):
                    print(f"  {line}")

    # Print summary
    print("\n" + "="*70)
    print(" TEST SUMMARY (hf_xet API)")
    print("="*70)
    print(f"\nQwen/Qwen3-4B-Thinking-2507:")
    print(f"  Upload:   {'✅ PASS' if results['qwen_4b_upload'] else '❌ FAIL'}")
    print(f"  Download: {'✅ PASS' if results['qwen_4b_download'] else '❌ FAIL'}")

    print(f"\nQwen/Qwen3-8B:")
    print(f"  Upload:   {'✅ PASS' if results['qwen_8b_upload'] else '❌ FAIL'}")
    print(f"  Download: {'✅ PASS' if results['qwen_8b_download'] else '❌ FAIL'}")

    all_passed = all(results.values())
    print("\n" + "="*70)
    if all_passed:
        print(" ✅ ALL TESTS PASSED")
        print(" Both Qwen models upload/download verified with hf_xet!")
    else:
        print(" ❌ SOME TESTS FAILED")
        failed = [k for k, v in results.items() if not v]
        print(f" Failed tests: {', '.join(failed)}")
    print("="*70 + "\n")

    return 0 if all_passed else 1

if __name__ == "__main__":
    sys.exit(main())
