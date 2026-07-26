use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The connection state requested by the operator or an MCP client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredConnection {
    Connected,
    Disconnected,
}

/// The observed lifecycle state of a registered downstream MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Registered,
    Starting,
    Initializing,
    Connected,
    Disconnected,
    Stopped,
    Failed,
}

impl LifecycleState {
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Registered, Self::Starting | Self::Stopped)
                | (
                    Self::Starting,
                    Self::Initializing | Self::Stopped | Self::Failed
                )
                | (
                    Self::Initializing,
                    Self::Connected | Self::Stopped | Self::Failed
                )
                | (Self::Connected, Self::Disconnected | Self::Failed)
                | (
                    Self::Disconnected,
                    Self::Starting | Self::Stopped | Self::Failed
                )
                | (Self::Stopped, Self::Starting)
                | (Self::Failed, Self::Starting | Self::Stopped)
        )
    }
}

/// Work the runtime should perform for a connect request.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectDisposition {
    Start,
    JoinExisting,
    AlreadyConnected,
}

/// Work the runtime should perform for a disconnect request.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectDisposition {
    Stop,
    CancelStartup,
    JoinExisting,
    AlreadyInactive,
}

/// State and diagnostic data for one managed server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Lifecycle {
    desired: DesiredConnection,
    state: LifecycleState,
    last_error: Option<String>,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            desired: DesiredConnection::Disconnected,
            state: LifecycleState::Registered,
            last_error: None,
        }
    }
}

impl Lifecycle {
    #[must_use]
    pub fn desired(&self) -> DesiredConnection {
        self.desired
    }

    #[must_use]
    pub fn state(&self) -> LifecycleState {
        self.state
    }

    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Records the desired state and starts or joins the connect operation.
    pub fn request_connect(&mut self) -> ConnectDisposition {
        self.desired = DesiredConnection::Connected;

        match self.state {
            LifecycleState::Registered
            | LifecycleState::Disconnected
            | LifecycleState::Stopped
            | LifecycleState::Failed => {
                self.state = LifecycleState::Starting;
                self.last_error = None;
                ConnectDisposition::Start
            }
            LifecycleState::Starting | LifecycleState::Initializing => {
                ConnectDisposition::JoinExisting
            }
            LifecycleState::Connected => ConnectDisposition::AlreadyConnected,
        }
    }

    /// Records the desired state without claiming transport cleanup has finished.
    pub fn request_disconnect(&mut self) -> DisconnectDisposition {
        let disconnect_in_progress = self.desired == DesiredConnection::Disconnected;
        self.desired = DesiredConnection::Disconnected;

        match self.state {
            LifecycleState::Starting | LifecycleState::Initializing if disconnect_in_progress => {
                DisconnectDisposition::JoinExisting
            }
            LifecycleState::Starting | LifecycleState::Initializing => {
                DisconnectDisposition::CancelStartup
            }
            LifecycleState::Connected | LifecycleState::Failed if disconnect_in_progress => {
                DisconnectDisposition::JoinExisting
            }
            LifecycleState::Connected | LifecycleState::Failed => DisconnectDisposition::Stop,
            LifecycleState::Registered | LifecycleState::Disconnected | LifecycleState::Stopped => {
                DisconnectDisposition::AlreadyInactive
            }
        }
    }

    pub fn transition_to(&mut self, next: LifecycleState) -> Result<(), LifecycleError> {
        if next == LifecycleState::Failed {
            return Err(LifecycleError::FailureRequiresReason);
        }

        self.apply_transition(next)
    }

    pub fn fail(&mut self, reason: impl Into<String>) -> Result<(), LifecycleError> {
        if self.state == LifecycleState::Failed {
            return Ok(());
        }

        if !self.state.can_transition_to(LifecycleState::Failed) {
            return Err(LifecycleError::InvalidTransition {
                from: self.state,
                to: LifecycleState::Failed,
            });
        }

        self.state = LifecycleState::Failed;
        self.last_error = Some(reason.into());
        Ok(())
    }

    fn apply_transition(&mut self, next: LifecycleState) -> Result<(), LifecycleError> {
        if !self.state.can_transition_to(next) {
            return Err(LifecycleError::InvalidTransition {
                from: self.state,
                to: next,
            });
        }

        self.state = next;
        if matches!(next, LifecycleState::Starting | LifecycleState::Stopped) {
            self.last_error = None;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum LifecycleError {
    #[error("invalid lifecycle transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: LifecycleState,
        to: LifecycleState,
    },
    #[error("transitioning to Failed requires a failure reason")]
    FailureRequiresReason,
}

#[cfg(test)]
mod tests {
    use super::{
        ConnectDisposition, DesiredConnection, DisconnectDisposition, Lifecycle, LifecycleError,
        LifecycleState,
    };

    const STATES: [LifecycleState; 7] = [
        LifecycleState::Registered,
        LifecycleState::Starting,
        LifecycleState::Initializing,
        LifecycleState::Connected,
        LifecycleState::Disconnected,
        LifecycleState::Stopped,
        LifecycleState::Failed,
    ];

    #[test]
    fn transition_table_is_explicit_for_every_state_pair() {
        for from in STATES {
            for to in STATES {
                let expected = matches!(
                    (from, to),
                    (
                        LifecycleState::Registered,
                        LifecycleState::Starting | LifecycleState::Stopped
                    ) | (
                        LifecycleState::Starting,
                        LifecycleState::Initializing
                            | LifecycleState::Stopped
                            | LifecycleState::Failed
                    ) | (
                        LifecycleState::Initializing,
                        LifecycleState::Connected
                            | LifecycleState::Stopped
                            | LifecycleState::Failed
                    ) | (
                        LifecycleState::Connected,
                        LifecycleState::Disconnected | LifecycleState::Failed
                    ) | (
                        LifecycleState::Disconnected,
                        LifecycleState::Starting | LifecycleState::Stopped | LifecycleState::Failed
                    ) | (LifecycleState::Stopped, LifecycleState::Starting)
                        | (
                            LifecycleState::Failed,
                            LifecycleState::Starting | LifecycleState::Stopped
                        )
                );

                assert_eq!(from.can_transition_to(to), expected, "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn repeated_connect_joins_startup() {
        let mut lifecycle = Lifecycle::default();

        assert_eq!(lifecycle.request_connect(), ConnectDisposition::Start);
        assert_eq!(lifecycle.state(), LifecycleState::Starting);
        assert_eq!(
            lifecycle.request_connect(),
            ConnectDisposition::JoinExisting
        );
        assert_eq!(lifecycle.desired(), DesiredConnection::Connected);
    }

    #[test]
    fn connect_dispositions_cover_every_state() {
        let cases = [
            (LifecycleState::Registered, ConnectDisposition::Start),
            (LifecycleState::Starting, ConnectDisposition::JoinExisting),
            (
                LifecycleState::Initializing,
                ConnectDisposition::JoinExisting,
            ),
            (
                LifecycleState::Connected,
                ConnectDisposition::AlreadyConnected,
            ),
            (LifecycleState::Disconnected, ConnectDisposition::Start),
            (LifecycleState::Stopped, ConnectDisposition::Start),
            (LifecycleState::Failed, ConnectDisposition::Start),
        ];

        for (state, expected) in cases {
            let mut lifecycle = lifecycle_in(state, DesiredConnection::Connected);
            assert_eq!(lifecycle.request_connect(), expected, "state: {state:?}");
        }
    }

    #[test]
    fn disconnect_during_initialization_requests_cancellation() {
        let mut lifecycle = Lifecycle::default();
        assert_eq!(lifecycle.request_connect(), ConnectDisposition::Start);
        lifecycle
            .transition_to(LifecycleState::Initializing)
            .expect("starting server may begin initialization");

        assert_eq!(
            lifecycle.request_disconnect(),
            DisconnectDisposition::CancelStartup
        );
        assert_eq!(lifecycle.desired(), DesiredConnection::Disconnected);
        assert_eq!(lifecycle.state(), LifecycleState::Initializing);
    }

    #[test]
    fn disconnect_dispositions_cover_every_state() {
        let cases = [
            (
                LifecycleState::Registered,
                DisconnectDisposition::AlreadyInactive,
            ),
            (
                LifecycleState::Starting,
                DisconnectDisposition::CancelStartup,
            ),
            (
                LifecycleState::Initializing,
                DisconnectDisposition::CancelStartup,
            ),
            (LifecycleState::Connected, DisconnectDisposition::Stop),
            (
                LifecycleState::Disconnected,
                DisconnectDisposition::AlreadyInactive,
            ),
            (
                LifecycleState::Stopped,
                DisconnectDisposition::AlreadyInactive,
            ),
            (LifecycleState::Failed, DisconnectDisposition::Stop),
        ];

        for (state, expected) in cases {
            let mut lifecycle = lifecycle_in(state, DesiredConnection::Connected);
            assert_eq!(lifecycle.request_disconnect(), expected, "state: {state:?}");
            assert_eq!(lifecycle.desired(), DesiredConnection::Disconnected);
        }
    }

    #[test]
    fn repeated_disconnect_joins_existing_stop() {
        let mut lifecycle = connected_lifecycle();

        assert_eq!(lifecycle.request_disconnect(), DisconnectDisposition::Stop);
        assert_eq!(
            lifecycle.request_disconnect(),
            DisconnectDisposition::JoinExisting
        );
        assert_eq!(lifecycle.state(), LifecycleState::Connected);
    }

    #[test]
    fn unexpected_exit_records_failure() {
        let mut lifecycle = connected_lifecycle();

        lifecycle
            .fail("downstream process exited with code 1")
            .expect("connected server may fail");

        assert_eq!(lifecycle.state(), LifecycleState::Failed);
        assert_eq!(
            lifecycle.last_error(),
            Some("downstream process exited with code 1")
        );
        assert_eq!(lifecycle.desired(), DesiredConnection::Connected);
    }

    #[test]
    fn repeated_failure_preserves_root_cause() {
        let mut lifecycle = connected_lifecycle();
        lifecycle.fail("process exited").expect("failure is valid");

        lifecycle
            .fail("cleanup timed out")
            .expect("repeated failure is idempotent");

        assert_eq!(lifecycle.last_error(), Some("process exited"));
    }

    #[test]
    fn reconnect_from_failure_clears_diagnostic() {
        let mut lifecycle = connected_lifecycle();
        lifecycle.fail("connection lost").expect("failure is valid");

        assert_eq!(lifecycle.request_connect(), ConnectDisposition::Start);
        assert_eq!(lifecycle.state(), LifecycleState::Starting);
        assert_eq!(lifecycle.last_error(), None);
    }

    #[test]
    fn failed_transition_requires_reason() {
        let mut lifecycle = Lifecycle::default();
        assert_eq!(lifecycle.request_connect(), ConnectDisposition::Start);

        assert_eq!(
            lifecycle.transition_to(LifecycleState::Failed),
            Err(LifecycleError::FailureRequiresReason)
        );
    }

    #[test]
    fn invalid_transition_preserves_state() {
        let mut lifecycle = Lifecycle::default();

        assert_eq!(
            lifecycle.transition_to(LifecycleState::Connected),
            Err(LifecycleError::InvalidTransition {
                from: LifecycleState::Registered,
                to: LifecycleState::Connected,
            })
        );
        assert_eq!(lifecycle.state(), LifecycleState::Registered);
    }

    fn connected_lifecycle() -> Lifecycle {
        let mut lifecycle = Lifecycle::default();
        assert_eq!(lifecycle.request_connect(), ConnectDisposition::Start);
        lifecycle
            .transition_to(LifecycleState::Initializing)
            .expect("starting server may initialize");
        lifecycle
            .transition_to(LifecycleState::Connected)
            .expect("initialized server may connect");
        lifecycle
    }

    fn lifecycle_in(state: LifecycleState, desired: DesiredConnection) -> Lifecycle {
        Lifecycle {
            desired,
            state,
            last_error: (state == LifecycleState::Failed).then(|| "previous failure".to_owned()),
        }
    }
}
