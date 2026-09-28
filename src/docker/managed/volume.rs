//! Read-only exact-name volume observation; not registry intent or admission.
use super::{ManagedDocker, PreflightError, VolumeName};
use serde::{
    Deserialize, Deserializer,
    de::{Error as _, MapAccess, Visitor},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

const INSPECT: &str = r#"{"name":{{json .Name}},"driver":{{json .Driver}},"scope":{{json .Scope}},"options":{{json .Options}},"created_at":{{json .CreatedAt}},"labels":{{json .Labels}}}"#;

/// Immutable exact-name daemon evidence; never a create/adopt/mount/delete authorization.
#[derive(PartialEq, Eq)]
pub(crate) enum VolumeObservation {
    Absent {
        name: VolumeName,
        daemon_id: String,
    },
    Present {
        name: VolumeName,
        daemon_id: String,
        created_at: String,
        labels: BTreeMap<String, String>,
    },
}

impl fmt::Debug for VolumeObservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent { .. } => f.write_str("Absent"),
            Self::Present { .. } => f.write_str("Present([redacted])"),
        }
    }
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Metadata {
    name: String,
    driver: String,
    scope: String,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    options: Option<BTreeMap<String, String>>,
    created_at: String,
    #[serde(deserialize_with = "required_labels")]
    labels: BTreeMap<String, String>,
}

// deserialize_with keeps the field required; unlike a plain Option it does not
// treat an absent key as null. Inside the present field, accept Docker's null.
fn required_labels<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    struct NullableLabels;
    impl<'de> Visitor<'de> for NullableLabels {
        type Value = BTreeMap<String, String>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("null or a map of unique literal labels")
        }
        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(BTreeMap::new())
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(BTreeMap::new())
        }
        fn visit_some<D: Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> Result<Self::Value, D::Error> {
            unique_labels(deserializer)
        }
    }
    deserializer.deserialize_option(NullableLabels)
}

fn unique_labels<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    struct Unique;
    impl<'de> Visitor<'de> for Unique {
        type Value = BTreeMap<String, String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a map of unique literal labels")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut labels = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, String>()? {
                if labels.len() >= 32 || labels.insert(key, value).is_some() {
                    return Err(M::Error::custom("too many or duplicate labels"));
                }
            }
            Ok(labels)
        }
    }
    deserializer.deserialize_map(Unique)
}

fn inspect(docker: &mut ManagedDocker, name: &VolumeName) -> Result<Metadata, PreflightError> {
    let bytes = docker.query(&["volume", "inspect", "--format", INSPECT, name.as_str()])?;
    let info: Metadata =
        serde_json::from_slice(&bytes).map_err(|_| PreflightError::InvalidResponse)?;
    if info.name != name.as_str() {
        return Err(PreflightError::Changed);
    }
    if info.driver != "local"
        || info.scope != "local"
        || info
            .options
            .as_ref()
            .is_some_and(|options| !options.is_empty())
        || info.created_at.is_empty()
        || info.created_at.len() > 128
        || info.created_at.chars().any(char::is_control)
        || info.labels.iter().any(|(key, value)| {
            key.is_empty()
                || key.len() > 128
                || value.len() > 512
                || key.chars().any(char::is_control)
                || value.chars().any(char::is_control)
        })
        || info
            .labels
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum::<usize>()
            > 8192
    {
        return Err(PreflightError::Unsupported);
    }
    Ok(info)
}

pub(super) fn observe(
    docker: &mut ManagedDocker,
    name: &VolumeName,
) -> Result<VolumeObservation, PreflightError> {
    // The supervised query verifies stdout/stderr completion and brackets the
    // list with frozen-daemon checks. Never interpret an incomplete list as empty.
    let bytes = docker.query(&["volume", "ls", "--format", "{{json .Name}}"])?;
    let text = std::str::from_utf8(&bytes).map_err(|_| PreflightError::InvalidResponse)?;
    if !text.is_empty() && !text.ends_with('\n') {
        return Err(PreflightError::InvalidResponse);
    }
    let mut names = BTreeSet::new();
    for line in text.lines() {
        if line.is_empty() || names.len() >= 1024 {
            return Err(PreflightError::InvalidResponse);
        }
        let candidate: String =
            serde_json::from_str(line).map_err(|_| PreflightError::InvalidResponse)?;
        VolumeName::new(&candidate).map_err(|_| PreflightError::InvalidResponse)?;
        if !names.insert(candidate) {
            return Err(PreflightError::InvalidResponse);
        }
    }
    let daemon_id = docker.daemon_id.clone().ok_or(PreflightError::Changed)?;
    if !names.contains(name.as_str()) {
        return Ok(VolumeObservation::Absent {
            name: name.clone(),
            daemon_id,
        });
    }
    let first = inspect(docker, name)?;
    let second = inspect(docker, name)?;
    if first != second {
        return Err(PreflightError::Changed);
    }
    Ok(VolumeObservation::Present {
        name: name.clone(),
        daemon_id,
        created_at: first.created_at,
        labels: first.labels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::Shutdown;
    use serde_json::{Value, json};
    use std::{
        fs,
        os::unix::{fs::PermissionsExt, net::UnixListener},
        path::PathBuf,
    };

    struct Fake {
        dir: tempfile::TempDir,
        executable: PathBuf,
        config: PathBuf,
        socket: PathBuf,
        _listener: UnixListener,
        _serial: Serial,
    }

    // Fake Docker calls race fixed runtime limits; parallel load causes flakes.
    // Reentrant per test thread because tests shadow live fakes.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    thread_local! {
        static HELD: std::cell::RefCell<(usize, Option<std::sync::MutexGuard<'static, ()>>)> =
            const { std::cell::RefCell::new((0, None)) };
    }
    struct Serial;
    impl Serial {
        fn acquire() -> Self {
            HELD.with_borrow_mut(|(depth, guard)| {
                if *depth == 0 {
                    *guard = Some(SERIAL.lock().unwrap_or_else(|e| e.into_inner()));
                }
                *depth += 1;
            });
            Self
        }
    }
    impl Drop for Serial {
        fn drop(&mut self) {
            HELD.with_borrow_mut(|(depth, guard)| {
                *depth -= 1;
                if *depth == 0 {
                    *guard = None;
                }
            });
        }
    }

    impl Fake {
        fn new() -> Self {
            let serial = Serial::acquire();
            let dir =
                tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
            let executable = dir.path().join("docker-fake");
            let config = dir.path().join("config");
            fs::create_dir(&config).unwrap();
            fs::set_permissions(&config, fs::Permissions::from_mode(0o700)).unwrap();
            let socket = dir.path().join("docker.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let script = r#"#!/usr/bin/python3
import json, os, pathlib, sys
root = pathlib.Path(__ROOT__)
a = sys.argv[5:]
with (root / 'calls').open('a') as log:
    log.write(json.dumps({'args':sys.argv[1:], 'env':{k:v for k,v in os.environ.items() if k not in ('__CF_USER_TEXT_ENCODING','SDKROOT','CPATH','LIBRARY_PATH','MANPATH')}, 'cwd':os.getcwd()}) + '\n')
key = {('info', '--format'):'info', ('volume', 'ls'):'list', ('volume', 'inspect'):'inspect'}.get(tuple(a[:2]))
if key is None: sys.exit(99)
if (root / (key + '-exit')).exists(): sys.exit(7)
sys.stdout.write((root / key).read_text())
if (root / (key + '-next')).exists():
    (root / key).write_text((root / (key + '-next')).read_text())
if (root / (key + '-info-after')).exists():
    (root / 'info').write_text((root / (key + '-info-after')).read_text())
"#.replace("__ROOT__", &serde_json::to_string(dir.path().to_str().unwrap()).unwrap());
            fs::write(&executable, script).unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            let fake = Self {
                dir,
                executable,
                config,
                socket,
                _listener: listener,
                _serial: serial,
            };
            fake.output(
                "info",
                json!({"id":"daemon-one","os_type":"linux","security_options":[]}).to_string(),
            );
            fake.output("list", "\"pi-home\"\n");
            fake.output("inspect", valid().to_string());
            fake
        }
        fn output(&self, key: &str, value: impl AsRef<[u8]>) {
            fs::write(self.dir.path().join(key), value).unwrap();
        }
        fn docker(&self) -> ManagedDocker {
            ManagedDocker::new(
                &self.executable,
                &format!("unix://{}", self.socket.display()),
                &self.config,
                Shutdown::new(),
            )
            .unwrap()
        }
        fn calls(&self) -> Vec<Value> {
            fs::read_to_string(self.dir.path().join("calls"))
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
        fn observe(&self) -> Result<VolumeObservation, PreflightError> {
            self.docker()
                .observe_volume(&VolumeName::new("pi-home").unwrap())
        }
    }
    fn valid() -> Value {
        json!({"name":"pi-home","driver":"local","scope":"local","options":null,"created_at":"2026-01-01T00:00:00Z","labels":{"io.pithos.broker.id":"literal"}})
    }

    #[test]
    fn absence_records_exact_query_and_frozen_daemon_without_authorizing_other_names() {
        for (query, list) in [
            ("pi-home", ""),
            ("pi-home", "\"other\"\n"),
            ("pi-other", ""),
        ] {
            let requested = VolumeName::new(query).unwrap();
            let fake = Fake::new();
            fake.output("list", list);
            let observed = fake.docker().observe_volume(&requested).unwrap();
            assert_eq!(
                observed,
                VolumeObservation::Absent {
                    name: requested.clone(),
                    daemon_id: "daemon-one".into(),
                },
                "query: {query}, list: {list:?}"
            );
            assert_eq!(format!("{observed:?}"), "Absent");
            assert_eq!(fake.calls().len(), 3, "absence must not inspect or create");
        }

        let requested = VolumeName::new("pi-home").unwrap();
        let fake = Fake::new();
        fake.output("list", "");
        fake.output(
            "list-info-after",
            json!({"id":"daemon-two","os_type":"linux","security_options":[]}).to_string(),
        );
        assert_eq!(
            fake.docker().observe_volume(&requested),
            Err(PreflightError::Changed)
        );
    }

    #[test]
    fn absence_requires_complete_list_and_frozen_commands() {
        let fake = Fake::new();
        fake.output("list", "");
        assert_eq!(
            fake.observe(),
            Ok(VolumeObservation::Absent {
                name: VolumeName::new("pi-home").unwrap(),
                daemon_id: "daemon-one".into(),
            })
        );
        let calls = fake.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            &calls[1]["args"].as_array().unwrap()[4..],
            json!(["volume", "ls", "--format", "{{json .Name}}"])
                .as_array()
                .unwrap()
        );
        for call in calls {
            assert_eq!(call["args"][0], "--host");
            assert_eq!(call["args"][1], format!("unix://{}", fake.socket.display()));
            assert_eq!(call["args"][2], "--config");
            assert_eq!(call["args"][3], json!(fake.config));
            assert_eq!(call["cwd"], json!(fake.config));
            assert!(
                call["env"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .all(|key| key == "LC_CTYPE")
            );
        }
    }

    #[test]
    fn presence_is_exact_and_inspected_twice() {
        let fake = Fake::new();
        assert_eq!(
            fake.observe(),
            Ok(VolumeObservation::Present {
                name: VolumeName::new("pi-home").unwrap(),
                daemon_id: "daemon-one".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                labels: BTreeMap::from([("io.pithos.broker.id".into(), "literal".into())]),
            })
        );
        let calls = fake.calls();
        assert_eq!(calls.len(), 9);
        for index in [4, 7] {
            assert_eq!(
                &calls[index]["args"].as_array().unwrap()[4..],
                json!(["volume", "inspect", "--format", super::INSPECT, "pi-home"])
                    .as_array()
                    .unwrap()
            );
        }
        assert!(
            calls
                .iter()
                .enumerate()
                .all(|(index, call)| [1, 4, 7].contains(&index) || call["args"][4] == "info")
        );
        let observed = fake.observe().unwrap();
        assert_eq!(format!("{observed:?}"), "Present([redacted])");
        let fake = Fake::new();
        fake.output("list", "\"other\"\n");
        assert_eq!(
            fake.observe(),
            Ok(VolumeObservation::Absent {
                name: VolumeName::new("pi-home").unwrap(),
                daemon_id: "daemon-one".into(),
            })
        );
    }

    #[test]
    fn malformed_missing_or_partial_list_is_not_absence() {
        for list in [
            "\n",
            "broken\n",
            "null\n",
            "\"other\"",
            "\"other\"\n\"other\"\n",
            "\"bad:rw\"\n",
        ] {
            let fake = Fake::new();
            fake.output("list", list);
            assert_eq!(
                fake.observe(),
                Err(PreflightError::InvalidResponse),
                "{list:?}"
            );
        }
        for key in ["list-exit", "inspect-exit", "info-exit"] {
            let fake = Fake::new();
            fake.output(key, "");
            assert_eq!(fake.observe(), Err(PreflightError::Unavailable));
        }
        let fake = Fake::new();
        fake.output("list", "x".repeat(65537));
        assert_eq!(fake.observe(), Err(PreflightError::Unavailable));
        let fake = Fake::new();
        fake.output(
            "list",
            (0..1025)
                .map(|i| format!("\"vol-{i}\"\n"))
                .collect::<String>(),
        );
        assert_eq!(fake.observe(), Err(PreflightError::InvalidResponse));
    }

    #[test]
    fn changed_daemon_and_inspect_fail_closed() {
        let fake = Fake::new();
        fake.output(
            "inspect-next",
            valid().to_string().replace("literal", "replaced"),
        );
        assert_eq!(fake.observe(), Err(PreflightError::Changed));
        let fake = Fake::new();
        fake.output(
            "inspect-next",
            valid().to_string().replace("2026-01-01", "2027-01-01"),
        );
        assert_eq!(fake.observe(), Err(PreflightError::Changed));
        let fake = Fake::new();
        fake.output(
            "list-info-after",
            json!({"id":"daemon-two","os_type":"linux","security_options":[]}).to_string(),
        );
        assert_eq!(fake.observe(), Err(PreflightError::Changed));
        let fake = Fake::new();
        fake.output(
            "inspect-next",
            valid().to_string().replace("pi-home", "pi-other"),
        );
        assert_eq!(fake.observe(), Err(PreflightError::Changed));
        let fake = Fake::new();
        fake.output(
            "inspect-info-after",
            json!({"id":"daemon-two","os_type":"linux","security_options":[]}).to_string(),
        );
        assert_eq!(fake.observe(), Err(PreflightError::Changed));
    }

    #[test]
    fn null_labels_are_present_but_missing_labels_are_invalid() {
        let fake = Fake::new();
        let mut info = valid();
        info["labels"] = Value::Null;
        fake.output("inspect", info.to_string());
        assert_eq!(
            fake.observe(),
            Ok(VolumeObservation::Present {
                name: VolumeName::new("pi-home").unwrap(),
                daemon_id: "daemon-one".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                labels: BTreeMap::new(),
            })
        );

        info.as_object_mut().unwrap().remove("labels");
        fake.output("inspect", info.to_string());
        assert_eq!(fake.observe(), Err(PreflightError::InvalidResponse));
    }

    #[test]
    fn invalid_metadata_is_never_accepted() {
        for (key, value) in [
            ("driver", json!("nfs")),
            ("scope", json!("global")),
            ("options", json!({"device":"/tmp"})),
            ("created_at", json!("")),
            ("created_at", json!("x".repeat(129))),
            ("labels", json!({"bad":"x".repeat(513)})),
            ("labels", json!({"bad\nkey":"x"})),
            ("labels", json!({"x":"\n"})),
        ] {
            let fake = Fake::new();
            let mut info = valid();
            info[key] = value;
            fake.output("inspect", info.to_string());
            assert_eq!(fake.observe(), Err(PreflightError::Unsupported), "{key}");
        }
        for field in ["created_at", "labels", "options", "driver", "scope", "name"] {
            let fake = Fake::new();
            let mut info = valid();
            info.as_object_mut().unwrap().remove(field);
            fake.output("inspect", info.to_string());
            assert_eq!(
                fake.observe(),
                Err(PreflightError::InvalidResponse),
                "{field}"
            );
        }
        let fake = Fake::new();
        let mut info = valid();
        info["extra"] = json!(1);
        fake.output("inspect", info.to_string());
        assert_eq!(fake.observe(), Err(PreflightError::InvalidResponse));
        let fake = Fake::new();
        let mut info = valid();
        info["labels"] = json!(
            (0..33)
                .map(|i| (format!("key-{i}"), "v"))
                .collect::<BTreeMap<_, _>>()
        );
        fake.output("inspect", info.to_string());
        assert_eq!(fake.observe(), Err(PreflightError::InvalidResponse));
        let fake = Fake::new();
        let mut info = valid();
        info["created_at"] = Value::Null;
        fake.output("inspect", info.to_string());
        assert_eq!(fake.observe(), Err(PreflightError::InvalidResponse));
        let fake = Fake::new();
        fake.output("inspect", r#"{"name":"pi-home","driver":"local","scope":"local","options":null,"created_at":"time","labels":{"x":"1","x":"2"}}"#);
        assert_eq!(fake.observe(), Err(PreflightError::InvalidResponse));
        let fake = Fake::new();
        fake.output("inspect", r#"{"name":"pi-home","driver":"local","scope":"local","options":null,"created_at":"time","labels":null,"labels":{}}"#);
        assert_eq!(fake.observe(), Err(PreflightError::InvalidResponse));
    }
}
