//! S3/MinIO storage backend

use super::{StorageBackend, StorageError, StorageResult};
use async_trait::async_trait;
use aws_sdk_s3::{Client, Config};
use aws_sdk_s3::config::Credentials;
use bytes::Bytes;

pub struct S3Storage {
    client: Client,
    bucket: String,
}

impl S3Storage {
    pub async fn new(
        bucket: &str,
        region: Option<&str>,
        endpoint: Option<&str>,
    ) -> StorageResult<Self> {
        let region = region.unwrap_or("us-east-1");

        let mut config_builder = Config::builder()
            .region(aws_sdk_s3::config::Region::new(region.to_string()))
            .credentials_provider(Credentials::new(
                std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_else(|_| "minioadmin".to_string()),
                std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".to_string()),
                None,
                None,
                "test",
            ));

        if let Some(endpoint) = endpoint {
            config_builder = config_builder.endpoint_url(endpoint);
            config_builder = config_builder.force_path_style(true);
        }

        let client = Client::from_conf(config_builder.build());

        Ok(Self {
            client,
            bucket: bucket.to_string(),
        })
    }
}

#[async_trait]
impl StorageBackend for S3Storage {
    async fn put(&self, key: &str, data: Bytes) -> StorageResult<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(data.to_vec().into())
            .send()
            .await
            .map_err(|e| StorageError::Internal(format!("S3 put failed: {}", e)))?;

        Ok(())
    }

    async fn get(&self, key: &str) -> StorageResult<Bytes> {
        let result = self.client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                if e.to_string().contains("NoSuchKey") {
                    StorageError::NotFound(key.to_string())
                } else {
                    StorageError::Internal(format!("S3 get failed: {}", e))
                }
            })?;

        let data = result.body.collect().await
            .map_err(|e| StorageError::Internal(format!("Failed to read body: {}", e)))?
            .into_bytes();

        Ok(data)
    }

    async fn exists(&self, key: &str) -> StorageResult<bool> {
        match self.client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if e.to_string().contains("NotFound") => Ok(false),
            Err(e) => Err(StorageError::Internal(format!("S3 head failed: {}", e))),
        }
    }

    async fn delete(&self, key: &str) -> StorageResult<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| StorageError::Internal(format!("S3 delete failed: {}", e)))?;

        Ok(())
    }
}
