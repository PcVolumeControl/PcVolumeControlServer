//! Checks and downloads against a local HTTP server on 127.0.0.1, with the
//! endpoints and the transport policy injected.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::download::{self, DownloadError, Progress};
use super::http::{Client, HttpError, Policy};
use super::release::tests::release_json;
use super::release::{self, CheckError, CheckOutcome, Endpoints, InstallKind, Release};
use super::verify::VerifyError;
use super::verify::tests::TestKey;
use super::version::ReleaseVersion;

#[derive(Clone, Default)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// Sends no Content-Length; the body ends when the connection closes.
    no_length: bool,
    /// Waits this long after the headers before sending the body.
    stall: Duration,
}

fn ok(body: impl Into<Vec<u8>>) -> Reply {
    Reply {
        status: 200,
        body: body.into(),
        ..Reply::default()
    }
}

fn status(status: u16) -> Reply {
    Reply {
        status,
        ..Reply::default()
    }
}

fn redirect(location: &str) -> Reply {
    Reply {
        status: 302,
        headers: vec![("Location".into(), location.into())],
        ..Reply::default()
    }
}

#[derive(Debug, Clone)]
struct Seen {
    path: String,
    headers: HashMap<String, String>,
}

/// Serves `routes` by path (404 for anything else), one connection at a
/// time per thread, and records each request.
struct Server {
    port: u16,
    routes: Arc<Mutex<HashMap<String, Reply>>>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let routes = Arc::new(Mutex::new(HashMap::<String, Reply>::new()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (r, s) = (Arc::clone(&routes), Arc::clone(&seen));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (r, s) = (Arc::clone(&r), Arc::clone(&s));
                std::thread::spawn(move || serve(stream, &r, &s));
            }
        });
        Self { port, routes, seen }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn route(&self, path: &str, reply: Reply) {
        self.routes.lock().unwrap().insert(path.to_owned(), reply);
    }

    fn endpoints(&self) -> Endpoints {
        Endpoints {
            latest: self.url("/repos/latest"),
            download: self.url("/download"),
            page: self.url("/tag"),
        }
    }

    fn client(&self) -> Client {
        Client::new(Policy::loopback(self.port))
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

fn serve(stream: TcpStream, routes: &Mutex<HashMap<String, Reply>>, seen: &Mutex<Vec<Seen>>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let path = line.split(' ').nth(1).unwrap_or("").to_owned();
    let mut headers = HashMap::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.trim_end().split_once(':') {
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    seen.lock().unwrap().push(Seen {
        path: path.clone(),
        headers,
    });
    let reply = routes
        .lock()
        .unwrap()
        .get(&path)
        .cloned()
        .unwrap_or_else(|| status(404));
    let mut head = format!("HTTP/1.1 {} Test\r\nConnection: close\r\n", reply.status);
    if !reply.no_length {
        head += &format!("Content-Length: {}\r\n", reply.body.len());
    }
    for (name, value) in &reply.headers {
        head += &format!("{name}: {value}\r\n");
    }
    head += "\r\n";
    let mut stream = &stream;
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.flush();
    std::thread::sleep(reply.stall);
    let _ = stream.write_all(&reply.body);
}

fn v(text: &str) -> ReleaseVersion {
    ReleaseVersion::parse(text).unwrap()
}

const INSTALLER: InstallKind = InstallKind::WindowsInstaller { all_users: false };

fn check(server: &Server, current: &str) -> Result<CheckOutcome, CheckError> {
    release::check(
        &server.client(),
        &server.endpoints(),
        &v(current),
        INSTALLER,
    )
}

/// A server publishing release 1.2.3 with an installer and its checksums.
fn publish(server: &Server, installer: &[u8]) {
    let name = "PcVolumeControl-1.2.3-setup.exe";
    let mut json: serde_json::Value =
        serde_json::from_str(&release_json("v1.2.3", &[name, "SHA256SUMS"])).unwrap();
    json["assets"][0]["size"] = installer.len().into();
    server.route("/repos/latest", ok(json.to_string()));
    let digest = Sha256::digest(installer);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let other = "0".repeat(64);
    server.route(
        "/download/v1.2.3/SHA256SUMS",
        ok(format!(
            "{other}  pcvolumecontrol_1.2.3-1_amd64.deb\n{hex}  {name}\n"
        )),
    );
    server.route(&format!("/download/v1.2.3/{name}"), ok(installer));
}

fn available(server: &Server) -> Release {
    match check(server, "1.2.2") {
        Ok(CheckOutcome::Available(release)) => release,
        other => panic!("{other:?}"),
    }
}

#[test]
fn newer_release_is_available() {
    let server = Server::start();
    publish(&server, b"installer");
    let release = available(&server);
    assert_eq!(release.version, v("1.2.3"));
    assert_eq!(release.page_url, server.url("/tag/v1.2.3"));
    let asset = release.asset.unwrap();
    assert_eq!(
        asset.url,
        server.url("/download/v1.2.3/PcVolumeControl-1.2.3-setup.exe")
    );
    let seen = server.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/repos/latest");
    let headers = &seen[0].headers;
    assert_eq!(
        headers.get("user-agent"),
        Some(&format!("pcvolumecontrol/{}", env!("CARGO_PKG_VERSION")))
    );
    assert_eq!(
        headers.get("accept").map(String::as_str),
        Some("application/vnd.github+json")
    );
    // Nothing that identifies the user or the machine.
    let mut names: Vec<&str> = headers.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(names, ["accept", "host", "user-agent"]);
}

#[test]
fn same_or_older_release_is_up_to_date() {
    let server = Server::start();
    publish(&server, b"installer");
    for current in ["1.2.3", "1.3.0"] {
        assert_eq!(
            check(&server, current),
            Ok(CheckOutcome::UpToDate { latest: v("1.2.3") })
        );
    }
}

#[test]
fn no_releases_yet() {
    let server = Server::start();
    assert_eq!(check(&server, "1.0.0"), Ok(CheckOutcome::NoReleases));
}

#[test]
fn rate_limits() {
    let server = Server::start();
    server.route(
        "/repos/latest",
        Reply {
            headers: vec![("X-RateLimit-Remaining".into(), "0".into())],
            ..status(403)
        },
    );
    assert_eq!(
        check(&server, "1.0.0"),
        Err(CheckError::Http(HttpError::RateLimited))
    );
    server.route(
        "/repos/latest",
        Reply {
            headers: vec![
                ("X-RateLimit-Remaining".into(), "40".into()),
                ("Retry-After".into(), "60".into()),
            ],
            ..status(403)
        },
    );
    assert_eq!(
        check(&server, "1.0.0"),
        Err(CheckError::Http(HttpError::RateLimited)),
        "secondary rate limit"
    );
    server.route("/repos/latest", status(429));
    assert_eq!(
        check(&server, "1.0.0"),
        Err(CheckError::Http(HttpError::RateLimited))
    );
    server.route("/repos/latest", status(403));
    assert_eq!(
        check(&server, "1.0.0"),
        Err(CheckError::Http(HttpError::Status(403)))
    );
    server.route("/repos/latest", status(500));
    assert_eq!(
        check(&server, "1.0.0"),
        Err(CheckError::Http(HttpError::Status(500)))
    );
}

#[test]
fn malformed_json() {
    let server = Server::start();
    server.route("/repos/latest", ok("{\"tag_name\": "));
    assert!(matches!(
        check(&server, "1.0.0"),
        Err(CheckError::Malformed(_))
    ));
    server.route("/repos/latest", ok("<html>maintenance</html>"));
    assert!(matches!(
        check(&server, "1.0.0"),
        Err(CheckError::Malformed(_))
    ));
}

#[test]
fn oversized_bodies_are_refused() {
    let server = Server::start();
    let big = vec![b' '; 1024 * 1024 + 1];
    server.route("/repos/latest", ok(big.clone()));
    assert!(matches!(
        check(&server, "1.0.0"),
        Err(CheckError::Http(HttpError::TooLarge(_)))
    ));
    server.route(
        "/repos/latest",
        Reply {
            no_length: true,
            ..ok(big)
        },
    );
    assert!(matches!(
        check(&server, "1.0.0"),
        Err(CheckError::Http(HttpError::TooLarge(_)))
    ));
}

#[test]
fn redirects_are_checked() {
    let server = Server::start();
    publish(&server, b"installer");
    let json = server.routes.lock().unwrap()["/repos/latest"].clone();
    server.route("/moved", json);
    server.route("/repos/latest", redirect("/moved"));
    assert!(matches!(
        check(&server, "1.0.0"),
        Ok(CheckOutcome::Available(_))
    ));

    for location in [
        "https://evil.example/latest".to_owned(),
        "http://localhost:1/latest".to_owned(),
        format!("http://127.0.0.1:{}/x", server.port + 1),
        "//evil.example/latest".to_owned(),
        "ftp://127.0.0.1/x".to_owned(),
    ] {
        server.route("/repos/latest", redirect(&location));
        assert!(
            matches!(
                check(&server, "1.0.0"),
                Err(CheckError::Http(HttpError::Disallowed(_)))
            ),
            "{location}"
        );
    }
    server.route("/repos/latest", status(302));
    assert!(matches!(
        check(&server, "1.0.0"),
        Err(CheckError::Http(HttpError::Disallowed(_)))
    ));

    server.route("/repos/latest", redirect("/loop"));
    server.route("/loop", redirect("/repos/latest"));
    assert_eq!(
        check(&server, "1.0.0"),
        Err(CheckError::Http(HttpError::TooManyRedirects))
    );
}

#[test]
fn slow_server_times_out() {
    let server = Server::start();
    server.route(
        "/repos/latest",
        Reply {
            stall: Duration::from_secs(3),
            ..ok(release_json("v1.2.3", &[]))
        },
    );
    let started = std::time::Instant::now();
    assert_eq!(
        check(&server, "1.0.0"),
        Err(CheckError::Http(HttpError::Timeout))
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

fn files_in(dir: &std::path::Path) -> usize {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| {
                if e.path().is_dir() {
                    files_in(&e.path())
                } else {
                    1
                }
            })
            .sum(),
        Err(_) => 0,
    }
}

#[test]
fn downloads_and_verifies_the_installer() {
    let server = Server::start();
    let installer = vec![7u8; 300_000];
    publish(&server, &installer);
    let release = available(&server);
    let dir = tempfile::tempdir().unwrap();
    let updates = dir.path().join("updates");
    let mut steps = Vec::new();
    let verified =
        download::download_and_verify(&server.client(), &release, None, &updates, &mut |p| {
            steps.push(p)
        })
        .unwrap();
    assert!(!verified.signed);
    assert_eq!(std::fs::read(&verified.path).unwrap(), installer);
    assert_eq!(
        verified.path.file_name().unwrap(),
        "PcVolumeControl-1.2.3-setup.exe"
    );
    assert_eq!(
        steps.first(),
        Some(&Progress::Downloading {
            done: 0,
            total: 300_000
        })
    );
    assert_eq!(steps.last(), Some(&Progress::Verifying));
    assert!(steps.contains(&Progress::Downloading {
        done: 300_000,
        total: 300_000
    }));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let parent = verified.path.parent().unwrap();
        let mode = std::fs::metadata(parent).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
    drop(verified);
    assert_eq!(files_in(&updates), 0, "dropping deletes the download");
}

#[test]
fn checksum_mismatch_leaves_nothing() {
    let server = Server::start();
    publish(&server, b"the real installer");
    let release = available(&server);
    server.route(
        "/download/v1.2.3/PcVolumeControl-1.2.3-setup.exe",
        ok(b"a tampered install".to_vec()),
    );
    let dir = tempfile::tempdir().unwrap();
    let result =
        download::download_and_verify(&server.client(), &release, None, dir.path(), &mut |_| {});
    assert_eq!(
        result.unwrap_err(),
        DownloadError::Verify(VerifyError::ChecksumMismatch)
    );
    assert_eq!(files_in(dir.path()), 0);
}

#[test]
fn download_failures_leave_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let download = |server: &Server, release: &Release| {
        download::download_and_verify(&server.client(), release, None, dir.path(), &mut |_| {})
            .unwrap_err()
    };
    let path = "/download/v1.2.3/PcVolumeControl-1.2.3-setup.exe";

    // Longer than the listed size.
    let server = Server::start();
    publish(&server, b"installer");
    let release = available(&server);
    server.route(path, ok(b"installer and more".to_vec()));
    assert_eq!(download(&server, &release), DownloadError::WrongSize);
    server.route(
        path,
        Reply {
            no_length: true,
            ..ok(b"installer and more".to_vec())
        },
    );
    assert_eq!(download(&server, &release), DownloadError::WrongSize);
    // Not text.
    server.route("/download/v1.2.3/SHA256SUMS", ok(vec![0xff, 0xfe]));
    assert_eq!(
        download(&server, &release),
        DownloadError::Verify(VerifyError::SumsNotText)
    );
    publish(&server, b"installer");
    // Shorter.
    server.route(
        path,
        Reply {
            no_length: true,
            ..ok(b"inst".to_vec())
        },
    );
    assert_eq!(download(&server, &release), DownloadError::WrongSize);
    // Redirected off the allowlist.
    server.route(path, redirect("https://evil.example/setup.exe"));
    assert!(matches!(
        download(&server, &release),
        DownloadError::Http(HttpError::Disallowed(_))
    ));
    // Stalled.
    server.route(
        path,
        Reply {
            stall: Duration::from_secs(3),
            ..ok(b"installer".to_vec())
        },
    );
    assert_eq!(
        download(&server, &release),
        DownloadError::Http(HttpError::Timeout)
    );
    // No entry in SHA256SUMS.
    server.route(
        "/download/v1.2.3/SHA256SUMS",
        ok(format!("{}  other\n", "0".repeat(64))),
    );
    assert!(matches!(
        download(&server, &release),
        DownloadError::Verify(VerifyError::MissingEntry(_))
    ));
    // Oversized SHA256SUMS.
    server.route("/download/v1.2.3/SHA256SUMS", ok(vec![b'0'; 65 * 1024]));
    assert!(matches!(
        download(&server, &release),
        DownloadError::Http(HttpError::TooLarge(_))
    ));
    assert_eq!(files_in(dir.path()), 0);

    let mut no_sums = release.clone();
    no_sums.checksums = None;
    assert_eq!(download(&server, &no_sums), DownloadError::NoChecksums);
    let mut huge = release;
    huge.asset.as_mut().unwrap().size = 101 * 1024 * 1024;
    assert_eq!(download(&server, &huge), DownloadError::TooLarge);
}

#[cfg(unix)]
#[test]
fn saving_failures_are_not_network_errors() {
    use std::os::unix::fs::PermissionsExt;
    let server = Server::start();
    publish(&server, b"installer");
    let release = available(&server);
    let dir = tempfile::tempdir().unwrap();
    let updates = dir.path().join("updates");
    std::fs::create_dir(&updates).unwrap();
    std::fs::set_permissions(&updates, std::fs::Permissions::from_mode(0o500)).unwrap();
    let result =
        download::download_and_verify(&server.client(), &release, None, &updates, &mut |_| {});
    std::fs::set_permissions(&updates, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(result, Err(DownloadError::Io(_))), "{result:?}");
}

/// Publishes a signature for the checksums and returns the release as a
/// check sees it.
fn publish_signature(server: &Server, signature: &[u8]) -> Release {
    let mut json: serde_json::Value = serde_json::from_str(&{
        let routes = server.routes.lock().unwrap();
        String::from_utf8(routes["/repos/latest"].body.clone()).unwrap()
    })
    .unwrap();
    json["assets"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"name": "SHA256SUMS.minisig", "size": signature.len()}));
    server.route("/repos/latest", ok(json.to_string()));
    server.route("/download/v1.2.3/SHA256SUMS.minisig", ok(signature));
    available(server)
}

#[test]
fn signature_is_required_when_a_key_is_built_in() {
    let server = Server::start();
    publish(&server, b"installer");
    let key = TestKey::new();
    let dir = tempfile::tempdir().unwrap();
    let download = |release: &Release, key: &TestKey| {
        download::download_and_verify(
            &server.client(),
            release,
            Some(&key.public),
            dir.path(),
            &mut |_| {},
        )
    };

    // No signature asset.
    let unsigned = available(&server);
    assert_eq!(
        download(&unsigned, &key).unwrap_err(),
        DownloadError::Verify(VerifyError::MissingSignature)
    );
    // Listed but gone.
    let sums = server.routes.lock().unwrap()["/download/v1.2.3/SHA256SUMS"]
        .body
        .clone();
    let release = publish_signature(&server, &key.sign(&sums));
    server.route("/download/v1.2.3/SHA256SUMS.minisig", status(404));
    assert_eq!(
        download(&release, &key).unwrap_err(),
        DownloadError::Verify(VerifyError::MissingSignature)
    );
    // Signed by another key.
    let release = publish_signature(&server, &TestKey::new().sign(&sums));
    assert_eq!(
        download(&release, &key).unwrap_err(),
        DownloadError::Verify(VerifyError::BadSignature)
    );
    // Valid.
    let release = publish_signature(&server, &key.sign(&sums));
    let verified = download(&release, &key).unwrap();
    assert!(verified.signed);
    drop(verified);
    // Checksums changed after signing.
    server.route(
        "/download/v1.2.3/SHA256SUMS",
        ok([&sums[..], b"\n"].concat()),
    );
    assert_eq!(
        download(&release, &key).unwrap_err(),
        DownloadError::Verify(VerifyError::BadSignature)
    );
    assert_eq!(files_in(dir.path()), 0);
}

/// Asks the real GitHub API once. Run with `--ignored --nocapture`.
#[test]
#[ignore = "contacts api.github.com"]
fn live_check() {
    let result = release::check(
        &Client::new(Policy::github()),
        &Endpoints::github(),
        &super::version::current(),
        InstallKind::LinuxTarball,
    );
    println!("{result:?}");
    assert!(result.is_ok(), "{result:?}");
}
