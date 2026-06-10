//! 性能监控和指标收集模块
//!
//! 提供服务器性能指标的收集和暴露功能

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// 全局指标收集器
#[derive(Debug)]
pub struct Metrics {
    /// HTTP 请求计数
    pub http_requests_total: AtomicU64,

    /// 按状态码分类的请求计数
    pub http_requests_by_status: [AtomicU64; 5], // 2xx, 3xx, 4xx, 5xx, other

    /// 存储操作计数
    pub storage_operations: AtomicU64,

    /// 上传字节数
    pub upload_bytes: AtomicU64,

    /// 下载字节数
    pub download_bytes: AtomicU64,

    /// 错误计数
    pub errors_total: AtomicU64,

    /// 活跃连接数
    pub active_connections: AtomicU64,

    /// 请求延迟总和（微秒）
    pub request_latency_us: AtomicU64,

    /// 请求延迟计数
    pub request_latency_count: AtomicU64,
}

impl Metrics {
    /// 创建新的指标收集器
    pub fn new() -> Self {
        Self {
            http_requests_total: AtomicU64::new(0),
            http_requests_by_status: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            storage_operations: AtomicU64::new(0),
            upload_bytes: AtomicU64::new(0),
            download_bytes: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            active_connections: AtomicU64::new(0),
            request_latency_us: AtomicU64::new(0),
            request_latency_count: AtomicU64::new(0),
        }
    }

    /// 记录 HTTP 请求
    pub fn record_request(&self, status_code: u16) {
        self.http_requests_total.fetch_add(1, Ordering::Relaxed);

        let status_index = match status_code {
            200..=299 => 0,
            300..=399 => 1,
            400..=499 => 2,
            500..=599 => 3,
            _ => 4,
        };

        self.http_requests_by_status[status_index].fetch_add(1, Ordering::Relaxed);
    }

    /// 记录存储操作
    pub fn record_storage_operation(&self) {
        self.storage_operations.fetch_add(1, Ordering::Relaxed);
    }

    /// 记录上传字节数
    pub fn record_upload_bytes(&self, bytes: u64) {
        self.upload_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// 记录下载字节数
    pub fn record_download_bytes(&self, bytes: u64) {
        self.download_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// 记录错误
    pub fn record_error(&self) {
        self.errors_total.fetch_add(1, Ordering::Relaxed);
    }

    /// 增加活跃连接数
    pub fn connection_opened(&self) {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
    }

    /// 减少活跃连接数
    pub fn connection_closed(&self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    /// 记录请求延迟
    pub fn record_latency(&self, start: Instant) {
        let elapsed = start.elapsed();
        let us = elapsed.as_micros() as u64;
        self.request_latency_us.fetch_add(us, Ordering::Relaxed);
        self.request_latency_count.fetch_add(1, Ordering::Relaxed);
    }

    /// 获取平均请求延迟（微秒）
    pub fn average_latency_us(&self) -> f64 {
        let total = self.request_latency_us.load(Ordering::Relaxed);
        let count = self.request_latency_count.load(Ordering::Relaxed);
        if count == 0 {
            0.0
        } else {
            total as f64 / count as f64
        }
    }

    /// 将所有指标导出为字符串格式（Prometheus 兼容）
    pub fn export_metrics(&self) -> String {
        let mut output = String::new();

        // HTTP 请求总数
        output.push_str("# HELP http_requests_total Total number of HTTP requests\n");
        output.push_str("# TYPE http_requests_total counter\n");
        output.push_str(&format!(
            "http_requests_total {}\n",
            self.http_requests_total.load(Ordering::Relaxed)
        ));

        // 按状态码分类的请求数
        output.push_str("# HELP http_requests_by_status HTTP requests by status code range\n");
        output.push_str("# TYPE http_requests_by_status counter\n");
        let status_labels = ["2xx", "3xx", "4xx", "5xx", "other"];
        for (i, label) in status_labels.iter().enumerate() {
            output.push_str(&format!(
                "http_requests_by_status{{status=\"{}\"}} {}\n",
                label,
                self.http_requests_by_status[i].load(Ordering::Relaxed)
            ));
        }

        // 存储操作
        output.push_str("# HELP storage_operations_total Total number of storage operations\n");
        output.push_str("# TYPE storage_operations_total counter\n");
        output.push_str(&format!(
            "storage_operations_total {}\n",
            self.storage_operations.load(Ordering::Relaxed)
        ));

        // 上传字节数
        output.push_str("# HELP upload_bytes_total Total bytes uploaded\n");
        output.push_str("# TYPE upload_bytes_total counter\n");
        output.push_str(&format!(
            "upload_bytes_total {}\n",
            self.upload_bytes.load(Ordering::Relaxed)
        ));

        // 下载字节数
        output.push_str("# HELP download_bytes_total Total bytes downloaded\n");
        output.push_str("# TYPE download_bytes_total counter\n");
        output.push_str(&format!(
            "download_bytes_total {}\n",
            self.download_bytes.load(Ordering::Relaxed)
        ));

        // 错误总数
        output.push_str("# HELP errors_total Total number of errors\n");
        output.push_str("# TYPE errors_total counter\n");
        output.push_str(&format!(
            "errors_total {}\n",
            self.errors_total.load(Ordering::Relaxed)
        ));

        // 活跃连接数
        output.push_str("# HELP active_connections Current number of active connections\n");
        output.push_str("# TYPE active_connections gauge\n");
        output.push_str(&format!(
            "active_connections {}\n",
            self.active_connections.load(Ordering::Relaxed)
        ));

        // 请求延迟（总计和计数，用于 Prometheus 计算平均值）
        output.push_str("# HELP request_latency_us_total Total request latency in microseconds\n");
        output.push_str("# TYPE request_latency_us_total counter\n");
        output.push_str(&format!(
            "request_latency_us_total {}\n",
            self.request_latency_us.load(Ordering::Relaxed)
        ));

        output.push_str("# HELP request_latency_count Total number of latency measurements\n");
        output.push_str("# TYPE request_latency_count counter\n");
        output.push_str(&format!(
            "request_latency_count {}\n",
            self.request_latency_count.load(Ordering::Relaxed)
        ));

        output
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

// 全局指标实例（使用 lazy_static）
lazy_static::lazy_static! {
    pub static ref GLOBAL_METRICS: Arc<Metrics> = Arc::new(Metrics::new());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_recording() {
        let metrics = Metrics::new();

        // 记录一些请求
        metrics.record_request(200);
        metrics.record_request(200);
        metrics.record_request(404);
        metrics.record_request(500);

        assert_eq!(metrics.http_requests_total.load(Ordering::Relaxed), 4);
        assert_eq!(metrics.http_requests_by_status[0].load(Ordering::Relaxed), 2); // 2xx
        assert_eq!(metrics.http_requests_by_status[2].load(Ordering::Relaxed), 1); // 4xx
        assert_eq!(metrics.http_requests_by_status[3].load(Ordering::Relaxed), 1); // 5xx

        // 记录存储操作
        metrics.record_storage_operation();
        metrics.record_storage_operation();
        assert_eq!(metrics.storage_operations.load(Ordering::Relaxed), 2);

        // 记录上传/下载字节
        metrics.record_upload_bytes(1000);
        metrics.record_download_bytes(2000);
        assert_eq!(metrics.upload_bytes.load(Ordering::Relaxed), 1000);
        assert_eq!(metrics.download_bytes.load(Ordering::Relaxed), 2000);

        // 记录错误
        metrics.record_error();
        assert_eq!(metrics.errors_total.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_status_code_buckets() {
        let metrics = Metrics::new();

        // 测试 3xx 状态码
        metrics.record_request(301);
        metrics.record_request(302);
        assert_eq!(metrics.http_requests_by_status[1].load(Ordering::Relaxed), 2); // 3xx

        // 测试 "other" 状态码（1xx 和超出范围的）
        metrics.record_request(100);
        metrics.record_request(600);
        assert_eq!(metrics.http_requests_by_status[4].load(Ordering::Relaxed), 2); // other

        assert_eq!(metrics.http_requests_total.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn test_connection_tracking() {
        let metrics = Metrics::new();

        metrics.connection_opened();
        metrics.connection_opened();
        assert_eq!(metrics.active_connections.load(Ordering::Relaxed), 2);

        metrics.connection_closed();
        assert_eq!(metrics.active_connections.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_latency_tracking() {
        let metrics = Metrics::new();

        let start = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(10));
        metrics.record_latency(start);

        let avg = metrics.average_latency_us();
        assert!(avg >= 10000.0, "Average latency should be at least 10ms (10000us)");
    }

    #[test]
    fn test_metrics_export() {
        let metrics = Metrics::new();
        metrics.record_request(200);
        metrics.record_storage_operation();

        let exported = metrics.export_metrics();
        assert!(exported.contains("http_requests_total 1"));
        assert!(exported.contains("storage_operations_total 1"));
        assert!(exported.contains("# HELP"));
        assert!(exported.contains("# TYPE"));
    }
}
