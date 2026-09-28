#![cfg(any(target_os = "linux", target_os = "macos"))]

#[cfg(target_os = "linux")]
use pithos::broker::transport::TransportError;
use pithos::broker::transport::{BrokerEndpoint, HostAccess};
use std::net::TcpListener;

#[test]
fn offline_endpoint_owns_exact_loopback_authority() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let expected = listener.local_addr().unwrap();

    let endpoint = BrokerEndpoint::offline(listener).unwrap();

    assert_eq!(endpoint.local_addr(), expected);
    assert_eq!(endpoint.advertised_authority(), expected.to_string());
    assert_eq!(endpoint.host_access(), HostAccess::Offline);
}

#[test]
fn offline_endpoint_rejects_wildcard_listener() {
    let listener = TcpListener::bind("0.0.0.0:0").unwrap();
    assert!(BrokerEndpoint::offline(listener).is_err());
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use pithos::{docker::ManagedDocker, lifecycle::Shutdown};
    use serde_json::{Value, json};
    use std::{
        fs,
        net::{Ipv4Addr, UdpSocket},
        os::unix::{fs::PermissionsExt, net::UnixListener},
    };

    struct Fixture {
        root: tempfile::TempDir,
        _socket: UnixListener,
    }

    impl Fixture {
        fn new(first: &Value, second: Option<&Value>) -> Self {
            let root =
                tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
            let config = root.path().join("config");
            fs::create_dir(&config).unwrap();
            fs::set_permissions(&config, fs::Permissions::from_mode(0o700)).unwrap();
            let socket = UnixListener::bind(root.path().join("docker.sock")).unwrap();
            let second = second.map(serde_json::to_string).transpose().unwrap();
            let first = serde_json::to_string(first).unwrap();
            let second = second.unwrap_or_else(|| first.clone());
            let script = format!(
                "#!/bin/sh\ncase \"$5\" in\ninfo) printf '%s\\n' '{{\"id\":\"daemon-one\",\"os_type\":\"linux\",\"security_options\":[]}}';;\nnetwork) n=0; test ! -f '{0}/count' || n=$(cat '{0}/count'); n=$((n+1)); printf '%s' \"$n\" > '{0}/count'; if test \"$n\" = 1; then printf '%s\\n' '{1}'; else printf '%s\\n' '{2}'; fi;;\n*) exit 99;;\nesac\n",
                root.path().display(),
                first,
                second,
            );
            let executable = root.path().join("docker");
            fs::write(&executable, script).unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            Self {
                root,
                _socket: socket,
            }
        }

        fn docker(&self) -> ManagedDocker {
            ManagedDocker::new(
                &self.root.path().join("docker"),
                &format!("unix://{}", self.root.path().join("docker.sock").display()),
                &self.root.path().join("config"),
                Shutdown::new(),
            )
            .unwrap()
        }

        fn calls(&self) -> usize {
            fs::read_to_string(self.root.path().join("count"))
                .unwrap_or_default()
                .parse()
                .unwrap_or(0)
        }
    }

    fn private_local_ipv4() -> Ipv4Addr {
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        socket.connect("192.0.2.1:9").unwrap();
        let address = match socket.local_addr().unwrap().ip() {
            std::net::IpAddr::V4(address) => address,
            _ => panic!("Linux fixture did not select IPv4"),
        };
        assert!(
            address.is_private(),
            "fixture requires the host's private IPv4 address"
        );
        address
    }

    fn containing_private_subnet(address: Ipv4Addr) -> String {
        let octets = address.octets();
        if octets[0] == 10 {
            "10.0.0.0/8".into()
        } else if octets[0] == 172 && (16..=31).contains(&octets[1]) {
            "172.16.0.0/12".into()
        } else if octets[0] == 192 && octets[1] == 168 {
            "192.168.0.0/16".into()
        } else {
            panic!("fixture address is not RFC1918")
        }
    }

    fn observation(subnet: &str, gateway: &str) -> Value {
        json!({
            "name": "bridge",
            "driver": "bridge",
            "scope": "local",
            "internal": false,
            "enable_ipv6": false,
            "ipam": {
                "Driver": "default",
                "Options": null,
                "Config": [{"Subnet": subnet, "Gateway": gateway}]
            }
        })
    }

    #[test]
    fn linux_endpoint_inspects_binds_exact_gateway_and_reinspects() {
        let gateway = private_local_ipv4();
        let observed = observation(&containing_private_subnet(gateway), &gateway.to_string());
        let fixture = Fixture::new(&observed, None);
        let mut docker = fixture.docker();

        let endpoint = BrokerEndpoint::linux(&mut docker).unwrap();

        assert_eq!(endpoint.local_addr().ip(), gateway);
        assert_eq!(
            endpoint.advertised_authority(),
            format!("host.docker.internal:{}", endpoint.local_addr().port())
        );
        assert!(
            matches!(endpoint.host_access(), HostAccess::LinuxHostGateway(observed) if observed.gateway() == gateway)
        );
        assert_eq!(fixture.calls(), 2, "bridge must be inspected around bind");
    }

    #[test]
    fn linux_endpoint_rejects_changed_observation_after_exact_bind() {
        let gateway = private_local_ipv4();
        let first = observation(&containing_private_subnet(gateway), &gateway.to_string());
        let mut second = first.clone();
        let candidate = match gateway.octets() {
            [10, 0, 0, 1] => "10.0.0.2",
            [10, _, _, _] => "10.0.0.1",
            [172, 16, 0, 1] => "172.16.0.2",
            [172, _, _, _] => "172.16.0.1",
            [192, 168, 0, 1] => "192.168.0.2",
            _ => "192.168.0.1",
        };
        second["ipam"]["Config"][0]["Gateway"] = json!(candidate);
        let fixture = Fixture::new(&first, Some(&second));
        let mut docker = fixture.docker();

        assert!(matches!(
            BrokerEndpoint::linux(&mut docker),
            Err(TransportError::Changed)
        ));
        assert_eq!(fixture.calls(), 2);
    }

    #[test]
    fn linux_endpoint_rejects_ambiguous_or_unsafe_bridge_observations() {
        let cases = [
            Value::Null,
            observation("203.0.113.0/24", "203.0.113.1"),
            observation("127.0.0.0/8", "127.0.0.1"),
            observation("224.0.0.0/24", "224.0.0.1"),
            observation("10.0.0.0/24", "10.0.0.0"),
            observation("10.0.0.0/24", "10.0.0.255"),
            observation("10.0.0.0/24", "0.0.0.0"),
            observation("2001:db8::/64", "2001:db8::1"),
            observation("10.0.0.1/24", "10.0.0.2"),
            observation("10.0.0.0/024", "10.0.0.1"),
            {
                let mut value = observation("10.0.0.0/24", "10.0.0.1");
                value["name"] = json!("foreign");
                value
            },
            {
                let mut value = observation("10.0.0.0/24", "10.0.0.1");
                value["driver"] = json!("overlay");
                value
            },
            {
                let mut value = observation("10.0.0.0/24", "10.0.0.1");
                value["scope"] = json!("swarm");
                value
            },
            {
                let mut value = observation("10.0.0.0/24", "10.0.0.1");
                value["ipam"]["Driver"] = json!("foreign");
                value
            },
            {
                let mut value = observation("10.0.0.0/24", "10.0.0.1");
                value["ipam"]["Options"] = json!({});
                value
            },
            {
                let mut value = observation("10.0.0.0/24", "10.0.0.1");
                value["ipam"]["Config"] = json!([
                    {"Subnet":"10.0.0.0/24","Gateway":"10.0.0.1"},
                    {"Subnet":"10.1.0.0/24","Gateway":"10.1.0.1"}
                ]);
                value
            },
            {
                let mut value = observation("10.0.0.0/24", "10.0.0.1");
                value["ipam"]["Config"][0]["IPRange"] = json!("10.0.0.0/25");
                value
            },
            {
                let mut value = observation("10.0.0.0/24", "10.0.0.1");
                value["unknown"] = json!(true);
                value
            },
            {
                let mut value = observation("10.0.0.0/24", "10.0.0.1");
                value["internal"] = json!(true);
                value
            },
            {
                let mut value = observation("10.0.0.0/24", "10.0.0.1");
                value["enable_ipv6"] = json!(true);
                value
            },
        ];

        for value in cases {
            let fixture = Fixture::new(&value, None);
            let mut docker = fixture.docker();
            assert!(
                matches!(
                    BrokerEndpoint::linux(&mut docker),
                    Err(TransportError::Bridge)
                ),
                "{value}"
            );
            assert_eq!(fixture.calls(), 1, "invalid bridge must not be rebound");
        }
    }

    #[test]
    fn linux_endpoint_never_falls_back_when_exact_gateway_is_unbindable() {
        let observed = observation("10.0.0.0/8", "10.255.255.254");
        let fixture = Fixture::new(&observed, None);
        let mut docker = fixture.docker();

        assert!(matches!(
            BrokerEndpoint::linux(&mut docker),
            Err(TransportError::Bind)
        ));
        assert_eq!(
            fixture.calls(),
            1,
            "bind failure must stop before reinspection"
        );
    }
}
