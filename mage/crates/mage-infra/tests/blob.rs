//! `ObjectBlobs` on the local filesystem and in memory: the shared `ArtifactStore` contract, and
//! dev HMAC URLs redeemed through `blob::local::dev_routes` (round trip; tampered signature,
//! expired URL, wrong method, wrong content type and oversize body rejected).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mage_core::domain::{ArtifactKind, OrgId, RunId};
use mage_core::ports::blob::{ArtifactStore, BlobKey, DownloadPolicy, PutOptions, SignedUrl, UploadPolicy};
use mage_core::ports::clock::Clock;
use mage_core::testing::{FakeClock, contract_artifact_store};
use mage_infra::blob::ObjectBlobs;
use mage_infra::blob::local::{DevSigner, dev_routes};
use tower::ServiceExt;
use url::Url;
use uuid::Uuid;

fn signer() -> DevSigner {
    DevSigner::new(&Url::parse("http://localhost:8080/").unwrap(), b"0123456789abcdef0123456789abcdef").unwrap()
}

#[tokio::test]
async fn local_filesystem_backend_passes_the_contract() {
    let scratch = tempfile::tempdir().unwrap();
    let roots = tempfile::tempdir().unwrap();
    let counter = std::sync::atomic::AtomicU32::new(0);
    contract_artifact_store(
        |clock| {
            let root = roots.path().join(counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst).to_string());
            async move { Arc::new(ObjectBlobs::local(&root, signer(), clock).unwrap()) as Arc<dyn ArtifactStore> }
        },
        scratch.path(),
    )
    .await;
}

#[tokio::test]
async fn in_memory_backend_passes_the_contract() {
    let scratch = tempfile::tempdir().unwrap();
    contract_artifact_store(|clock| async move { Arc::new(ObjectBlobs::in_memory(signer(), clock)) as Arc<dyn ArtifactStore> }, scratch.path()).await;
}

#[tokio::test]
async fn large_files_upload_in_parts_and_keep_metadata() {
    let root = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new());
    let blobs = ObjectBlobs::local(root.path(), signer(), clock.clone()).unwrap();
    let src = root.path().join("big.bin");
    let body: Vec<u8> = (0..(mage_infra::blob::PART_BYTES + 123_457)).map(|n| (n % 253) as u8).collect();
    std::fs::write(&src, &body).unwrap();
    let key = BlobKey::parse("org/a/repos/b/bundles/big.tar.zst").unwrap();
    let opts = PutOptions { content_type: "application/zstd".into(), if_absent: true };
    let receipt = blobs.put_file(&key, &src, &opts).await.unwrap();
    assert_eq!((receipt.bytes, receipt.created), (body.len() as u64, true));
    let meta = blobs.head(&key).await.unwrap();
    assert_eq!((meta.bytes, meta.content_type.as_deref(), meta.updated_at), (body.len() as u64, Some("application/zstd"), clock.now()));
    assert_eq!(blobs.get_range(&key, 16_777_200..16_777_220).await.unwrap(), body[16_777_200..16_777_220].to_vec(), "range across the part boundary");
    let again = blobs.put_file(&key, &src, &opts).await.unwrap();
    assert_eq!((again.created, again.sha256), (false, receipt.sha256), "if_absent skips an existing object but still hashes the file");
}

/// Send `method` to a signed URL through the dev routes.
async fn redeem(router: &axum::Router, method: &str, url: &str, headers: &[(&str, &str)], body: Vec<u8>) -> (StatusCode, Vec<u8>) {
    let url = Url::parse(url).unwrap();
    let target = format!("{}?{}", url.path(), url.query().unwrap_or(""));
    let mut request = Request::builder().method(method).uri(target);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = router.clone().oneshot(request.body(Body::from(body)).unwrap()).await.unwrap();
    let status = response.status();
    (status, axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec())
}

fn content_type(url: &SignedUrl) -> String {
    url.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("content-type")).map(|(_, v)| v.clone()).unwrap()
}

#[tokio::test]
async fn dev_urls_round_trip_and_reject_tampering() {
    let root = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new());
    let blobs = Arc::new(ObjectBlobs::local(root.path(), signer(), clock.clone()).unwrap());
    let router = dev_routes(blobs.clone()).unwrap();
    let org = OrgId::from_uuid(Uuid::from_u128(0x0501));
    let key = BlobKey::run_artifact(org, RunId::from_uuid(Uuid::from_u128(9)), ArtifactKind::Video, "attempt-1.webm").unwrap();
    let ttl = Duration::from_secs(900);
    let up = blobs.signed_upload(&key, &UploadPolicy { content_type: "video/webm".into(), max_bytes: 8, ttl }).await.unwrap();
    let ct = content_type(&up);

    assert_eq!(redeem(&router, "PUT", &up.url, &[("content-type", "text/html")], b"webm".to_vec()).await.0, StatusCode::FORBIDDEN, "wrong content type");
    assert_eq!(redeem(&router, "PUT", &up.url, &[("content-type", &ct)], b"123456789".to_vec()).await.0, StatusCode::PAYLOAD_TOO_LARGE, "oversize body");
    let tampered = up.url.replace("max=8", "max=9999");
    assert_eq!(redeem(&router, "PUT", &tampered, &[("content-type", &ct)], b"webm".to_vec()).await.0, StatusCode::FORBIDDEN, "tampered max");
    let other_key = up.url.replace("attempt-1.webm", "attempt-2.webm");
    assert_eq!(redeem(&router, "PUT", &other_key, &[("content-type", &ct)], b"webm".to_vec()).await.0, StatusCode::FORBIDDEN, "URL for another key");
    assert_eq!(redeem(&router, "GET", &up.url, &[], vec![]).await.0, StatusCode::FORBIDDEN, "an upload URL cannot download");
    assert!(blobs.head(&key).await.is_err(), "nothing stored by rejected uploads");

    assert_eq!(redeem(&router, "PUT", &up.url, &[("content-type", &ct)], b"webm".to_vec()).await.0, StatusCode::OK);
    let meta = blobs.head(&key).await.unwrap();
    assert_eq!((meta.bytes, meta.content_type.as_deref()), (4, Some("video/webm")));

    let down = blobs.signed_download(&key, &DownloadPolicy { ttl: Duration::from_secs(60) }).await.unwrap();
    assert_eq!(redeem(&router, "GET", &down.url, &[], vec![]).await, (StatusCode::OK, b"webm".to_vec()));
    assert_eq!(redeem(&router, "PUT", &down.url, &[("content-type", &ct)], b"evil".to_vec()).await.0, StatusCode::FORBIDDEN, "a download URL cannot upload");

    clock.advance(Duration::from_secs(61));
    assert_eq!(redeem(&router, "GET", &down.url, &[], vec![]).await.0, StatusCode::FORBIDDEN, "expired");
    let missing = BlobKey::run_artifact(org, RunId::from_uuid(Uuid::from_u128(9)), ArtifactKind::Video, "absent.webm").unwrap();
    let absent = blobs.signed_download(&missing, &DownloadPolicy { ttl: Duration::from_secs(60) }).await.unwrap();
    assert_eq!(redeem(&router, "GET", &absent.url, &[], vec![]).await.0, StatusCode::NOT_FOUND);
}
