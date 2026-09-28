#![cfg(any(target_os = "linux", target_os = "macos"))]
//! The run network and the detached Chromium sidecar: durable intent before
//! every mutation, strict ownership checks, and containers-before-network
//! cleanup through the frozen selection.
#[path = "fixtures/bounded.rs"]
mod bounded;
#[path = "fixtures/canonical_temp.rs"]
mod tempfile;

use pithos::{
    broker::{journal::State, resources::ResourceManifest},
    docker::{BrowserInputs, HostIdentity, ImmutableImageId, ManagedDocker},
    lifecycle::Shutdown,
};
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
};

const ENTRYPOINT: &str = r#"["/usr/bin/tini","--","node","runtime/server.mjs"]"#;

fn image() -> ImmutableImageId {
    ImmutableImageId::new(&format!("sha256:{}", "e".repeat(64))).unwrap()
}
fn identity() -> HostIdentity {
    HostIdentity::effective().unwrap()
}
fn seccomp_bytes() -> &'static [u8] {
    pithos::browser::assets::FILES
        .iter()
        .find(|(name, _)| *name == "runtime/seccomp.json")
        .unwrap()
        .1
}

struct Fixture {
    root: tempfile::TempDir,
    executable: PathBuf,
    config: PathBuf,
    server: PathBuf,
    seccomp: PathBuf,
    _socket: UnixListener,
    _serial: bounded::Permit<'static>,
}
static SERIAL: bounded::Bounded = bounded::Bounded::new(4);
impl Fixture {
    fn new() -> Self {
        let serial = SERIAL.acquire();
        let root = tempfile::tempdir().unwrap();
        for name in ["config", "run", "private", "state"] {
            fs::create_dir(root.path().join(name)).unwrap();
            fs::set_permissions(root.path().join(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let server = root.path().join("private/server.json");
        let seccomp = root.path().join("private/seccomp.json");
        fs::write(&server, b"{}").unwrap();
        fs::write(&seccomp, seccomp_bytes()).unwrap();
        for file in [&server, &seccomp] {
            fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let executable = root.path().join("docker");
        let script = r#"#!/usr/bin/python3
import csv, json, os, pathlib, sys
root = pathlib.Path(__ROOT__)
state = root/'state'
a = sys.argv[5:]
with (root/'calls').open('a') as f: f.write(json.dumps(a)+'\n')
def mode(name): return (root/name).exists()
def emit(value): print(json.dumps(value))
def load(kind):
    return {p.stem: json.loads(p.read_text()) for p in state.glob(kind+'-*.json')}
def save(kind, name, value): (state/(kind+'-'+name+'.json')).write_text(json.dumps(value))
def next_id():
    n = len(list(state.glob('*.json'))) + 1
    return format(n, '064x')
def anchored(f, prefix):
    k, v = f.split('=', 1)
    return k, v[len(prefix):-1] if k == 'name' else v
if a[0] == 'info':
    emit({'id':'daemon-one','os_type':'linux','security_options':[]})
elif a[:2] == ['image','inspect']:
    assert a[-1] == __IMAGE__
    emit({'id':__IMAGE__,'user':__USER__,'volumes':None,
          'env':['HOME=/tmp/browser-home','USER=browser','LOGNAME=browser'],
          'labels':{'io.pithos.broker.browser-fingerprint':__HASH__}})
elif a[:2] == ['network','create']:
    if mode('network-create-fails'): sys.exit(1)
    labels = dict(a[i+1].split('=',1) for i,v in enumerate(a) if v=='--label')
    assert a[a.index('--driver')+1] == 'bridge'
    n = {'id':next_id(),'name':a[-1],'driver':'bridge','scope':'local','internal':False,
         'attachable':False,'ingress':False,'labels':labels}
    if mode('foreign-network-label'): n['labels']['io.pithos.probe.run'] = 'foreign'
    save('net', a[-1], n)
    print(n['id'])
elif a[:2] == ['network','ls']:
    k, v = anchored(a[a.index('--filter')+1], '^')
    for n in load('net').values():
        if n[k] == v: emit(n['id'])
elif a[:2] == ['network','inspect']:
    n = next(n for n in load('net').values() if n['id'] == a[-1])
    attached = {c['id']: {} for c in load('ctr').values() if c['host']['NetworkMode'] == n['name']}
    if mode('foreign-attached'): attached['f'*64] = {}
    emit(dict(n, containers=attached))
elif a[:2] == ['network','rm']:
    n = next(n for n in load('net').values() if n['id'] == a[-1])
    assert not any(c['host']['NetworkMode'] == n['name'] for c in load('ctr').values()), 'network in use'
    (state/('net-'+n['name']+'.json')).unlink()
elif a[0] == 'run':
    manifest = json.loads((root/'run/resources.json').read_text())
    journal = json.loads((root/'run/journal.json').read_text())
    name = a[a.index('--name')+1]
    r = next(r for r in manifest['resources'] if r['name'] == name)
    assert next(j for j in journal['records'] if j['request_id'] == r['request_id'])['state'] == 'running'
    (root/'run-args').write_text(json.dumps(a))
    def opts(k): return [a[i+1] for i,v in enumerate(a) if v == k]
    labels = dict(v.split('=',1) for v in opts('--label'))
    security = []
    for v in opts('--security-opt'):
        security.append('seccomp=' + pathlib.Path(v[8:]).read_text() if v.startswith('seccomp=') else v)
    if '--security-opt=no-new-privileges' in a: security.insert(0, 'no-new-privileges')
    mounts = []; hosts = []
    for v in opts('--mount'):
        kv = dict((p.split('=',1)+[''])[:2] for p in next(csv.reader([v])))
        ro = 'readonly' in kv
        mounts.append({'Type':'bind','Source':kv['source'],'Destination':kv['target'],'RW':not ro,'Propagation':'rprivate'})
        hosts.append({'Type':'bind','Source':kv['source'],'Target':kv['target'],'ReadOnly':ro})
    tmpfs = dict(v.split(':',1) for v in opts('--tmpfs'))
    ports = {'6080/tcp':[{'HostIp':'127.0.0.1','HostPort':''}]} if '127.0.0.1::6080' in opts('-p') else {}
    size = lambda k: next(int(v.split('=')[1][:-1]) for v in a if v.startswith(k+'='))
    image = a[-1]
    cid = next_id()
    c = {'id':cid,'name':'/'+name,'image':image,
         'config':{'Image':image,'User':a[a.index('--user')+1],'Entrypoint':json.loads(__ENTRYPOINT__),'Cmd':None,
                   'Labels':labels,'Volumes':None,'Tty':False,'OpenStdin':False,'WorkingDir':'/opt/pithos-browser'},
         'host':{'NetworkMode':a[a.index('--network')+1],'ReadonlyRootfs':'--read-only' in a,'Privileged':False,
                 'AutoRemove':False,'RestartPolicy':{'Name':'no','MaximumRetryCount':0},'CapDrop':['ALL'],
                 'SecurityOpt':security,'CapAdd':None,'GroupAdd':None,'Binds':None,'Devices':None,'PidMode':'',
                 'IpcMode':'private','UsernsMode':'','Mounts':hosts,'ExtraHosts':None,'Tmpfs':tmpfs,
                 'PortBindings':ports,'ShmSize':size('--shm-size')*1024*1024,
                 'Memory':size('--memory')*1024*1024*1024,'PidsLimit':int(a[a.index('--pids-limit')+1]) if '--pids-limit' in a else int(next(v.split('=')[1] for v in a if v.startswith('--pids-limit=')))},
         'mounts':mounts,
         'aliases':opts('--network-alias'),
         'state':{'Status':'running','Running':True,'ExitCode':0,'Error':'','OOMKilled':False,'Dead':False,
                  'Health':{'Status':'unhealthy' if mode('unhealthy') else 'healthy'}}}
    if mode('exits'): c['state'].update({'Status':'exited','Running':False,'ExitCode':1})
    if mode('tampered-browser'): c['host']['CapAdd'] = ['SYS_ADMIN']
    save('ctr', name, c)
    print(cid)
elif a[:2] == ['container','ls']:
    k, v = anchored(a[a.index('--filter')+1], '^/')
    for c in load('ctr').values():
        if (k == 'id' and c['id'] == v) or (k == 'name' and c['name'] == '/'+v): emit(c['id'])
elif a[:2] == ['container','inspect'] and 'NetworkSettings.Ports' in a[3]:
    c = next(c for c in load('ctr').values() if c['id'] == a[-1])
    host = '0.0.0.0' if mode('viewer-public') else '127.0.0.1'
    ports = {'6080/tcp':[{'HostIp':host,'HostPort':'49153'}]} if c['host']['PortBindings'] else {}
    emit({'id':c['id'],'ports':ports})
elif a[:2] == ['container','inspect']:
    emit(next(c for c in load('ctr').values() if c['id'] == a[-1]))
elif a[:2] == ['container','rm']:
    assert a[2] == '--force'
    c = next(c for c in load('ctr').values() if c['id'] == a[-1])
    (state/('ctr-'+c['name'][1:]+'.json')).unlink()
else:
    sys.exit(99)
"#
        .replace("__ROOT__", &serde_json::to_string(root.path()).unwrap())
        .replace("__ENTRYPOINT__", &serde_json::to_string(ENTRYPOINT).unwrap())
        .replace("__IMAGE__", &serde_json::to_string(image().as_str()).unwrap())
        .replace("__USER__", &serde_json::to_string(&identity().docker_user()).unwrap())
        .replace(
            "__HASH__",
            &serde_json::to_string(&pithos::browser::assets::fingerprint_with_identity(identity()))
                .unwrap(),
        );
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = UnixListener::bind(root.path().join("socket")).unwrap();
        Self {
            config: root.path().join("config"),
            root,
            executable,
            server,
            seccomp,
            _socket: socket,
            _serial: serial,
        }
    }
    fn docker(&self) -> ManagedDocker {
        ManagedDocker::new(
            &self.executable,
            &format!("unix://{}", self.root.path().join("socket").display()),
            &self.config,
            Shutdown::new(),
        )
        .unwrap()
    }
    fn manifest(&self) -> ResourceManifest {
        ResourceManifest::open(&self.root.path().join("run"), "run-1").unwrap()
    }
    fn set(&self, mode: &str) {
        fs::write(self.root.path().join(mode), b"").unwrap();
    }
    fn inputs(&self, viewer: bool) -> (ImmutableImageId, PathBuf, PathBuf, bool) {
        (image(), self.server.clone(), self.seccomp.clone(), viewer)
    }
    fn calls(&self) -> Vec<Vec<String>> {
        fs::read_to_string(self.root.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
    fn mutations(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| {
                matches!(
                    (c[0].as_str(), c.get(1).map(String::as_str)),
                    ("run", _) | ("network", Some("create" | "rm")) | ("container", Some("rm"))
                )
            })
            .map(|c| format!("{} {}", c[0], c[1]))
            .collect()
    }
    fn live(&self) -> usize {
        fs::read_dir(self.root.path().join("state"))
            .unwrap()
            .count()
    }
    fn run_args(&self) -> Vec<String> {
        serde_json::from_slice(&fs::read(self.root.path().join("run-args")).unwrap()).unwrap()
    }
}

fn start(
    f: &Fixture,
    docker: &mut ManagedDocker,
    manifest: &mut ResourceManifest,
    viewer: bool,
) -> Result<(), pithos::docker::OwnedProbeError> {
    let network = docker.create_run_network(manifest, "network-1")?;
    let (image, server, seccomp, viewer) = f.inputs(viewer);
    docker.start_browser(
        manifest,
        "browser-1",
        &network,
        BrowserInputs {
            image: &image,
            identity: identity(),
            server: &server,
            seccomp: &seccomp,
            viewer,
        },
    )
}

#[test]
fn network_and_sidecar_are_owned_then_removed_containers_first() {
    let f = Fixture::new();
    let mut docker = f.docker();
    let mut manifest = f.manifest();
    start(&f, &mut docker, &mut manifest, true).unwrap();
    assert_eq!(f.live(), 2);
    assert!(!manifest.is_settled(), "live sidecar and network are debt");
    docker.reconcile_resources(&mut manifest).unwrap();
    assert!(manifest.is_settled());
    assert_eq!(f.live(), 0);
    assert_eq!(
        f.mutations(),
        ["network create", "run -d", "container rm", "network rm"]
    );
    assert!(
        manifest
            .records()
            .iter()
            .all(|r| r.state() == State::Succeeded)
    );
    // Reopening restores the same settled evidence without replay.
    drop(manifest);
    assert!(f.manifest().is_settled());
}

#[test]
fn sidecar_argv_is_the_hardened_legacy_policy_on_the_run_network() {
    for viewer in [true, false] {
        let f = Fixture::new();
        let mut docker = f.docker();
        let mut manifest = f.manifest();
        start(&f, &mut docker, &mut manifest, viewer).unwrap();
        let args = f.run_args();
        let name = &args[args.iter().position(|a| a == "--name").unwrap() + 1];
        let network = &args[args.iter().position(|a| a == "--network").unwrap() + 1];
        assert!(name.starts_with("pithos-probe-") && network.starts_with("pithos-probe-"));
        let mut expected: Vec<String> = [
            "run",
            "-d",
            "--pull=never",
            "--name",
            name,
            "--label",
            &format!("io.pithos.probe.name={name}"),
            "--label",
            "io.pithos.probe.request=browser-1",
            "--label",
            "io.pithos.probe.run=run-1",
            "--network",
            network,
            "--network-alias",
            "browser",
            "--user",
            &identity().docker_user(),
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--security-opt",
            &format!("seccomp={}", f.seccomp.display()),
            "--read-only",
            "--shm-size=512m",
            "--memory=2g",
            "--pids-limit=512",
            "--tmpfs",
            "/tmp:rw,nosuid,nodev,size=512m,mode=1777",
            "--mount",
            // The shared helper quotes the source, as for every broker bind.
            &format!(
                "type=bind,\"source={}\",target=/run/pithos-browser/server.json,readonly",
                f.server.display()
            ),
        ]
        .map(str::to_owned)
        .to_vec();
        if viewer {
            expected.extend(["-p".into(), "127.0.0.1::6080".into()]);
        }
        expected.push(image().as_str().into());
        assert_eq!(args, expected);
        for forbidden in [
            "--privileged",
            "docker.sock",
            "/workspace",
            "/home/pi",
            "--cap-add",
        ] {
            assert!(!args.iter().any(|a| a.contains(forbidden)), "{forbidden}");
        }
        docker.reconcile_resources(&mut manifest).unwrap();
    }
}

#[test]
fn unhealthy_or_exited_sidecar_fails_start_and_cleanup_still_removes_all() {
    for failure in ["unhealthy", "exits"] {
        let f = Fixture::new();
        f.set(failure);
        let mut docker = f.docker();
        let mut manifest = f.manifest();
        assert!(
            start(&f, &mut docker, &mut manifest, false).is_err(),
            "{failure}"
        );
        docker.reconcile_resources(&mut manifest).unwrap();
        assert!(manifest.is_settled(), "{failure}");
        assert_eq!(f.live(), 0, "{failure}");
    }
}

#[test]
fn tampered_sidecar_is_quarantined_never_removed() {
    let f = Fixture::new();
    f.set("tampered-browser");
    let mut docker = f.docker();
    let mut manifest = f.manifest();
    assert!(start(&f, &mut docker, &mut manifest, false).is_err());
    assert!(docker.reconcile_resources(&mut manifest).is_err());
    assert!(!manifest.is_settled());
    assert!(!f.mutations().contains(&"container rm".to_string()));
    // The network stays too: it cannot be emptied while the sidecar is unknown.
    assert!(!f.mutations().contains(&"network rm".to_string()));
}

#[test]
fn foreign_network_state_is_never_removed() {
    for foreign in ["foreign-network-label", "foreign-attached"] {
        let f = Fixture::new();
        f.set(foreign);
        let mut docker = f.docker();
        let mut manifest = f.manifest();
        let _ = start(&f, &mut docker, &mut manifest, false);
        assert!(
            docker.reconcile_resources(&mut manifest).is_err(),
            "{foreign}"
        );
        assert!(!manifest.is_settled(), "{foreign}");
        assert!(
            !f.mutations().contains(&"network rm".to_string()),
            "{foreign}"
        );
    }
}

#[test]
fn seccomp_profile_must_be_the_bundled_one() {
    let f = Fixture::new();
    fs::write(&f.seccomp, b"{\"defaultAction\":\"SCMP_ACT_ALLOW\"}").unwrap();
    let mut docker = f.docker();
    let mut manifest = f.manifest();
    assert!(start(&f, &mut docker, &mut manifest, false).is_err());
    assert!(!f.mutations().contains(&"run -d".to_string()));
    docker.reconcile_resources(&mut manifest).unwrap();
    assert_eq!(f.live(), 0);
}

#[test]
fn debug_output_never_names_private_paths() {
    let f = Fixture::new();
    let (image, server, seccomp, viewer) = f.inputs(true);
    let inputs = BrowserInputs {
        image: &image,
        identity: identity(),
        server: &server,
        seccomp: &seccomp,
        viewer,
    };
    let text = format!("{inputs:?}");
    assert!(!text.contains(server.to_str().unwrap()));
}

#[test]
fn viewer_is_reported_only_on_ipv4_loopback() {
    for (mode, expected) in [
        (None, Some("http://127.0.0.1:49153/")),
        (Some("viewer-public"), None),
    ] {
        let f = Fixture::new();
        if let Some(mode) = mode {
            f.set(mode);
        }
        let mut docker = f.docker();
        let mut manifest = f.manifest();
        start(&f, &mut docker, &mut manifest, true).unwrap();
        let viewer = docker.browser_viewer(&manifest, "browser-1");
        assert_eq!(viewer.ok().as_deref(), expected, "{mode:?}");
        docker.reconcile_resources(&mut manifest).unwrap();
    }
    // Headless publishes nothing.
    let f = Fixture::new();
    let mut docker = f.docker();
    let mut manifest = f.manifest();
    start(&f, &mut docker, &mut manifest, false).unwrap();
    assert!(docker.browser_viewer(&manifest, "browser-1").is_err());
    docker.reconcile_resources(&mut manifest).unwrap();
}
