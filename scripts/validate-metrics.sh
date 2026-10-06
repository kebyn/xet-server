#!/bin/bash

echo "=== Xet Server Metrics Validation ==="

# Configuration: CAS server port and an internal-scope token (required by /metrics)
XET_PORT="${XET_PORT:-8081}"
INTERNAL_TOKEN="${INTERNAL_TOKEN:?set INTERNAL_TOKEN to an internal_xxx token}"

# Fetch metrics
echo "Fetching metrics from server..."
METRICS=$(curl -s -H "Authorization: Bearer ${INTERNAL_TOKEN}" \
    "http://127.0.0.1:${XET_PORT}/metrics")

if [ -z "${METRICS}" ]; then
    echo "❌ Failed to fetch metrics"
    exit 1
fi

echo ""
echo "=== All Metrics ==="
echo "${METRICS}"

echo ""
echo "=== Key Metrics Summary ==="

# Upload/download byte counters (business dimensions, recorded by handlers)
UPLOAD_BYTES=$(echo "${METRICS}" | grep "^upload_bytes_total" | awk '{print $2}')
DOWNLOAD_BYTES=$(echo "${METRICS}" | grep "^download_bytes_total" | awk '{print $2}')

echo "Upload Bytes: ${UPLOAD_BYTES:-0}"
echo "Download Bytes: ${DOWNLOAD_BYTES:-0}"

# Request metrics (recorded by the HTTP middleware)
REQUESTS_2XX=$(echo "${METRICS}" | grep 'http_requests_by_status{status="2xx"}' | awk '{print $2}')
STORAGE_OPS=$(echo "${METRICS}" | grep "^storage_operations_total" | awk '{print $2}')

echo "Successful Requests (2xx): ${REQUESTS_2XX:-0}"
echo "Storage Operations: ${STORAGE_OPS:-0}"

echo ""
echo "=== Validation Results ==="

PASS=true

if [ -z "${UPLOAD_BYTES}" ]; then
    echo "⚠️  Upload bytes metric is missing"
    PASS=false
else
    echo "✅ Upload bytes: ${UPLOAD_BYTES}"
fi

if [ -z "${DOWNLOAD_BYTES}" ]; then
    echo "⚠️  Download bytes metric is missing"
    PASS=false
else
    echo "✅ Download bytes: ${DOWNLOAD_BYTES}"
fi

if [ -z "${REQUESTS_2XX}" ]; then
    echo "⚠️  2xx request counter is missing"
    PASS=false
else
    echo "✅ Successful requests (2xx): ${REQUESTS_2XX}"
fi

echo ""
if [ "${PASS}" = true ]; then
    echo "=== ✅ All Metrics Present ==="
    exit 0
else
    echo "=== ⚠️  Some Metrics Are Missing ==="
    exit 1
fi
