//! Offline app invocation policy only. No execution, daemon ownership or admission.
//!
//! A future owner must prove the image was built by the host, verify its exact
//! immutable ID and preflight image configuration (especially `VOLUME`: Docker
//! creates anonymous volumes even without `--mount`/`--volume`). The owner must
//! also secure the network, persist intent, supervise children, inspect exact
//! metadata and reconcile cleanup. These tokens are not proof of those actions.

use crate::docker::ImmutableImageId;
use sha2::{Digest, Sha256};
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum AppPlanError {
    #[error("invalid app plan identity")]
    Identity,
    #[error("invalid app plan command")]
    Command,
    #[error("invalid app plan network")]
    Network,
}

pub(crate) struct HostIssuedAppImage(ImmutableImageId);
impl HostIssuedAppImage {
    /// Only the future owned host build may issue this token; an immutable ID by
    /// itself does not establish build provenance, authorization or admission.
    pub(crate) fn from_owned_build(id: ImmutableImageId) -> Self {
        Self(id)
    }
}
impl fmt::Debug for HostIssuedAppImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HostIssuedAppImage([redacted])")
    }
}

pub(crate) struct TrustedAppNetwork(String);
impl TrustedAppNetwork {
    /// Only the future network owner may issue this token after securing it.
    pub(crate) fn from_owned_network(name: &str) -> Result<Self, AppPlanError> {
        if !valid_name(name, 63) {
            return Err(AppPlanError::Network);
        }
        Ok(Self(name.to_owned()))
    }
}
impl fmt::Debug for TrustedAppNetwork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TrustedAppNetwork([redacted])")
    }
}

// Alphanumeric leading/trailing bytes and internal lowercase ASCII digits/hyphens.
fn valid_name(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.as_bytes()[0].is_ascii_lowercase()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub(crate) struct AppLaunchPlan {
    image: HostIssuedAppImage,
    run: String,
    logical: String,
    command: Vec<String>,
    network: Option<TrustedAppNetwork>,
}
impl AppLaunchPlan {
    pub(crate) fn new(
        image: HostIssuedAppImage,
        run: &str,
        logical: &str,
        command: &[String],
        network: Option<TrustedAppNetwork>,
    ) -> Result<Self, AppPlanError> {
        if run.len() != 32
            || !run
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || !valid_name(logical, 32)
            || matches!(logical, "pi" | "browser" | "broker")
            || logical.starts_with("pithos-")
        {
            return Err(AppPlanError::Identity);
        }
        if command.is_empty()
            || command.len() > 32
            || command
                .iter()
                .any(|arg| arg.is_empty() || arg.len() > 256 || arg.chars().any(char::is_control))
            || command.iter().map(String::len).sum::<usize>() > 4096
        {
            return Err(AppPlanError::Command);
        }
        Ok(Self {
            image,
            run: run.into(),
            logical: logical.into(),
            command: command.to_vec(),
            network,
        })
    }

    /// Frozen Docker CLI arguments, not an executable operation. The image
    /// configuration must be preflighted separately for anonymous volumes.
    pub(crate) fn docker_run_argv(&self) -> Vec<String> {
        // Fixed options and at most 32 literal command tokens; no caller Docker flags.
        // Frame both components before hashing so boundaries cannot collide.
        // The first 128 bits keep names bounded without embedding raw identities.
        let mut hasher = Sha256::new();
        for component in [&self.run, &self.logical] {
            hasher.update((component.len() as u64).to_be_bytes());
            hasher.update(component.as_bytes());
        }
        let digest = hasher.finalize();
        let suffix: String = digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let name = format!("pithos-app-{suffix}");
        let mut argv = vec![
            "run".into(),
            "--name".into(),
            name.clone(),
            "--label".into(),
            format!("io.pithos.broker.app.run={}", self.run),
            "--label".into(),
            format!("io.pithos.broker.app.logical={}", self.logical),
            "--label".into(),
            format!("io.pithos.broker.app.name={name}"),
            "--pull=never".into(),
            "--read-only".into(),
            "--cap-drop=ALL".into(),
            "--security-opt=no-new-privileges".into(),
            "--pids-limit=128".into(),
            "--memory=256m".into(),
            "--user=65532:65532".into(),
        ];
        if let Some(network) = &self.network {
            argv.extend(["--network".into(), network.0.clone()]);
            argv.extend(["--network-alias".into(), name]);
        } else {
            argv.push("--network=none".into());
        }
        argv.extend(["--".into(), self.image.0.as_str().into()]);
        argv.extend(self.command.iter().cloned());
        argv
    }
}
impl fmt::Debug for AppLaunchPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AppLaunchPlan([redacted])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> HostIssuedAppImage {
        HostIssuedAppImage::from_owned_build(
            ImmutableImageId::new(&format!("sha256:{}", "a".repeat(64))).unwrap(),
        )
    }
    fn command() -> Vec<String> {
        vec!["/bin/echo".into(), "--privileged".into(), "$LITERAL".into()]
    }

    #[test]
    fn host_style_run_id_and_max_logical_have_unique_bounded_frozen_names() {
        let run = "0123456789abcdef0123456789abcdef";
        let other_run = "0123456789abcdef0123456789abcdee";
        let logical = "w".repeat(32);
        let plan = AppLaunchPlan::new(image(), run, &logical, &command(), None).unwrap();
        let same = AppLaunchPlan::new(image(), run, &logical, &command(), None).unwrap();
        let different_run =
            AppLaunchPlan::new(image(), other_run, &logical, &command(), None).unwrap();
        let different_logical =
            AppLaunchPlan::new(image(), run, &"v".repeat(32), &command(), None).unwrap();
        let args = plan.docker_run_argv();
        let name = &args[2];
        assert!(name.len() <= 63);
        assert_eq!(name, "pithos-app-d30d2b5974733de750ccd97c7a9843a7");
        assert_eq!(name.len(), "pithos-app-".len() + 32);
        assert!(name.starts_with("pithos-app-"));
        assert!(
            name["pithos-app-".len()..]
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_eq!(name, &same.docker_run_argv()[2]);
        assert_ne!(name, &different_run.docker_run_argv()[2]);
        assert_ne!(name, &different_logical.docker_run_argv()[2]);
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--label", &format!("io.pithos.broker.app.run={run}")])
        );
        assert!(args.windows(2).any(|pair| pair
            == [
                "--label",
                &format!("io.pithos.broker.app.logical={logical}")
            ]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--label", &format!("io.pithos.broker.app.name={name}")])
        );
        assert!(!format!("{plan:?}").contains(run));
        assert!(!format!("{plan:?}").contains(&logical));
    }

    #[test]
    fn offline_plan_renders_exact_frozen_argv_with_image_boundary() {
        let plan = AppLaunchPlan::new(
            image(),
            "0123456789abcdef0123456789abcdef",
            "web",
            &command(),
            None,
        )
        .unwrap();
        assert_eq!(
            plan.docker_run_argv(),
            vec![
                "run",
                "--name",
                "pithos-app-bf26251e1cc799882f795cbe7f4e08af",
                "--label",
                "io.pithos.broker.app.run=0123456789abcdef0123456789abcdef",
                "--label",
                "io.pithos.broker.app.logical=web",
                "--label",
                "io.pithos.broker.app.name=pithos-app-bf26251e1cc799882f795cbe7f4e08af",
                "--pull=never",
                "--read-only",
                "--cap-drop=ALL",
                "--security-opt=no-new-privileges",
                "--pids-limit=128",
                "--memory=256m",
                "--user=65532:65532",
                "--network=none",
                "--",
                &format!("sha256:{}", "a".repeat(64)),
                "/bin/echo",
                "--privileged",
                "$LITERAL",
            ]
        );
    }

    #[test]
    fn invalid_identities_and_networks_are_rejected_without_echoing_inputs() {
        let valid_run = "0123456789abcdef0123456789abcdef";
        for (run, logical) in [
            ("", "web"),
            ("-run", "web"),
            ("a".repeat(31).as_str(), "web"),
            ("a".repeat(33).as_str(), "web"),
            ("A".repeat(32).as_str(), "web"),
            ("g".repeat(32).as_str(), "web"),
            (("a".repeat(31) + "-").as_str(), "web"),
            (valid_run, "pi"),
            (valid_run, "browser"),
            (valid_run, "broker"),
            (valid_run, "pithos-db"),
            (valid_run, "web_foo"),
            (valid_run, "web\nsecret"),
            (valid_run, "w".repeat(33).as_str()),
        ] {
            let result = AppLaunchPlan::new(image(), run, logical, &command(), None);
            assert!(
                matches!(result, Err(AppPlanError::Identity)),
                "{run:?} {logical:?}"
            );
        }
        for name in ["", "-net", "net\nsecret", "n".repeat(64).as_str()] {
            assert!(matches!(
                TrustedAppNetwork::from_owned_network(name),
                Err(AppPlanError::Network)
            ));
        }
        let network = TrustedAppNetwork::from_owned_network("isolated-net").unwrap();
        let plan =
            AppLaunchPlan::new(image(), valid_run, "web", &command(), Some(network)).unwrap();
        let args = plan.docker_run_argv();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--network", "isolated-net"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--network-alias", args[2].as_str()])
        );
        assert!(!args.iter().any(|arg| arg == "--network=none"));
        assert!(
            !format!(
                "{:?}",
                TrustedAppNetwork::from_owned_network("private-net").unwrap()
            )
            .contains("private-net")
        );
    }

    #[test]
    fn names_are_unambiguous_across_runs_and_logical_apps() {
        let run = "0123456789abcdef0123456789abcdef";
        let first = AppLaunchPlan::new(image(), run, "c", &command(), None).unwrap();
        let second = AppLaunchPlan::new(image(), run, "b-c", &command(), None).unwrap();
        assert_ne!(first.docker_run_argv()[2], second.docker_run_argv()[2]);
        assert_eq!(
            first.docker_run_argv()[2],
            "pithos-app-cc3986424cd360240a14dc7cb48fc84c"
        );
    }

    #[test]
    fn command_is_bounded_literal_and_debug_and_errors_are_redacted() {
        let run = "0123456789abcdef0123456789abcdef";
        for command in [
            vec![],
            vec!["".into()],
            vec!["a\0secret".into()],
            vec!["a\nsecret".into()],
            vec!["x".repeat(257)],
            vec!["x".into(); 33],
            vec!["x".repeat(256); 17],
        ] {
            let result = AppLaunchPlan::new(image(), run, "web", &command, None);
            assert!(matches!(result, Err(AppPlanError::Command)));
            let err = result.err().unwrap();
            assert!(!format!("{err:?} {err}").contains("secret"));
        }
        let plan =
            AppLaunchPlan::new(image(), run, "web", &["private-token".into()], None).unwrap();
        assert!(!format!("{plan:?}").contains("private-token"));
        assert!(!format!("{:?}", image()).contains("sha256:"));
    }
}
