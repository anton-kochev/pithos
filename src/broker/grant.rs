//! Host approval values, independent of project configuration and transport.

/// Broker actions. Recognizing an action does not make it available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Status,
    Build,
    Run,
    Readiness,
    Logs,
    Stop,
    Compose,
    Exec,
}

/// Immutable host approval, constructed explicitly rather than deserialized.
///
/// This is a permission ceiling, not proof that a broker is ready to launch.
/// Only a trusted host entry point should construct it after explicit approval;
/// project configuration and request payloads must never do so.
///
/// Approval cannot be inferred from a default:
/// ```compile_fail,E0277
/// use pithos::broker::grant::HostGrant;
/// let grant: HostGrant = Default::default();
/// ```
/// Nor can a serialized value confer it:
/// ```compile_fail,E0277
/// use pithos::broker::grant::HostGrant;
/// let grant: HostGrant = serde_json::from_str("{}").unwrap();
/// ```
/// Its representation is private:
/// ```compile_fail,E0451
/// use pithos::broker::grant::HostGrant;
/// let grant = HostGrant { _private: () };
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct HostGrant {
    permissions: u16,
}

impl HostGrant {
    const STATUS: u16 = 1 << 0;
    const RUN: u16 = 1 << 1;
    const BUILD: u16 = 1 << 2;
    const READINESS: u16 = 1 << 3;
    const LOGS: u16 = 1 << 4;
    const STOP: u16 = 1 << 5;
    const COMPOSE: u16 = 1 << 6;
    const EXEC: u16 = 1 << 7;

    /// Freeze explicit host approval for status only.
    pub fn status_only() -> Self {
        Self {
            permissions: Self::STATUS,
        }
    }

    /// Freeze explicit host approval for status plus one managed Pi run.
    ///
    /// This does not grant application builds, Compose, exec, logs or any other
    /// broker action. Only a trusted host entry point may construct and retain it.
    pub fn managed_pi_run() -> Self {
        Self {
            permissions: Self::STATUS | Self::RUN,
        }
    }

    /// Freeze explicit host approval for all recognized workspace actions.
    ///
    /// This grants an authority ceiling, not a ready-to-use broker: the CLI
    /// still refuses before launch until the workspace workflow is verified.
    /// Only a trusted host entry point may construct and retain it.
    pub fn workspace() -> Self {
        Self {
            permissions: Self::STATUS
                | Self::BUILD
                | Self::RUN
                | Self::READINESS
                | Self::LOGS
                | Self::STOP
                | Self::COMPOSE
                | Self::EXEC,
        }
    }

    /// Whether this host approval permits the requested action.
    pub fn permits(&self, action: Action) -> bool {
        let required = match action {
            Action::Status => Self::STATUS,
            Action::Run => Self::RUN,
            Action::Build => Self::BUILD,
            Action::Readiness => Self::READINESS,
            Action::Logs => Self::LOGS,
            Action::Stop => Self::STOP,
            Action::Compose => Self::COMPOSE,
            Action::Exec => Self::EXEC,
        };
        self.permissions & required == required
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_only_denies_every_other_action() {
        let grant = HostGrant::status_only();
        for action in [
            Action::Build,
            Action::Run,
            Action::Readiness,
            Action::Logs,
            Action::Stop,
            Action::Compose,
            Action::Exec,
        ] {
            assert!(!grant.permits(action), "unexpected authority: {action:?}");
        }
    }

    #[test]
    fn status_only_permits_status() {
        assert!(HostGrant::status_only().permits(Action::Status));
    }

    #[test]
    fn workspace_grants_every_action_and_is_distinct_from_status_only() {
        let workspace = HostGrant::workspace();
        assert_ne!(workspace, HostGrant::status_only());
        for action in [
            Action::Status,
            Action::Build,
            Action::Run,
            Action::Readiness,
            Action::Logs,
            Action::Stop,
            Action::Compose,
            Action::Exec,
        ] {
            assert!(workspace.permits(action), "missing authority: {action:?}");
        }
    }

    #[test]
    fn managed_pi_run_grants_only_status_and_run() {
        let grant = HostGrant::managed_pi_run();
        assert!(grant.permits(Action::Status));
        assert!(grant.permits(Action::Run));
        for action in [
            Action::Build,
            Action::Readiness,
            Action::Logs,
            Action::Stop,
            Action::Compose,
            Action::Exec,
        ] {
            assert!(!grant.permits(action), "unexpected authority: {action:?}");
        }
    }
}
