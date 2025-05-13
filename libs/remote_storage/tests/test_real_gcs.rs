#![allow(dead_code)]
#![allow(unused)]

use anyhow::Context;
use futures::StreamExt;
use futures::stream::Stream;
use remote_storage::{
    DownloadKind, DownloadOpts, GCSConfig, GenericRemoteStorage, RemotePath, RemoteStorageConfig,
    RemoteStorageKind, StorageMetadata,
};
use std::collections::HashMap;
use std::io::Cursor;
use std::ops::Bound;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;
use test_context::{AsyncTestContext, test_context};
use tokio_util::sync::CancellationToken;

// A minimal working GCS client I can pass around in async context

async fn create_gcs_client() -> anyhow::Result<Arc<GenericRemoteStorage>> {
    let bucket_name = std::env::var("GCS_TEST_BUCKET").expect("GCS_TEST_BUCKET must be set");
    let gcs_config = GCSConfig {
        bucket_name,
        prefix_in_bucket: Some("pageserver/".into()),
        max_keys_per_list_response: Some(100),
        concurrency_limit: std::num::NonZero::new(100).unwrap(),
    };

    let remote_storage_config = RemoteStorageConfig {
        storage: RemoteStorageKind::GCS(gcs_config),
        timeout: Duration::from_secs(120),
        small_timeout: std::time::Duration::from_secs(30),
    };
    Ok(Arc::new(
        GenericRemoteStorage::from_config(&remote_storage_config)
            .await
            .context("remote storage init")?,
    ))
}

struct EnabledGCS {
    client: Arc<GenericRemoteStorage>,
}

impl EnabledGCS {
    async fn setup() -> Self {
        let client = create_gcs_client()
            .await
            .context("gcs client creation")
            .expect("gcs client creation failed");
        EnabledGCS { client }
    }
}

impl AsyncTestContext for EnabledGCS {
    async fn setup() -> Self {
        Self::setup().await
    }
}

#[test_context(EnabledGCS)]
#[tokio::test]
async fn gcs_test_suite_upload_download_delete(ctx: &mut EnabledGCS) -> anyhow::Result<()> {
    let gcs = &ctx.client;

    let source_file = std::io::Cursor::new(vec![0; 256]);
    let file_size = 256 as usize;
    let reader = tokio_util::io::ReaderStream::with_capacity(source_file, file_size);
    // shared function arguments
    let remote_path = RemotePath::from_string("small_file.dat")?;
    let cancel = CancellationToken::new();

    // order matters and that's okay for now
    let res = gcs
        .upload(
            reader,
            file_size,
            &remote_path,
            Some(StorageMetadata::from([(
                "name",
                "pageserver/small_file.dat",
            )])),
            &cancel,
        )
        .await?;

    let opts = DownloadOpts {
        etag: None,
        byte_start: Bound::Unbounded,
        byte_end: Bound::Unbounded,
        version_id: None,
        kind: DownloadKind::Small,
    };
    let res = gcs.download(&remote_path, &opts, &cancel).await?;
    let mut stream = std::pin::pin!(res.download_stream);
    while let Some(item) = stream.next().await {
        let bytes = item?;
        if !bytes.len() == 256 {
            panic!("failed")
        }
    }

    let paths = [remote_path];
    gcs.delete_objects(&paths, &cancel).await?;

    Ok(())
}
