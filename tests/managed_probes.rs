#![cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "fixtures/bounded.rs"]
mod bounded;
#[path = "fixtures/canonical_temp.rs"]
mod tempfile;

use pithos::{
    broker::{credential::RunCredential, journal::State, resources::ResourceManifest},
    docker::{HostIdentity, ImmutableImageId, ManagedDocker, VolumeName},
    lifecycle::{Shutdown, ShutdownReason},
};
use serde_json::Value;
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
};

struct Fixture {
    root: tempfile::TempDir,
    executable: PathBuf,
    config: PathBuf,
    _socket: UnixListener,
    // Fake Docker calls race fixed runtime limits; unbounded parallel load flakes.
    _serial: bounded::Permit<'static>,
}
static SERIAL: bounded::Bounded = bounded::Bounded::new(4);
impl Fixture {
    fn new() -> Self {
        let serial = SERIAL.acquire();
        let root = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        for name in ["config", "run"] {
            fs::create_dir(root.path().join(name)).unwrap();
            fs::set_permissions(root.path().join(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let executable = root.path().join("docker");
        let script = r#"#!/usr/bin/python3
import csv, json, os, pathlib, signal, subprocess, sys, time
root = pathlib.Path(__ROOT__)
a = sys.argv[5:]
with (root/'calls').open('a') as f: f.write(json.dumps(sys.argv[1:])+'\n')
def mode(name): return (root/name).exists()
def emit(value): print(json.dumps(value))
def option(name): return a[a.index(name)+1]
if mode('slow-control'): time.sleep(1.9)
if a[0]=='info':
    emit({'id':'changed' if mode('changed') else 'daemon-one','os_type':'linux','security_options':[]})
elif a[:2]==['image','inspect']:
    if '\"Volumes\"' in a[3]:
        emit({'id':__IMAGE__, 'volumes': {'/unexpected':{}} if mode('volumes') else None})
    else:
        emit({'id':__IMAGE__, 'user':__USER__, 'env':['HOME=/home/pi','USER=pi','LOGNAME=pi']})
elif a[:2]==['volume','ls']:
    if not mode('missing-home') or mode('created'): emit('pi-home')
elif a[:2]==['volume','create']:
    (root/'created').touch()
    labels = dict(a[i+1].split('=',1) for i,v in enumerate(a) if v=='--label')
    (root/'volume-labels').write_text(json.dumps({} if mode('foreign-create') else labels))
    print(a[-1])
elif a[:2]==['volume','inspect'] and '"labels"' in a[3]:
    if (root/'volume-labels').exists(): labels=json.loads((root/'volume-labels').read_text())
    elif mode('labelled-home'): labels={'io.pithos.broker.home':'provisioned'}
    else: labels=None
    emit({'name':'pi-home','labels':labels})
elif a[:2]==['volume','inspect']:
    emit({'name':'pi-home','driver':'local','scope':'local','options':None,'created_at':'stable'})
elif a[0]=='run':
    journal = json.loads((root/'run/journal.json').read_text())
    manifest = json.loads((root/'run/resources.json').read_text())
    name = option('--name')
    entry = next(r for r in manifest['resources'] if r['name']==name)
    assert next(r for r in journal['records'] if r['request_id']==entry['request_id'])['state']=='running'
    assert entry['daemon_id']=='daemon-one' and entry['image']==__IMAGE__
    assert entry['observed_id'] is None and not entry['local_reaped']
    assert '--rm' not in a and '--pull=never' in a and '--network=none' in a
    assert '--cap-drop=ALL' in a and '--read-only' in a and '--security-opt=no-new-privileges' in a
    assert option('--user')==__USER__ and '--entrypoint=/usr/bin/python3' in a
    labels = dict(a[i+1].split('=',1) for i,v in enumerate(a) if v=='--label')
    assert labels==entry['labels']
    mounts=[]; host_mounts=[]
    for i,v in enumerate(a):
        if v!='--mount': continue
        pieces=next(csv.reader([a[i+1]]))
        kv=dict((s.split('=',1)+[''])[:2] for s in pieces)
        ro='readonly' in kv
        # Only a home copy-up mount is writable: a volume without volume-nocopy.
        assert ro or (kv['type']=='volume' and 'volume-nocopy' not in kv)
        m={'Type':kv['type'],'Source':kv['source'],'Destination':kv['target'],'RW':not ro,'Propagation':''}
        hm={'Type':kv['type'],'Source':kv['source'],'Target':kv['target']}
        if ro: hm['ReadOnly']=True
        if kv['type']=='volume':
            assert ro != ('volume-nocopy' not in kv)
            m['Name']=kv['source']; m['Driver']='local'; m['Source']='/var/lib/docker/volumes/'+kv['source']+'/_data'
            if 'volume-nocopy' in kv: hm['VolumeOptions']={'NoCopy':True}
        else: m['Propagation']='rprivate'
        mounts.append(m); host_mounts.append(hm)
    cmd=a[a.index(__IMAGE__)+1:]
    assert cmd[:3]==['-I','-S','-c']
    cid = 'b'*64 if len(manifest['resources']) == 1 else format(len(manifest['resources']), '064x')
    container={'id':cid,'name':'/'+name,'image':__IMAGE__,
        'config':{'User':__USER__,'Image':__IMAGE__,'Entrypoint':['/usr/bin/python3'],'Cmd':cmd,'Labels':labels,'Volumes':None},
        'host':{'NetworkMode':'none','ReadonlyRootfs':True,'Privileged':False,'AutoRemove':False,'CapDrop':['ALL'],'CapAdd':None,'SecurityOpt':['no-new-privileges'],'GroupAdd':None,'Binds':None,'Devices':None,'PidMode':'','IpcMode':'private','UsernsMode':'','Mounts':host_mounts},
        'mounts':mounts, 'state':{'Status':'exited','Running':False,'ExitCode':1 if mode('failed') else 0,'Error':'','OOMKilled':False,'Dead':False}}
    if mode('inherited-labels'):
        container['config']['Labels'].update({'dev.pithos.identity.version':'1','org.opencontainers.image.title':'Pi'})
    if mode('normalized-security'): container['host']['SecurityOpt']=['no-new-privileges:true']
    if mode('extra-reserved-label'): container['config']['Labels']['io.pithos.probe.foreign']='other'
    if mode('ambiguous-security'): container['host']['SecurityOpt']=['no-new-privileges', 'no-new-privileges:false']
    if mode('false-security'): container['host']['SecurityOpt']=['no-new-privileges:false']
    if mode('foreign'): container['config']['Labels']['io.pithos.probe.run']='foreign'
    if mode('wrong-name'): container['name']='/foreign'
    if mode('wrong-id'): container['id']='short-id'
    if mode('wrong-config'): container['host']['Privileged']=True
    if mode('wrong-image'): container['image']='sha256:'+'c'*64
    if mode('wrong-command'): container['config']['Cmd']=['-c','foreign']
    if mode('wrong-mount'): container['mounts']=[{'Type':'bind','Source':'/foreign','Destination':'/foreign','RW':True}]
    if mode('daemon-after-run'): (root/'changed').touch()
    if mode('config-after-run'): (root/'config'/'config.json').write_text('{}')
    if mode('delay'):
        (root/'late.json').write_text(json.dumps(container))
        (root/'ran').touch()
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        time.sleep(60)
    if not mode('empty'): (root/'container.json').write_text(json.dumps(container))
    (root/'ran').touch()
    if mode('incomplete-run'): sys.stdout.write('x'*70000)
    if mode('hang'):
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        time.sleep(60)
    sys.exit(1 if mode('failed') or mode('lost') or mode('empty') else 0)
elif a[:2]==['container','ls']:
    if any(v=='volume=pi-home' for v in a): pass
    elif (root/'container.json').exists():
        c=json.loads((root/'container.json').read_text())
        selected=option('--filter')
        if selected == 'id='+c['id'] or selected == 'name=^'+c['name']+'$': emit(c['id'])
elif a[:2]==['container','inspect']:
    assert a[-1]==json.loads((root/'container.json').read_text())['id']
    count=int((root/'inspections').read_text())+1 if (root/'inspections').exists() else 1
    (root/'inspections').write_text(str(count))
    container=json.loads((root/'container.json').read_text())
    if count==2 and mode('reassign-name'): container['name']='/foreign'
    if count==2 and mode('reassign-id'): container['id']='c'*64
    if count==2 and mode('exit-changed'): container['state']['ExitCode']=1
    if mode('invalid-inspect'): sys.stdout.write('{'); sys.exit(0)
    emit(container)
    if mode('incomplete-inspect'): sys.stdout.write(' '*70000)
elif a[:2]==['container','rm']:
    cid=json.loads((root/'container.json').read_text())['id']
    assert a==['container','rm','--force',cid]
    entry=json.loads((root/'run/resources.json').read_text())['resources'][-1]
    assert entry['observed_id']==cid and entry['local_reaped']
    if mode('rename-after-rm'):
        container=json.loads((root/'container.json').read_text())
        container['name']='/foreign'
        (root/'container.json').write_text(json.dumps(container))
    else: (root/'container.json').unlink()
    if mode('lost-rm'): sys.exit(1)
else: sys.exit(99)
"#
        .replace("__ROOT__", &serde_json::to_string(root.path().to_str().unwrap()).unwrap())
        .replace("__IMAGE__", &serde_json::to_string(image().as_str()).unwrap())
        .replace("__USER__", &serde_json::to_string(&identity().docker_user()).unwrap());
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = UnixListener::bind(root.path().join("socket")).unwrap();
        let config = root.path().join("config");
        Self {
            root,
            executable,
            config,
            _socket: socket,
            _serial: serial,
        }
    }
    fn docker(&self, shutdown: Shutdown) -> ManagedDocker {
        ManagedDocker::new(
            &self.executable,
            &format!("unix://{}", self.root.path().join("socket").display()),
            &self.config,
            shutdown,
        )
        .unwrap()
    }
    fn manifest(&self) -> ResourceManifest {
        ResourceManifest::open(&self.root.path().join("run"), "run-1").unwrap()
    }
    fn mode(&self, name: &str) {
        fs::write(self.root.path().join(name), "").unwrap();
    }
    fn calls(&self) -> Vec<Vec<String>> {
        fs::read_to_string(self.root.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
fn identity() -> HostIdentity {
    HostIdentity::effective().unwrap()
}
fn image() -> ImmutableImageId {
    ImmutableImageId::new(&format!("sha256:{}", "a".repeat(64))).unwrap()
}

#[test]
fn inherited_image_labels_and_normalized_hardening_are_accepted() {
    let mut failures = Vec::new();
    for mode in ["inherited-labels", "normalized-security"] {
        let f = Fixture::new();
        f.mode(mode);
        let mut m = f.manifest();
        let result =
            f.docker(Shutdown::new())
                .probe_account(&mut m, "account-1", &image(), identity());
        if result.is_err() || !m.is_settled() {
            failures.push(format!("{mode}: {result:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
fn foreign_or_ambiguous_inspection_never_authorizes_removal() {
    for mode in [
        "foreign",
        "wrong-name",
        "wrong-id",
        "wrong-config",
        "wrong-image",
        "wrong-command",
        "wrong-mount",
        "extra-reserved-label",
        "false-security",
        "ambiguous-security",
        "daemon-after-run",
        "config-after-run",
    ] {
        let f = Fixture::new();
        f.mode(mode);
        let mut m = f.manifest();
        let mut docker = f.docker(Shutdown::new());
        assert!(
            docker
                .probe_account(&mut m, "account-1", &image(), identity())
                .is_err(),
            "{mode}"
        );
        assert!(docker.reconcile_probes(&mut m).is_err(), "{mode}");
        assert!(!m.is_settled(), "{mode}");
        assert_eq!(m.records()[0].state(), State::Indeterminate);
        assert!(f.root.path().join("container.json").exists(), "{mode}");
        assert!(
            !f.calls()
                .iter()
                .any(|a| a[4..].starts_with(&["container".into(), "rm".into()])),
            "{mode}"
        );
    }
}

#[test]
fn image_volumes_and_missing_home_are_rejected_before_intent() {
    for mode in ["volumes", "missing-home"] {
        let f = Fixture::new();
        f.mode(mode);
        let mut m = f.manifest();
        let mut docker = f.docker(Shutdown::new());
        assert!(
            docker
                .probe_home(
                    &mut m,
                    "home-1",
                    &VolumeName::new("pi-home").unwrap(),
                    &image(),
                    identity()
                )
                .is_err()
        );
        if mode == "volumes" {
            assert!(
                docker
                    .probe_account(&mut m, "account-1", &image(), identity())
                    .is_err()
            );
        }
        assert!(m.records().is_empty());
        assert!(m.is_settled());
        assert!(!f.calls().iter().any(|a| a[4] == "run"));
    }
}

#[test]
fn duplicate_requests_and_conflicting_payloads_never_replay() {
    let f = Fixture::new();
    let mut m = f.manifest();
    let mut docker = f.docker(Shutdown::new());
    docker
        .probe_account(&mut m, "request-1", &image(), identity())
        .unwrap();
    let calls = f.calls().len();
    assert!(
        docker
            .probe_account(&mut m, "request-1", &image(), identity())
            .is_err()
    );
    assert!(
        docker
            .probe_home(
                &mut m,
                "request-1",
                &VolumeName::new("pi-home").unwrap(),
                &image(),
                identity()
            )
            .is_err()
    );
    let other = ImmutableImageId::new(&format!("sha256:{}", "c".repeat(64))).unwrap();
    assert!(
        docker
            .probe_account(&mut m, "request-1", &other, identity())
            .is_err()
    );
    assert_eq!(f.calls().len(), calls);
    assert_eq!(m.records().len(), 1);
    assert!(m.is_settled());
}

#[test]
fn cancelled_wrappers_do_not_write_intent_or_spawn() {
    let f = Fixture::new();
    let mut m = f.manifest();
    let shutdown = Shutdown::new();
    let mut docker = f.docker(shutdown.clone());
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let credential = RunCredential::create(dir.path(), "http://localhost:1234").unwrap();
    shutdown.request(ShutdownReason::Requested);
    assert!(
        docker
            .probe_account(&mut m, "account-1", &image(), identity())
            .is_err()
    );
    assert!(
        docker
            .probe_home(
                &mut m,
                "home-1",
                &VolumeName::new("pi-home").unwrap(),
                &image(),
                identity()
            )
            .is_err()
    );
    assert!(
        docker
            .probe_credential(&mut m, "credential-1", &credential, &image(), identity())
            .is_err()
    );
    assert!(m.records().is_empty());
    assert!(m.is_settled());
    assert!(f.calls().is_empty());
    assert!(!docker.has_child());
}

#[test]
fn recovery_completes_journal_after_durable_removal_without_more_docker_calls() {
    let f = Fixture::new();
    let mut m = f.manifest();
    f.docker(Shutdown::new())
        .probe_account(&mut m, "account-1", &image(), identity())
        .unwrap();
    drop(m);
    let file = f.root.path().join("run/journal.json");
    let mut journal: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    journal["records"][0]["state"] = "running".into();
    fs::write(file, serde_json::to_vec(&journal).unwrap()).unwrap();
    m = f.manifest();
    let calls = f.calls().len();
    assert!(f.docker(Shutdown::new()).reconcile_probes(&mut m).is_ok());
    assert!(m.is_settled());
    assert_eq!(m.records()[0].state(), State::Failed);
    assert_eq!(f.calls().len(), calls);
}

#[test]
fn reconcile_deadline_bounds_calls_within_a_single_resource() {
    let f = Fixture::new();
    f.mode("foreign");
    let mut m = f.manifest();
    let mut docker = f.docker(Shutdown::new());
    assert!(
        docker
            .probe_account(&mut m, "account-1", &image(), identity())
            .is_err()
    );
    let path = f.root.path().join("container.json");
    let mut c: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    c["config"]["Labels"]["io.pithos.probe.run"] = "run-1".into();
    fs::write(&path, serde_json::to_vec(&c).unwrap()).unwrap();
    f.mode("slow-control");
    let start = std::time::Instant::now();
    assert!(docker.reconcile_probes(&mut m).is_err());
    assert!(
        start.elapsed() < std::time::Duration::from_millis(33500),
        "single-resource calls escaped the 32s deadline: {:?}",
        start.elapsed()
    );
    assert!(!m.is_settled());
    while docker.has_child() {
        docker.poll_child();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn revalidation_and_incomplete_rpc_never_grant_removal_authority() {
    for mode in [
        "reassign-name",
        "reassign-id",
        "invalid-inspect",
        "incomplete-inspect",
    ] {
        let f = Fixture::new();
        f.mode(mode);
        let mut m = f.manifest();
        assert!(
            f.docker(Shutdown::new())
                .probe_account(&mut m, "account-1", &image(), identity())
                .is_err(),
            "{mode}"
        );
        assert!(!m.is_settled(), "{mode}");
        assert!(f.root.path().join("container.json").exists(), "{mode}");
        assert!(
            !f.calls()
                .iter()
                .any(|a| a[4..].starts_with(&["container".into(), "rm".into()])),
            "{mode}"
        );
    }
}

#[test]
fn name_absence_or_lost_remove_reply_cannot_erase_uncertainty() {
    for mode in ["rename-after-rm", "lost-rm"] {
        let f = Fixture::new();
        f.mode(mode);
        let mut m = f.manifest();
        let mut docker = f.docker(Shutdown::new());
        assert!(
            docker
                .probe_account(&mut m, "account-1", &image(), identity())
                .is_err(),
            "{mode}"
        );
        assert!(docker.reconcile_probes(&mut m).is_err());
        assert!(!m.is_settled());
        assert_eq!(m.records()[0].state(), State::Indeterminate);
        assert_eq!(
            f.calls()
                .iter()
                .filter(|a| a[4..].starts_with(&["container".into(), "rm".into()]))
                .count(),
            1
        );
    }
}

#[test]
fn failed_and_lost_reply_probes_cleanup_but_never_return_success() {
    for mode in ["failed", "lost", "incomplete-run", "exit-changed"] {
        let f = Fixture::new();
        f.mode(mode);
        let mut m = f.manifest();
        let result =
            f.docker(Shutdown::new())
                .probe_account(&mut m, "account-1", &image(), identity());
        assert!(result.is_err());
        assert!(
            m.is_settled(),
            "{mode} left a confirmed owned container unsettled"
        );
        assert_eq!(m.records()[0].state(), State::Failed);
        assert!(!f.root.path().join("container.json").exists());
    }
}

#[test]
fn uncertain_empty_listing_is_terminal_and_late_create_is_never_replayed() {
    let f = Fixture::new();
    f.mode("empty");
    let mut m = f.manifest();
    let mut docker = f.docker(Shutdown::new());
    assert!(
        docker
            .probe_account(&mut m, "account-1", &image(), identity())
            .is_err()
    );
    assert_eq!(
        m.records()[0].state(),
        State::Indeterminate,
        "empty listing was not durably quarantined"
    );
    assert!(!m.is_settled());
    let calls = f.calls().len();
    assert!(
        docker
            .probe_account(&mut m, "account-1", &image(), identity())
            .is_err()
    );
    assert_eq!(f.calls().len(), calls);
    drop(m);
    drop(docker);
    let mut m = f.manifest();
    let mut docker = f.docker(Shutdown::new());
    assert!(docker.reconcile_probes(&mut m).is_err());
    assert_eq!(m.records()[0].state(), State::Indeterminate);
    assert!(!m.is_settled());
}

fn cancel_after_run(f: &Fixture, mode: &str) -> (ManagedDocker, ResourceManifest) {
    f.mode(mode);
    let mut m = f.manifest();
    let shutdown = Shutdown::new();
    let mut docker = f.docker(shutdown.clone());
    let root = f.root.path().to_owned();
    let cancel = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !root.join("ran").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        shutdown.request(ShutdownReason::Requested);
    });
    assert!(
        docker
            .probe_account(&mut m, "account-1", &image(), identity())
            .is_err()
    );
    cancel.join().unwrap();
    assert!(!docker.has_child());
    (docker, m)
}

#[test]
fn cleanup_has_uncancelled_same_selection_authority_after_shutdown() {
    let f = Fixture::new();
    let (mut docker, mut m) = cancel_after_run(&f, "hang");
    assert!(docker.reconcile_probes(&mut m).is_ok());
    assert!(
        m.is_settled(),
        "cancelled work token also cancelled cleanup"
    );
    assert!(!f.root.path().join("container.json").exists());
}

#[test]
fn delayed_create_after_empty_scan_is_cleaned_without_erasing_uncertainty() {
    let f = Fixture::new();
    let (mut docker, mut m) = cancel_after_run(&f, "delay");
    assert_eq!(m.records()[0].state(), State::Indeterminate);
    assert!(!m.is_settled());
    fs::rename(
        f.root.path().join("late.json"),
        f.root.path().join("container.json"),
    )
    .unwrap();
    assert!(docker.reconcile_probes(&mut m).is_err());
    assert!(
        !f.root.path().join("container.json").exists(),
        "late owned resource not reconciled"
    );
    assert!(!m.is_settled());
    assert_eq!(m.records()[0].state(), State::Indeterminate);
    assert_eq!(f.calls().iter().filter(|a| a[4] == "run").count(), 1);
}

#[test]
fn credential_probe_executes_without_disclosing_payload() {
    let f = Fixture::new();
    let mut m = f.manifest();
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let credential = RunCredential::create(dir.path(), "http://localhost:1234").unwrap();
    let result = f.docker(Shutdown::new()).probe_credential(
        &mut m,
        "credential-1",
        &credential,
        &image(),
        identity(),
    );
    assert!(result.is_ok(), "credential probe failed: {result:?}");
    assert!(m.is_settled());
    assert!(!format!("{:?}", f.calls()).contains(credential.token().expose_secret()));
}

#[test]
fn home_and_credential_are_typed_nonroot_owned_readonly_probes() {
    let f = Fixture::new();
    let mut m = f.manifest();
    let mut docker = f.docker(Shutdown::new());
    let home = VolumeName::new("pi-home").unwrap();
    let result = docker.probe_home(&mut m, "home-1", &home, &image(), identity());
    assert!(result.is_ok(), "home probe failed: {result:?}");
    let credentials = f.root.path().join("credentials");
    fs::create_dir(&credentials).unwrap();
    fs::set_permissions(&credentials, fs::Permissions::from_mode(0o700)).unwrap();
    let mut credential = RunCredential::create(&credentials, "http://localhost:1234").unwrap();
    let result = docker.probe_credential(&mut m, "credential-1", &credential, &image(), identity());
    assert!(result.is_ok(), "credential probe failed: {result:?}");
    assert_eq!(m.records().len(), 2);
    assert!(m.records().iter().all(|r| r.state() == State::Succeeded));
    assert!(m.is_settled());
    let evidence = fs::read_to_string(f.root.path().join("run/resources.json")).unwrap();
    assert!(!evidence.contains(credential.token().expose_secret()));
    assert!(!evidence.contains("http://localhost:1234"));
    let calls = f.calls();
    let runs: Vec<_> = calls.iter().filter(|a| a[4] == "run").collect();
    assert_eq!(runs.len(), 2);
    assert!(
        runs[0]
            .iter()
            .any(|v| v == "type=volume,source=pi-home,target=/home/pi,readonly,volume-nocopy")
    );
    assert!(
        runs[1].iter().any(
            |v| v.contains("target=/run/pithos-broker/client.json") && v.ends_with(",readonly")
        )
    );
    credential.cleanup().unwrap();
}

#[test]
fn account_probe_records_before_effect_inspects_and_cleans_by_immutable_id() {
    let f = Fixture::new();
    let mut m = f.manifest();
    let result = f
        .docker(Shutdown::new())
        .probe_account(&mut m, "account-1", &image(), identity());
    assert!(result.is_ok(), "account probe did not succeed: {result:?}");
    assert!(m.is_settled());
    assert_eq!(m.records()[0].state(), State::Succeeded);
    assert!(!f.root.path().join("container.json").exists());
    let manifest: Value =
        serde_json::from_slice(&fs::read(f.root.path().join("run/resources.json")).unwrap())
            .unwrap();
    let entry = &manifest["resources"][0];
    assert_eq!(entry["observed_id"], "b".repeat(64));
    assert!(entry["name"].as_str().unwrap().starts_with("pithos-probe-"));
    assert!(
        f.calls()
            .iter()
            .any(|a| a[4..].starts_with(&["container".into(), "rm".into()]))
    );
}

fn provision(
    f: &Fixture,
) -> (
    Result<(), pithos::docker::OwnedProbeError>,
    ResourceManifest,
) {
    let mut m = f.manifest();
    let result = f.docker(Shutdown::new()).provision_home(
        &mut m,
        "home-provision",
        &VolumeName::new("pi-home").unwrap(),
        &image(),
        identity(),
    );
    (result, m)
}

fn subcommands(f: &Fixture) -> Vec<String> {
    f.calls().iter().map(|c| c[4..6].join(" ")).collect()
}

#[test]
fn missing_home_is_created_labelled_and_filled_by_copy_up_as_the_host_user() {
    let f = Fixture::new();
    f.mode("missing-home");
    let (result, m) = provision(&f);
    result.unwrap();
    assert!(m.is_settled());
    let calls = f.calls();
    let create = calls
        .iter()
        .find(|c| c[4..6] == ["volume", "create"])
        .unwrap();
    assert_eq!(
        &create[4..],
        [
            "volume",
            "create",
            "--label",
            "io.pithos.broker.home=provisioned",
            "pi-home"
        ]
    );
    let run = calls.iter().find(|c| c[4] == "run").unwrap();
    // Copy-up needs a writable volume mount without volume-nocopy; the image's
    // /home/pi (owned by the host identity) seeds the empty volume.
    assert!(run.contains(&"type=volume,source=pi-home,target=/home/pi".to_string()));
    assert!(run.contains(&identity().docker_user()));
    assert!(
        run.contains(&"--read-only".to_string()) && run.contains(&"--network=none".to_string())
    );
    assert!(subcommands(&f).contains(&"container rm".to_string()));
}

#[test]
fn existing_home_without_the_broker_label_is_never_touched() {
    let f = Fixture::new();
    let (result, m) = provision(&f);
    result.unwrap();
    assert!(m.is_settled());
    let subcommands = subcommands(&f);
    assert!(
        !subcommands
            .iter()
            .any(|c| c == "volume create" || c.starts_with("run"))
    );
}

#[test]
fn broker_labelled_home_is_refilled_without_recreating() {
    let f = Fixture::new();
    f.mode("labelled-home");
    let (result, m) = provision(&f);
    result.unwrap();
    assert!(m.is_settled());
    let subcommands = subcommands(&f);
    assert!(!subcommands.contains(&"volume create".to_string()));
    assert!(subcommands.iter().any(|c| c.starts_with("run")));
}

#[test]
fn home_created_concurrently_by_someone_else_is_refused_without_copy_up() {
    let f = Fixture::new();
    f.mode("missing-home");
    f.mode("foreign-create");
    let (result, _) = provision(&f);
    assert!(result.is_err());
    assert!(!subcommands(&f).iter().any(|c| c.starts_with("run")));
}
