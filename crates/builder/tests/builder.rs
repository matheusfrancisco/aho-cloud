//! The builder end to end with fake `git`, `buildctl` and `pack` scripts (each records its argv,
//! one line per call, arguments separated by `\x1f`) and in-process HTTP servers.
// Helpers outside `#[test]` fns are test code too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use axum::routing::get;
use cloud_api::ApiError;
use cloud_api::internal::{BuildJob, BuildResult, routes};
use cloud_builder::{Builder, Config, router};
use cloud_contract::manifest::{Build, Source};
use flate2::Compression;
use flate2::write::GzEncoder;
use parking_lot::Mutex;
use tar::EntryType;

const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba98";
const DIGEST: &str = "sha256:6c3c624b58dbbcd3c0dd82b4c53f04194d1247c6eebdaab7c610cf7d66709b3b";
const TOKEN: &str = "service-token-for-tests";
const REPO: &str = "https://git.example.test/acme/shop.git";
const IMAGE: &str = "registry.test/aho/shop:r1";

/// A tempdir with `bin/` (fake tools and their records) and `work/` (the builder's work dir).
struct Rig {
    tmp: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("bin")).unwrap();
        Self { tmp }
    }

    fn bin(&self) -> PathBuf {
        self.tmp.path().join("bin")
    }

    fn work(&self) -> PathBuf {
        fs::canonicalize(self.tmp.path().join("work")).unwrap()
    }

    fn config(&self) -> Config {
        let mut c = Config::new("registry.test/aho", self.tmp.path().join("work"), TOKEN);
        c.git = self.bin().join("git");
        c.buildctl = self.bin().join("buildctl");
        c.pack = self.bin().join("pack");
        c.timeout = Duration::from_secs(20);
        c
    }

    fn builder(&self) -> Builder {
        Builder::new(self.config()).unwrap()
    }

    fn script(&self, name: &str, body: &str) {
        let record = self.bin().join(format!("{name}.argv"));
        let text = format!("#!/bin/sh\nline=$(printf '%s\\037' \"$@\")\nprintf '%s\\n' \"$line\" >> '{}'\n{body}\n", record.display());
        let path = self.bin().join(name);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `git` whose checkout writes a small tree (plus `on_checkout`) and whose `HEAD` is `head`.
    fn fake_git(&self, head: &str, on_checkout: &str) {
        self.script(
            "git",
            &format!(
                r#"case "$1" in
  init) mkdir .git ;;
  checkout) mkdir -p docker && printf 'FROM scratch\nCOPY . /\n' > docker/app.Dockerfile && printf 'FROM scratch\n' > Dockerfile && printf 'hi\n' > index.html
    {on_checkout} ;;
  rev-parse) echo {head} ;;
esac"#
            ),
        );
    }

    /// `buildctl` that saves the Dockerfile it was given and a listing of its context, then runs `then`.
    fn fake_buildctl(&self, then: &str) {
        self.script(
            "buildctl",
            &format!(
                r#"meta=; df=; ctx=; file=
while [ $# -gt 0 ]; do
  case "$1" in
    --metadata-file) meta=$2; shift ;;
    --local) case "$2" in context=*) ctx=${{2#context=}} ;; dockerfile=*) df=${{2#dockerfile=}} ;; esac; shift ;;
    --opt) case "$2" in filename=*) file=${{2#filename=}} ;; esac; shift ;;
  esac
  shift
done
cp "$df/$file" '{bin}/seen.Dockerfile'
(cd "$ctx" && find .) > '{bin}/seen.context'
{then}"#,
                bin = self.bin().display()
            ),
        );
    }

    fn fake_pack(&self, digest_line: &str) {
        self.script(
            "pack",
            &format!("echo '===> EXPORTING'\necho '[exporter] Saving {IMAGE}...'\necho '[exporter] *** Images (6c3c624b58db):'\n{digest_line}\necho 'Successfully built image {IMAGE}'"),
        );
    }

    fn calls(&self, name: &str) -> Vec<Vec<String>> {
        let Ok(text) = fs::read_to_string(self.bin().join(format!("{name}.argv"))) else { return Vec::new() };
        text.lines().map(|l| l.strip_suffix('\u{1f}').unwrap_or(l).split('\u{1f}').map(String::from).collect()).collect()
    }

    fn seen(&self, what: &str) -> String {
        fs::read_to_string(self.bin().join(format!("seen.{what}"))).unwrap()
    }

    fn seen_context(&self) -> Vec<String> {
        let mut names: Vec<String> = self.seen("context").lines().map(String::from).collect();
        names.sort();
        names
    }

    /// Work dir entries: each build's scratch dir must be gone once it answers.
    fn leftovers(&self) -> usize {
        fs::read_dir(self.work()).unwrap().count()
    }
}

fn metadata(json: &str) -> String {
    format!("printf '%s' '{json}' > \"$meta\"")
}

fn write_digest() -> String {
    metadata(&format!(r#"{{"containerimage.digest":"{DIGEST}","image.name":"{IMAGE}"}}"#))
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn job(source: Source, build: Build) -> BuildJob {
    BuildJob { project: "shop".into(), release: "r1".into(), source, build, archive_url: None }
}

fn git() -> Source {
    Source::Git { repo: REPO.into(), commit: HEAD.into() }
}

fn dockerfile(path: &str, context: &str) -> Build {
    Build::Dockerfile { path: path.into(), context: context.into() }
}

#[track_caller]
fn built(r: BuildResult) -> (String, String) {
    match r {
        BuildResult::Built { image_digest, log } => (image_digest, log),
        BuildResult::Failed { reason, log } => panic!("build failed: {reason}\n{log}"),
    }
}

#[track_caller]
fn failed(r: BuildResult) -> (String, String) {
    match r {
        BuildResult::Failed { reason, log } => (reason, log),
        BuildResult::Built { image_digest, .. } => panic!("unexpectedly built {image_digest}"),
    }
}

/// The per-build scratch dir, from the buildctl call's `--metadata-file` (its last argument).
fn scratch_of(call: &[String]) -> PathBuf {
    Path::new(call.last().unwrap()).parent().unwrap().to_path_buf()
}

// ---- argv per build kind ----------------------------------------------------------------------

#[tokio::test]
async fn dockerfile_build_fetches_the_exact_commit_and_runs_buildctl_with_push() {
    let rig = Rig::new();
    rig.fake_git(HEAD, "");
    rig.fake_buildctl(&write_digest());
    let mut config = rig.config();
    config.buildkit_addr = Some("tcp://buildkitd:1234".into());
    let builder = Builder::new(config).unwrap();

    let (digest, log) = built(builder.build(&job(git(), dockerfile("docker/app.Dockerfile", "."))).await);
    assert_eq!(digest, DIGEST);
    assert_eq!(
        rig.calls("git"),
        vec![
            strs(&["init"]),
            strs(&["fetch", "--depth", "1", REPO, HEAD]),
            strs(&["checkout", "FETCH_HEAD"]),
            strs(&["rev-parse", "HEAD"])
        ]
    );
    let calls = rig.calls("buildctl");
    assert_eq!(calls.len(), 1);
    let scratch = scratch_of(&calls[0]);
    assert_eq!(scratch.parent().unwrap(), rig.work());
    assert!(scratch.file_name().unwrap().to_string_lossy().starts_with("shop-r1-"));
    let src = scratch.join("src");
    assert_eq!(
        calls[0],
        strs(&[
            "--addr",
            "tcp://buildkitd:1234",
            "build",
            "--progress",
            "plain",
            "--frontend",
            "dockerfile.v0",
            "--local",
            &format!("context={}", src.display()),
            "--local",
            &format!("dockerfile={}", src.join("docker").display()),
            "--opt",
            "filename=app.Dockerfile",
            "--output",
            &format!("type=image,name={IMAGE},push=true"),
            "--metadata-file",
            &scratch.join("metadata.json").display().to_string(),
        ])
    );
    assert_eq!(rig.seen("Dockerfile"), "FROM scratch\nCOPY . /\n");
    assert!(log.contains(&format!("checked out {HEAD}")) && log.contains(&format!("image {DIGEST}")), "{log}");
    assert_eq!(rig.leftovers(), 0);
}

#[tokio::test]
async fn command_build_generates_an_exec_form_dockerfile_outside_the_source() {
    let rig = Rig::new();
    rig.fake_git(HEAD, "");
    rig.fake_buildctl(&write_digest());
    let build = Build::Command { command: strs(&["sh", "-c", "npm ci && npm run build"]), base_image: "node:22-alpine".into() };

    let (digest, _) = built(rig.builder().build(&job(git(), build)).await);
    assert_eq!(digest, DIGEST);
    assert_eq!(
        rig.seen("Dockerfile"),
        "FROM node:22-alpine\nCOPY . /workspace\nWORKDIR /workspace\nRUN [\"sh\",\"-c\",\"npm ci && npm run build\"]\n"
    );
    let call = &rig.calls("buildctl")[0];
    let scratch = scratch_of(call);
    assert!(call.contains(&format!("context={}", scratch.join("src").display())));
    assert!(call.contains(&format!("dockerfile={}", scratch.join("command").display())));
    assert!(call.contains(&"filename=Dockerfile".to_owned()));
    // The context is exactly the checkout; the generated Dockerfile is not copied into the image.
    assert_eq!(rig.seen_context(), strs(&[".", "./.git", "./Dockerfile", "./docker", "./docker/app.Dockerfile", "./index.html"]));
}

#[tokio::test]
async fn buildpack_build_runs_pack_publish_and_reads_the_exporter_digest() {
    let rig = Rig::new();
    rig.fake_git(HEAD, "");
    rig.fake_pack(&format!("echo '[exporter] *** Digest: {DIGEST}'"));
    let build = Build::Buildpack { builder: "paketobuildpacks/builder-jammy-base".into() };

    let (digest, log) = built(rig.builder().build(&job(git(), build)).await);
    assert_eq!(digest, DIGEST);
    let calls = rig.calls("pack");
    assert_eq!(calls.len(), 1);
    let src = PathBuf::from(&calls[0][5]);
    assert_eq!(src.parent().unwrap().parent().unwrap(), rig.work());
    assert_eq!(
        calls[0],
        strs(&["build", IMAGE, "--builder", "paketobuildpacks/builder-jammy-base", "--path", &src.display().to_string(), "--publish"])
    );
    assert!(rig.calls("buildctl").is_empty());
    assert!(log.contains("Successfully built image"), "{log}");
}

#[tokio::test]
async fn prebuilt_oci_returns_the_manifest_digest_without_running_anything() {
    let rig = Rig::new();
    // No fake tools exist: a build that tried to run one would fail.
    let builder = rig.builder();
    let (digest, _) = built(builder.build(&job(Source::Oci { digest: DIGEST.into() }, Build::Prebuilt)).await);
    assert_eq!(digest, DIGEST);

    let (reason, _) = failed(builder.build(&job(Source::Oci { digest: "sha256:latest".into() }, Build::Prebuilt)).await);
    assert!(reason.contains("not a sha256 image digest"), "{reason}");
    for (source, build) in [(Source::Oci { digest: DIGEST.into() }, dockerfile("Dockerfile", ".")), (git(), Build::Prebuilt)] {
        let (reason, _) = failed(builder.build(&job(source, build)).await);
        assert!(reason.contains("prebuilt"), "{reason}");
    }
    assert_eq!(rig.leftovers(), 0);
}

// ---- digests ----------------------------------------------------------------------------------

#[tokio::test]
async fn missing_or_malformed_digests_fail_the_build() {
    for (then, expect) in [
        (metadata(r#"{"image.name":"x"}"#), "no `containerimage.digest`"),
        (metadata(r#"{"containerimage.digest":"sha256:nothex"}"#), "`sha256:nothex` is not a sha256 image digest"),
        (metadata("not json"), "not JSON"),
        (String::new(), "reading buildctl metadata"),
    ] {
        let rig = Rig::new();
        rig.fake_git(HEAD, "");
        rig.fake_buildctl(&then);
        let (reason, _) = failed(rig.builder().build(&job(git(), dockerfile("Dockerfile", "."))).await);
        assert!(reason.contains(expect), "{reason} / {expect}");
    }
    for (line, expect) in [
        ("echo '[exporter] *** Images (6c3c624b58db):'", "no `*** Digest:` line"),
        ("echo '[exporter] *** Digest: sha256:abc'", "`sha256:abc` is not a sha256 image digest"),
    ] {
        let rig = Rig::new();
        rig.fake_git(HEAD, "");
        rig.fake_pack(line);
        let (reason, _) = failed(rig.builder().build(&job(git(), Build::Buildpack { builder: "paketo/builder".into() })).await);
        assert!(reason.contains(expect), "{reason} / {expect}");
    }
}

// ---- source checks ----------------------------------------------------------------------------

#[tokio::test]
async fn commit_mismatch_is_refused_before_building() {
    let rig = Rig::new();
    rig.fake_git(OTHER, "");
    rig.fake_buildctl(&write_digest());
    let (reason, _) = failed(rig.builder().build(&job(git(), dockerfile("Dockerfile", "."))).await);
    assert_eq!(reason, format!("fetched HEAD `{OTHER}` is not the requested commit {HEAD}"));
    assert!(rig.calls("buildctl").is_empty());
    assert_eq!(rig.leftovers(), 0);
}

#[tokio::test]
async fn dockerfile_and_context_cannot_leave_the_checkout_through_its_symlinks() {
    let rig = Rig::new();
    rig.fake_git(HEAD, "ln -s / root && ln -s ../../../etc/passwd Dockerfile.evil && ln -s docker inside");
    rig.fake_buildctl(&write_digest());
    let builder = rig.builder();
    for (build, field) in [(dockerfile("Dockerfile", "root"), "build.context"), (dockerfile("Dockerfile.evil", "."), "build.path")] {
        let (reason, _) = failed(builder.build(&job(git(), build)).await);
        assert_eq!(reason, format!("`{field}` resolves outside the source tree"));
    }
    assert!(rig.calls("buildctl").is_empty());

    // Links that stay inside are followed, and buildctl gets the resolved paths.
    let (digest, _) = built(builder.build(&job(git(), dockerfile("inside/app.Dockerfile", "inside"))).await);
    assert_eq!(digest, DIGEST);
    let call = &rig.calls("buildctl")[0];
    let src = scratch_of(call).join("src");
    assert!(call.contains(&format!("context={}", src.join("docker").display())), "{call:?}");
    assert!(call.contains(&format!("dockerfile={}", src.join("docker").display())), "{call:?}");
}

#[tokio::test]
async fn job_fields_that_would_reach_tool_arguments_unchecked_are_refused_before_anything_runs() {
    let rig = Rig::new();
    rig.fake_git(HEAD, "");
    rig.fake_buildctl(&write_digest());
    rig.fake_pack(&format!("echo '[exporter] *** Digest: {DIGEST}'"));
    let builder = rig.builder();
    let with = |f: fn(&mut BuildJob)| {
        let mut j = job(git(), dockerfile("Dockerfile", "."));
        f(&mut j);
        j
    };
    let cases = [
        (with(|j| j.release = "r1,push=false".into()), "release `r1,push=false` is not a valid image tag"),
        (with(|j| j.project = "../shop".into()), "project `../shop` is not a slug"),
        (with(|j| j.source = Source::Git { repo: "file:///etc".into(), commit: HEAD.into() }), "must be an https:// URL"),
        (with(|j| j.source = Source::Git { repo: "--upload-pack=touch /tmp/x".into(), commit: HEAD.into() }), "must be an https:// URL"),
        (with(|j| j.source = Source::Git { repo: REPO.into(), commit: "main".into() }), "commit `main` is not a full 40-hex id"),
        (with(|j| j.build = Build::Buildpack { builder: "--env=X".into() }), "builder `--env=X` is not an image reference"),
        (
            with(|j| j.build = Build::Command { command: strs(&["make"]), base_image: "alpine\nRUN curl evil".into() }),
            "is not an image reference",
        ),
        (with(|j| j.build = Build::Command { command: Vec::new(), base_image: "alpine".into() }), "command builds need a command"),
        (with(|j| j.build = dockerfile("../outside/Dockerfile", ".")), "`build.path` climbs out with `..`"),
        (with(|j| j.build = dockerfile("Dockerfile", "/")), "`build.context` is absolute"),
        (with(|j| j.source = Source::Archive { hash: "blake3:abc".into() }), "archive sources need `archive_url`"),
        (
            with(|j| {
                j.source = Source::Archive { hash: "blake3:abc".into() };
                j.archive_url = Some("http://127.0.0.1:9/blob".into());
            }),
            "archive hash `blake3:abc` is not `blake3:` plus 64 hex",
        ),
    ];
    for (j, expect) in cases {
        let (reason, _) = failed(builder.build(&j).await);
        assert!(reason.contains(expect), "{reason} / {expect}");
    }
    for tool in ["git", "buildctl", "pack"] {
        assert!(rig.calls(tool).is_empty(), "{tool} ran");
    }
    assert_eq!(rig.leftovers(), 0);
}

// ---- archives ---------------------------------------------------------------------------------

/// Raw tar entries (names written byte for byte, so hostile paths survive), gzip-compressed.
fn tgz(entries: &[(EntryType, &str, &str, &[u8])]) -> Vec<u8> {
    let mut b = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
    for (kind, name, link, data) in entries {
        let mut h = tar::Header::new_ustar();
        h.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
        h.as_old_mut().linkname[..link.len()].copy_from_slice(link.as_bytes());
        h.set_entry_type(*kind);
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append(&h, *data).unwrap();
    }
    b.into_inner().unwrap().finish().unwrap()
}

fn blake3_of(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

type Seen = Arc<Mutex<Vec<Option<String>>>>;

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// Serves `bytes` at `/blobs/src.tar.gz`, recording each request's `Authorization` header.
async fn blob_server(bytes: Vec<u8>) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let record = seen.clone();
    let app = axum::Router::new().route(
        "/blobs/src.tar.gz",
        get(move |headers: HeaderMap| {
            let (record, bytes) = (record.clone(), bytes.clone());
            async move {
                record.lock().push(headers.get("authorization").and_then(|v| v.to_str().ok()).map(String::from));
                bytes
            }
        }),
    );
    (format!("{}/blobs/src.tar.gz", serve(app).await), seen)
}

fn archive_job(hash: String, url: String) -> BuildJob {
    BuildJob { archive_url: Some(url), ..job(Source::Archive { hash }, dockerfile("Dockerfile", ".")) }
}

#[tokio::test]
async fn archive_whose_bytes_do_not_match_the_manifest_hash_is_refused() {
    let rig = Rig::new();
    rig.fake_buildctl(&write_digest());
    let bytes = tgz(&[(EntryType::Regular, "Dockerfile", "", b"FROM scratch\n")]);
    let (url, _) = blob_server(bytes.clone()).await;
    let claimed = blake3_of(b"some other archive");
    let (reason, _) = failed(rig.builder().build(&archive_job(claimed.clone(), url)).await);
    assert_eq!(reason, format!("archive hash mismatch: the manifest names {claimed}, the fetched bytes are {}", blake3_of(&bytes)));
    assert!(rig.calls("buildctl").is_empty());
    assert_eq!(rig.leftovers(), 0);
}

#[tokio::test]
async fn archive_with_traversal_or_escaping_symlink_is_refused() {
    for (entries, expect) in [
        (vec![(EntryType::Regular, "Dockerfile", "", &b"FROM scratch\n"[..]), (EntryType::Regular, "../../evil", "", b"x")], "climbs out"),
        (vec![(EntryType::Regular, "/evil", "", &b"x"[..])], "absolute path"),
        (
            vec![(EntryType::Regular, "Dockerfile", "", &b"FROM scratch\n"[..]), (EntryType::Symlink, "up", "../../..", b"")],
            "points outside",
        ),
        (vec![(EntryType::Symlink, "l", "..", &b""[..]), (EntryType::Regular, "l/evil", "", b"x")], "through a symlink"),
    ] {
        let rig = Rig::new();
        rig.fake_buildctl(&write_digest());
        let bytes = tgz(&entries);
        let (url, _) = blob_server(bytes.clone()).await;
        let (reason, _) = failed(rig.builder().build(&archive_job(blake3_of(&bytes), url)).await);
        assert!(reason.starts_with("extracting the archive: ") && reason.contains(expect), "{reason} / {expect}");
        assert!(rig.calls("buildctl").is_empty());
        assert!(!rig.tmp.path().join("evil").exists() && !rig.work().join("evil").exists());
    }
}

// ---- limits -----------------------------------------------------------------------------------

#[tokio::test]
async fn timeout_kills_the_build_and_its_children_and_returns_failed() {
    let rig = Rig::new();
    let pidfile = rig.bin().join("sleep.pid");
    rig.fake_buildctl(&format!("sleep 30 &\necho $! > '{}'\nwait", pidfile.display()));
    // An archive source: no tool runs before buildctl, so the limit is all buildctl's.
    let bytes = tgz(&[(EntryType::Regular, "Dockerfile", "", b"FROM scratch\n")]);
    let (url, _) = blob_server(bytes.clone()).await;
    let mut config = rig.config();
    config.timeout = Duration::from_secs(3);
    let builder = Builder::new(config).unwrap();

    let started = Instant::now();
    let (reason, log) = failed(builder.build(&archive_job(blake3_of(&bytes), url)).await);
    assert_eq!(reason, "build timed out after 3s");
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert!(log.contains("killed: the build ran out of time"), "{log}");

    // The grandchild `sleep` died with the group (poll: its new parent may not have reaped it yet).
    let pid: i32 = fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    let pid = nix::unistd::Pid::from_raw(pid);
    let gone = Instant::now();
    while nix::sys::signal::kill(pid, None).is_ok() {
        assert!(gone.elapsed() < Duration::from_secs(5), "sleep {pid} survived the timeout");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(rig.leftovers(), 0);
}

#[tokio::test]
async fn concurrency_limit_is_honored() {
    let rig = Rig::new();
    let events = rig.bin().join("events");
    rig.fake_buildctl(&format!("echo start >> '{e}'\nsleep 0.5\necho end >> '{e}'\n{}", write_digest(), e = events.display()));
    // Archive jobs: buildctl is the only tool, so builds that hold a slot together overlap in it.
    let bytes = tgz(&[(EntryType::Regular, "Dockerfile", "", b"FROM scratch\n")]);
    let (url, _) = blob_server(bytes.clone()).await;
    let mut config = rig.config();
    config.concurrency = 2;
    let builder = Arc::new(Builder::new(config).unwrap());

    let tasks: Vec<_> = (0..5)
        .map(|i| {
            let (builder, j) = (builder.clone(), BuildJob { release: format!("r{i}"), ..archive_job(blake3_of(&bytes), url.clone()) });
            tokio::spawn(async move { builder.build(&j).await })
        })
        .collect();
    for t in tasks {
        built(t.await.unwrap());
    }
    let (mut running, mut peak) = (0, 0);
    for event in fs::read_to_string(&events).unwrap().lines() {
        running += if event == "start" { 1 } else { -1 };
        peak = peak.max(running);
    }
    assert_eq!(peak, 2);
    assert_eq!(rig.calls("buildctl").len(), 5);
}

#[tokio::test]
async fn log_is_bounded_to_the_tail() {
    let rig = Rig::new();
    rig.fake_git(HEAD, "");
    rig.fake_buildctl("i=0\nwhile [ $i -lt 3000 ]; do echo \"step $i\" >&2; i=$((i+1)); done\nexit 3");
    let mut config = rig.config();
    config.log_limit = 4096;
    let builder = Builder::new(config).unwrap();
    let (reason, log) = failed(builder.build(&job(git(), dockerfile("Dockerfile", "."))).await);
    assert!(reason.contains("buildctl` failed (exit status: 3)"), "{reason}");
    let (marker, body) = log.split_once('\n').unwrap();
    assert!(marker.starts_with("[… ") && marker.ends_with(" bytes of earlier output dropped …]"), "{marker}");
    assert!(body.len() <= 4096, "{}", body.len());
    assert!(body.contains("step 2999\n") && !body.contains("step 0\n"));
}

// ---- HTTP -------------------------------------------------------------------------------------

#[tokio::test]
async fn build_route_requires_the_service_token() {
    let rig = Rig::new();
    rig.fake_git(HEAD, "");
    rig.fake_buildctl(&write_digest());
    let url = format!("{}{}", serve(router(Arc::new(rig.builder()))).await, routes::BUILD);
    let client = reqwest::Client::new();
    let body = job(git(), dockerfile("Dockerfile", "."));

    for auth in [None, Some("Bearer wrong-token"), Some(&format!("Basic {TOKEN}")[..]), Some(&format!("Bearer {TOKEN}x")[..])] {
        let mut request = client.post(&url).json(&body);
        if let Some(auth) = auth {
            request = request.header("authorization", auth);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 401, "{auth:?}");
        assert_eq!(response.headers()["www-authenticate"], "Bearer");
        assert_eq!(response.json::<ApiError>().await.unwrap().code, "unauthorized");
    }
    assert!(rig.calls("git").is_empty() && rig.calls("buildctl").is_empty());

    let response =
        client.post(&url).bearer_auth(TOKEN).header("content-type", "application/json").body("{\"project\":").send().await.unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(response.json::<ApiError>().await.unwrap().code, "invalid");
    assert!(rig.calls("git").is_empty());
}

#[tokio::test]
async fn archive_job_end_to_end_through_the_http_route() {
    let rig = Rig::new();
    rig.fake_buildctl(&write_digest());
    let global = b"52 comment=0123456789abcdef0123456789abcdef01234567\n";
    let bytes = tgz(&[
        (EntryType::XGlobalHeader, "pax_global_header", "", &global[..]),
        (EntryType::Regular, "Dockerfile", "", b"FROM scratch\nCOPY . /\n"),
        (EntryType::Directory, "app/", "", b""),
        (EntryType::Regular, "app/index.html", "", b"<h1>hi</h1>\n"),
        (EntryType::Symlink, "app/current", "index.html", b""),
    ]);
    let (blob_url, seen) = blob_server(bytes.clone()).await;
    let url = format!("{}{}", serve(router(Arc::new(rig.builder()))).await, routes::BUILD);

    let response =
        reqwest::Client::new().post(&url).bearer_auth(TOKEN).json(&archive_job(blake3_of(&bytes), blob_url)).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let (digest, log) = built(response.json::<BuildResult>().await.unwrap());
    assert_eq!(digest, DIGEST);
    assert_eq!(*seen.lock(), vec![Some(format!("Bearer {TOKEN}"))]);
    assert_eq!(rig.seen("Dockerfile"), "FROM scratch\nCOPY . /\n");
    assert_eq!(rig.seen_context(), strs(&[".", "./Dockerfile", "./app", "./app/current", "./app/index.html"]));
    assert!(log.contains(&format!("fetched {} bytes, {}", bytes.len(), blake3_of(&bytes))), "{log}");
    assert_eq!(rig.leftovers(), 0);
}
