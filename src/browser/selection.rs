#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum BrowserMode {
    #[default]
    Interactive,
    Headless,
}

impl BrowserMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Headless => "headless",
        }
    }
}

/// Invocation-scoped runtime selection, independent of project configuration.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum BrowserSelection {
    #[default]
    Disabled,
    Enabled(BrowserMode),
}

impl BrowserSelection {
    pub const fn mode(self) -> Option<BrowserMode> {
        match self {
            Self::Disabled => None,
            Self::Enabled(mode) => Some(mode),
        }
    }

    pub const fn client_layer(self) -> BrowserClientLayer {
        match self {
            Self::Disabled => BrowserClientLayer::Absent,
            Self::Enabled(_) => BrowserClientLayer::Included,
        }
    }
}

/// Image construction never depends on the runtime display mode.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum BrowserClientLayer {
    #[default]
    Absent,
    Included,
}
