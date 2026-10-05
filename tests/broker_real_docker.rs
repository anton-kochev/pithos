#![cfg(target_os = "macos")]
//! Opt-in Docker Desktop acceptance for the production broker path. Never runs
//! by default: it builds images and starts containers on the real daemon, and
//! writes private run state under the real HOME.
//!
//! Run: `PITHOS_BROKER_DOCKER_TEST=1 cargo test --test broker_real_docker -- --ignored`

use pithos::{
    broker::{grant::HostGrant, host::HostInputs, runtime::RuntimePoll},
    lifecycle::ShutdownReason,
};
use std::{
    fs::{self, File},
    io::{self, Read},
    os::{
        fd::FromRawFd,
        unix::{fs::PermissionsExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const PROJECT: &str = "pithos-broker-acceptance";
const VOLUME: &str = "pithos-home-pithos-broker-acceptance";
// Run inside the managed Pi container: authenticated and unauthenticated
// status requests through the advertised host endpoint.
const PROBE: &str = r#"
import json, urllib.request as u, urllib.error as e
c = json.load(open('/run/pithos-broker/client.json'))
url = c['endpoint'] + '/v1/status'
ok = u.urlopen(u.Request(url, headers={'Authorization': 'Bearer ' + c['token']}), timeout=5)
print('authorized', ok.status, ok.read().decode().strip())
try:
    u.urlopen(url, timeout=5)
    print('anonymous accepted')
except e.HTTPError as err:
    print('anonymous', err.code)
print('endpoint', c['endpoint'].split(':')[1])
import os
print('sessions writable', os.access('/workspace/.pi/sessions', os.W_OK))
import subprocess
# Docker Desktop shows the mount point as root-owned; git must still accept it.
git = subprocess.run(['git', '-C', '/workspace', 'status', '--porcelain'], capture_output=True, text=True)
print('git status', git.returncode, git.stderr.strip()[:120])
"#;

// Run inside the browser-enabled Pi container: serve a page from Pi and
// read it back through the sidecar's Chromium under the legacy alias.
const BROWSER_PROBE: &str = r#"
set -e
mkdir -p /tmp/site
echo '<html><head><title>acceptance</title></head><body><h1>hello from pi</h1></body></html>' > /tmp/site/index.html
nohup python3 -m http.server 3000 --bind 0.0.0.0 --directory /tmp/site >/tmp/site.log 2>&1 &
sleep 1
pithos-browser open
pithos-browser goto http://pithos-app:3000
pithos-browser snapshot
"#;

// Run inside the managed Pi container with the image's own Node: load the
// mounted broker extension exactly as Pi does, then drive its five tools
// against the live broker and reach the app over the run network.
const APPS_PROBE: &str = r#"
const ext = (await import('/run/pithos-broker/extension.mjs')).default;
const tools = new Map();
ext({ registerTool: (t) => tools.set(t.name, t) });
console.log('tools', [...tools.keys()].sort().join(','));
const call = async (name, id, params) => {
  try {
    const r = await tools.get(name).execute(id, params, undefined, () => {}, {});
    return r.content[0].text;
  } catch (e) { return 'ERROR ' + e.message; }
};
console.log('build', await call('pithos_app_build', 'c1', { app: 'web', dockerfile: 'app/Dockerfile', context: 'app' }));
const run = await call('pithos_app_run', 'c2', { app: 'web' });
console.log('run', run);
const host = run.match(/host (pithos-app-[0-9a-f]{32})/)[1];
let page = '';
for (let i = 0; i < 50 && !page; i++) {
  try { page = await (await fetch(`http://${host}:8080/`)).text(); } catch { await new Promise(r => setTimeout(r, 200)); }
}
console.log('page', page.trim());
console.log('logs', (await call('pithos_app_logs', 'c3', { app: 'web', tail: 20 })).includes('GET /'));
console.log('second run', await call('pithos_app_run', 'c4', { app: 'web' }));
console.log('stop', await call('pithos_app_stop', 'c5', { app: 'web' }));
console.log('status', await call('pithos_app_status', 'c6', { app: 'web' }));
"#;

/// The tests share one project name and home volume, and each refuses to
/// start while another's managed resources exist: run them one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

/// A minimal ASP.NET Core app, built by the broker from `src/Api/Dockerfile`.
const DOTNET_APP: [(&str, &str); 3] = [
    (
        "Api.csproj",
        r#"<Project Sdk="Microsoft.NET.Sdk.Web">
  <PropertyGroup>
    <TargetFramework>net8.0</TargetFramework>
    <ImplicitUsings>enable</ImplicitUsings>
    <Nullable>enable</Nullable>
    <InvariantGlobalization>true</InvariantGlobalization>
  </PropertyGroup>
</Project>
"#,
    ),
    (
        "Program.cs",
        r#"var app = WebApplication.Create(args);
app.MapGet("/", () => Results.Content(
    "<html><head><title>pithos dotnet</title></head><body><h1>hello from aspnet</h1></body></html>",
    "text/html"));
app.Run();
"#,
    ),
    (
        "Dockerfile",
        "FROM mcr.microsoft.com/dotnet/sdk:8.0 AS build\n\
         WORKDIR /src\n\
         COPY Api.csproj .\n\
         RUN dotnet restore\n\
         COPY Program.cs .\n\
         RUN dotnet publish -c Release -o /out --no-restore\n\
         FROM mcr.microsoft.com/dotnet/aspnet:8.0\n\
         WORKDIR /app\n\
         COPY --from=build /out .\n\
         ENV ASPNETCORE_HTTP_PORTS=8080 DOTNET_EnableDiagnostics=0\n\
         ENTRYPOINT [\"dotnet\", \"Api.dll\"]\n",
    ),
];

// In Pi, through the mounted extension: build and run the .NET app, then
// retry HTTP until Kestrel listens ("running" is not "listening").
const DOTNET_UP: &str = r#"
const ext = (await import('/run/pithos-broker/extension.mjs')).default;
const tools = new Map();
ext({ registerTool: (t) => tools.set(t.name, t) });
const call = async (name, id, params) => {
  try { return (await tools.get(name).execute(id, params, undefined, () => {}, {})).content[0].text; }
  catch (e) { return 'ERROR ' + e.message; }
};
console.log('build', await call('pithos_app_build', 'd1', { app: 'api', dockerfile: 'src/Api/Dockerfile', context: 'src/Api' }));
const run = await call('pithos_app_run', 'd2', { app: 'api' });
console.log('run', run);
const host = run.match(/host (pithos-app-[0-9a-f]{32})/)?.[1];
let page = '';
for (let i = 0; i < 150 && host && !page; i++) {
  try { page = await (await fetch(`http://${host}:8080/`)).text(); } catch { await new Promise(r => setTimeout(r, 200)); }
}
console.log('host', host);
console.log('ready', page.includes('hello from aspnet'));
"#;

// In Pi: Chromium in the sidecar opens the app by its generated host name.
const DOTNET_BROWSE: &str = r#"
set -e
pithos-browser open
pithos-browser goto "http://$1:8080/"
pithos-browser snapshot
pithos-browser screenshot --filename=dotnet.png
head -c 8 /tmp/pithos-browser/artifacts/dotnet.png | od -An -tx1 | tr -d ' 
'; echo ' png-magic'
"#;

const DOTNET_DOWN: &str = r#"
const ext = (await import('/run/pithos-broker/extension.mjs')).default;
const tools = new Map();
ext({ registerTool: (t) => tools.set(t.name, t) });
const call = async (name, id, params) => {
  try { return (await tools.get(name).execute(id, params, undefined, () => {}, {})).content[0].text; }
  catch (e) { return 'ERROR ' + e.message; }
};
console.log('logs', (await call('pithos_app_logs', 'd3', { app: 'api', tail: 50 })).includes('Now listening on'));
console.log('stop', await call('pithos_app_stop', 'd4', { app: 'api' }));
console.log('status', await call('pithos_app_status', 'd5', { app: 'api' }));
"#;

fn exec_in(id: &str, args: &[&str]) -> String {
    let output = Command::new("docker")
        .arg("exec")
        .arg(id)
        .args(args)
        .output()
        .unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// In Pi: the database answers on its run-network name.
const PG_TCP: &str = r#"
import socket, time
for _ in range(100):
    try:
        socket.create_connection(('pithos-postgres', 5432), timeout=2).close(); print('pg tcp ok'); break
    except OSError: time.sleep(0.2)
else: print('pg tcp failed')
"#;

/// A small ASP.NET Core app that Pi runs itself: it reads the run's
/// `PITHOS_POSTGRES_*`, writes a row and renders it.
const PG_DOTNET_APP: [(&str, &str); 2] = [
    (
        "Web.csproj",
        r#"<Project Sdk="Microsoft.NET.Sdk.Web">
  <PropertyGroup>
    <TargetFramework>net10.0</TargetFramework>
    <ImplicitUsings>enable</ImplicitUsings>
    <Nullable>enable</Nullable>
    <InvariantGlobalization>true</InvariantGlobalization>
  </PropertyGroup>
  <ItemGroup>
    <PackageReference Include="Npgsql" Version="9.0.3" />
  </ItemGroup>
</Project>
"#,
    ),
    (
        "Program.cs",
        r#"using Npgsql;

static string Env(string key) =>
    Environment.GetEnvironmentVariable(key) ?? throw new InvalidOperationException($"{key} is not set");

var connection = new NpgsqlConnectionStringBuilder
{
    Host = Env("PITHOS_POSTGRES_HOST"),
    Port = int.Parse(Env("PITHOS_POSTGRES_PORT")),
    Username = Env("PITHOS_POSTGRES_USER"),
    Password = Env("PITHOS_POSTGRES_PASSWORD"),
    Database = Env("PITHOS_POSTGRES_DATABASE"),
}.ConnectionString;
await using var data = NpgsqlDataSource.Create(connection);
await using (var setup = data.CreateCommand(
    "create table notes(text text not null); insert into notes values ('hello from postgres')"))
{
    await setup.ExecuteNonQueryAsync();
}

var app = WebApplication.Create(args);
app.MapGet("/", async () =>
{
    await using var query = data.CreateCommand("select text from notes");
    var text = (string?)await query.ExecuteScalarAsync();
    return Results.Content(
        $"<html><head><title>pithos pg</title></head><body><h1>{text}</h1></body></html>",
        "text/html");
});
app.Run();
"#,
    ),
];

// In Pi: run the app in the background, then wait until it answers.
const PG_DOTNET_UP: &str = r#"
cd /workspace/src/Web
nohup dotnet run --urls http://0.0.0.0:5000 > /tmp/web.log 2>&1 &
python3 - <<'PY'
import time, urllib.request
for _ in range(900):
    try:
        print('web', urllib.request.urlopen('http://127.0.0.1:5000/', timeout=2).read().decode().strip()); break
    except Exception: time.sleep(1)
else: print('web never answered'); print(open('/tmp/web.log').read()[-3000:])
PY
"#;

// In Pi: Chromium in the sidecar opens the app Pi serves.
const PG_DOTNET_BROWSE: &str = r#"
set -e
pithos-browser open
pithos-browser goto "http://pithos-app:5000/"
pithos-browser snapshot
"#;

/// .NET inside Pi against the broker's Postgres, seen through Chromium.
fn pg_dotnet_probe(pi: &str) -> String {
    let mut result = exec_in(pi, &["bash", "-c", PG_DOTNET_UP]);
    result.push_str(&exec_in(pi, &["bash", "-c", PG_DOTNET_BROWSE]));
    result
}

/// The broker's Postgres: reachable from Pi, the configured database exists,
/// and it runs as the fixed user with only tmpfs mounts.
fn postgres_probe(pi: &str) -> String {
    let mut result = exec_in(pi, &["python3", "-c", PG_TCP]);
    let Some(pg) = docker(&[
        "ps",
        "-q",
        "--filter",
        "label=io.pithos.probe.request=runtime-postgres-v1",
    ])
    .lines()
    .next()
    .map(str::to_owned) else {
        return result + "no postgres container\n";
    };
    result.push_str(&exec_in(
        &pg,
        &[
            "psql",
            "-U",
            "postgres",
            "-d",
            "app",
            "-tAc",
            "select 'db ' || current_database()",
        ],
    ));
    result.push_str(&docker(&[
        "inspect",
        "--format",
        "pg user {{.Config.User}} readonly {{.HostConfig.ReadonlyRootfs}} mounts {{range .Mounts}}{{.Type}} {{end}}",
        &pg,
    ]));
    // Pi's connection details, and its password works for a TCP login.
    result.push_str(&exec_in(
        pi,
        &[
            "sh",
            "-c",
            "echo pg env $PITHOS_POSTGRES_HOST $PITHOS_POSTGRES_PORT $PITHOS_POSTGRES_USER $PITHOS_POSTGRES_DATABASE",
        ],
    ));
    let password = exec_in(pi, &["printenv", "PITHOS_POSTGRES_PASSWORD"])
        .trim()
        .to_owned();
    let url = exec_in(pi, &["printenv", "PITHOS_POSTGRES_URL"]);
    if !password.is_empty()
        && url.trim() == format!("postgresql://postgres:{password}@pithos-postgres:5432/app")
    {
        result.push_str("url ok\n");
    }
    // Over the network name, not loopback: the image trusts loopback, so
    // only this path proves the password. A wrong one must be refused.
    for (candidate, label) in [(password.as_str(), "login"), ("wrong", "wrong-password")] {
        result.push_str(&exec_in(
            &pg,
            &[
                "env",
                &format!("PGPASSWORD={candidate}"),
                "psql",
                "-h",
                "pithos-postgres",
                "-U",
                "postgres",
                "-d",
                "app",
                "-tAc",
                &format!("select '{label} ' || 'ok'"),
            ],
        ));
    }
    result
}

/// The .NET flow from inside Pi, plus a host-side check that no managed
/// container publishes a port other than the sidecar's loopback viewer.
fn dotnet_probe(pi: &str) -> String {
    let mut result = exec_in(pi, &["node", "--input-type=module", "-e", DOTNET_UP]);
    let host = result
        .lines()
        .find_map(|l| l.strip_prefix("host pithos-app-"))
        .map(|hash| format!("pithos-app-{hash}"));
    for id in docker(&["ps", "-q", "--filter", "label=io.pithos.probe.run"]).lines() {
        result.push_str(&docker(&[
            "inspect",
            "--format",
            r#"ports {{index .Config.Labels "io.pithos.probe.request"}} {{json .HostConfig.PortBindings}}"#,
            id,
        ]));
    }
    if let Some(host) = host {
        result.push_str(&exec_in(
            pi,
            &["bash", "-c", DOTNET_BROWSE, "browse", &host],
        ));
    }
    result.push_str(&exec_in(
        pi,
        &["node", "--input-type=module", "-e", DOTNET_DOWN],
    ));
    result
}

fn opted_in() -> bool {
    std::env::var("PITHOS_BROKER_DOCKER_TEST").as_deref() == Ok("1")
}

fn docker(args: &[&str]) -> String {
    let output = Command::new("docker").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Starts with no home volume: the broker must create and seed it. Removes
/// the volume afterwards.
struct HomeVolume;
impl HomeVolume {
    fn absent() -> Self {
        let _ = Command::new("docker")
            .args(["volume", "rm", "-f", VOLUME])
            .output();
        Self
    }
}
impl Drop for HomeVolume {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["volume", "rm", "-f", VOLUME])
            .output();
    }
}

fn project(pithos: &[u8]) -> (tempfile::TempDir, PathBuf, Vec<u8>) {
    let parent = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let workspace = fs::canonicalize(parent.path()).unwrap().join(PROJECT);
    fs::create_dir(&workspace).unwrap();
    fs::set_permissions(&workspace, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(workspace.join(".pithos"), pithos).unwrap();
    (parent, workspace, pithos.to_vec())
}

fn managed_containers() -> Vec<String> {
    docker(&["ps", "-aq", "--filter", "label=io.pithos.probe.run"])
        .lines()
        .map(str::to_owned)
        .collect()
}

fn managed_networks() -> Vec<String> {
    docker(&[
        "network",
        "ls",
        "-q",
        "--filter",
        "label=io.pithos.probe.run",
    ])
    .lines()
    .map(str::to_owned)
    .collect()
}

/// Runs in a PTY child: the production host path, with a probe executed inside
/// the live managed Pi container while this process keeps serving status.
#[test]
fn broker_child() {
    let Ok(result_path) = std::env::var("PITHOS_BROKER_CHILD") else {
        return;
    };
    let mode = std::env::var("PITHOS_BROKER_CHILD_MODE").unwrap_or_default();
    let browser = mode == "browser";
    let apps = mode == "apps";
    let dotnet = mode == "dotnet";
    let postgres = mode == "postgres";
    let pg_dotnet = mode == "pg-dotnet";
    // Default config (project-stored sessions), plus the sidecar when asked.
    let config: &[u8] = if browser || dotnet {
        b"toolchains: {}\n"
    } else if postgres {
        b"toolchains: {}\npostgres: {version: \"17.10\", database: app}\n"
    } else if pg_dotnet {
        b"toolchains: {dotnet: \"10.0\"}\npostgres: {version: \"17.10\", database: app}\n"
    } else {
        b"toolchains: {}\n"
    };
    let (_parent, workspace, pithos) = project(config);
    // A real repository, like any project Pi works in.
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&workspace)
        .status()
        .unwrap();
    assert!(init.success());
    if apps {
        let app = workspace.join("app");
        fs::create_dir(&app).unwrap();
        fs::write(
            app.join("Dockerfile"),
            "FROM python:3.12-alpine\nWORKDIR /srv\nCOPY index.html .\nCMD [\"python3\", \"-m\", \"http.server\", \"8080\"]\n",
        )
        .unwrap();
        fs::write(app.join("index.html"), "hello from app\n").unwrap();
    }
    if pg_dotnet {
        let web = workspace.join("src/Web");
        fs::create_dir_all(&web).unwrap();
        for (name, content) in PG_DOTNET_APP {
            fs::write(web.join(name), content).unwrap();
        }
    }
    if dotnet {
        let api = workspace.join("src/Api");
        fs::create_dir_all(&api).unwrap();
        for (name, content) in DOTNET_APP {
            fs::write(api.join(name), content).unwrap();
        }
    }
    let inputs = HostInputs::prepare(HostGrant::workspace(), workspace, pithos)
        .unwrap()
        .with_browser(if browser || dotnet || pg_dotnet {
            pithos::browser::BrowserSelection::Enabled(pithos::browser::BrowserMode::Interactive)
        } else {
            pithos::browser::BrowserSelection::Disabled
        });
    let mut coordinator = match inputs.start(HostGrant::workspace()) {
        Ok(coordinator) => coordinator,
        Err(mut failure) => {
            let error = failure.error.to_string();
            let _ = failure.poll_cleanup();
            fs::write(&result_path, format!("start failed: {error}")).unwrap();
            return;
        }
    };
    let viewer = coordinator.browser_viewer().map(|(url, password)| {
        let status = Command::new("curl")
            .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", url])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        format!(
            "viewer {} {status} password-file {}",
            url.starts_with("http://127.0.0.1:"),
            password.exists()
        )
    });
    let probe: Arc<Mutex<Option<String>>> = Arc::default();
    let sink = probe.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now()
            + Duration::from_secs(if apps || dotnet || postgres || pg_dotnet {
                1800
            } else {
                60
            });
        let result = loop {
            let running = docker(&[
                "ps",
                "-q",
                "--filter",
                "label=io.pithos.probe.request=runtime-pi-v1",
                "--filter",
                "status=running",
            ]);
            if let Some(id) = running.lines().next() {
                let mut result = String::new();
                let mut probes = vec![vec!["python3", "-c", PROBE]];
                if browser {
                    probes.push(vec!["bash", "-c", BROWSER_PROBE]);
                }
                if apps {
                    probes.push(vec!["node", "--input-type=module", "-e", APPS_PROBE]);
                }
                if dotnet {
                    probes.clear();
                }
                for probe in probes {
                    let output = Command::new("docker")
                        .arg("exec")
                        .arg(id)
                        .args(probe)
                        .output()
                        .unwrap();
                    result.push_str(&String::from_utf8_lossy(&output.stdout));
                    result.push_str(&String::from_utf8_lossy(&output.stderr));
                }
                if dotnet {
                    result.push_str(&dotnet_probe(id));
                }
                if postgres {
                    result.push_str(&postgres_probe(id));
                }
                if pg_dotnet {
                    result.push_str(&pg_dotnet_probe(id));
                }
                break result;
            }
            if Instant::now() > deadline {
                break "no running managed Pi container".into();
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        *sink.lock().unwrap() = Some(result);
    });
    let deadline = Instant::now()
        + Duration::from_secs(if apps || dotnet || postgres || pg_dotnet {
            1800
        } else {
            120
        });
    let outcome = loop {
        match coordinator.poll() {
            Ok(RuntimePoll::Running) => {}
            other => break format!("runtime ended early: {other:?}"),
        }
        if let Some(result) = probe.lock().unwrap().take() {
            break result;
        }
        if Instant::now() > deadline {
            break "probe deadline".into();
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    coordinator.request_shutdown(ShutdownReason::Requested);
    let settled = coordinator.run_until_terminal(Duration::from_millis(20));
    let _ = coordinator.close_signals();
    let viewer = viewer.unwrap_or_else(|| "viewer none".into());
    fs::write(
        &result_path,
        format!("{outcome}\n{viewer}\nsettled {settled:?}\n"),
    )
    .unwrap();
}

fn run_child_in_pty(result: &Path, mode: &str) -> String {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "broker_child", "--nocapture"])
        .env("PITHOS_BROKER_CHILD", result)
        .env("PITHOS_BROKER_CHILD_MODE", mode);
    let (mut child, mut master) = spawn_in_pty(command);
    // A first build of a toolchain image can take many minutes.
    let deadline = Instant::now() + Duration::from_secs(2400);
    let mut output = Vec::new();
    loop {
        drain(&mut master, &mut output);
        if output.len() > 1024 * 1024 {
            output.drain(..output.len() - 64 * 1024);
        }
        if let Some(status) = child.try_wait().unwrap() {
            let tail = String::from_utf8_lossy(&output[output.len().saturating_sub(4096)..]);
            assert!(status.success(), "child failed: {tail}");
            return fs::read_to_string(result).unwrap_or_else(|_| format!("no result: {tail}"));
        }
        if Instant::now() >= deadline {
            // SAFETY: child is unreaped and owns its isolated session.
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            child.wait().unwrap();
            panic!("child deadline");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
#[ignore = "requires Docker Desktop and PITHOS_BROKER_DOCKER_TEST=1"]
fn docker_desktop_pi_reaches_broker_and_run_settles_clean() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        opted_in(),
        "explicit real-Docker acceptance opt-in required"
    );
    assert!(
        managed_containers().is_empty(),
        "stale managed containers present; refusing to run"
    );
    let _home = HomeVolume::absent();
    let scratch = tempfile::tempdir().unwrap();
    let result = run_child_in_pty(&scratch.path().join("result"), "status");
    eprintln!("{result}");
    assert!(result.contains("authorized 200"), "{result}");
    assert!(result.contains("\"phase\":\"ready\""), "{result}");
    assert!(result.contains("anonymous 401"), "{result}");
    assert!(
        result.contains("endpoint //host.docker.internal"),
        "{result}"
    );
    assert!(result.contains("settled Complete"), "{result}");
    assert!(result.contains("sessions writable True"), "{result}");
    assert!(result.contains("git status 0 "), "{result}");
    assert!(
        managed_containers().is_empty(),
        "managed container left behind"
    );
    let label = docker(&[
        "volume",
        "inspect",
        "--format",
        r#"{{index .Labels "io.pithos.broker.home"}}"#,
        VOLUME,
    ]);
    assert_eq!(
        label.trim(),
        "provisioned",
        "broker did not create its labelled home"
    );
    let leases = Path::new(&std::env::var_os("HOME").unwrap()).join(".pithos-home-leases");
    let key = sha256_hex(VOLUME);
    let uses = leases.join(key).join("uses");
    assert!(
        fs::read_dir(&uses).map_or(true, |mut d| d.next().is_none()),
        "home lease debt left behind"
    );
}

fn sha256_hex(value: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
#[ignore = "requires Docker Desktop and PITHOS_BROKER_DOCKER_TEST=1"]
fn docker_desktop_pi_drives_chromium_on_the_run_network() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        opted_in(),
        "explicit real-Docker acceptance opt-in required"
    );
    assert!(
        managed_containers().is_empty() && managed_networks().is_empty(),
        "stale managed resources present; refusing to run"
    );
    let _home = HomeVolume::absent();
    let scratch = tempfile::tempdir().unwrap();
    let result = run_child_in_pty(&scratch.path().join("result"), "browser");
    eprintln!("{result}");
    assert!(result.contains("authorized 200"), "{result}");
    assert!(result.contains("hello from pi"), "{result}");
    assert!(
        result.contains("viewer true 200 password-file true"),
        "{result}"
    );
    assert!(result.contains("settled Complete"), "{result}");
    assert!(managed_containers().is_empty(), "container left behind");
    assert!(managed_networks().is_empty(), "network left behind");
}

#[test]
#[ignore = "requires Docker Desktop and PITHOS_BROKER_DOCKER_TEST=1"]
fn docker_desktop_pi_builds_runs_reaches_and_stops_a_workspace_app() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        opted_in(),
        "explicit real-Docker acceptance opt-in required"
    );
    assert!(
        managed_containers().is_empty() && managed_networks().is_empty(),
        "stale managed resources present; refusing to run"
    );
    let _home = HomeVolume::absent();
    let scratch = tempfile::tempdir().unwrap();
    let result = run_child_in_pty(&scratch.path().join("result"), "apps");
    eprintln!("{result}");
    for expected in [
        "tools pithos_app_build,pithos_app_logs,pithos_app_run,pithos_app_status,pithos_app_stop",
        "build Built web",
        "run web is running at host pithos-app-",
        "page hello from app",
        "logs true",
        "second run ERROR run failed: this app is already running",
        "stop web stopped.",
        "status web: stopped",
        "settled Complete",
    ] {
        assert!(result.contains(expected), "missing {expected:?}\n{result}");
    }
    assert!(managed_containers().is_empty(), "container left behind");
    assert!(managed_networks().is_empty(), "network left behind");
}

/// Opens a PTY, starts `command` as a session leader on it, and returns the
/// child with the nonblocking master.
fn spawn_in_pty(mut command: Command) -> (std::process::Child, File) {
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: valid descriptor outputs; no optional name/termios/winsize storage.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    // SAFETY: successful openpty returned new, exclusively owned descriptors.
    let (master_file, slave_file) =
        unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    for fd in [master, slave] {
        // SAFETY: owned live descriptors; keep the master out of children.
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
            -1
        );
    }
    // SAFETY: owned master; nonblocking bounded output collection.
    assert_ne!(
        unsafe { libc::fcntl(master, libc::F_SETFL, libc::O_NONBLOCK) },
        -1
    );
    command
        .stdin(slave_file.try_clone().unwrap())
        .stdout(slave_file.try_clone().unwrap())
        .stderr(slave_file);
    // SAFETY: only async-signal-safe calls after fork.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    (command.spawn().unwrap(), master_file)
}

fn drain(master: &mut File, output: &mut Vec<u8>) {
    let mut buffer = [0; 4096];
    loop {
        match master.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => output.extend_from_slice(&buffer[..n]),
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.raw_os_error() == Some(libc::EIO) =>
            {
                break;
            }
            Err(e) => panic!("PTY read: {e}"),
        }
    }
}

fn running_pi() -> Option<String> {
    docker(&[
        "ps",
        "-q",
        "--filter",
        "label=io.pithos.probe.request=runtime-pi-v1",
        "--filter",
        "status=running",
    ])
    .lines()
    .next()
    .map(str::to_owned)
}

#[test]
#[ignore = "requires Docker Desktop and PITHOS_BROKER_DOCKER_TEST=1"]
fn docker_desktop_cli_workspace_run_shows_viewer_and_settles_when_pi_quits() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    use std::io::Write;
    assert!(
        opted_in(),
        "explicit real-Docker acceptance opt-in required"
    );
    assert!(
        managed_containers().is_empty() && managed_networks().is_empty(),
        "stale managed resources present; refusing to run"
    );
    let _home = HomeVolume::absent();
    // A pi.extensions package, installed by the image entrypoint as in legacy runs.
    let (_parent, workspace, _) = project(
        b"toolchains: {}\npi:\n  version: \"0.84.4\"\n  extensions:\n    \"@pithos-kit/themes\": npm:0.1.0\n",
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_pithos"));
    command
        .current_dir(&workspace)
        .args(["--broker=workspace", "--browser"])
        .env("NO_COLOR", "1");
    let (mut child, mut master) = spawn_in_pty(command);
    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut pi = None;
    let mut quit_sent: Option<Instant> = None;
    let mut terminated = false;
    let (mut seen, mut quiet_since) = (0, Instant::now());
    // Keep draining until exit: macOS holds a closing TTY until it is read.
    let status = loop {
        drain(&mut master, &mut output);
        if let Some(status) = child.try_wait().unwrap() {
            drain(&mut master, &mut output);
            break status;
        }
        if output.len() != seen {
            (seen, quiet_since) = (output.len(), Instant::now());
        }
        if pi.is_none() {
            pi = running_pi();
        }
        // Once Pi's TUI has settled, quit Pi itself.
        if pi.is_some() && quit_sent.is_none() && quiet_since.elapsed() > Duration::from_secs(5) {
            master.write_all(b"/quit\r").unwrap();
            quit_sent = Some(Instant::now());
        }
        if !terminated && Instant::now() >= deadline {
            // SAFETY: child is unreaped and owns its isolated session.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
            terminated = true;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let text = String::from_utf8_lossy(&output).into_owned();
    let tail = &text[text.len().saturating_sub(8192)..];
    let plain = String::from_utf8_lossy(&strip_ansi(&output)).into_owned();
    eprintln!(
        "--- screen ---\n{}",
        &plain[plain.len().saturating_sub(6000)..]
    );
    assert!(!terminated, "deadline; SIGTERM sent:\n{tail}");
    assert!(pi.is_some(), "managed Pi never ran:\n{tail}");
    // Pi's own loader picked up the mounted broker extension and the skill.
    for expected in [
        "[Extensions]",
        "extension.mjs",
        "[Skills]",
        "browser-automation",
        // The entrypoint installed the pi.extensions package from the manifest.
        "Installed npm:@pithos-kit/themes@0.1.0",
    ] {
        assert!(
            plain.contains(expected),
            "missing {expected:?} on Pi's screen"
        );
    }
    assert!(
        !plain.contains("Failed to load extension"),
        "extension load error"
    );
    assert!(
        text.contains("» browser: viewer: http://127.0.0.1:"),
        "no viewer line:\n{tail}"
    );
    // Every startup step is announced before it runs, so a slow start
    // (an image build, home checks) never looks like a hang.
    for step in [
        "» broker: preparing the Pi image",
        "» broker: preparing the browser image ...",
        "» broker: checking the Pi home volume ...",
        "» broker: creating the run network ...",
        "» broker: starting the browser ...",
        "» broker: starting Pi ...",
    ] {
        assert!(text.contains(step), "missing {step:?}:\n{tail}");
    }
    assert!(status.success(), "exit {status:?}:\n{tail}");
    assert!(managed_containers().is_empty(), "container left behind");
    assert!(managed_networks().is_empty(), "network left behind");
}

/// Drops CSI/OSC escape sequences so the TTY text can be read and matched.
fn strip_ansi(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'[' => {
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                }
                b']' => {
                    while i < bytes.len() && bytes[i] != 0x07 && bytes[i] != b'\\' {
                        i += 1;
                    }
                }
                _ => i += 1,
            }
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    out
}

#[test]
#[ignore = "requires Docker Desktop and PITHOS_BROKER_DOCKER_TEST=1"]
fn docker_desktop_pi_builds_a_dotnet_app_and_chromium_browses_it() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        opted_in(),
        "explicit real-Docker acceptance opt-in required"
    );
    assert!(
        managed_containers().is_empty() && managed_networks().is_empty(),
        "stale managed resources present; refusing to run"
    );
    let _home = HomeVolume::absent();
    let scratch = tempfile::tempdir().unwrap();
    let result = run_child_in_pty(&scratch.path().join("result"), "dotnet");
    eprintln!("{result}");
    for expected in [
        "build Built api",
        "run api is running at host pithos-app-",
        "ready true",
        "hello from aspnet",
        "89504e470d0a1a0a png-magic",
        "logs true",
        "stop api stopped.",
        "status api: stopped",
        "settled Complete",
    ] {
        assert!(result.contains(expected), "missing {expected:?}\n{result}");
    }
    // Only the sidecar publishes, and only its loopback viewer.
    let ports: Vec<&str> = result.lines().filter(|l| l.starts_with("ports ")).collect();
    assert!(
        ports.iter().any(|l| l.starts_with("ports app-")),
        "{result}"
    );
    for line in &ports {
        if line.starts_with("ports app-") || line.starts_with("ports runtime-pi") {
            assert!(line.ends_with("{}") || line.ends_with("null"), "{line}");
        } else {
            assert!(
                !line.contains("0.0.0.0") && !line.contains("\"HostIp\":\"\""),
                "{line}"
            );
        }
    }
    assert!(managed_containers().is_empty(), "container left behind");
    assert!(managed_networks().is_empty(), "network left behind");
}

#[test]
#[ignore = "requires Docker Desktop and PITHOS_BROKER_DOCKER_TEST=1"]
fn docker_desktop_broker_runs_postgres_next_to_pi_and_removes_it() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        opted_in(),
        "explicit real-Docker acceptance opt-in required"
    );
    assert!(
        managed_containers().is_empty() && managed_networks().is_empty(),
        "stale managed resources present; refusing to run"
    );
    let volumes_before = docker(&["volume", "ls", "-q"]);
    let _home = HomeVolume::absent();
    let scratch = tempfile::tempdir().unwrap();
    let result = run_child_in_pty(&scratch.path().join("result"), "postgres");
    eprintln!("{result}");
    assert!(!result.contains("wrong-password ok"), "{result}");
    for expected in [
        "pg tcp ok",
        "db app",
        "pg user 65532:65532 readonly true mounts tmpfs tmpfs",
        "pg env pithos-postgres 5432 postgres app",
        "url ok",
        "login ok",
        "settled Complete",
    ] {
        assert!(result.contains(expected), "missing {expected:?}\n{result}");
    }
    assert!(managed_containers().is_empty(), "container left behind");
    assert!(managed_networks().is_empty(), "network left behind");
    // tmpfs over every image VOLUME: no anonymous volume was created.
    let before: std::collections::BTreeSet<&str> = volumes_before.lines().collect();
    let after = docker(&["volume", "ls", "-q"]);
    let new: Vec<&str> = after
        .lines()
        .filter(|v| !before.contains(v) && *v != VOLUME)
        .collect();
    assert!(new.is_empty(), "new volumes: {new:?}");
}

#[test]
#[ignore = "requires Docker Desktop and PITHOS_BROKER_DOCKER_TEST=1"]
fn docker_desktop_pi_runs_dotnet_against_postgres_and_chromium_shows_the_row() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        opted_in(),
        "explicit real-Docker acceptance opt-in required"
    );
    assert!(
        managed_containers().is_empty() && managed_networks().is_empty(),
        "stale managed resources present; refusing to run"
    );
    let _home = HomeVolume::absent();
    let scratch = tempfile::tempdir().unwrap();
    let result = run_child_in_pty(&scratch.path().join("result"), "pg-dotnet");
    eprintln!("{result}");
    for expected in [
        // The app, running inside Pi, read the row back from Postgres...
        "web <html><head><title>pithos pg</title>",
        // ...and Chromium in the sidecar rendered it.
        "Page Title: pithos pg",
        "heading \"hello from postgres\"",
        "settled Complete",
    ] {
        assert!(result.contains(expected), "missing {expected:?}\n{result}");
    }
    assert!(managed_containers().is_empty(), "container left behind");
    assert!(managed_networks().is_empty(), "network left behind");
}

/// The acceptance home's outstanding-use markers (lease debt).
fn home_debt() -> Vec<String> {
    let home = std::env::var_os("HOME").unwrap();
    let uses = Path::new(&home)
        .join(".pithos-home-leases")
        .join(sha256_hex(VOLUME))
        .join("uses");
    fs::read_dir(uses)
        .map(|d| {
            d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
#[ignore = "requires Docker Desktop and PITHOS_BROKER_DOCKER_TEST=1"]
fn docker_desktop_double_ctrl_c_during_startup_settles_without_home_debt() {
    use std::io::Write;
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        opted_in(),
        "explicit real-Docker acceptance opt-in required"
    );
    assert!(
        managed_containers().is_empty() && managed_networks().is_empty(),
        "stale managed resources present; refusing to run"
    );
    assert!(home_debt().is_empty(), "stale home debt; refusing to run");
    let _home = HomeVolume::absent();
    let (_parent, workspace, _) = project(b"toolchains: {}\n");
    let mut command = Command::new(env!("CARGO_BIN_EXE_pithos"));
    command
        .current_dir(&workspace)
        .arg("--broker=workspace")
        .env("NO_COLOR", "1");
    let (mut child, mut master) = spawn_in_pty(command);
    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut interrupted = false;
    let status = loop {
        drain(&mut master, &mut output);
        if let Some(status) = child.try_wait().unwrap() {
            drain(&mut master, &mut output);
            break status;
        }
        // Ctrl-C twice once startup reaches the home checks, as a user would.
        if !interrupted
            && String::from_utf8_lossy(&output).contains("» broker: checking the Pi home volume")
        {
            master.write_all(b"\x03").unwrap();
            std::thread::sleep(Duration::from_millis(300));
            master.write_all(b"\x03").unwrap();
            interrupted = true;
        }
        assert!(Instant::now() < deadline, "deadline");
        std::thread::sleep(Duration::from_millis(20));
    };
    let text = String::from_utf8_lossy(&output).into_owned();
    assert!(
        interrupted,
        "startup never reached the home checks:\n{text}"
    );
    assert!(!status.success(), "{text}");
    assert!(managed_containers().is_empty(), "container left behind");
    assert!(managed_networks().is_empty(), "network left behind");
    let debt = home_debt();
    assert!(debt.is_empty(), "home debt left {debt:?}:\n{text}");
}

/// Plant a dead holder's marker on the acceptance home, as the lease writes it.
fn plant_home_debt() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap();
    let key = Path::new(&home)
        .join(".pithos-home-leases")
        .join(sha256_hex(VOLUME));
    let uses = key.join("uses");
    for dir in [&key, &uses] {
        if !dir.exists() {
            fs::create_dir(dir).unwrap();
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    if !key.join("lease").exists() {
        fs::write(key.join("lease"), b"").unwrap();
        fs::set_permissions(key.join("lease"), fs::Permissions::from_mode(0o600)).unwrap();
    }
    let marker = uses.join("d".repeat(64));
    fs::write(&marker, b"pithos-home-use-v1\noutstanding\n").unwrap();
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
    marker
}

#[test]
#[ignore = "requires Docker Desktop and PITHOS_BROKER_DOCKER_TEST=1"]
fn docker_desktop_broker_clears_a_dead_runs_home_lock_and_runs() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        opted_in(),
        "explicit real-Docker acceptance opt-in required"
    );
    assert!(home_debt().is_empty(), "stale home debt; refusing to run");
    let _home = HomeVolume::absent();
    let marker = plant_home_debt();
    let scratch = tempfile::tempdir().unwrap();
    let result = run_child_in_pty(&scratch.path().join("result"), "status");
    let _ = fs::remove_file(&marker);
    eprintln!("{result}");
    assert!(result.contains("authorized 200"), "{result}");
    assert!(result.contains("settled Complete"), "{result}");
    assert!(home_debt().is_empty(), "debt left: {:?}", home_debt());
}

#[test]
#[ignore = "requires Docker Desktop and PITHOS_BROKER_DOCKER_TEST=1"]
fn docker_desktop_broker_keeps_the_home_lock_while_a_container_mounts_the_home() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        opted_in(),
        "explicit real-Docker acceptance opt-in required"
    );
    assert!(home_debt().is_empty(), "stale home debt; refusing to run");
    let _home = HomeVolume::absent();
    // What a killed pithos can leave: a container that still uses the home.
    let holder = "pithos-acceptance-home-holder";
    let _ = Command::new("docker").args(["rm", "-f", holder]).output();
    docker(&[
        "run",
        "-d",
        "--name",
        holder,
        "-v",
        &format!("{VOLUME}:/home"),
        "alpine",
        "sleep",
        "300",
    ]);
    let marker = plant_home_debt();
    let (_parent, workspace, _) = project(b"toolchains: {}\n");
    let output = Command::new(env!("CARGO_BIN_EXE_pithos"))
        .current_dir(&workspace)
        .arg("--broker=workspace")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let kept = marker.exists();
    let _ = Command::new("docker").args(["rm", "-f", holder]).output();
    let _ = fs::remove_file(&marker);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("still mounted by a container"), "{stderr}");
    assert!(kept, "debt must stay while a container mounts the home");
}
