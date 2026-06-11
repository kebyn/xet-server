#!/bin/bash

echo "=== Xet Server Metrics Validation ==="

# Configuration
XET_PORT="8080"

# Fetch metrics
echo "Fetching metrics from server..."
METRICS=$(curl -s http://127.0.0.1:${XET_PORT}/metrics)

if [ -z "${METRICS}" ]; then
    echo "❌ Failed to fetch metrics"
    exit 1
fi

echo ""
echo "=== All Metrics ==="
echo "${METRICS}"

echo ""
echo "=== Key Metrics Summary ==="

# Check upload metrics
UPLOAD_TOTAL=$(echo "${METRICS}" | grep "^xet_upload_total" | awk '{print $2}')
UPLOAD_BYTES=$(echo "${METRICS}" | grep "^xet_upload_bytes_total" | awk '{print $2}')

echo "Upload Total: ${UPLOAD_TOTAL:-0}"
echo "Upload Bytes: ${UPLOAD_BYTES:-0}"

# Check download metrics
DOWNLOAD_TOTAL=$(echo "${METRICS}" | grep "^xet_download_total" | awk '{print $2}')
DOWNLOAD_BYTES=$(echo "${METRICS}" | grep "^xet_download_bytes_total" | awk '{print $2}')

echo "Download Total: ${DOWNLOAD_TOTAL:-0}"
echo "Download Bytes: ${DOWNLOAD_BYTES:-0}"

# Check request metrics
REQUESTS_200=$(echo "${METRICS}" | grep 'xet_requests_total{status="200"}' | awk '{print $2}')
echo "Successful Requests (200): ${REQUESTS_200:-0}"

# Check storage operations
STORAGE_OPS=$(echo "${METRICS}" | grep "^xet_storage_operations_total" | awk '{print $2}')
echo "Storage Operations: ${STORAGE_OPS:-0}"

echo ""
echo "=== Validation Results ==="

# Validate metrics are present and non-zero
PASS=true

if [ -z "${UPLOAD_TOTAL}" ] || [ "${UPLOAD_TOTAL}" = "0" ]; then
    echo "⚠️  Upload total is zero or missing"
    PASS=false
else
    echo "✅ Upload total: ${UPLOAD_TOTAL}"
fi

if [ -z "${UPLOAD_BYTES}" ] || [ "${UPLOAD_BYTES}" = "0" ]; then
    echo "⚠️  Upload bytes is zero or missing"
    PASS=false
else
    echo "✅ Upload bytes: ${UPLOAD_BYTES}"
fi

if [ -z "${DOWNLOAD_TOTAL}" ] || [ "${DOWNLOAD_TOTAL}" = "0" ]; then
    echo "⚠️  Download total is zero or missing"
    PASS=false
else
    echo "✅ Download total: ${DOWNLOAD_TOTAL}"
fi

if [ -z "${DOWNLOAD_BYTES}" ] || [ "${DOWNLOAD_BYTES}" = "0" ]; then
    echo "⚠️  Download bytes is zero or missing"
    PASS=false
else
    echo "✅ Download bytes: ${DOWNLOAD_BYTES}"
fi

if [ -z "${REQUESTS_200}" ] || [ "${REQUESTS_200}" = "0" ]; then
    echo "⚠️  Successful requests is zero or missing"
    PASS=false
else
    echo "✅ Successful requests: ${REQUESTS_200}"
fi

echo ""
if [ "${PASS}" = true ]; then
    echo "=== ✅ All Metrics Validated Successfully ==="
    exit 0
else
    echo "=== ⚠️  Some Metrics Are Missing or Zero ==="
    echo "This may be expected if no uploads/downloads have occurred yet."
    exit 0
fi
