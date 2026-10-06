//! `ArtifactStore` contract (storage.md "Testing" → `ArtifactStore`). Signed URL redemption is
//! backend-specific (GCS / dev HMAC / the fake's grants) and tested per adapter; this suite checks
//! the URL shape every backend shares.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::artifact::ArtifactKind;
use crate::domain::ids::{BlobOid, OrgId, RepoId, RunId, TreeOid};
use crate::ports::blob::{
    ArtifactStore, BlobError, BlobKey, DownloadPolicy, HttpMethod, MAX_SIGNED_URL_TTL, PutOptions, PutReceipt, UploadPolicy,
};
use crate::ports::clock::Clock;
use crate::ports::common::PageRequest;
use crate::testing::clock::FakeClock;

/// Runs every `ArtifactStore` scenario, each against a fresh store from `make`. `scratch` is an
/// empty writable directory for the file-streaming methods.
pub async fn contract_artifact_store<F, Fut>(make: F, scratch: &Path)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = Arc<dyn ArtifactStore>>,
{
    macro_rules! scenario {
        ($name:ident $(, $arg:expr)?) => {{
            let clock = Arc::new(FakeClock::new());
            let store = make(clock.clone()).await;
            $name(&clock, &*store $(, $arg)?).await;
        }};
    }
    scenario!(put_get_head_round_trip);
    scenario!(if_absent_keeps_the_first_bytes);
    scenario!(ranges_follow_the_object_store_rules);
    scenario!(prefixes_list_and_delete_segment_wise);
    scenario!(files_stream_in_and_out, scratch);
    scenario!(signed_urls_carry_method_expiry_and_signed_headers);
}

fn org(n: u128) -> OrgId {
    OrgId::from_uuid(Uuid::from_u128(0x0400 + n))
}

fn run_key(org: OrgId, name: &str) -> BlobKey {
    BlobKey::run_artifact(org, RunId::from_uuid(Uuid::from_u128(1)), ArtifactKind::Log, name).expect("valid artifact key")
}

fn text(if_absent: bool) -> PutOptions {
    PutOptions { content_type: "text/plain; charset=utf-8".into(), if_absent }
}

fn receipt(body: &[u8], created: bool) -> PutReceipt {
    PutReceipt { bytes: body.len() as u64, sha256: Sha256::digest(body).into(), created }
}

async fn put_get_head_round_trip(clock: &FakeClock, store: &dyn ArtifactStore) {
    let key = run_key(org(1), "attempt-1.log");
    let body = b"line one\nline two\n".to_vec();
    assert_eq!(store.get(&key, 1024).await, Err(BlobError::NotFound));
    assert_eq!(store.head(&key).await, Err(BlobError::NotFound));
    assert_eq!(store.get_range(&key, 0..1).await, Err(BlobError::NotFound));

    assert_eq!(store.put(&key, body.clone(), &text(false)).await, Ok(receipt(&body, true)));
    assert_eq!(store.get(&key, body.len() as u64).await, Ok(body.clone()), "max_bytes equal to the size is enough");
    assert_eq!(store.get(&key, 4).await, Err(BlobError::TooLarge { limit: 4 }));
    let meta = store.head(&key).await.unwrap();
    assert_eq!((meta.bytes, meta.content_type.as_deref(), meta.updated_at), (body.len() as u64, Some("text/plain; charset=utf-8"), clock.now()));

    clock.advance(Duration::from_secs(5));
    let replaced = b"replaced".to_vec();
    assert_eq!(store.put(&key, replaced.clone(), &text(false)).await, Ok(receipt(&replaced, true)), "a plain put overwrites");
    assert_eq!(store.get(&key, 1024).await, Ok(replaced));
    assert_eq!(store.head(&key).await.unwrap().updated_at, clock.now());

    store.delete(&key).await.unwrap();
    assert_eq!(store.get(&key, 1024).await, Err(BlobError::NotFound));
    assert_eq!(store.delete(&key).await, Ok(()), "deleting a missing object is Ok");
}

async fn if_absent_keeps_the_first_bytes(_: &FakeClock, store: &dyn ArtifactStore) {
    let key = BlobKey::blob_content(org(1), BlobOid::from_bytes([0xab; 20]));
    let first = b"package main\n".to_vec();
    assert_eq!(store.put(&key, first.clone(), &text(true)).await, Ok(receipt(&first, true)));
    let second = b"something else".to_vec();
    assert_eq!(store.put(&key, second.clone(), &text(true)).await, Ok(receipt(&second, false)), "existing object: created false");
    assert_eq!(store.get(&key, 1024).await, Ok(first), "the existing object is not rewritten");
}

async fn ranges_follow_the_object_store_rules(_: &FakeClock, store: &dyn ArtifactStore) {
    let key = run_key(org(1), "range.log");
    store.put(&key, b"0123456789".to_vec(), &text(false)).await.unwrap();
    assert_eq!(store.get_range(&key, 2..5).await, Ok(b"234".to_vec()));
    assert_eq!(store.get_range(&key, 0..10).await, Ok(b"0123456789".to_vec()));
    assert_eq!(store.get_range(&key, 7..100).await, Ok(b"789".to_vec()), "end beyond the size is cut");
    assert!(matches!(store.get_range(&key, 10..12).await, Err(BlobError::Invalid(_))), "start at the size");
    assert!(matches!(store.get_range(&key, 4..4).await, Err(BlobError::Invalid(_))), "empty range");
    #[allow(clippy::reversed_empty_ranges)]
    let inverted = 5..3;
    assert!(matches!(store.get_range(&key, inverted).await, Err(BlobError::Invalid(_))));
}

async fn prefixes_list_and_delete_segment_wise(_: &FakeClock, store: &dyn ArtifactStore) {
    let mine = [
        run_key(org(1), "a.log"),
        run_key(org(1), "b.log"),
        BlobKey::blob_content(org(1), BlobOid::from_bytes([1; 20])),
        BlobKey::source_bundle(org(1), RepoId::from_uuid(Uuid::from_u128(2)), TreeOid::from_bytes([3; 20])),
    ];
    let theirs = run_key(org(2), "a.log");
    let sibling = BlobKey::parse("org/a/x").unwrap();
    let not_sibling = BlobKey::parse("org/ab/x").unwrap();
    for key in mine.iter().chain([&theirs, &sibling, &not_sibling]) {
        store.put(key, key.as_str().as_bytes().to_vec(), &text(false)).await.unwrap();
    }

    let prefix = BlobKey::org_prefix(org(1));
    let mut listed = Vec::new();
    let mut page = PageRequest::first(3);
    loop {
        let result = store.list_prefix(&prefix, page.clone()).await.unwrap();
        assert!(result.items.len() <= 3);
        listed.extend(result.items.iter().map(|(key, meta)| {
            assert_eq!(meta.bytes, key.as_str().len() as u64);
            key.clone()
        }));
        match result.next {
            Some(cursor) => page = page.after(cursor),
            None => break,
        }
    }
    let mut expected = mine.to_vec();
    expected.sort();
    assert_eq!(listed, expected, "every key under the prefix, in key order, across pages");

    let a = BlobKey::parse("org/a").unwrap();
    let under_a: Vec<BlobKey> = store.list_prefix(&a, PageRequest::first(10)).await.unwrap().items.into_iter().map(|(k, _)| k).collect();
    assert_eq!(under_a, vec![sibling.clone()], "`org/a` does not cover `org/ab/…`");

    assert_eq!(store.delete_prefix(&prefix).await, Ok(4));
    assert!(store.list_prefix(&prefix, PageRequest::first(10)).await.unwrap().items.is_empty());
    assert!(store.head(&theirs).await.is_ok(), "other tenants untouched");
    assert_eq!(store.delete_prefix(&prefix).await, Ok(0));
    assert_eq!(store.delete_prefix(&a).await, Ok(1));
    assert!(store.head(&not_sibling).await.is_ok());
}

async fn files_stream_in_and_out(_: &FakeClock, store: &dyn ArtifactStore, scratch: &Path) {
    let src = scratch.join("bundle.tar.zst");
    let body: Vec<u8> = (0..70_000u32).map(|n| (n % 251) as u8).collect();
    std::fs::write(&src, &body).expect("write scratch file");
    let key = BlobKey::source_bundle(org(1), RepoId::from_uuid(Uuid::from_u128(5)), TreeOid::from_bytes([9; 20]));
    let opts = PutOptions { content_type: "application/zstd".into(), if_absent: false };
    assert_eq!(store.put_file(&key, &src, &opts).await, Ok(receipt(&body, true)));
    assert_eq!(store.get(&key, body.len() as u64).await, Ok(body.clone()));
    assert!(store.put_file(&key, &scratch.join("missing"), &opts).await.is_err(), "a missing source file fails");

    let dst = scratch.join("download.tar.zst");
    assert_eq!(store.get_to_file(&key, &dst).await, Ok(body.len() as u64));
    assert_eq!(std::fs::read(&dst).expect("read download"), body);
    assert!(!scratch.join("download.tar.zst.tmp").exists(), "the temporary file is renamed into place");
    let absent = BlobKey::source_bundle(org(1), RepoId::from_uuid(Uuid::from_u128(5)), TreeOid::from_bytes([8; 20]));
    assert_eq!(store.get_to_file(&absent, &scratch.join("absent")).await, Err(BlobError::NotFound));
    assert!(!scratch.join("absent").exists());
}

async fn signed_urls_carry_method_expiry_and_signed_headers(clock: &FakeClock, store: &dyn ArtifactStore) {
    let key = BlobKey::run_artifact(org(1), RunId::from_uuid(Uuid::from_u128(1)), ArtifactKind::Video, "attempt-1.webm").unwrap();
    let ttl = Duration::from_secs(15 * 60);
    let upload = store.signed_upload(&key, &UploadPolicy { content_type: "video/webm".into(), max_bytes: 512 << 20, ttl }).await.unwrap();
    assert_eq!((upload.method, upload.expires_at), (HttpMethod::Put, clock.now() + ttl));
    let content_type = upload.headers.iter().find(|(name, _)| name.eq_ignore_ascii_case("content-type")).map(|(_, v)| v.as_str());
    assert_eq!(content_type, Some("video/webm"), "the content type is a signed header: {:?}", upload.headers);
    assert!(!upload.url.is_empty());

    let download = store.signed_download(&key, &DownloadPolicy { ttl: Duration::from_secs(60) }).await.unwrap();
    assert_eq!((download.method, download.expires_at), (HttpMethod::Get, clock.now() + Duration::from_secs(60)));
    assert_ne!(download.url, upload.url);
    let other = store.signed_download(&run_key(org(1), "other.log"), &DownloadPolicy { ttl: Duration::from_secs(60) }).await.unwrap();
    assert_ne!(other.url, download.url, "URLs are per key");

    let ceiling = store.signed_download(&key, &DownloadPolicy { ttl: MAX_SIGNED_URL_TTL }).await.unwrap();
    assert_eq!(ceiling.expires_at, clock.now() + MAX_SIGNED_URL_TTL);
    for ttl in [Duration::ZERO, MAX_SIGNED_URL_TTL + Duration::from_secs(1)] {
        assert!(matches!(store.signed_download(&key, &DownloadPolicy { ttl }).await, Err(BlobError::Invalid(_))), "ttl {ttl:?}");
        let policy = UploadPolicy { content_type: "video/webm".into(), max_bytes: 1, ttl };
        assert!(matches!(store.signed_upload(&key, &policy).await, Err(BlobError::Invalid(_))), "ttl {ttl:?}");
    }
}
