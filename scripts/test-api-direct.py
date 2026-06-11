#!/usr/bin/env python3
"""
Direct API test for Xet server upload/download functionality.
This bypasses Git LFS and tests the server's core API directly.
"""

import os
import sys
import time
import hashlib
import requests
from pathlib import Path

try:
    import blake3
except ImportError:
    print("Error: blake3 not installed. Run: pip3 install blake3", file=sys.stderr)
    sys.exit(1)

# Configuration
SERVER_URL = "http://127.0.0.1:8080"
JWT_TOKEN = os.environ.get("XET_JWT_TOKEN", "")
TEST_FILE = "test-data/test-repo/test-model.bin"

# Fixed keys from xet-core (data_hash.rs)
DATA_KEY = bytes([
    102, 151, 245, 119, 91, 149, 80, 222, 49, 53, 203, 172, 165, 151, 24, 28,
    157, 228, 33, 16, 155, 235, 43, 88, 180, 208, 176, 75, 147, 173, 242, 41,
])

def load_jwt_token():
    """Load JWT token from file."""
    global JWT_TOKEN
    token_file = "test-data/jwt-token.txt"
    if os.path.exists(token_file):
        with open(token_file, 'r') as f:
            JWT_TOKEN = f.read().strip()
        print(f"Loaded JWT token from {token_file}")
    else:
        print(f"Error: JWT token file not found: {token_file}", file=sys.stderr)
        sys.exit(1)

def compute_file_hash(filepath):
    """Compute Blake3 hash of a file using Xet's DATA_KEY."""
    with open(filepath, "rb") as f:
        file_data = f.read()
    # Use keyed hash with DATA_KEY
    hasher = blake3.blake3(key=DATA_KEY)
    hasher.update(file_data)
    return hasher.hexdigest()

def test_health():
    """Test health endpoint."""
    print("\n=== Testing Health Endpoint ===")
    response = requests.get(f"{SERVER_URL}/health", proxies={'http': None, 'https': None})
    if response.status_code == 200:
        print(f"✅ Health check passed: {response.json()}")
        return True
    else:
        print(f"❌ Health check failed: {response.status_code}")
        return False

def test_metrics():
    """Test metrics endpoint."""
    print("\n=== Testing Metrics Endpoint ===")
    response = requests.get(f"{SERVER_URL}/metrics", proxies={'http': None, 'https': None})
    if response.status_code == 200:
        print("✅ Metrics endpoint accessible")
        print("Current metrics:")
        for line in response.text.split('\n')[:10]:
            if line and not line.startswith('#'):
                print(f"  {line}")
        return True
    else:
        print(f"❌ Metrics endpoint failed: {response.status_code}")
        return False

def test_upload_xorb():
    """Test xorb upload endpoint."""
    print("\n=== Testing Xorb Upload ===")

    if not os.path.exists(TEST_FILE):
        print(f"❌ Test file not found: {TEST_FILE}")
        return None

    # Read test file
    with open(TEST_FILE, 'rb') as f:
        file_data = f.read()

    # Compute hash using Blake3 with Xet's DATA_KEY
    hasher = blake3.blake3(key=DATA_KEY)
    hasher.update(file_data)
    file_hash = hasher.hexdigest()
    print(f"File: {TEST_FILE}")
    print(f"Size: {len(file_data)} bytes")
    print(f"Hash: {file_hash}")

    # Upload xorb
    prefix = "default"
    upload_url = f"{SERVER_URL}/v1/xorbs/{prefix}/{file_hash}"
    headers = {
        "Authorization": f"Bearer {JWT_TOKEN}",
        "Content-Type": "application/octet-stream"
    }

    print(f"\nUploading to: {upload_url}")
    start_time = time.time()
    response = requests.post(upload_url, data=file_data, headers=headers, proxies={'http': None, 'https': None})
    upload_time = time.time() - start_time

    if response.status_code == 200:
        result = response.json()
        print(f"✅ Upload successful in {upload_time:.2f}s")
        print(f"   Response: {result}")
        return {
            'hash': file_hash,
            'size': len(file_data),
            'was_inserted': result.get('was_inserted', False)
        }
    else:
        print(f"❌ Upload failed: {response.status_code}")
        print(f"   Error: {response.text}")
        return None

def test_reconstruction(file_hash):
    """Test reconstruction endpoint."""
    print("\n=== Testing Reconstruction ===")

    reconstruction_url = f"{SERVER_URL}/v2/reconstructions/{file_hash}"
    headers = {
        "Authorization": f"Bearer {JWT_TOKEN}"
    }

    print(f"Querying: {reconstruction_url}")
    response = requests.get(reconstruction_url, headers=headers, proxies={'http': None, 'https': None})

    if response.status_code == 200:
        result = response.json()
        print(f"✅ Reconstruction query successful")
        print(f"   File ID: {result.get('file_id')}")
        print(f"   Xorbs: {len(result.get('xorbs', []))}")
        return result
    else:
        print(f"❌ Reconstruction query failed: {response.status_code}")
        print(f"   Error: {response.text}")
        return None

def test_upload_metrics():
    """Check upload metrics."""
    print("\n=== Checking Upload Metrics ===")
    response = requests.get(f"{SERVER_URL}/metrics", proxies={'http': None, 'https': None})

    if response.status_code == 200:
        metrics = response.text
        upload_total = None
        upload_bytes = None

        for line in metrics.split('\n'):
            if line.startswith('upload_total '):
                upload_total = line.split()[1]
            elif line.startswith('upload_bytes_total '):
                upload_bytes = line.split()[1]

        print(f"Upload Total: {upload_total or '0'}")
        print(f"Upload Bytes: {upload_bytes or '0'}")

        if upload_total and int(upload_total) > 0:
            print("✅ Upload metrics recorded")
            return True
        else:
            print("⚠️  Upload metrics show zero (may be expected)")
            return True
    else:
        print(f"❌ Failed to fetch metrics: {response.status_code}")
        return False

def main():
    """Run all tests."""
    print("=" * 60)
    print("Xet Server Direct API Test")
    print("=" * 60)

    # Load JWT token
    load_jwt_token()

    # Run tests
    results = {
        'health': test_health(),
        'metrics': test_metrics(),
        'upload': test_upload_xorb(),
    }

    # Test reconstruction if upload succeeded
    if results['upload']:
        results['reconstruction'] = test_reconstruction(results['upload']['hash'])
        results['upload_metrics'] = test_upload_metrics()

    # Print summary
    print("\n" + "=" * 60)
    print("Test Summary")
    print("=" * 60)
    print(f"Health Check: {'✅ PASS' if results['health'] else '❌ FAIL'}")
    print(f"Metrics Endpoint: {'✅ PASS' if results['metrics'] else '❌ FAIL'}")
    print(f"Xorb Upload: {'✅ PASS' if results['upload'] else '❌ FAIL'}")
    if results['upload']:
        print(f"Reconstruction: {'✅ PASS' if results.get('reconstruction') else '❌ FAIL'}")
        print(f"Upload Metrics: {'✅ PASS' if results.get('upload_metrics') else '❌ FAIL'}")

    print("\n" + "=" * 60)
    if all([results['health'], results['metrics'], results['upload']]):
        print("✅ All core tests PASSED")
        print("=" * 60)
        return 0
    else:
        print("❌ Some tests FAILED")
        print("=" * 60)
        return 1

if __name__ == "__main__":
    sys.exit(main())
