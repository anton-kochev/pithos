#![cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "fixtures/bounded.rs"]
mod bounded;

use pithos::{
    broker::{
        credential::RunCredential,
        grant::HostGrant,
        host::HostInputs,
        journal::State,
        resources::ResourceManifest,
        runtime::{BrokerRuntime, RuntimePhase, RuntimePoll, RuntimeSetup},
        transport::{BrokerEndpoint, HostAccess},
    },
    docker::{
        HomeLease, HostIdentity, ImmutableImageId, ManagedDocker, OwnedProbeError, PiBrowser,
        PiInputs, VolumeName,
    },
    dockerfile::PI_LAUNCH_ARGV,
    lifecycle::{InteractiveChild, InteractiveLimits, InteractivePoll, Shutdown, ShutdownReason},
};
use serde_json::Value;
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    os::{
        fd::FromRawFd,
        unix::{fs::PermissionsExt, net::UnixListener, process::CommandExt},
    },
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

const HOST_CONFIG: &[u8] = b"toolchains: {}\nsessions: {storage: volume}\npi: {version: '0.84.4', extensions: {x: 'npm:1.0'}}\n";

fn image() -> ImmutableImageId {
    ImmutableImageId::new(&format!("sha256:{}", "a".repeat(64))).unwrap()
}
fn identity() -> HostIdentity {
    HostIdentity::effective().unwrap()
}

struct Fixture {
    root: tempfile::TempDir,
    _socket: UnixListener,
}
// The macOS /usr/bin/python3 shim picks its tool from argv[0], so a symlink
// named `python` fails. Link the interpreter the shim actually runs.
fn python_interpreter() -> PathBuf {
    let output = Command::new("/usr/bin/python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .unwrap();
    assert!(output.status.success());
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        for name in ["config", "run", "credential", "work,\"space"] {
            fs::create_dir(root.path().join(name)).unwrap();
            fs::set_permissions(root.path().join(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::os::unix::fs::symlink(python_interpreter(), root.path().join("python")).unwrap();
        #[cfg(target_os = "linux")]
        {
            let socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
            socket.connect("192.0.2.1:9").unwrap();
            let gateway = socket.local_addr().unwrap().ip().to_string();
            let octets = gateway.parse::<std::net::Ipv4Addr>().unwrap().octets();
            let subnet = match octets {
                [10, ..] => "10.0.0.0/8",
                [172, 16..=31, ..] => "172.16.0.0/12",
                [192, 168, ..] => "192.168.0.0/16",
                _ => panic!("fixture requires a private host IPv4 address"),
            };
            fs::write(root.path().join("gateway"), gateway).unwrap();
            fs::write(root.path().join("subnet"), subnet).unwrap();
        }
        #[cfg(not(target_os = "linux"))]
        fs::write(root.path().join("gateway"), "10.1.2.3").unwrap();
        let script = r#"#!__PYTHON__
import csv, hashlib, json, os, pathlib, signal, sys, time
root=pathlib.Path(__ROOT__)
a=sys.argv[5:]
def emit(v): print(json.dumps(v))
def opt(k): return a[a.index(k)+1]
def mode(k): return (root/k).exists()
# A workspace-grant coordinator always puts Pi on the owned run network.
def on_network(): return mode('network') or mode('host-coordinator')
def home_volume(): return __HOST_VOLUME__ if mode('host-coordinator') else 'pi-home'
with (root/'calls').open('a') as f: f.write(json.dumps(sys.argv[1:])+'\n')
if a[0]=='info':
    emit({'id':'foreign' if mode('changed-daemon') else 'daemon-one','os_type':'linux','security_options':[]})
    if mode('remove-interpreter'): (root/'python').unlink()
elif a[:2]==['network','inspect'] and a[-1]!='e'*64:
    gateway=(root/'gateway').read_text()
    if mode('bridge-changed'): gateway='10.255.255.254' if gateway!='10.255.255.254' else '10.255.255.253'
    emit({'name':'bridge','driver':'bridge','scope':'local','internal':False,'enable_ipv6':False,
          'ipam':{'Driver':'default','Options':None,'Config':[{'Subnet':(root/'subnet').read_text(),'Gateway':gateway}]}})
elif a[:2]==['network','create']:
    labels=dict(a[i+1].split('=',1) for i,v in enumerate(a) if v=='--label')
    n={'id':'e'*64,'name':a[-1],'driver':'bridge','scope':'local','internal':False,'attachable':False,'ingress':False,'labels':labels}
    (root/'network.json').write_text(json.dumps(n)); print(n['id'])
elif a[:2]==['network','ls']:
    n=json.loads((root/'network.json').read_text()) if (root/'network.json').exists() else None
    if n and opt('--filter') in ['id='+n['id'],'name=^'+n['name']+'$']: emit(n['id'])
elif a[:2]==['network','inspect'] and a[-1]=='e'*64:
    n=json.loads((root/'network.json').read_text())
    c=json.loads((root/'container.json').read_text()) if (root/'container.json').exists() else None
    emit(dict(n,containers={c['id']:{}} if c and c['host']['NetworkMode']==n['name'] else {}))
elif a[:2]==['network','rm']:
    assert a[-1]=='e'*64 and not (root/'container.json').exists(), 'network in use'
    (root/'network.json').unlink()
elif a[:2]==['image','ls'] and mode('host-coordinator'):
    assert '--no-trunc' in a and opt('--filter')=='label=io.pithos.broker.identity-fingerprint='+__FINGERPRINT__
    emit(__IMAGE__)
elif a[:2]==['image','inspect']:
    if mode('host-coordinator') and a[-1]==__BASE_REF__: emit({'id':__BASE_IMAGE__})
    elif mode('host-coordinator') and '\"Labels\"' in a[3]:
        assert a[-1]==__IMAGE__
        emit({'id':__IMAGE__,'user':__USER__,'env':['HOME=/home/pi','USER=pi','LOGNAME=pi'],
              'volumes':None,'labels':{'io.pithos.broker.identity-fingerprint':__FINGERPRINT__}})
    elif '\"Volumes\"' in a[3]: emit({'id':__IMAGE__,'volumes':{'/extra':{}} if mode('volumes') else None})
    elif '\"Cmd\"' in a[3]:
        emit({'id':__IMAGE__,'cmd':[] if mode('empty-cmd') else ['pi','--session-dir','/home/pi/.pi/agent/sessions']})
        (root/'cmd-queried').touch()
    else: emit({'id':__IMAGE__,'user':__USER__,'env':['HOME=/home/pi','USER=pi','LOGNAME=pi']})
elif a[:2]==['volume','ls']: emit(home_volume())
elif a[:2]==['volume','inspect']:
    if mode('host-coordinator'): assert a[-1]==home_volume()
    emit({'name':home_volume(),'driver':'local','scope':'local','options':None,'created_at':'stable'})
    if mode('spawn-fail') and mode('cmd-queried'):
        count=int((root/'volume-count').read_text())+1 if mode('volume-count') else 1
        (root/'volume-count').write_text(str(count))
        if count==2: (root/'remove-interpreter').touch()
    if mode('bridge-changes-before-intent') and mode('cmd-queried'): (root/'bridge-changed').touch()
elif a[0]=='run':
    manifest=json.loads((root/'run/resources.json').read_text())
    journal=json.loads((root/'run/journal.json').read_text())
    r=next(r for r in manifest['resources'] if r['name']==opt('--name'))
    assert next(j for j in journal['records'] if j['request_id']==r['request_id'])['state']=='running'
    assert r['observed_id'] is None and not r['local_reaped']
    pi=r['spec']['operation']['kind']=='pi'
    assert '--rm' not in a and '--pull=never' in a
    assert '--cap-drop=ALL' in a and '--security-opt=no-new-privileges' in a
    if mode('host-coordinator'): assert str(root/'socket') not in ' '.join(a) and str(root/'config') not in ' '.join(a)
    assert opt('--user')==__USER__
    labels=dict(a[i+1].split('=',1) for i,v in enumerate(a) if v=='--label')
    assert labels==r['labels']
    mounts=[]; hosts=[]
    extra_host_indices=[i for i,v in enumerate(a) if v=='--add-host']
    assert not any(v.startswith('--add-host=') for v in a)
    extra_hosts=[a[i+1] for i in extra_host_indices]
    for i,v in enumerate(a):
        if v!='--mount': continue
        kv=dict((p.split('=',1)+[''])[:2] for p in next(csv.reader([a[i+1]])))
        ro='readonly' in kv
        m={'Type':kv['type'],'Source':kv['source'],'Destination':kv['target'],'RW':not ro,'Propagation':'rprivate'}
        h={'Type':kv['type'],'Source':kv['source'],'Target':kv['target'],'ReadOnly':ro}
        if kv['type']=='volume':
            assert 'volume-nocopy' in kv
            m.update({'Name':kv['source'],'Driver':'local','Source':'/var/lib/docker/volumes/'+kv['source']+'/_data','Propagation':''})
            h['VolumeOptions']={'NoCopy':True}
        if pi and mode('omitted-rw-default') and not ro: h.pop('ReadOnly')
        mounts.append(m); hosts.append(h)
    cmd=a[a.index(__IMAGE__)+1:]
    assert hashlib.sha256(json.dumps(cmd,separators=(',',':'),ensure_ascii=False).encode()).hexdigest()==r['spec']['program_digest']
    entry='/usr/local/bin/entrypoint.sh' if pi else '/usr/bin/python3'
    assert '--entrypoint='+entry in a
    if pi:
        assert all(os.isatty(fd) for fd in [0,1,2]) and os.tcgetpgrp(0)==os.getpgrp()
        assert '-it' in a and '--read-only' not in a
        if on_network():
            network=json.loads((root/'network.json').read_text())['name']
            assert '--network=bridge' not in a and opt('--network')==network and opt('--network-alias')=='pithos-app'
        else:
            assert '--network=bridge' in a and '--network' not in a
        assert opt('--workdir')=='/workspace'
        # A workspace-grant coordinator also mounts the broker's Pi extension.
        extension=mode('host-coordinator')
        # Its config declares pi.extensions: the entrypoint's manifest is a private read-only copy.
        assert len(mounts)==3+(2 if mode('browser') else 0)+(2 if extension else 0)
        by={m['Destination']:m for m in mounts}
        if extension:
            assert by['/run/pithos-broker/extension.mjs']['Source']==str(root/'credential/pithos-broker.mjs')
            assert not by['/run/pithos-broker/extension.mjs']['RW']
            assert by['/etc/pithos/extensions.list']['Source']==str(root/'credential/extensions.list')
            assert not by['/etc/pithos/extensions.list']['RW']
            assert (root/'credential/extensions.list').read_text()=='x\tnpm:1.0\n'
        if mode('browser'):
            assert by['/run/pithos-browser/client.json']['Source']==str(root/'browser-run/client.json')
            assert by['/run/pithos-browser/skills']['Source']==str(root/'browser-run/skills')
            assert not by['/run/pithos-browser/client.json']['RW'] and not by['/run/pithos-browser/skills']['RW']
        assert by['/workspace']['Source']==str(root/'work,\"space') and by['/workspace']['RW']
        assert by['/home/pi']['Name']==home_volume() and by['/home/pi']['RW']
        assert by['/run/pithos-broker/client.json']['Source']==str(root/'credential/broker-client.json')
        assert not by['/run/pithos-broker/client.json']['RW']
        # Docker's own flags only: everything after the image belongs to Pi.
        # The only env source Pi may get is the broker's private Postgres file.
        allowed=['--env-file'] if mode('pg-env') else []
        assert not any(v in a[:a.index(__IMAGE__)] for v in ['--privileged','--env','-e','--env-file','--volume','-v'] if v not in allowed)
        if mode('pg-env'):
            source=str(root/'pg/pi-postgres.env')
            assert a[:a.index(__IMAGE__)].count('--env-file')==1 and opt('--env-file')==source
            assert r['spec']['operation']['env_source']==source
            assert 'pg-secret' not in '\n'.join(a)
        access=r['spec']['operation']['host_access']
        gateway=(root/'gateway').read_text()
        expected_hosts=['host.docker.internal:'+gateway] if access=='linux-host-gateway' else []
        assert extra_hosts==expected_hosts
        if access=='linux-host-gateway': assert r['spec']['operation']['gateway']==gateway
        assert all(i<a.index(__IMAGE__) for i in extra_host_indices)
        assert not any(k.startswith('DOCKER_') or k.startswith('PITHOS_') for k in os.environ)
        if mode('host-coordinator'): assert (set(os.environ) - set(('__CF_USER_TEXT_ENCODING','SDKROOT','CPATH','LIBRARY_PATH','MANPATH'))).issubset({'LC_CTYPE'})
        assert cmd==(__PI_LAUNCH_ARGV__+['--extension','/run/pithos-broker/extension.mjs'] if mode('host-coordinator') else ['pi','private-host-prompt'] if mode('explicit') else ['pi','--skill','/run/pithos-browser/skills/browser-automation'] if mode('browser') else ['pi','--session-dir','/home/pi/.pi/agent/sessions'])
        credential=json.loads((root/'credential/broker-client.json').read_text())
        token=credential['token']; endpoint=credential['endpoint']
        environment=json.dumps(dict(os.environ))
        assert token not in json.dumps(manifest) and token not in json.dumps(journal) and token not in '\n'.join(a) and token not in environment
        assert endpoint not in '\n'.join(a) and endpoint not in environment
        assert 'private-host-prompt' not in json.dumps(manifest)
        (root/'pi-ran').touch()
        if mode('connected-runtime') or mode('host-coordinator'): time.sleep(0.25)
    else:
        assert cmd[:3]==['-I','-S','-c'] and '--network=none' in a and '--read-only' in a
    cid=format(len(manifest['resources']),'064x')
    c={'id':cid,'name':'/'+r['name'],'image':__IMAGE__,
       'config':{'Image':__IMAGE__,'User':__USER__,'Entrypoint':[entry],'Cmd':cmd,'Labels':labels,'Volumes':None,'Tty':pi,'OpenStdin':pi,'AttachStdin':pi,'AttachStdout':pi,'AttachStderr':pi,'StdinOnce':pi,'WorkingDir':'/workspace' if pi else ''},
       'host':{'NetworkMode':(opt('--network') if on_network() else 'bridge') if pi else 'none','ReadonlyRootfs':not pi,'Privileged':False,'AutoRemove':False,'RestartPolicy':{'Name':'no','MaximumRetryCount':0},'CapDrop':['ALL'],'SecurityOpt':['no-new-privileges'],'CapAdd':None,'GroupAdd':None,'Binds':None,'Devices':None,'PidMode':'','IpcMode':'private','UsernsMode':'','Mounts':hosts,'ExtraHosts':extra_hosts or None},
       'mounts':mounts,'state':{'Status':'running' if pi and mode('detached') else 'exited','Running':pi and mode('detached'),'ExitCode':0,'Error':'','OOMKilled':False,'Dead':False}}
    if pi and mode('engine-failed'): c['state']['ExitCode']=7
    # Docker Desktop may record shared host paths under its VM mount.
    if pi and mode('desktop-host-mnt'):
        for x in c['mounts']+c['host']['Mounts']:
            if x['Type']=='bind': x['Source']='/host_mnt'+x['Source']
    if pi and mode('wrong-network'): c['host']['NetworkMode']='host'
    if pi and mode('wrong-entrypoint'): c['config']['Entrypoint']=['/bin/sh']
    if pi and mode('wrong-command'): c['config']['Cmd']=['foreign']
    if pi and mode('wrong-image'): c['image']='sha256:'+'c'*64
    if pi and mode('wrong-user'): c['config']['User']='0:0'
    if pi and mode('wrong-label'): c['config']['Labels']['io.pithos.probe.run']='foreign'
    if pi and mode('wrong-mount'): c['mounts'][2]['RW']=True
    if pi and mode('extra-mount'): c['host']['Mounts'].append({'Type':'bind','Source':'/var/run/docker.sock','Target':'/var/run/docker.sock'})
    if pi and mode('wrong-tty'): c['config']['Tty']=False
    if pi and mode('wrong-attach'): c['config']['AttachStdout']=False
    if pi and mode('wrong-stdin-once'): c['config']['StdinOnce']=False
    if pi and mode('wrong-restart'): c['host']['RestartPolicy']={'Name':'always','MaximumRetryCount':0}
    if pi and mode('wrong-hardening'): c['host']['CapDrop']=[]
    if pi and mode('altered-host-access'): c['host']['ExtraHosts']=['host.docker.internal:10.255.255.253']
    if pi and mode('missing-host-access'): c['host']['ExtraHosts']=None
    if pi and mode('duplicate-host-access'): c['host']['ExtraHosts']*=2
    if pi and mode('foreign-host-access'): c['host']['ExtraHosts']=['foreign.example:'+ (root/'gateway').read_text()]
    if pi and (mode('offline-extra-hosts') or mode('desktop-extra-hosts')): c['host']['ExtraHosts']=['foreign.example:host-gateway']
    if pi and mode('malformed-host-access'): c['host']['ExtraHosts']='host.docker.internal:host-gateway'
    if pi and mode('daemon-change'): (root/'changed-daemon').touch()
    if pi and mode('client-change'): os.chmod(root/'docker',0o500)
    if not (pi and mode('empty-create')): (root/'container.json').write_text(json.dumps(c))
    if pi and (mode('connected-runtime-interrupt') or mode('connected-runtime-terminate')): time.sleep(0.25)
    if pi and (mode('signaled') or mode('connected-runtime-signaled')): os.kill(os.getpid(), signal.SIGTERM)
    sys.exit(9 if pi and (mode('cli-failed') or mode('connected-runtime-nonzero') or mode('connected-runtime-record-retry') or mode('connected-runtime-requested')) else 0)
elif a[:2]==['container','ls']:
    if opt('--filter')=='volume='+home_volume():
        if mode('busy'): emit('f'*64)
    elif (root/'container.json').exists():
        c=json.loads((root/'container.json').read_text())
        if opt('--filter') in ['id='+c['id'],'name=^'+c['name']+'$']: emit(c['id'])
elif a[:2]==['container','inspect']:
    c=json.loads((root/'container.json').read_text()); assert a[-1]==c['id']; emit(c)
elif a[:2]==['container','rm']:
    c=json.loads((root/'container.json').read_text()); assert a==['container','rm','--force',c['id']]
    r=json.loads((root/'run/resources.json').read_text())['resources'][-1]
    assert r['local_reaped'] and r['observed_id']==c['id']
    (root/'container.json').unlink()
else: sys.exit(99)
"#
            .replace("__PYTHON__", root.path().join("python").to_str().unwrap())
            .replace("__ROOT__", &serde_json::to_string(root.path()).unwrap())
            .replace("__IMAGE__", &serde_json::to_string(image().as_str()).unwrap())
            .replace("__USER__", &serde_json::to_string(&identity().docker_user()).unwrap())
            .replace("__HOST_VOLUME__", &serde_json::to_string(&format!(
                "pithos-home-{}",
                pithos::project::name_from_path(&root.path().join("work,\"space")).unwrap()
            )).unwrap())
            .replace("__BASE_REF__", &serde_json::to_string("ghcr.io/anton-kochev/pithos:base").unwrap())
            .replace("__BASE_IMAGE__", &serde_json::to_string(&format!("sha256:{}", "b".repeat(64))).unwrap())
            .replace("__PI_LAUNCH_ARGV__", &serde_json::to_string(&PI_LAUNCH_ARGV).unwrap())
            .replace("__FINGERPRINT__", &serde_json::to_string(&pithos::docker::managed_image_cache::fingerprint(
                &pithos::config::load(HOST_CONFIG).unwrap(),
                HOST_CONFIG,
                identity(),
                &ImmutableImageId::new(&format!("sha256:{}", "b".repeat(64))).unwrap(),
            ).unwrap()).unwrap());
        fs::write(root.path().join("docker"), script).unwrap();
        fs::set_permissions(
            root.path().join("docker"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let socket = UnixListener::bind(root.path().join("socket")).unwrap();
        Self {
            root,
            _socket: socket,
        }
    }
    fn docker(&self, shutdown: Shutdown) -> ManagedDocker {
        ManagedDocker::new(
            &self.root.path().join("docker"),
            &format!("unix://{}", self.root.path().join("socket").display()),
            &self.root.path().join("config"),
            shutdown,
        )
        .unwrap()
    }
    fn workspace(&self) -> PathBuf {
        self.root.path().join("work,\"space")
    }
    fn manifest(&self) -> ResourceManifest {
        ResourceManifest::open(&self.root.path().join("run"), "run-1").unwrap()
    }
    fn snapshot(&self) -> Value {
        serde_json::from_slice(&fs::read(self.root.path().join("run/resources.json")).unwrap())
            .unwrap()
    }
    fn mode(&self, mode: &str) {
        fs::write(self.root.path().join(mode), "").unwrap();
    }
    fn credential(&self) -> RunCredential {
        RunCredential::create(
            self.root.path().join("credential"),
            "http://127.0.0.1:12345",
        )
        .unwrap()
    }
    /// Private run files the host writes for a browser-enabled Pi.
    fn browser_files(&self) -> (PathBuf, PathBuf) {
        let dir = self.root.path().join("browser-run");
        let skills = dir.join("skills");
        fs::create_dir_all(skills.join("browser-automation")).unwrap();
        for path in [&dir, &skills] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::write(skills.join("browser-automation/SKILL.md"), "skill").unwrap();
        let client = dir.join("client.json");
        fs::write(&client, "{}").unwrap();
        fs::set_permissions(&client, fs::Permissions::from_mode(0o600)).unwrap();
        (client, skills)
    }
    fn admit(&self, docker: &mut ManagedDocker, m: &mut ResourceManifest, c: &RunCredential) {
        docker
            .probe_account(m, "account-1", &image(), identity())
            .unwrap();
        docker
            .probe_home(
                m,
                "home-1",
                &VolumeName::new("pi-home").unwrap(),
                &image(),
                identity(),
            )
            .unwrap();
        docker
            .probe_credential(m, "credential-1", c, &image(), identity())
            .unwrap();
        assert!(m.is_settled());
    }
}

#[test]
fn pi_fixture() {
    let Ok(mode) = std::env::var("PITHOS_PI_FIXTURE") else {
        return;
    };
    let f = Fixture::new();
    if mode == "host-coordinator" {
        f.mode(&mode);
        let stage = f.root.path().join("stage");
        let lease_root = f.root.path().join(".pithos-home-leases");
        for path in [&stage, &lease_root] {
            fs::create_dir(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        // This fixture runs in its own PTY subprocess, never in the concurrent test process.
        unsafe { std::env::set_var("HOME", f.root.path()) };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let yaml = HOST_CONFIG;
        let selected = HostInputs {
            workspace: f.workspace(),
            pithos: yaml.to_vec(),
            executable: f.root.path().join("docker"),
            socket: f.root.path().join("socket"),
            config: f.root.path().join("config"),
            stage_root: stage,
            run_directory: f.root.path().join("credential"),
            manifest_directory: f.root.path().join("run"),
            lease_root: lease_root.clone(),
            run_id: "host-run".into(),
            command: Vec::new(),
            interactive_limits: InteractiveLimits::default(),
        }
        .validate()
        .unwrap();
        let volume = VolumeName::new(&format!(
            "pithos-home-{}",
            pithos::project::name_from_path(&f.workspace()).unwrap()
        ))
        .unwrap();
        assert_eq!(selected.volume().as_str(), volume.as_str());
        let endpoint = BrokerEndpoint::offline(listener).unwrap();
        let mut coordinator = match selected.start_offline(HostGrant::workspace(), endpoint) {
            Ok(owner) => owner,
            Err(failure) => panic!("host coordinator setup failed: {}", failure.error),
        };
        let credential: Value = serde_json::from_slice(
            &fs::read(f.root.path().join("credential/broker-client.json")).unwrap(),
        )
        .unwrap();
        let mut peer = TcpStream::connect(address).unwrap();
        peer.set_nonblocking(true).unwrap();
        write!(
            peer,
            "GET /v1/status HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\n\r\n",
            address,
            credential["token"].as_str().unwrap()
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut response = Vec::new();
        loop {
            let result = coordinator.poll().unwrap();
            let mut bytes = [0; 1024];
            match peer.read(&mut bytes) {
                Ok(0) => {}
                Ok(count) => response.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("status read failed: {error}"),
            }
            if result == RuntimePoll::Complete {
                break;
            }
            assert!(Instant::now() < deadline, "host coordinator did not settle");
            std::thread::sleep(Duration::from_millis(2));
        }
        let response = String::from_utf8(response).unwrap();
        assert!(response.contains("HTTP/1.1 200 OK"), "{response:?}");
        assert!(response.contains("\"phase\":\"ready\""), "{response:?}");
        assert_eq!(coordinator.terminal_exit_code(), Some(0));
        assert!(f.root.path().join("pi-ran").exists());
        let calls = fs::read_to_string(f.root.path().join("calls")).unwrap();
        let calls: Vec<Vec<String>> = calls
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            calls
                .iter()
                .any(|a| a.windows(2).any(|w| w == ["image", "ls"]))
        );
        assert!(calls.iter().any(|a| {
            a.windows(2).any(|w| w == ["image", "inspect"])
                && a.last()
                    .is_some_and(|id| id == "ghcr.io/anton-kochev/pithos:base")
        }));
        assert!(calls.iter().any(|a| {
            a.windows(2).any(|w| w == ["image", "inspect"])
                && a.iter()
                    .any(|arg| arg.contains(r#"(index .Config "Labels")"#))
                && a.last().is_some_and(|id| id == image().as_str())
        }));
        assert!(
            !calls
                .iter()
                .any(|a| a.iter().any(|arg| arg == "build" || arg == "pull"))
        );
        let pi = calls
            .iter()
            .find(|a| {
                a.iter().any(|arg| arg == "run")
                    && a.iter().any(|arg| arg == image().as_str())
                    && a.iter().any(|arg| arg == "-it")
            })
            .unwrap();
        let pos = pi.iter().position(|arg| arg == image().as_str()).unwrap();
        // The workspace grant loads the broker's app tools into Pi.
        assert_eq!(
            &pi[pos + 1..],
            [
                PI_LAUNCH_ARGV.as_slice(),
                &["--extension", "/run/pithos-broker/extension.mjs"]
            ]
            .concat()
        );
        assert!(
            !f.root.path().join("credential/pithos-broker.mjs").exists(),
            "extension file removed at cleanup"
        );
        assert!(
            !f.root.path().join("credential/extensions.list").exists(),
            "extensions manifest removed at cleanup"
        );
        assert!(!pi[4..].iter().any(|arg| {
            arg.contains(&f.root.path().join("socket").display().to_string())
                || arg.contains(&f.root.path().join("config").display().to_string())
                || arg.contains(credential["token"].as_str().unwrap())
                || arg.contains(&address.to_string())
        }));
        assert_eq!(
            fs::read_dir(f.root.path().join("stage")).unwrap().count(),
            0,
            "cache hit must not build"
        );
        assert!(!f.root.path().join("credential/broker-client.json").exists());
        assert!(coordinator.close_signals().unwrap());
        drop(coordinator);
        let manifest = ResourceManifest::open(&f.root.path().join("run"), "host-run").unwrap();
        assert!(manifest.is_settled());
        // Account, home, credential, the workspace run network, and Pi.
        assert_eq!(manifest.records().len(), 5);
        let lease = HomeLease::broker(&lease_root, &volume).unwrap();
        lease.finish().unwrap();
        return;
    }
    if matches!(
        mode.as_str(),
        "connected-runtime"
            | "connected-runtime-record-retry"
            | "connected-runtime-record-retry-interrupt"
            | "connected-runtime-nonzero"
            | "connected-runtime-signaled"
            | "connected-runtime-interrupt"
            | "connected-runtime-terminate"
            | "connected-runtime-requested"
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut runtime = BrokerRuntime::begin(
            HostGrant::managed_pi_run(),
            BrokerEndpoint::offline(listener).unwrap(),
            RuntimeSetup {
                executable: f.root.path().join("docker"),
                socket: f.root.path().join("socket"),
                config: f.root.path().join("config"),
                run_directory: f.root.path().join("credential"),
                manifest_directory: f.root.path().join("run"),
                lease_root: f.root.path().join("leases"),
                run_id: "connected-run".into(),
                volume: VolumeName::new("pi-home").unwrap(),
                image: image(),
                identity: identity(),
                workspace: f.workspace(),
                command: Vec::new(),
                interactive_limits: InteractiveLimits::default(),
                browser: None,
                stage_root: None,
                extensions: None,
                postgres: None,
            },
        )
        .unwrap();
        f.mode(&mode);
        runtime.admit_and_start_pi().unwrap();
        assert_eq!(runtime.phase(), RuntimePhase::Ready);
        assert_eq!(runtime.terminal_exit_code(), None);
        if mode == "connected-runtime-record-retry"
            || mode == "connected-runtime-record-retry-interrupt"
        {
            let marker = f.root.path().join("pi-ran");
            let marker_deadline = Instant::now() + Duration::from_secs(10);
            while !marker.exists() {
                assert!(
                    Instant::now() < marker_deadline,
                    "Pi did not reach the fixture marker"
                );
                std::thread::sleep(Duration::from_millis(2));
            }

            let resources = f.root.path().join("run/resources.json");
            let saved_resources = f.root.path().join("run/saved-resources.json");
            fs::rename(&resources, &saved_resources).unwrap();
            fs::create_dir(&resources).unwrap();

            assert_eq!(
                runtime.run_until_terminal(Duration::from_millis(2)),
                RuntimePoll::RecoveryRequired
            );
            assert_eq!(runtime.phase(), RuntimePhase::RecoveryRequired);
            assert_eq!(runtime.terminal_exit_code(), None);
            if mode == "connected-runtime-record-retry-interrupt" {
                runtime.shutdown_token().request(ShutdownReason::Interrupt);
            }
            assert!(f.root.path().join("credential/broker-client.json").exists());
            assert!(
                HomeLease::broker(
                    &f.root.path().join("leases"),
                    &VolumeName::new("pi-home").unwrap(),
                )
                .is_err(),
                "recovery must retain the home lease"
            );
            assert_eq!(
                runtime.poll_cleanup(),
                RuntimePoll::RecoveryRequired,
                "a failed reopen must retain the pending report and evidence"
            );
            assert!(f.root.path().join("credential/broker-client.json").exists());

            fs::remove_dir(&resources).unwrap();
            fs::rename(&saved_resources, &resources).unwrap();
            let cleanup_deadline = Instant::now() + Duration::from_secs(10);
            while matches!(
                runtime.poll_cleanup(),
                RuntimePoll::Running | RuntimePoll::RecoveryRequired
            ) {
                assert!(
                    Instant::now() < cleanup_deadline,
                    "reopened runtime did not durably reconcile the Pi exit"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(runtime.phase(), RuntimePhase::Complete);
            assert_eq!(
                runtime.terminal_exit_code(),
                Some(if mode == "connected-runtime-record-retry-interrupt" {
                    130
                } else {
                    9
                })
            );
            assert!(!f.root.path().join("credential/broker-client.json").exists());
            drop(runtime);
            let manifest =
                ResourceManifest::open(&f.root.path().join("run"), "connected-run").unwrap();
            assert!(manifest.is_settled());
            assert_eq!(manifest.records().len(), 4);
            let lease = HomeLease::broker(
                &f.root.path().join("leases"),
                &VolumeName::new("pi-home").unwrap(),
            )
            .unwrap();
            lease.finish().unwrap();
            return;
        }
        if mode != "connected-runtime" {
            assert_eq!(runtime.terminal_exit_code(), None);
            if mode == "connected-runtime-interrupt" || mode == "connected-runtime-terminate" {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !f.root.path().join("container.json").exists() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(2));
                }
                assert!(matches!(
                    runtime.request_shutdown(if mode == "connected-runtime-interrupt" {
                        ShutdownReason::Interrupt
                    } else {
                        ShutdownReason::Terminate
                    }),
                    RuntimePoll::Running | RuntimePoll::Complete
                ));
            } else if mode == "connected-runtime-requested" {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !f.root.path().join("pi-ran").exists() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(2));
                }
                std::thread::sleep(Duration::from_millis(400));
                assert!(matches!(
                    runtime.request_shutdown(ShutdownReason::Requested),
                    RuntimePoll::Running | RuntimePoll::Complete
                ));
            }
            assert_eq!(
                runtime.run_until_terminal(Duration::from_millis(2)),
                RuntimePoll::Complete
            );
            assert_eq!(
                runtime.terminal_exit_code(),
                Some(match mode.as_str() {
                    "connected-runtime-nonzero" => 9,
                    "connected-runtime-signaled" => 143,
                    "connected-runtime-interrupt" => 130,
                    "connected-runtime-terminate" => 143,
                    "connected-runtime-requested" => 9,
                    _ => unreachable!(),
                })
            );
            assert!(!f.root.path().join("credential/broker-client.json").exists());
            return;
        }
        let credential: Value = serde_json::from_slice(
            &fs::read(f.root.path().join("credential/broker-client.json")).unwrap(),
        )
        .unwrap();
        let mut peer = TcpStream::connect(runtime.local_addr()).unwrap();
        peer.set_nonblocking(true).unwrap();
        write!(
            peer,
            "GET /v1/status HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\n\r\n",
            runtime.advertised_authority(),
            credential["token"].as_str().unwrap()
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut response = Vec::new();
        loop {
            let result = runtime.poll().unwrap();
            let mut bytes = [0; 1024];
            match peer.read(&mut bytes) {
                Ok(0) => {}
                Ok(count) => response.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("status read failed: {error}"),
            }
            if result == RuntimePoll::Complete {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "connected runtime did not settle"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(runtime.terminal_exit_code(), Some(0));
        let response = String::from_utf8(response).unwrap();
        assert!(response.contains("HTTP/1.1 200 OK"), "{response:?}");
        assert!(response.contains("\"phase\":\"ready\""), "{response:?}");
        assert!(!f.root.path().join("credential/broker-client.json").exists());
        drop(runtime);
        let manifest = ResourceManifest::open(&f.root.path().join("run"), "connected-run").unwrap();
        assert!(manifest.is_settled());
        assert_eq!(manifest.records().len(), 4);
        let lease = HomeLease::broker(
            &f.root.path().join("leases"),
            &VolumeName::new("pi-home").unwrap(),
        )
        .unwrap();
        lease.finish().unwrap();
        return;
    }
    if mode.starts_with("browser") || mode == "network-only" || mode == "pg-env" {
        browser_fixture(&f, &mode);
        return;
    }
    let shutdown = Shutdown::new();
    let mut docker = f.docker(shutdown.clone());
    let mut m = f.manifest();
    let mut c = f.credential();
    let volume = VolumeName::new("pi-home").unwrap();
    let lease = HomeLease::broker(&f.root.path().join("leases"), &volume).unwrap();
    f.admit(&mut docker, &mut m, &c);
    fs::write(f.root.path().join(&mode), "").unwrap();
    if mode == "intent-storage-failure" {
        fs::rename(
            f.root.path().join("run/resources.json"),
            f.root.path().join("run/saved-resources.json"),
        )
        .unwrap();
        fs::create_dir(f.root.path().join("run/resources.json")).unwrap();
    }
    let child_shutdown = if mode == "cancel-child" {
        Shutdown::new()
    } else {
        shutdown.clone()
    };
    if mode == "cancel-child" {
        child_shutdown.request(ShutdownReason::Requested);
    }
    let mut child = InteractiveChild::new(InteractiveLimits::default(), child_shutdown).unwrap();
    let command = if mode == "explicit" {
        vec!["pi".into(), "private-host-prompt".into()]
    } else {
        vec![]
    };
    #[cfg(target_os = "linux")]
    let endpoint = if mode == "linux-host-gateway"
        || mode == "bridge-changes-before-intent"
        || (mode.ends_with("host-access")
            && mode != "offline-extra-hosts"
            && mode != "desktop-extra-hosts")
    {
        Some(BrokerEndpoint::linux(&mut docker).unwrap())
    } else {
        None
    };
    let start = docker.start_pi(
        &mut m,
        &mut child,
        "pi-1",
        PiInputs {
            volume: &volume,
            image: &image(),
            identity: identity(),
            workspace: &f.workspace(),
            credential: &c,
            command: &command,
            host_access: if mode == "docker-desktop" || mode == "desktop-extra-hosts" {
                HostAccess::DockerDesktop
            } else {
                #[cfg(target_os = "linux")]
                {
                    endpoint
                        .as_ref()
                        .map_or(HostAccess::Offline, BrokerEndpoint::host_access)
                }
                #[cfg(not(target_os = "linux"))]
                {
                    HostAccess::Offline
                }
            },
            network: None,
            browser: None,
            extension: None,
            extensions_list: None,
            env_file: None,
        },
    );
    if mode == "intent-storage-failure" {
        assert!(start.is_err());
        assert!(!child.is_in_flight());
        assert!(
            !m.is_settled(),
            "failed durable publication retains evidence"
        );
        assert!(!f.root.path().join("pi-ran").exists());
        assert!(f.root.path().join("credential/broker-client.json").exists());
        return;
    }
    if mode == "bridge-changes-before-intent" {
        assert!(start.is_err(), "changed bridge must reject Pi");
        assert_eq!(m.records().len(), 3, "no Pi intent may be persisted");
        assert!(!child.is_in_flight());
        assert!(!f.root.path().join("pi-ran").exists());
        assert!(f.root.path().join("credential/broker-client.json").exists());
        return;
    }
    if matches!(mode.as_str(), "no-tty" | "cancel-child" | "spawn-fail") {
        assert!(start.is_err());
        assert!(!child.is_in_flight());
        assert!(m.is_settled(), "known pre-exec failure must settle");
        assert_eq!(m.records()[3].state(), State::Failed);
        assert_eq!(f.snapshot()["resources"][3]["not_spawned"], true);
        assert!(!f.root.path().join("pi-ran").exists());
        c.cleanup().unwrap();
        lease.finish().unwrap();
        return;
    }
    assert!(start.is_ok(), "admitted Pi must actually start: {start:?}");
    assert!(child.is_in_flight());
    assert!(!m.is_settled());
    assert!(
        docker.reconcile_resources(&mut m).is_err(),
        "must not remove before local reap"
    );
    assert_eq!(m.records()[3].state(), State::Running);
    let deadline = Instant::now() + Duration::from_secs(10);
    let report = loop {
        match child.poll() {
            InteractivePoll::Finished(report) => break report,
            InteractivePoll::Running => {}
            other => panic!("unexpected Pi poll: {other:?}"),
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(
        f.root.path().join("pi-ran").exists(),
        "Docker launch policy assertion failed"
    );
    assert!(
        !f.snapshot()["resources"][3]["local_reaped"]
            .as_bool()
            .unwrap()
    );
    if mode == "lost-local-report" {
        drop(m);
        drop(docker);
        let mut m = f.manifest();
        let mut docker = f.docker(shutdown);
        assert!(docker.reconcile_resources(&mut m).is_err());
        assert!(!m.is_settled());
        assert_eq!(m.records()[3].state(), State::Indeterminate);
        assert!(f.root.path().join("container.json").exists());
        assert!(f.root.path().join("credential/broker-client.json").exists());
        return;
    }
    assert!(docker.record_pi_exit(&mut m, "account-1", &report).is_err());
    fs::create_dir(f.root.path().join("other-run")).unwrap();
    fs::set_permissions(
        f.root.path().join("other-run"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let mut other = ResourceManifest::open(&f.root.path().join("other-run"), "run-2").unwrap();
    assert!(docker.record_pi_exit(&mut other, "pi-1", &report).is_err());
    docker.record_pi_exit(&mut m, "pi-1", &report).unwrap();
    assert!(
        !m.is_settled(),
        "local CLI success cannot release credentials"
    );
    assert_eq!(m.records()[3].state(), State::Running);
    if mode == "shutdown-cleanup" {
        shutdown.request(ShutdownReason::Requested);
    }
    if mode == "recover-recorded-exit" {
        drop(m);
        m = f.manifest();
        docker = f.docker(shutdown.clone());
        assert_eq!(m.records()[3].state(), State::Reconciling);
    }
    if matches!(
        mode.as_str(),
        "empty-create" | "daemon-change" | "client-change"
    ) || mode.starts_with("wrong-")
        || mode.ends_with("host-access")
        || mode.ends_with("extra-hosts")
        || mode == "extra-mount"
        || (mode == "desktop-host-mnt" && cfg!(target_os = "linux"))
    {
        assert!(
            docker.reconcile_resources(&mut m).is_err(),
            "{mode}: must quarantine"
        );
        assert!(!m.is_settled());
        assert_eq!(m.records()[3].state(), State::Indeterminate);
        let calls = fs::read_to_string(f.root.path().join("calls")).unwrap();
        let removals = calls.lines().filter(|line| line.contains("\"rm\"")).count();
        assert_eq!(removals, 3, "{mode}: must not remove mismatched Pi");
        assert!(f.root.path().join("credential/broker-client.json").exists());
        return;
    }
    docker.reconcile_resources(&mut m).unwrap();
    assert!(m.is_settled());
    let expected = if matches!(
        mode.as_str(),
        "detached" | "engine-failed" | "cli-failed" | "signaled"
    ) {
        State::Failed
    } else {
        State::Succeeded
    };
    assert_eq!(m.records()[3].state(), expected);
    if mode == "signaled" {
        assert_eq!(f.snapshot()["resources"][3]["pi_exit"]["code"], Value::Null);
        assert_eq!(
            f.snapshot()["resources"][3]["pi_exit"]["signal"],
            libc::SIGTERM
        );
    } else {
        assert_eq!(
            f.snapshot()["resources"][3]["pi_exit"]["code"],
            if mode == "cli-failed" { 9 } else { 0 }
        );
    }
    drop(m);
    let mut m = f.manifest();
    assert!(m.is_settled());
    assert!(
        docker
            .start_pi(
                &mut m,
                &mut child,
                "pi-1",
                PiInputs {
                    volume: &volume,
                    image: &image(),
                    identity: identity(),
                    workspace: &f.workspace(),
                    credential: &c,
                    command: &command,
                    host_access: HostAccess::Offline,
                    network: None,
                    browser: None,
                    extension: None,
                    extensions_list: None,
                    env_file: None,
                }
            )
            .is_err(),
        "never replay an existing Pi intent"
    );
    assert!(!f.root.path().join("container.json").exists());
    c.cleanup().unwrap();
    lease.finish().unwrap();
}

/// Pi on the run's browser network: only a network this manifest owns is
/// accepted, and cleanup removes Pi before the network.
fn browser_fixture(f: &Fixture, mode: &str) {
    let shutdown = Shutdown::new();
    let mut docker = f.docker(shutdown.clone());
    let mut m = f.manifest();
    let mut c = f.credential();
    let volume = VolumeName::new("pi-home").unwrap();
    let lease = HomeLease::broker(&f.root.path().join("leases"), &volume).unwrap();
    f.admit(&mut docker, &mut m, &c);
    // Every mode joins the run network; only "browser" modes add its files.
    f.mode("network");
    let with_files = mode != "network-only" && mode != "pg-env";
    // Workspace runs with a database hand Pi its private connection file.
    let pg_env = (mode == "pg-env").then(|| {
        f.mode("pg-env");
        let dir = f.root.path().join("pg");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let file = dir.join("pi-postgres.env");
        fs::write(&file, "PITHOS_POSTGRES_PASSWORD=pg-secret\n").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        file
    });
    if with_files {
        f.mode("browser");
    }
    let (client, skills) = f.browser_files();
    let other_dir = f.root.path().join("other-run");
    fs::create_dir(&other_dir).unwrap();
    fs::set_permissions(&other_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let mut other = ResourceManifest::open(&other_dir, "run-2").unwrap();
    let network = if mode == "browser-foreign-network" {
        docker.create_run_network(&mut other, "network-1").unwrap()
    } else {
        docker.create_run_network(&mut m, "network-1").unwrap()
    };
    let mut child = InteractiveChild::new(InteractiveLimits::default(), shutdown.clone()).unwrap();
    let command: Vec<String> = if !with_files {
        vec![
            "pi".into(),
            "--session-dir".into(),
            "/home/pi/.pi/agent/sessions".into(),
        ]
    } else {
        [
            "pi",
            "--skill",
            "/run/pithos-browser/skills/browser-automation",
        ]
        .map(str::to_owned)
        .to_vec()
    };
    let start = docker.start_pi(
        &mut m,
        &mut child,
        "pi-1",
        PiInputs {
            volume: &volume,
            image: &image(),
            identity: identity(),
            workspace: &f.workspace(),
            credential: &c,
            command: &command,
            host_access: HostAccess::Offline,
            network: Some(&network),
            browser: with_files.then_some(PiBrowser {
                client: &client,
                skills: &skills,
            }),
            extension: None,
            extensions_list: None,
            env_file: pg_env.as_deref(),
        },
    );
    if mode == "browser-foreign-network" {
        assert!(
            matches!(start, Err(OwnedProbeError::Admission)),
            "{start:?}"
        );
        assert!(!child.is_in_flight());
        assert!(!f.root.path().join("pi-ran").exists());
        docker.reconcile_resources(&mut other).unwrap();
        c.cleanup().unwrap();
        lease.finish().unwrap();
        return;
    }
    start.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let report = loop {
        match child.poll() {
            InteractivePoll::Finished(report) => break report,
            InteractivePoll::Running => {}
            other => panic!("unexpected Pi poll: {other:?}"),
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(f.root.path().join("pi-ran").exists());
    docker.record_pi_exit(&mut m, "pi-1", &report).unwrap();
    docker.reconcile_resources(&mut m).unwrap();
    assert!(m.is_settled());
    assert!(m.records().iter().all(|r| r.state() == State::Succeeded));
    let calls: Vec<Vec<String>> = fs::read_to_string(f.root.path().join("calls"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let at = |cmd: [&str; 2]| calls.iter().rposition(|c| c.windows(2).any(|w| w == cmd));
    let (container_rm, network_rm) = (
        at(["container", "rm"]).unwrap(),
        at(["network", "rm"]).unwrap(),
    );
    assert!(
        container_rm < network_rm,
        "Pi must be removed before its network"
    );
    c.cleanup().unwrap();
    lease.finish().unwrap();
}

#[test]
fn browser_pi_joins_only_its_own_run_network_and_leaves_first() {
    run_fixture("browser");
    run_fixture("browser-foreign-network");
}

#[test]
fn workspace_pi_joins_the_run_network_without_browser_files() {
    run_fixture("network-only");
}

#[test]
fn pi_gets_only_the_private_postgres_env_file() {
    run_fixture("pg-env");
}

// Fake Docker calls race fixed runtime limits; unbounded parallel load flakes.
static SERIAL: bounded::Bounded = bounded::Bounded::new(4);

fn run_fixture(mode: &str) {
    let _serial = SERIAL.acquire();
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
    let (mut master_file, slave_file) =
        unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    for fd in [master, slave] {
        // SAFETY: owned live descriptors. Do not leak the master into children.
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
            -1
        );
    }
    // SAFETY: owned master; bounded nonblocking output collection.
    assert_ne!(
        unsafe { libc::fcntl(master, libc::F_SETFL, libc::O_NONBLOCK) },
        -1
    );
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "pi_fixture", "--nocapture"])
        .env("PITHOS_PI_FIXTURE", mode)
        .stdin(slave_file.try_clone().unwrap())
        .stdout(slave_file.try_clone().unwrap())
        .stderr(slave_file);
    // SAFETY: only async-signal-safe calls after fork. Stdin is the PTY slave;
    // setsid gives this isolated fixture its own session and process group.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut output = Vec::new();
    loop {
        let mut buffer = [0; 4096];
        for _ in 0..16 {
            match master_file.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => output.extend_from_slice(&buffer[..n]),
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.raw_os_error() == Some(libc::EIO) =>
                {
                    break;
                }
                Err(e) => panic!("PTY read: {e}"),
            }
        }
        assert!(output.len() < 128 * 1024);
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "{mode}: {}",
                String::from_utf8_lossy(&output)
            );
            return;
        }
        if Instant::now() >= deadline {
            // SAFETY: child is still unreaped and owns this isolated group.
            unsafe {
                libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
            }
            child.wait().unwrap();
            panic!(
                "{mode}: fixture deadline: {}",
                String::from_utf8_lossy(&output)
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn offline_host_coordinator_admits_serves_status_runs_fixed_pi_and_settles() {
    run_fixture("host-coordinator");
}

#[test]
fn connected_runtime_admits_serves_status_runs_pi_and_settles_in_order() {
    run_fixture("connected-runtime");
}

#[test]
fn signal_wins_after_pi_reap_when_manifest_record_is_retried() {
    run_fixture("connected-runtime-record-retry-interrupt");
}

#[test]
fn connected_runtime_retries_a_reaped_pi_report_after_manifest_storage_recovers() {
    run_fixture("connected-runtime-record-retry");
}

#[test]
fn connected_runtime_exposes_nonzero_only_after_settlement() {
    run_fixture("connected-runtime-nonzero");
}

#[test]
fn connected_runtime_exposes_signaled_pi_after_settlement() {
    run_fixture("connected-runtime-signaled");
}

#[test]
fn connected_runtime_interrupt_shutdown_takes_precedence() {
    run_fixture("connected-runtime-interrupt");
}

#[test]
fn connected_runtime_terminate_shutdown_takes_precedence() {
    run_fixture("connected-runtime-terminate");
}

#[test]
fn connected_runtime_requested_shutdown_preserves_an_exited_nonzero_pi_status() {
    run_fixture("connected-runtime-requested");
}

#[test]
fn managed_pi_default_command_starts_with_durable_intent_under_real_pty() {
    run_fixture("default");
}

#[test]
fn docker_omitted_readonly_false_mount_defaults_are_accepted() {
    run_fixture("omitted-rw-default");
}

#[test]
fn docker_desktop_host_mnt_bind_sources_are_the_same_mount_only_on_macos() {
    run_fixture("desktop-host-mnt");
}

#[test]
fn managed_pi_applies_the_exact_authorized_host_access_policy() {
    #[cfg(target_os = "linux")]
    run_fixture("linux-host-gateway");
    run_fixture("docker-desktop");
}

#[cfg(target_os = "linux")]
#[test]
fn bridge_change_after_preflight_rejects_pi_before_durable_intent() {
    run_fixture("bridge-changes-before-intent");
}

#[test]
fn signaled_cli_is_recorded_as_failed_after_confirmed_cleanup() {
    run_fixture("signaled");
}

#[test]
fn explicit_host_argv_is_opaque_and_absent_from_durable_state() {
    run_fixture("explicit");
}

#[test]
fn cli_success_detach_and_nonzero_exits_still_require_engine_cleanup() {
    for mode in [
        "detached",
        "engine-failed",
        "cli-failed",
        "shutdown-cleanup",
    ] {
        run_fixture(mode);
    }
}

#[test]
fn mismatched_or_ambiguous_pi_never_authorizes_removal_or_credential_release() {
    for mode in [
        "wrong-network",
        "wrong-entrypoint",
        "wrong-command",
        "wrong-image",
        "wrong-user",
        "wrong-label",
        "wrong-mount",
        "extra-mount",
        "wrong-tty",
        "wrong-attach",
        "wrong-stdin-once",
        "wrong-restart",
        "wrong-hardening",
        "missing-host-access",
        "altered-host-access",
        "duplicate-host-access",
        "foreign-host-access",
        "malformed-host-access",
        "offline-extra-hosts",
        "desktop-extra-hosts",
        "empty-create",
        "daemon-change",
        "client-change",
    ] {
        if cfg!(target_os = "linux")
            || !mode.ends_with("host-access")
            || mode == "offline-extra-hosts"
            || mode == "desktop-extra-hosts"
        {
            run_fixture(mode);
        }
    }
}

#[test]
fn start_requires_matching_probes_and_rechecks_host_paths_and_home_before_intent() {
    let _serial = SERIAL.acquire();
    let f = Fixture::new();
    let shutdown = Shutdown::new();
    let mut docker = f.docker(shutdown.clone());
    let mut m = f.manifest();
    let c = f.credential();
    let volume = VolumeName::new("pi-home").unwrap();
    let lease = HomeLease::broker(&f.root.path().join("leases"), &volume).unwrap();
    let mut child = InteractiveChild::new(InteractiveLimits::default(), shutdown.clone()).unwrap();
    // Each individual probe is necessary in the very same manifest.
    for count in 0..3 {
        assert!(
            docker
                .start_pi(
                    &mut m,
                    &mut child,
                    "pi-1",
                    PiInputs {
                        volume: &volume,
                        image: &image(),
                        identity: identity(),
                        workspace: &f.workspace(),
                        credential: &c,
                        command: &[],
                        host_access: HostAccess::Offline,
                        network: None,
                        browser: None,
                        extension: None,
                        extensions_list: None,
                        env_file: None,
                    }
                )
                .is_err()
        );
        assert_eq!(m.records().len(), count);
        match count {
            0 => {
                docker
                    .probe_account(&mut m, "account-1", &image(), identity())
                    .unwrap();
            }
            1 => {
                docker
                    .probe_home(&mut m, "home-1", &volume, &image(), identity())
                    .unwrap();
            }
            _ => {
                docker
                    .probe_credential(&mut m, "credential-1", &c, &image(), identity())
                    .unwrap();
            }
        }
    }
    fs::create_dir(f.root.path().join("other-credential")).unwrap();
    fs::set_permissions(
        f.root.path().join("other-credential"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let other_credential = RunCredential::create(
        f.root.path().join("other-credential"),
        "http://127.0.0.1:12345",
    )
    .unwrap();
    let alias = f.root.path().join("workspace-alias");
    std::os::unix::fs::symlink(f.workspace(), &alias).unwrap();
    let crlf = f.root.path().join("work\r\nspace");
    fs::create_dir(&crlf).unwrap();
    let modes = [
        "wrong-image",
        "wrong-identity",
        "wrong-home",
        "wrong-credential",
        "workspace-alias",
        "workspace-crlf",
        "workspace-control",
        "workspace-public",
        "parent-public",
        "credential-replaced",
        "volumes",
        "busy",
        "empty-cmd",
        "nul-command",
        "cancel-work",
    ];
    for mode in modes {
        let image = image();
        let other_image = ImmutableImageId::new(&format!("sha256:{}", "b".repeat(64))).unwrap();
        let other_volume = VolumeName::new("other-home").unwrap();
        let other_identity = HostIdentity::new(identity().uid() + 1, identity().gid()).unwrap();
        let mut workspace = f.workspace();
        if mode == "workspace-alias" {
            workspace = alias.clone();
        }
        if mode == "workspace-crlf" {
            workspace = crlf.clone();
        }
        if mode == "workspace-control" {
            workspace = f.root.path().into();
        }
        if mode == "workspace-public" {
            fs::set_permissions(&workspace, fs::Permissions::from_mode(0o777)).unwrap();
        }
        if mode == "parent-public" {
            fs::set_permissions(f.root.path(), fs::Permissions::from_mode(0o777)).unwrap();
        }
        if mode == "credential-replaced" {
            fs::rename(
                f.root.path().join("credential/broker-client.json"),
                f.root.path().join("credential/saved"),
            )
            .unwrap();
            fs::write(
                f.root.path().join("credential/broker-client.json"),
                "untrusted replacement",
            )
            .unwrap();
        }
        if matches!(mode, "volumes" | "busy" | "empty-cmd") {
            f.mode(mode);
        }
        if mode == "cancel-work" {
            shutdown.request(ShutdownReason::Requested);
        }
        let command = if mode == "nul-command" {
            vec!["pi\0evil".into()]
        } else {
            vec![]
        };
        let result = docker.start_pi(
            &mut m,
            &mut child,
            "pi-1",
            PiInputs {
                volume: if mode == "wrong-home" {
                    &other_volume
                } else {
                    &volume
                },
                image: if mode == "wrong-image" {
                    &other_image
                } else {
                    &image
                },
                identity: if mode == "wrong-identity" {
                    other_identity
                } else {
                    identity()
                },
                workspace: &workspace,
                credential: if mode == "wrong-credential" {
                    &other_credential
                } else {
                    &c
                },
                command: &command,
                host_access: HostAccess::Offline,
                network: None,
                browser: None,
                extension: None,
                extensions_list: None,
                env_file: None,
            },
        );
        assert!(result.is_err(), "{mode}");
        assert_eq!(m.records().len(), 3, "{mode}: must fail before intent");
        assert!(m.is_settled());
        assert!(!child.is_in_flight());
        if matches!(mode, "volumes" | "busy" | "empty-cmd") {
            fs::remove_file(f.root.path().join(mode)).unwrap();
        }
        if mode == "workspace-public" {
            fs::set_permissions(&workspace, fs::Permissions::from_mode(0o700)).unwrap();
        }
        if mode == "parent-public" {
            fs::set_permissions(f.root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        if mode == "credential-replaced" {
            fs::remove_file(f.root.path().join("credential/broker-client.json")).unwrap();
            fs::rename(
                f.root.path().join("credential/saved"),
                f.root.path().join("credential/broker-client.json"),
            )
            .unwrap();
        }
    }
    assert!(!f.root.path().join("pi-ran").exists());
    lease.finish().unwrap();
}

#[test]
fn workspace_cannot_mutate_a_frozen_docker_selection_alias() {
    let _serial = SERIAL.acquire();
    let f = Fixture::new();
    let alias = f.workspace().join("docker-alias");
    std::os::unix::fs::symlink(f.root.path().join("docker"), &alias).unwrap();
    let shutdown = Shutdown::new();
    let mut docker = ManagedDocker::new(
        &alias,
        &format!("unix://{}", f.root.path().join("socket").display()),
        &f.root.path().join("config"),
        shutdown.clone(),
    )
    .unwrap();
    let mut m = f.manifest();
    let c = f.credential();
    let volume = VolumeName::new("pi-home").unwrap();
    let lease = HomeLease::broker(&f.root.path().join("leases"), &volume).unwrap();
    f.admit(&mut docker, &mut m, &c);
    let mut child = InteractiveChild::new(InteractiveLimits::default(), shutdown).unwrap();
    assert!(
        docker
            .start_pi(
                &mut m,
                &mut child,
                "pi-1",
                PiInputs {
                    volume: &volume,
                    image: &image(),
                    identity: identity(),
                    workspace: &f.workspace(),
                    credential: &c,
                    command: &[],
                    host_access: HostAccess::Offline,
                    network: None,
                    browser: None,
                    extension: None,
                    extensions_list: None,
                    env_file: None,
                }
            )
            .is_err()
    );
    assert_eq!(
        m.records().len(),
        3,
        "agent-writable selection alias must be rejected before intent"
    );
    assert!(m.is_settled());
    assert!(!child.is_in_flight());
    lease.finish().unwrap();
}

#[test]
fn recovery_uses_recorded_reap_evidence_but_never_invents_it() {
    run_fixture("recover-recorded-exit");
    run_fixture("lost-local-report");
}

#[test]
fn manifest_publication_failure_never_executes_pi_or_releases_evidence() {
    run_fixture("intent-storage-failure");
}

#[test]
fn known_interactive_preexec_failures_leave_no_daemon_debt() {
    for mode in ["cancel-child", "spawn-fail"] {
        run_fixture(mode);
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "pi_fixture", "--nocapture"])
        .env("PITHOS_PI_FIXTURE", "no-tty")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
