//! `juliaup server add/list/remove` against a local server that publishes a
//! channel of its own, end to end through `juliaup add` and the launcher.

#![cfg(unix)]

use flate2::write::GzEncoder;
use flate2::Compression;
use predicates::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tiny_http::{Header, Response, Server};

mod utils;
use utils::TestEnv;

const CHANNEL: &str = "mock-1.0";
const MARKER: &str = "MOCK JULIA";

fn version_key() -> String {
    // The build metadata needs at least four parts, the second of which
    // juliaup reads as the architecture; the rest is a label.
    "1.0.0+mock.x64.mock.mock".to_string()
}

/// A tarball shaped like a distribution: one top-level directory holding a
/// `bin/julia` that is a shell script printing a marker.
fn build_distribution_tarball() -> Vec<u8> {
    let script = format!("#!/bin/sh\necho '{MARKER}'\n");
    let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
    {
        let mut tar = tar::Builder::new(&mut gz);
        let mut header = tar::Header::new_gnu();
        header.set_size(script.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "mock-dist/bin/julia", script.as_bytes())
            .unwrap();
        tar.finish().unwrap();
    }
    gz.finish().unwrap()
}

struct MockServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(db_version: &str, tarball: Vec<u8>) -> Self {
        let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
        let port = server.server_addr().to_ip().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}");
        let triple = juliaup::get_juliaup_target();
        let db_path = format!("/juliaup/versiondb/versiondb-{db_version}-{triple}.json");
        let db = format!(
            r#"{{"Version":"{db_version}","AvailableVersions":{{"{key}":{{"UrlPath":"dist/mock.tar.gz"}}}},"AvailableChannels":{{"{CHANNEL}":{{"Version":"{key}"}}}}}}"#,
            key = version_key()
        );
        let db_version = db_version.to_string();

        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let server = Arc::clone(&server);
            let stop = Arc::clone(&stop);
            thread::spawn(move || loop {
                match server.recv_timeout(Duration::from_millis(100)) {
                    Ok(Some(request)) => {
                        let url = request.url().to_string();
                        let response = if url.ends_with("CHANNELDBVERSION") {
                            Response::from_string(db_version.clone())
                        } else if url == db_path {
                            Response::from_string(db.clone())
                        } else if url == "/dist/mock.tar.gz" {
                            Response::from_data(tarball.clone())
                        } else {
                            Response::from_string("not found")
                                .with_status_code(404)
                                .with_header(
                                    Header::from_bytes(&b"etag"[..], &b"\"mock\""[..]).unwrap(),
                                )
                        };
                        let _ = request.respond(response);
                    }
                    Ok(None) => {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            })
        };

        MockServer {
            base_url,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[test]
fn server_add_resolves_and_installs_a_foreign_channel() {
    let env = TestEnv::new();
    let server = MockServer::start("1.0.0", build_distribution_tarball());

    env.juliaup()
        .args(["server", "add", &server.base_url, "--name", "mock"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Added"));

    env.juliaup()
        .args(["server", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("primary"))
        .stdout(predicate::str::contains("mock"))
        .stdout(predicate::str::contains("1.0.0"));

    let config = std::fs::read_to_string(env.config_path()).unwrap();
    assert!(config.contains("\"Servers\""), "config: {config}");
    assert!(config.contains(&server.base_url), "config: {config}");

    // The channel is visible without any download having happened.
    env.juliaup()
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains(CHANNEL));

    // Adding the same server twice, by URL or by name, is refused.
    env.juliaup()
        .args(["server", "add", &format!("{}/", server.base_url)])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already configured"));
    env.juliaup()
        .args(["server", "add", "http://127.0.0.1:9/", "--name", "mock"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already configured"));

    // Installing resolves the channel through the added server; the absolute
    // UrlPath makes the download go there rather than to the primary.
    env.juliaup().args(["add", CHANNEL]).assert().success();

    env.julia()
        .arg(format!("+{CHANNEL}"))
        .assert()
        .success()
        .stdout(predicate::str::contains(MARKER));

    // Removing the server drops the channel from what can be added, but not
    // what is installed.
    env.juliaup()
        .args(["server", "remove", "mock"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Removed"));

    let config = std::fs::read_to_string(env.config_path()).unwrap();
    assert!(!config.contains("\"Servers\""), "config: {config}");
    let port = server.base_url.rsplit(':').next().unwrap();
    assert!(
        !env.depot_path()
            .join("juliaup")
            .join("servers")
            .join(format!("127.0.0.1_{port}"))
            .exists(),
        "server cache should be gone"
    );

    env.juliaup()
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains(CHANNEL).not());

    env.juliaup()
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains(CHANNEL));

    env.julia()
        .arg(format!("+{CHANNEL}"))
        .assert()
        .success()
        .stdout(predicate::str::contains(MARKER));
}

#[test]
fn server_add_rejects_what_is_not_a_juliaup_server() {
    let env = TestEnv::new();

    // Nothing listens here.
    env.juliaup()
        .args(["server", "add", "http://127.0.0.1:9/"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "does not serve a juliaup version database",
        ));

    // Plain HTTP off localhost is refused before any request is made.
    env.juliaup()
        .args(["server", "add", "http://mirror.example.com/"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("HTTPS"));

    let config = std::fs::read_to_string(env.config_path()).unwrap_or_default();
    assert!(!config.contains("\"Servers\""), "config: {config}");

    env.juliaup()
        .args(["server", "remove", "nothing"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("No server"));
}

#[test]
fn server_list_without_servers_shows_only_the_primary() {
    let env = TestEnv::new();
    env.juliaup()
        .args(["server", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("primary"))
        .stdout(predicate::str::contains("julialang-s3.julialang.org"));
}
