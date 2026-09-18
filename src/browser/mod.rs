//! Optional host-owned browser lifecycle. No Docker authority enters either container.
pub mod assets;
mod process;

use crate::{
    config::{BrowserConfig, BrowserMode},
    output::{Style, narrate},
};
use fs2::FileExt;
use process::{args, docker};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const LABEL: &str = "dev.pithos.browser-run";
/// Network aliases the two containers answer to inside a run's own network.
/// These are addressed over plain HTTP, so neither may be a label that browsers
/// force to HTTPS. A bare label matching an HSTS-preloaded gTLD does exactly
/// that: `app` matched the preloaded `app` gTLD, whose entry carries
/// `include_subdomains`, so Chromium upgraded `http://app:3000` and failed with
/// `ERR_SSL_PROTOCOL_ERROR` against a sidecar that serves no TLS. The upgrade is
/// compiled into the browser and is not disabled by `HttpsUpgrades` being off.
const DEV_ALIAS: &str = "pithos-app";
const SIDECAR_ALIAS: &str = "browser";
/// gTLDs preloaded as HTTPS-only, which a bare alias must never equal.
#[cfg(test)]
const HSTS_PRELOADED_LABELS: &[&str] = &[
    "app",
    "dev",
    "new",
    "page",
    "zip",
    "foo",
    "gle",
    "prof",
    "rsvp",
    "day",
    "boo",
    "meme",
    "ing",
    "mov",
    "channel",
    "nexus",
    "search",
    "bank",
    "insurance",
];
const SKILL_TARGET: &str = "/home/pi/.agents/skills/pithos-browser";
static ACTIVE: Mutex<Option<OwnedRun>> = Mutex::new(None);
static OPERATION: Mutex<()> = Mutex::new(());
static CANCELLED: AtomicBool = AtomicBool::new(false);
static SIGNAL: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn signal_exit() -> Option<u8> {
    match SIGNAL.load(Ordering::SeqCst) {
        0 => None,
        signal => Some(128 + signal),
    }
}
#[cfg(unix)]
pub fn install_signal_handlers() -> io::Result<()> {
    use signal_hook::{
        consts::{SIGINT, SIGTERM},
        iterator::Signals,
    };
    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            SIGNAL.store(signal as u8, Ordering::SeqCst);
            interrupt_cleanup();
            std::process::exit(128 + signal);
        }
    });
    Ok(())
}
#[cfg(not(unix))]
pub fn install_signal_handlers() -> io::Result<()> {
    ctrlc::set_handler(|| {
        interrupt_cleanup();
        std::process::exit(130);
    })
    .map_err(io::Error::other)
}

#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    #[error("browser: {0}")]
    Io(#[from] io::Error),
    #[error("browser: {0}")]
    Operation(&'static str),
    #[error("browser image is not cached; run `pithos build` without --no-build")]
    CacheMiss,
}
type Result<T> = std::result::Result<T, BrowserError>;
fn checked(values: &[OsString], stage: &'static str) -> Result<String> {
    if CANCELLED.load(Ordering::SeqCst) {
        return Err(BrowserError::Operation("interrupted"));
    }
    let reply = docker(values)?;
    if !reply.success {
        return Err(BrowserError::Operation(stage));
    }
    Ok(reply.stdout)
}
pub fn image_tag() -> String {
    format!("pithos-browser:{}", assets::fingerprint())
}

/// The legacy base resolver can bootstrap-pull. Enabled cache-only launches
/// must not enter that path, even if their project image is already cached.
pub fn cached_base_id() -> io::Result<String> {
    let reply = docker(&args(&[
        "image",
        "inspect",
        "--format",
        "{{.Id}}",
        crate::docker::BASE_IMAGE_REF,
    ]))?;
    if reply.success && !reply.stdout.is_empty() {
        return Ok(reply.stdout);
    }
    Err(io::Error::other(
        "base image is not cached or cannot be inspected; run pithos build without --no-build after restoring Docker access",
    ))
}

/// Prepare images only; no networks, run state, credentials or viewer.
pub fn ensure_image(cache_only: bool, rebuild: bool, style: Style) -> Result<String> {
    let tag = image_tag();
    let cached = docker(&args(&["image", "inspect", &tag]))?.success;
    if cached && !rebuild {
        return Ok(tag);
    }
    if cache_only {
        return Err(BrowserError::CacheMiss);
    }
    narrate(
        style,
        "» browser:",
        "preparing pinned client/Chromium image (first build downloads browser and display dependencies)",
    );
    let context = tempfile::tempdir()?;
    assets::extract_to(context.path())?;
    let status = Command::new("docker")
        .args(["build", "--pull=false", "-t", &tag, "-f"])
        .arg(context.path().join("browser/runtime/Dockerfile"))
        .arg(context.path())
        .stdin(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(BrowserError::Operation("image preparation failed"));
    }
    Ok(tag)
}

#[derive(Debug, Clone)]
struct OwnedRun {
    id: String,
    root: PathBuf,
}
impl OwnedRun {
    fn network(&self) -> String {
        format!("pithos-browser-{}", self.id)
    }
    fn sidecar(&self) -> String {
        format!("pithos-browser-{}-browser", self.id)
    }
    fn dev(&self) -> String {
        format!("pithos-browser-{}-dev", self.id)
    }
    /// Check the label and immutable ID together, then remove that ID. A name
    /// can be reassigned between inspect and remove; it is not an ownership token.
    fn remove(&self, kind: &str, name: &str) -> bool {
        let format = format!(
            "{{{{.Id}}}} {{{{json (index .{}Labels \"{LABEL}\")}}}}",
            if kind == "container" { "Config." } else { "" }
        );
        let Ok(reply) = docker(&args(&[kind, "inspect", "--format", &format, name])) else {
            return false;
        };
        if !reply.success {
            // Distinguish absent from daemon failure: enumeration must succeed.
            return self.absent(kind, name);
        }
        let Some((resource_id, owner)) = reply.stdout.split_once(' ') else {
            return false;
        };
        // Quoting preserves label whitespace across the command runner's trim.
        if owner != format!("\"{}\"", self.id)
            || resource_id.len() != 64
            || !resource_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return false;
        }
        let values = if kind == "container" {
            args(&[kind, "rm", "-f", resource_id])
        } else {
            args(&[kind, "rm", resource_id])
        };
        if docker(&values).is_ok_and(|r| r.success) {
            return true;
        }
        // Removal can fail while the teardown is still settling: Docker detaches
        // network endpoints asynchronously after a forced container removal, so
        // a `network rm` issued immediately afterwards intermittently reports
        // active endpoints. Absence is the outcome that matters, and a failed
        // enumeration still means "engine unavailable", never "absent", so a
        // genuine failure keeps the lease record for the next launch to retry.
        self.absent(kind, name)
    }
    /// Exact-name enumeration, briefly retried. A successful empty listing
    /// proves absence; a failed listing means the engine is unreachable, which
    /// is not absence. Names are matched anchored, so a foreign resource that
    /// took this name reads as present rather than removed.
    fn absent(&self, kind: &str, name: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let list = if kind == "container" {
                args(&[
                    "container",
                    "ls",
                    "-aq",
                    "--filter",
                    &format!("name=^/{name}$"),
                ])
            } else {
                args(&["network", "ls", "-q", "--filter", &format!("name=^{name}$")])
            };
            match docker(&list) {
                Ok(reply) if !reply.success => return false,
                Ok(reply) if reply.stdout.is_empty() => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
    fn cleanup(&self) -> bool {
        let dev = self.remove("container", &self.dev());
        let browser = self.remove("container", &self.sidecar());
        let network = self.remove("network", &self.network());
        // Keep the lease record if the engine is unavailable; a later enabled
        // invocation retries cleanup before it creates any new browser state.
        if dev && browser && network {
            match fs::remove_dir_all(&self.root) {
                Ok(()) => true,
                Err(e) => e.kind() == io::ErrorKind::NotFound,
            }
        } else {
            false
        }
    }
}

#[derive(Debug)]
pub struct BrowserRun {
    owned: OwnedRun,
    _lease: File,
    viewer_url: Option<String>,
}
impl BrowserRun {
    pub fn start(config: BrowserConfig, image: &str, dev_image: &str) -> Result<Self> {
        let _operation = OPERATION.lock().unwrap();
        if !config.enabled {
            return Err(BrowserError::Operation("disabled browser must not start"));
        }
        let home =
            std::env::var_os("HOME")
                .filter(|x| !x.is_empty())
                .ok_or(BrowserError::Operation(
                    "HOME is required for private browser leases",
                ))?;
        let state = PathBuf::from(home).join(".pithos-browser-runs");
        private_dir(&state)?;
        recover(&state)?;
        let owned = OwnedRun {
            id: random_hex(16)?,
            root: PathBuf::new(),
        };
        let owned = OwnedRun {
            root: state.join(&owned.id),
            ..owned
        };
        private_dir(&owned.root)?;
        let lease = private_file(&owned.root.join("lease"), b"")?;
        lease.try_lock_exclusive()?;
        let mut run = Self {
            owned,
            _lease: lease,
            viewer_url: None,
        };
        *ACTIVE.lock().unwrap() = Some(run.owned.clone());
        // Even short-lived helpers need ownership before they start: killing a
        // Docker client does not necessarily stop its container. Helpers reuse
        // the reserved dev name serially, before the real dev container starts.
        let version = checked(
            &run.helper_args(args(&[
                "run", "--rm", "--network", "none", "--entrypoint", "/usr/bin/node",
                dev_image, "-p",
                "require('/opt/pi-npm/lib/node_modules/@earendil-works/pi-coding-agent/package.json').version",
            ])),
            "cannot inspect Pi compatibility",
        )?;
        if !supported_pi(&version) {
            return Err(BrowserError::Operation(
                "browser skill requires Pi >= 0.84.4; update pi.version and rebuild",
            ));
        }
        let capability = random_hex(32)?;
        let password = if config.mode == BrowserMode::Interactive {
            Some(random_hex(32)?)
        } else {
            None
        };
        let server = format!(
            "{{\"mode\":\"{}\",\"runId\":\"{}\",\"capability\":\"{}\"{}}}",
            config.mode.as_str(),
            run.owned.id,
            capability,
            password
                .as_ref()
                .map(|p| format!(",\"password\":\"{p}\""))
                .unwrap_or_default()
        );
        private_file(&run.owned.root.join("server.json"), server.as_bytes())?;
        private_file(
            &run.owned.root.join("client.json"),
            format!("{{\"endpoint\":\"ws://browser:3000/{capability}\"}}").as_bytes(),
        )?;
        if let Some(password) = password {
            private_file(&run.owned.root.join("viewer-password"), password.as_bytes())?;
        }
        assets::extract_to(&run.owned.root)?;
        let network = run.owned.network();
        checked(
            &args(&[
                "network",
                "create",
                "--label",
                &format!("{LABEL}={}", run.owned.id),
                &network,
            ]),
            "cannot create browser network",
        )?;
        checked(
            &Self::sidecar_args(&run.owned, config.mode, image)?,
            "cannot start sandboxed browser sidecar",
        )?;
        let deadline = Instant::now() + Duration::from_secs(65);
        loop {
            let health = checked(
                &args(&[
                    "inspect",
                    "--format",
                    "{{.State.Running}} {{if .State.Health}}{{.State.Health.Status}}{{end}}",
                    &run.owned.sidecar(),
                ]),
                "cannot inspect browser readiness",
            )?;
            if health == "true healthy" {
                break;
            }
            if !health.starts_with("true ") || Instant::now() >= deadline {
                let diagnostic = docker(&args(&["logs", "--tail", "10", &run.owned.sidecar()]))
                    .ok().and_then(|reply| reply.diagnostic)
                    .unwrap_or("sandbox/display/RPC readiness failed or timed out; see browser troubleshooting; never dump secret config");
                return Err(BrowserError::Operation(diagnostic));
            }
            thread::sleep(Duration::from_millis(250));
        }
        if config.mode == BrowserMode::Interactive {
            let address = checked(
                &args(&["port", &run.owned.sidecar(), "6080/tcp"]),
                "cannot resolve loopback viewer port",
            )?;
            if !valid_viewer_address(&address) {
                return Err(BrowserError::Operation(
                    "viewer was not published exclusively on IPv4 loopback",
                ));
            }
            run.viewer_url = Some(format!("http://{address}/"));
        }
        Ok(run)
    }
    fn sidecar_args(owned: &OwnedRun, mode: BrowserMode, image: &str) -> Result<Vec<OsString>> {
        let mut values = args(&[
            "run",
            "-d",
            "--pull=never",
            "--name",
            &owned.sidecar(),
            "--label",
            &format!("{LABEL}={}", owned.id),
            "--network",
            &owned.network(),
            "--network-alias",
            SIDECAR_ALIAS,
            "--user",
            "501:20",
            "--cap-drop=ALL",
            "--security-opt",
            "no-new-privileges=true",
            "--read-only",
            "--shm-size=512m",
            "--memory=2g",
            "--pids-limit=512",
            "--tmpfs",
            "/tmp:rw,nosuid,nodev,size=512m,mode=1777",
        ]);
        values.push("--security-opt".into());
        let mut seccomp = OsString::from("seccomp=");
        seccomp.push(owned.root.join("browser/runtime/seccomp.json"));
        values.push(seccomp);
        values.extend([
            "--mount".into(),
            readonly_bind(
                &owned.root.join("server.json"),
                "/run/pithos-browser/server.json",
            )?,
        ]);
        if mode == BrowserMode::Interactive {
            values.extend(args(&["-p", "127.0.0.1::6080"]));
        }
        values.push(image.into());
        Ok(values)
    }
    pub fn viewer_url(&self) -> Option<&str> {
        self.viewer_url.as_deref()
    }
    pub fn password_path(&self) -> PathBuf {
        self.owned.root.join("viewer-password")
    }
    pub fn dev_name(&self) -> String {
        self.owned.dev()
    }
    pub fn configure_dev(&self, values: &mut Vec<OsString>) -> io::Result<()> {
        let name = values
            .iter()
            .position(|s| s == "--name")
            .expect("dev container name");
        values[name + 1] = self.owned.dev().into();
        let mut extra = args(&[
            "--pull=never",
            "--network",
            &self.owned.network(),
            "--network-alias",
            DEV_ALIAS,
            "--label",
            &format!("{LABEL}={}", self.owned.id),
        ]);
        for (source, target) in [
            (
                self.owned.root.join("client.json"),
                "/run/pithos-browser/client.json",
            ),
            (self.owned.root.join("browser/skills"), SKILL_TARGET),
        ] {
            extra.extend(["--mount".into(), readonly_bind(&source, target)?]);
        }
        values.splice(1..1, extra);
        Ok(())
    }
    fn helper_args(&self, mut values: Vec<OsString>) -> Vec<OsString> {
        values.splice(
            1..1,
            args(&[
                "--pull=never",
                "--name",
                &self.owned.dev(),
                "--label",
                &format!("{LABEL}={}", self.owned.id),
            ]),
        );
        values
    }
    pub(crate) fn run_helper(
        &self,
        values: Vec<OsString>,
        failure: &'static str,
    ) -> io::Result<()> {
        let _operation = OPERATION.lock().unwrap();
        if CANCELLED.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "browser launch interrupted",
            ));
        }
        let reply = docker(&self.helper_args(values))?;
        if reply.success {
            Ok(())
        } else {
            Err(io::Error::other(failure))
        }
    }
    pub fn prepare_skill_mount(&self, image: &str, project: &str) -> io::Result<()> {
        self.run_helper(args(&[
            "run",
            "--rm",
            "--network",
            "none",
            "--user",
            "501:20",
            "--entrypoint",
            "/usr/bin/python3",
            "-v",
            &format!("pithos-home-{project}:/home/pi"),
            image,
            "-c",
            include_str!("prepare_skill.py"),
        ]), "browser skill mount collision or permissions error at ~/.agents/skills/pithos-browser; user content was not replaced")
    }
    pub fn spawn_dev(&self, command: &mut Command) -> io::Result<Child> {
        let _operation = OPERATION.lock().unwrap();
        if CANCELLED.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "browser launch interrupted",
            ));
        }
        command.spawn()
    }
    pub fn healthy(&self) -> bool {
        docker(&args(&[
            "inspect",
            "--format",
            "{{.State.Running}} {{if .State.Health}}{{.State.Health.Status}}{{end}}",
            &self.owned.sidecar(),
        ]))
        .is_ok_and(|r| r.success && r.stdout == "true healthy")
    }
}
impl Drop for BrowserRun {
    fn drop(&mut self) {
        if !self.owned.cleanup() {
            eprintln!(
                "browser: cleanup incomplete; the next enabled launch will retry owned stale resources"
            );
        }
        let mut active = ACTIVE.lock().unwrap();
        if active.as_ref().is_some_and(|x| x.id == self.owned.id) {
            *active = None;
        }
    }
}
/// Called explicitly before signal exit. Drop alone is not sufficient.
pub fn interrupt_cleanup() {
    CANCELLED.store(true, Ordering::SeqCst);
    let _operation = OPERATION.lock().unwrap();
    let owned = ACTIVE.lock().unwrap().clone();
    if let Some(owned) = owned {
        let _ = owned.cleanup();
    }
}
fn recover(state: &Path) -> Result<()> {
    for entry in fs::read_dir(state)? {
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if !valid_run_id(&id) {
            continue;
        }
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        }
        let path = entry.path().join("lease");
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let lease = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(lease) => lease,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        match lease.try_lock_exclusive() {
            Ok(()) => {
                if !(OwnedRun {
                    id,
                    root: entry.path(),
                })
                .cleanup()
                {
                    return Err(BrowserError::Operation(
                        "stale browser cleanup failed: engine unavailable or ownership mismatch; inspect ownership labels (not secret config) before retrying",
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn random_hex(bytes: usize) -> io::Result<String> {
    let mut value = vec![0; bytes];
    getrandom::fill(&mut value).map_err(io::Error::other)?;
    Ok(value.iter().map(|b| format!("{b:02x}")).collect())
}
fn valid_run_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_viewer_address(value: &str) -> bool {
    value
        .strip_prefix("127.0.0.1:")
        .is_some_and(|p| p.parse::<u16>().is_ok_and(|p| p > 0))
}
fn supported_pi(version: &str) -> bool {
    let numbers: Option<Vec<u32>> = version.split('.').map(|v| v.parse().ok()).collect();
    numbers.is_some_and(|n| n.len() == 3 && (n[0], n[1], n[2]) >= (0, 84, 4))
}
fn readonly_bind(source: &Path, target: &str) -> io::Result<OsString> {
    let mount = crate::sessions::bind_mount(source, target)?;
    let mut mount = mount;
    mount.push(",readonly");
    Ok(mount)
}
fn private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return Err(io::Error::other(
            "browser state path must be a real private directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::other(
                "browser state directory must have mode 0700",
            ));
        }
    }
    Ok(())
}
fn private_file(path: &Path, bytes: &[u8]) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).read(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, OwnedRun) {
        let root = tempfile::tempdir().unwrap();
        let run = OwnedRun {
            id: "a".repeat(32),
            root: root.path().into(),
        };
        (root, run)
    }
    #[test]
    fn network_aliases_are_reachable_over_plain_http() {
        // A bare alias equal to an HSTS-preloaded gTLD is force-upgraded to
        // HTTPS by browsers, which makes the container unreachable over the
        // plain HTTP it actually serves. `app` regressed exactly this way.
        for alias in [DEV_ALIAS, SIDECAR_ALIAS] {
            assert!(
                !HSTS_PRELOADED_LABELS.contains(&alias),
                "network alias {alias:?} is an HSTS-preloaded gTLD; browsers force it to HTTPS"
            );
            assert!(!alias.is_empty() && alias.len() <= 63);
            assert!(
                alias
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    && !alias.starts_with('-')
                    && !alias.ends_with('-'),
                "network alias {alias:?} is not a valid DNS label"
            );
        }
        assert_ne!(DEV_ALIAS, SIDECAR_ALIAS);
    }
    #[test]
    fn sidecar_mode_security_and_mount_contract() {
        let (_dir, run) = fixture();
        for mode in [BrowserMode::Headless, BrowserMode::Interactive] {
            let values = BrowserRun::sidecar_args(&run, mode, "image").unwrap();
            let text = values
                .iter()
                .map(|x| x.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            assert!(text.contains("--cap-drop=ALL"));
            assert!(text.contains("--shm-size=512m"));
            assert!(!text.contains("--privileged"));
            assert!(!text.contains("SYS_ADMIN"));
            assert!(!text.contains("docker.sock"));
            assert!(!text.contains("/workspace"));
            assert!(!text.contains("/home/pi"));
            assert_eq!(
                text.contains("127.0.0.1::6080"),
                mode == BrowserMode::Interactive
            );
            assert!(!text.contains("::3000"));
            assert!(!text.contains("::5900"));
        }
    }
    #[test]
    fn viewer_and_run_identity_validation() {
        assert!(valid_run_id(&"a".repeat(32)));
        for id in ["", "../x", "not-an-owned-run"] {
            assert!(!valid_run_id(id));
        }
        assert!(valid_viewer_address("127.0.0.1:1234"));
        for value in [
            "0.0.0.0:1234",
            "127.0.0.1:0",
            "127.0.0.1:65536",
            "127.0.0.1:1234\n0.0.0.0:1234",
        ] {
            assert!(!valid_viewer_address(value));
        }
        assert!(supported_pi("0.84.4"));
        assert!(supported_pi("0.85.1"));
        assert!(!supported_pi("0.84.3"));
        assert!(!supported_pi("0.85.1-alpha"));
    }
    #[test]
    fn secrets_are_exclusive_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        private_file(&path, b"test-only").unwrap();
        assert!(private_file(&path, b"overwrite").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
