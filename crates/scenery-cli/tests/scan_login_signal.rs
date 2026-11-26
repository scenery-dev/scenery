//! `scenery scan --once --login` as a real process: SIGINT, SIGTERM or SIGHUP
//! after sign-in still revokes the session, then the process exits 130.
//!
//! A tiny std-only HTTP server stands in for the backend: it answers the
//! token exchange and the revoke, and never answers `GET /api/scan/session`,
//! so the child is reliably parked after sign-in when the signal lands.
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const REFRESH: &str = "RT_SIGNAL_secret_789";

enum Event {
    Session,
    Revoke(String),
}

/// Read one request: (request line, body).
fn read_request(stream: &mut TcpStream) -> (String, String) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).unwrap();
        if h == "\r\n" || h.is_empty() {
            break;
        }
        let lower = h.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            len = v.trim().parse().unwrap();
        }
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).unwrap();
    (line, String::from_utf8(body).unwrap())
}

fn respond(stream: &mut TcpStream, body: &str) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}

fn handle(mut stream: TcpStream, events: mpsc::Sender<Event>) {
    let (line, body) = read_request(&mut stream);
    if line.starts_with("POST /portal/auth/cli/token") {
        let tokens = format!(r#"{{"access_token":"AT_sig","refresh_token":"{REFRESH}"}}"#);
        respond(&mut stream, &tokens);
    } else if line.starts_with("GET /api/scan/session") {
        let _ = events.send(Event::Session);
        // Never answer: hold the child here until it is signalled.
        thread::sleep(Duration::from_secs(60));
    } else if line.starts_with("POST /portal/auth/revoke") {
        let _ = events.send(Event::Revoke(body));
        respond(&mut stream, r#"{"revoked":true,"families_revoked":1}"#);
    } else {
        respond(&mut stream, "{}");
    }
}

fn serve() -> (String, mpsc::Receiver<Event>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            thread::spawn(move || handle(stream, tx));
        }
    });
    (url, rx)
}

fn query(url: &str, key: &str) -> String {
    let q = url.split_once('?').unwrap().1;
    let raw = q
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
        .unwrap();
    raw.replace("%3A", ":").replace("%2F", "/")
}

/// Complete the browser step: GET the callback with the URL's state.
fn complete_sign_in(auth_url: &str) {
    let redirect = query(auth_url, "redirect_uri");
    let state = query(auth_url, "state");
    let host = redirect
        .strip_prefix("http://")
        .unwrap()
        .split_once('/')
        .unwrap()
        .0
        .to_string();
    let mut s = TcpStream::connect(&host).unwrap();
    write!(
        s,
        "GET /oauth/cli/callback?code=C1&state={state} HTTP/1.1\r\nHost: {host}\r\n\
         Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
}

fn signal_revokes(sig: libc::c_int) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (url, events) = serve();
    let mut child = Command::new(env!("CARGO_BIN_EXE_scenery"))
        .args(["scan", "--once", "--login", "--registry-url", &url])
        .env("HOME", &home)
        .env("TMPDIR", tmp.path())
        .env("SCENERY_NO_BROWSER", "1")
        .env("SCENERY_DISABLE_KEYCHAIN", "1")
        .env_remove("XDG_DATA_HOME")
        .env_remove("SCENERY_SCAN_TOKEN")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut lines = BufReader::new(stderr).lines();
    let auth_url = loop {
        let line = lines
            .next()
            .expect("child exited before printing the URL")
            .unwrap();
        let t = line.trim();
        if t.starts_with("http://") && t.contains("/portal/auth/cli/authorize") {
            break t.to_string();
        }
    };
    // Keep draining stderr so the child never blocks on a full pipe.
    thread::spawn(move || for _ in lines {});
    complete_sign_in(&auth_url);
    match events.recv_timeout(Duration::from_secs(20)).unwrap() {
        Event::Session => {}
        Event::Revoke(_) => panic!("revoked before the signal"),
    }
    // SAFETY: kill(2) on our own child's pid has no memory-safety preconditions.
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, sig) };
    assert_eq!(rc, 0);
    let body = match events.recv_timeout(Duration::from_secs(20)).unwrap() {
        Event::Revoke(b) => b,
        Event::Session => panic!("second session request"),
    };
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["refresh_token"], REFRESH);
    assert_eq!(v["scope"], "family");
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(130), "{status:?}");
}

#[test]
fn sigterm_after_sign_in_revokes_session() {
    signal_revokes(libc::SIGTERM);
}

#[test]
fn sigint_after_sign_in_revokes_session() {
    signal_revokes(libc::SIGINT);
}

#[test]
fn sighup_after_sign_in_revokes_session() {
    signal_revokes(libc::SIGHUP);
}
