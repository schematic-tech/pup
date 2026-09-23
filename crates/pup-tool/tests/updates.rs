use std::{fs, process::Command};

fn command(profile: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pup"));
    command
        .env("PUP_CONFIG_DIR", profile)
        .env("NO_COLOR", "1")
        .env_remove("PUP_NO_UPDATE_CHECK");
    command
}

#[test]
fn cached_notice_is_styled_as_text_on_stderr_and_keeps_json_and_version_output_clean() {
    let profile = tempfile::tempdir().unwrap();
    let cache = profile.path().join("updates");
    fs::create_dir(&cache).unwrap();
    fs::write(
        cache.join("releases.json"),
        br#"{"pup-tool":"999.0.0","another-tool":"1.0.0"}"#,
    )
    .unwrap();
    let output = command(profile.path()).arg("--version").output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        concat!("pup ", env!("CARGO_PKG_VERSION"), "\n")
    );
    let notice = String::from_utf8(output.stderr).unwrap();
    assert!(notice.contains("Pup 999.0.0 is available"));
    assert!(notice.contains(concat!("(installed: ", env!("CARGO_PKG_VERSION"), ")")));
    assert!(notice.contains("curl -fsSL https://get.schematic.tech/pup.sh | bash\n"));
    assert!(!notice.contains('\x1b'));

    let output = command(profile.path()).args(["logout", "--json"]).output().unwrap();
    assert!(output.status.success());
    serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
    assert!(output.stderr.is_empty());

    let output = command(profile.path())
        .env("PUP_NO_UPDATE_CHECK", "1")
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(!cache.join("last-attempt").exists());
}

#[test]
fn missing_or_corrupt_cache_is_silent_and_a_failed_refresh_does_not_spawn_again_this_hour() {
    let profile = tempfile::tempdir().unwrap();
    let cache = profile.path().join("updates");
    fs::create_dir(&cache).unwrap();
    fs::write(cache.join("last-attempt"), b"").unwrap();
    for contents in [None, Some("<error>offline</error>"), Some(r#"{"pup-tool":"0.0.1"}"#)] {
        if let Some(contents) = contents {
            fs::write(cache.join("releases.json"), contents).unwrap();
        }
        let output = command(profile.path()).arg("--version").output().unwrap();
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
    }
    assert!(!cache.join("refresh.lock").exists());
}

#[test]
fn foreground_exits_while_the_detached_worker_is_waiting_on_the_network() {
    use fs2::FileExt;
    use std::{
        fs::OpenOptions,
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::{Duration, Instant},
    };

    let profile = tempfile::tempdir().unwrap();
    // Stall a local HTTPS proxy. This exercises the real child and its production
    // URL without contacting the internet or making the URL configurable for users.
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    let start = Instant::now();
    let output = command(profile.path())
        .env("HTTPS_PROXY", &proxy_url)
        .env("https_proxy", &proxy_url)
        .env("ALL_PROXY", &proxy_url)
        .env("all_proxy", &proxy_url)
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(start.elapsed() < Duration::from_secs(5));
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mut socket, _) = loop {
        match proxy.accept() {
            Ok(connection) => break connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "detached worker never connected");
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    };
    socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        request.extend_from_slice(&byte);
    }
    assert!(request.starts_with(b"CONNECT get.schematic.tech:443 "));
    let cache = profile.path().join("updates");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(cache.join("refresh.lock"))
        .unwrap();
    assert!(
        lock.try_lock_exclusive().is_err(),
        "worker should still be fetching after the foreground exits"
    );
    let output = command(profile.path()).arg("--version").output().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(
        proxy.accept().is_err(),
        "another startup must not create a second fetch"
    );

    socket
        .write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .unwrap();
    drop(socket);
    let deadline = Instant::now() + Duration::from_secs(5);
    while lock.try_lock_exclusive().is_err() {
        assert!(Instant::now() < deadline, "failed worker did not release its lock");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!cache.join("releases.json").exists());
}
