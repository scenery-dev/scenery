//! Tests exercise the real socket plumbing over a loopback Unix socket this
//! test creates and tears down itself — never a real daemon socket, and
//! never a process we didn't spawn ourselves (both ends are this same test
//! process).

use super::*;
use std::os::unix::net::UnixListener;

fn temp_socket_path(name: &str) -> camino::Utf8PathBuf {
    // AF_UNIX socket paths are limited to `sizeof(sockaddr_un.sun_path)`
    // (104 bytes on macOS, 108 on Linux) — `std::env::temp_dir()` on macOS
    // is already a long per-process path (`/var/folders/.../T/`), so keep
    // the filename itself very short rather than risk "path must be
    // shorter than SUN_LEN". `/tmp` is a short, stable fallback available
    // on every platform this crate targets.
    let short_name: String = name.chars().take(6).collect();
    let unique = format!(
        "vh{}-{:x}.sock",
        short_name,
        std::process::id()
            ^ (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u32)
    );
    camino::Utf8PathBuf::from_path_buf(std::path::PathBuf::from("/tmp").join(unique)).unwrap()
}

#[test]
fn peer_creds_reports_our_own_pid_and_uid_on_a_loopback_socket() {
    let path = temp_socket_path("creds");
    let listener = UnixListener::bind(path.as_std_path()).unwrap();

    let accept_thread = std::thread::spawn(move || {
        let (_server_stream, _) = listener.accept().unwrap();
        // Keep the connection open briefly so the client side can read
        // peer creds before it's torn down.
        std::thread::sleep(Duration::from_millis(100));
    });

    let client = UnixStream::connect(path.as_std_path()).unwrap();
    let creds = peer_creds(&client);

    // SAFETY: getuid takes no arguments and cannot fail.
    let our_uid = unsafe { libc::getuid() };

    assert_eq!(creds.pid, Some(std::process::id()));
    assert_eq!(creds.uid, Some(our_uid));

    drop(client);
    accept_thread.join().unwrap();
    let _ = std::fs::remove_file(path.as_std_path());
}

#[test]
fn query_version_parses_a_crafted_initialize_response() {
    let path = temp_socket_path("version");
    let listener = UnixListener::bind(path.as_std_path()).unwrap();

    let server_thread = std::thread::spawn(move || {
        let (mut server_stream, _) = listener.accept().unwrap();
        let _request = protocol_frame::read_framed_blocking(&mut server_stream)
            .unwrap()
            .unwrap();
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "vh-takeover-probe",
            "result": {
                "protocolVersion": "2024-11-05",
                "serverInfo": { "name": "vectorhawkd", "version": "1.0.42" },
                "capabilities": { "tools": {} }
            }
        });
        let body = serde_json::to_vec(&response).unwrap();
        protocol_frame::write_framed_blocking(&mut server_stream, &body).unwrap();
    });

    let mut client = UnixStream::connect(path.as_std_path()).unwrap();
    let version = query_version(&mut client, Duration::from_secs(2));

    assert_eq!(version, Some(semver::Version::parse("1.0.42").unwrap()));

    server_thread.join().unwrap();
    let _ = std::fs::remove_file(path.as_std_path());
}

#[test]
fn query_version_returns_none_when_nothing_is_listening() {
    let path = temp_socket_path("nobody-home");
    // Nothing bound here — the connect itself must fail cleanly.
    let (creds, version) = connect_and_probe(&path, Duration::from_millis(200));
    assert_eq!(creds, PeerCreds::default());
    assert_eq!(version, None);
}

#[test]
fn query_version_returns_none_on_malformed_response() {
    let path = temp_socket_path("malformed");
    let listener = UnixListener::bind(path.as_std_path()).unwrap();

    let server_thread = std::thread::spawn(move || {
        let (mut server_stream, _) = listener.accept().unwrap();
        let _request = protocol_frame::read_framed_blocking(&mut server_stream)
            .unwrap()
            .unwrap();
        // Reply with something that isn't a valid initialize result at all.
        protocol_frame::write_framed_blocking(&mut server_stream, b"not json").unwrap();
    });

    let mut client = UnixStream::connect(path.as_std_path()).unwrap();
    let version = query_version(&mut client, Duration::from_secs(2));
    assert_eq!(version, None);

    server_thread.join().unwrap();
    let _ = std::fs::remove_file(path.as_std_path());
}
