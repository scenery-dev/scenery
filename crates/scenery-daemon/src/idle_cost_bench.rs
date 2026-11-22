//! Idle-cost harness: per-tick CPU and memory of the daemon's periodic work
//! at N synthetic skills, against a local mock registry (never dev or prod).
//!
//! Ignored by default. Run it on a release build, one thread:
//!
//! ```text
//! cargo test -p scenery-daemon --release --lib idle_cost -- --ignored --nocapture --test-threads=1
//! SCENERY_BENCH_SIZES=50,100 cargo test ...   # pick the sizes (default 50,100,500,1000)
//! ```
//!
//! Per N it builds a temp HOME (`SCENERY_DISABLE_KEYCHAIN=1`, its own state
//! dir) holding N skills, each a realistic SKILL.md plus three bundled files:
//! 40% real folders in `~/.claude/skills` (Claude Code), 40% in
//! `~/.agents/skills` (the shared root Cursor and others read), 20% installed
//! by Scenery (marker rows, drift-scanned). Two client configs carry six
//! shadow MCP servers; `~/.claude.json` is padded to ~190 KB like a real one.
//! The mock answers the discovery upload with a `skip`/`hold` directive per
//! skill, as prod does for an unchanged machine.
//!
//! It then measures, with `getrusage(RUSAGE_SELF)` (all threads, so the
//! `spawn_blocking` pool and reqwest's blocking-client threads are counted):
//! - the 300 s work: discoveries + auto-manage, drift, the registry sync tick;
//! - the 60 s work: token refresh check + client wiring pass.
//!
//! Each is measured in the steady state (nothing changed, warm caches) and right after
//! one skill's SKILL.md was edited. RSS is sampled with `ps` after each phase.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::managed_paths::ENV_MUTEX;
use base64::Engine;
use mockito::{Matcher, Server, ServerGuard};
use scenery_core::{audit::SqliteAuditBuffer, registry::RegistryClient, state::AppState};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

fn cpu_ms() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: getrusage writes into the zeroed struct we own.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as f64 * 1000.0 + t.tv_usec as f64 / 1000.0;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

fn rss_mb() -> f64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    out.parse::<f64>().unwrap_or(0.0) / 1024.0
}

fn skill_md(slug: &str, i: usize) -> String {
    let mut body = format!(
        "---\nname: {slug}\ndescription: Synthetic skill number {i} used to measure the \
         daemon's idle cost. Use when the user asks about topic {i}.\nlicense: MIT\n---\n\n\
         # {slug}\n\n"
    );
    for p in 0..24 {
        body.push_str(&format!(
            "Step {p}: read the relevant files, summarise what changed, and propose the \
             smallest safe edit. Keep the output short and cite file paths.\n"
        ));
    }
    body
}

fn write_skill(dir: &Path, slug: &str, i: usize) {
    fs::create_dir_all(dir.join("scripts")).unwrap();
    fs::create_dir_all(dir.join("references")).unwrap();
    fs::write(dir.join("SKILL.md"), skill_md(slug, i)).unwrap();
    let py: String = (0..60)
        .map(|l| format!("def step_{l}(x):\n    return x + {l}\n"))
        .collect();
    fs::write(dir.join("scripts/helper.py"), py).unwrap();
    // SCENERY_BENCH_BIG_KB: one big reference per skill (memory high-water).
    let big_kb: usize = std::env::var("SCENERY_BENCH_BIG_KB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let guide = "Reference notes for the skill. ".repeat(160 + big_kb * 33);
    fs::write(dir.join("references/guide.md"), guide).unwrap();
    fs::write(
        dir.join("template.json"),
        format!("{{\"skill\":\"{slug}\",\"fields\":[\"a\",\"b\",\"c\"]}}"),
    )
    .unwrap();
}

/// Backdate every file so nothing reads as "recently modified".
fn backdate(dir: &Path) {
    let old = SystemTime::now() - Duration::from_secs(86_400);
    for e in walk(dir) {
        if let Ok(f) = fs::File::options().write(true).open(&e) {
            let _ = f.set_modified(old);
        }
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn jwt_far_future() -> String {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let exp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 30 * 86_400;
    format!(
        "{}.{}.sig",
        b64.encode(br#"{"alg":"HS256"}"#),
        b64.encode(format!(r#"{{"sub":"u1","exp":{exp}}}"#))
    )
}

fn write_client_configs(home: &Path) {
    // ~190 KB of project history, like a real ~/.claude.json, plus 4 shadows.
    let mut projects = serde_json::Map::new();
    for p in 0..120 {
        projects.insert(
            format!("/Users/someone/code/project-{p}"),
            serde_json::json!({
                "allowedTools": [], "history": (0..8).map(|h| format!("prompt {h} in project {p} with some text")).collect::<Vec<_>>(),
                "mcpServers": {}, "lastCost": 0.12, "lastSessionId": format!("{p:08}-aaaa-bbbb-cccc-dddddddddddd")
            }),
        );
    }
    let claude = serde_json::json!({
        "numStartups": 400,
        "projects": projects,
        "mcpServers": {
            "github": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"]},
            "fs": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]},
            "notes": {"type": "http", "url": "https://notes.example.com/mcp"},
            "local-py": {"command": "uvx", "args": ["mcp-server-time"]}
        }
    });
    fs::write(
        home.join(".claude.json"),
        serde_json::to_string_pretty(&claude).unwrap(),
    )
    .unwrap();
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(home.join(".cursor")).unwrap();
    fs::write(
        home.join(".cursor/mcp.json"),
        serde_json::json!({"mcpServers": {
            "linear": {"url": "https://mcp.linear.app/sse"},
            "pg": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-postgres", "postgres://localhost/db"]}
        }})
        .to_string(),
    )
    .unwrap();
}

struct World {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: Arc<AppState>,
    server: ServerGuard,
    registry: Arc<RegistryClient>,
    audit: SqliteAuditBuffer,
    edit_target: PathBuf,
    prev_home: Option<std::ffi::OsString>,
}

impl Drop for World {
    fn drop(&mut self) {
        match &self.prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}

fn build_world(n: usize) -> World {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", &home);
    std::env::set_var("SCENERY_DISABLE_KEYCHAIN", "1");
    let root = camino::Utf8PathBuf::from_path_buf(tmp.path().join("data")).unwrap();
    let state = Arc::new(AppState::bootstrap_in(root).unwrap());
    write_client_configs(&home);

    let claude_root = home.join(".claude/skills");
    let agents_root = home.join(".agents/skills");
    fs::create_dir_all(&claude_root).unwrap();
    fs::create_dir_all(&agents_root).unwrap();
    let pusher = crate::managed_paths::ManagedPathsPusher::new(&state);
    let mut edit_target = PathBuf::new();
    for i in 0..n {
        let slug = format!("skill-{i:04}");
        match i % 5 {
            0 | 1 => write_skill(&claude_root.join(&slug), &slug, i),
            2 | 3 => write_skill(&agents_root.join(&slug), &slug, i),
            _ => {
                let files = vec![
                    (
                        "scripts/helper.py".to_string(),
                        b"def f():\n    return 1\n".repeat(60),
                    ),
                    (
                        "references/guide.md".to_string(),
                        b"Reference notes. ".repeat(300),
                    ),
                    ("template.json".to_string(), b"{\"a\":1}".to_vec()),
                ];
                pusher
                    .push_skill(&slug, None, skill_md(&slug, i).as_bytes(), &files)
                    .unwrap();
            }
        }
        if i == 2 {
            edit_target = agents_root.join(&slug).join("SKILL.md");
        }
    }
    backdate(&home);

    let mut server = Server::new();
    let url = server.url();
    crate::test_support::pair_device(&state, &url);
    state.set_sync_state("device_id", "dev-1").unwrap();
    scenery_core::auth::save_tokens(&state, &url, &jwt_far_future(), "refresh").unwrap();

    // One directive per scanned skill: alternate hold / skip, as prod answers
    // an unchanged machine.
    let items = crate::managed_paths::discoveries::collect_discoveries(
        &crate::managed_paths::discoveries::extra_roots(),
        &state,
    );
    let directives: Vec<serde_json::Value> = items
        .iter()
        .enumerate()
        .map(|(k, it)| {
            let (action, reason) = if k % 2 == 0 {
                ("hold", "dev_skill_held")
            } else {
                ("skip", "already_tracked")
            };
            serde_json::json!({"discovery_id": format!("d{k}"), "source_path": it.source_path,
                               "action": action, "reason": reason})
        })
        .collect();
    let body = serde_json::json!({
        "accepted": [], "deduped": [], "rejected": [],
        "directives": directives,
        "policy": {"auto_manage_skills": true, "auto_submit_mcp": true}
    })
    .to_string();
    server
        .mock("POST", "/portal/managed-paths/discoveries")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(body)
        .expect_at_least(0)
        .create();
    for (m, p) in [
        ("GET", "/api/runner/approved-servers"),
        ("POST", "/api/runner/audit"),
        ("POST", "/skills/status"),
        ("POST", "/portal/managed-paths/drift"),
        ("GET", "/api/sync/snapshot"),
    ] {
        let b = match p {
            "/api/runner/approved-servers" => r#"{"servers":[]}"#,
            "/skills/status" => r#"{"statuses":{},"unknown":[]}"#,
            _ => "{}",
        };
        server
            .mock(m, p)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(b)
            .expect_at_least(0)
            .create();
    }
    server
        .mock("GET", Matcher::Any)
        .with_status(404)
        .expect_at_least(0)
        .create();

    let registry = Arc::new(RegistryClient::new(&url));
    let audit = SqliteAuditBuffer::new(Arc::clone(&registry), &state);
    World {
        _tmp: tmp,
        home,
        state,
        server,
        registry,
        audit,
        edit_target,
        prev_home,
    }
}

#[derive(Default, Clone, Copy)]
struct Cost {
    /// The local scan alone (part of `discoveries`).
    scan: f64,
    discoveries: f64,
    drift: f64,
    sync: f64,
    poll60: f64,
}

impl Cost {
    fn tick300(&self) -> f64 {
        self.discoveries + self.drift + self.sync
    }
}

fn measure<F: FnOnce()>(f: F) -> f64 {
    let t = cpu_ms();
    f();
    cpu_ms() - t
}

fn one_round(w: &World, rt: &tokio::runtime::Runtime) -> Cost {
    let url = w.server.url();
    let scan = measure(|| {
        let clients = scenery_mcp::setup::detect_ai_clients();
        let _ = crate::managed_paths::discoveries::collect_all_discoveries(
            &crate::managed_paths::discoveries::extra_roots(),
            &clients,
            &w.state,
            &url,
        );
    });
    let discoveries = measure(|| {
        rt.block_on(crate::managed_paths::discoveries::run_once(
            Arc::clone(&w.state),
            url.clone(),
        ))
        .unwrap();
    });
    let drift = measure(|| {
        let d = crate::managed_paths::drift::DriftScanner::new(Arc::clone(&w.state), url.clone())
            .unwrap();
        rt.block_on(d.run_cycle()).unwrap();
    });
    let cache: scenery_mcp::tools::UpdateCheckCache = Arc::new(Mutex::new(HashMap::new()));
    let flag = w.state.block_third_party_handle();
    let sync = measure(|| {
        crate::run_sync_tick(
            &w.registry,
            &w.audit,
            &w.state.db_path,
            &w.state.root_dir,
            &cache,
            None,
            &flag,
        )
        .unwrap();
    });
    let poll60 = measure(|| {
        let _ = crate::refresh_tick(&w.state, &url);
        let _ = scenery_mcp::wire::wire_clients_in(
            &w.home,
            Path::new("/"),
            scenery_mcp::wire::WirePolicy::FirstTimeOnly,
            &*w.state,
            Some((39140, "key".to_string())),
        );
    });
    Cost {
        scan,
        discoveries,
        drift,
        sync,
        poll60,
    }
}

fn avg(v: &[Cost]) -> Cost {
    let k = v.len() as f64;
    let mut a = Cost::default();
    for c in v {
        a.scan += c.scan / k;
        a.discoveries += c.discoveries / k;
        a.drift += c.drift / k;
        a.sync += c.sync / k;
        a.poll60 += c.poll60 / k;
    }
    a
}

#[test]
#[ignore = "measurement harness; run explicitly with --release --ignored --nocapture"]
fn idle_cost_scaling() {
    let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let sizes: Vec<usize> = std::env::var("SCENERY_BENCH_SIZES")
        .ok()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![50, 100, 500, 1000]);
    let rounds: usize = std::env::var("SCENERY_BENCH_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    println!(
        "\n{:>5} | {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} | {:>9} {:>9} | {:>7} {:>7}",
        "N",
        "scan ms",
        "disc ms",
        "drift ms",
        "sync ms",
        "300s ms",
        "60s ms",
        "edit300",
        "first",
        "RSS MB",
        "peak"
    );
    for n in sizes {
        let w = build_world(n);
        // The racy-clean guard memoises only trees older than 2 s.
        std::thread::sleep(Duration::from_millis(2_200));
        let rss_before = rss_mb();
        let first = one_round(&w, &rt);
        if std::env::var("SCENERY_BENCH_RSS_TRACE").is_ok() {
            println!(
                "  N={n} RSS before first tick {rss_before:.1} MB, after {:.1} MB",
                rss_mb()
            );
        }
        let mut steady = Vec::new();
        for _ in 0..rounds {
            steady.push(one_round(&w, &rt));
        }
        let s = avg(&steady);
        if std::env::var("SCENERY_BENCH_RSS_TRACE").is_ok() {
            // RSS over many steady rounds: growth here would be a leak.
            for r in 0..60 {
                one_round(&w, &rt);
                if r % 10 == 9 {
                    println!("  N={n} round {:>3}: RSS {:.1} MB", r + 1, rss_mb());
                }
            }
        }
        // One edit, then the next tick (the edited tree is re-read).
        let mut text = fs::read_to_string(&w.edit_target).unwrap();
        text.push_str("\nAn edit.\n");
        fs::write(&w.edit_target, text).unwrap();
        let e = one_round(&w, &rt);
        let rss = rss_mb();
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: as in cpu_ms.
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
        let peak_mb = if cfg!(target_os = "macos") {
            ru.ru_maxrss as f64 / 1_048_576.0
        } else {
            ru.ru_maxrss as f64 / 1024.0
        };
        println!(
            "{:>5} | {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>9.1} | {:>9.1} {:>9.1} | {:>7.1} {:>7.1}",
            n,
            s.scan,
            s.discoveries,
            s.drift,
            s.sync,
            s.tick300(),
            s.poll60,
            e.tick300(),
            first.tick300(),
            rss,
            peak_mb
        );
        drop(w);
    }
}

/// The whole daemon (`run_daemon`) idling in a temp HOME against the local
/// mock registry, paired, with the 300 s scans shortened
/// (`SCENERY_BENCH_SCAN_SECS`, default 5): RSS sampled every 10 s for
/// `SCENERY_BENCH_DAEMON_SECS` (default 180). A leak grows without bound; a
/// warm-up high-water mark plateaus.
#[test]
#[ignore = "measurement harness; run explicitly with --release --ignored --nocapture"]
fn idle_rss_daemon() {
    let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let n: usize = std::env::var("SCENERY_BENCH_SIZES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let secs: u64 = std::env::var("SCENERY_BENCH_DAEMON_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(180);
    let scan = std::env::var("SCENERY_BENCH_SCAN_SECS").unwrap_or_else(|_| "5".into());
    let w = build_world(n);
    // Hard guard: the daemon's own state dir must be inside the temp HOME.
    let root = AppState::resolve_root_dir().unwrap();
    assert!(
        root.as_std_path().starts_with(&w.home),
        "refusing to run: state dir {root} is not under the temp HOME"
    );
    let state = AppState::bootstrap_in(root).unwrap();
    let url = w.server.url();
    crate::test_support::pair_device(&state, &url);
    state.set_sync_state("device_id", "dev-1").unwrap();
    scenery_core::auth::save_tokens(&state, &url, &jwt_far_future(), "refresh").unwrap();
    std::env::set_var("SCENERY_DISCOVERIES_INTERVAL_SECS", &scan);
    std::env::set_var("SCENERY_DRIFT_SCAN_INTERVAL_SECS", &scan);
    std::env::set_var("SCENERY_REGISTRY_URL", &url);

    let sock = camino::Utf8PathBuf::from_path_buf(w.home.join("bench.sock")).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = rt.block_on(crate::run_daemon(crate::DaemonOpts {
            registry_url: Some(url),
            socket_path_override: Some(sock),
            ollama_url: Some("http://127.0.0.1:9".into()),
            ollama_model: None,
        }));
    });
    let t0 = std::time::Instant::now();
    let c0 = cpu_ms();
    while t0.elapsed().as_secs() < secs {
        std::thread::sleep(Duration::from_secs(10));
        println!(
            "  t={:>4}s RSS {:>5.1} MB  cpu {:>7.0} ms",
            t0.elapsed().as_secs(),
            rss_mb(),
            cpu_ms() - c0
        );
    }
    // The daemon thread is left running; the test process exits.
    std::process::exit(0);
}
