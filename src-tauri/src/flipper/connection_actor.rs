//! Single-owner execution foundation for the Flipper connection.
//!
//! The physical transport is blocking, so one dedicated OS thread owns the
//! [`FlipperClient`] by value. Async callers submit typed jobs through a
//! bounded Tokio MPSC queue and receive their result through a per-request
//! oneshot channel. The actor-owned protobuf path assigns command IDs, owns
//! framing, and classifies every inbound message while remaining deliberately
//! serialized. Screen and serial CLI modes likewise keep every read, write,
//! and acknowledged transition on the actor thread. All live AppState command
//! paths submit work to this owner; no shared FlipperClient slot remains.

use std::fmt;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use prost::Message;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::error::{is_fatal_transport_error, FlipperError, Result as FlipperResult};
use crate::flipper::client::FlipperClient;
use crate::flipper::framing::{
    read_message_until, write_message, DeadlineReadError, MAX_FRAME_SIZE,
};
use crate::flipper::session::check_response;
use crate::flipper::transport::TransportKind;
use crate::flipper::{BLE_TIMEOUT_SCREEN, SERIAL_TIMEOUT_NORMAL, SERIAL_TIMEOUT_SCREEN};
use crate::pb;
use crate::pb::main::Content;
use crate::pb_gui;
use crate::pb_system;
use crate::state::ConnectionMode;

/// Default number of waiting control messages.
///
/// Running work is not counted in this bound. Producers receive
/// [`ConnectionActorError::QueueFull`] rather than growing memory without a
/// limit.
pub const DEFAULT_QUEUE_CAPACITY: usize = 32;
const MAX_RPC_RESPONSE_FRAMES: usize = 1024;
/// Aggregate protobuf-body budget for ordinary, non-streaming RPC responses.
/// Storage streaming will use separate bounded reader/writer APIs rather than
/// raising this in-memory limit.
const MAX_RPC_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RPC_REQUEST_BYTES: usize = MAX_FRAME_SIZE;
const DEFAULT_RPC_RESPONSE_DEADLINE: Duration = Duration::from_secs(30);
const DEFAULT_CLI_OUTPUT_CAPACITY: usize = 64;
const CLI_READ_TIMEOUT: Duration = Duration::from_millis(50);
const CLI_PRE_EXIT_DRAIN_DEADLINE: Duration = Duration::from_millis(100);
const CLI_READ_CHUNK_BYTES: usize = 512;
const MAX_CLI_COMMAND_BYTES: usize = 1024;
const CLI_START_RPC_SESSION: &[u8] = b"start_rpc_session\r";
const CLI_RPC_HANDOFF_MARKER: &[u8] = b"start_rpc_session\r\n";
const CLI_INTERRUPT: &[u8] = &[0x03];
const CLI_PING_PAYLOAD: &[u8] = &[0xDE, 0xAD, 0xBE, 0xEF];
const INPUT_PRESS: i32 = 0;
const INPUT_RELEASE: i32 = 1;
const INPUT_SHORT: i32 = 2;
const INPUT_LONG: i32 = 3;

#[derive(Clone, Copy)]
struct RpcDispatchLimits {
    max_frames: usize,
    max_bytes: usize,
    max_request_bytes: usize,
    response_deadline: Duration,
}

impl Default for RpcDispatchLimits {
    fn default() -> Self {
        Self {
            max_frames: MAX_RPC_RESPONSE_FRAMES,
            max_bytes: MAX_RPC_RESPONSE_BYTES,
            max_request_bytes: MAX_RPC_REQUEST_BYTES,
            response_deadline: DEFAULT_RPC_RESPONSE_DEADLINE,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct ActorConfig {
    rpc_limits: RpcDispatchLimits,
}

/// Observable lifecycle and protocol state of the actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Rpc,
    ScreenStreaming,
    Cli,
    Transitioning,
    ShuttingDown,
}

/// One bounded item from the actor-owned CLI byte stream.
///
/// The session identifier lets consumers discard bytes from an older CLI
/// session. Tokio broadcast receivers surface overflow explicitly through
/// [`broadcast::error::RecvError::Lagged`]; callers may resubscribe to resume
/// at the current tail without creating unbounded storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CliOutputEvent {
    SessionStarted { session_id: u64 },
    Data { session_id: u64, bytes: Vec<u8> },
    SessionEnded { session_id: u64 },
}

impl ConnectionState {
    fn from_byte(value: u8) -> Self {
        match value {
            1 => Self::Rpc,
            2 => Self::ScreenStreaming,
            3 => Self::Cli,
            4 => Self::Transitioning,
            5 => Self::ShuttingDown,
            _ => Self::Disconnected,
        }
    }

    const fn as_byte(self) -> u8 {
        match self {
            Self::Disconnected => 0,
            Self::Rpc => 1,
            Self::ScreenStreaming => 2,
            Self::Cli => 3,
            Self::Transitioning => 4,
            Self::ShuttingDown => 5,
        }
    }
}

impl From<ConnectionMode> for ConnectionState {
    fn from(mode: ConnectionMode) -> Self {
        match mode {
            ConnectionMode::Rpc => Self::Rpc,
            ConnectionMode::ScreenStreaming => Self::ScreenStreaming,
            ConnectionMode::Cli => Self::Cli,
        }
    }
}

impl fmt::Display for ConnectionState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => formatter.write_str("disconnected"),
            Self::Rpc => formatter.write_str("RPC"),
            Self::ScreenStreaming => formatter.write_str("screen streaming"),
            Self::Cli => formatter.write_str("CLI"),
            Self::Transitioning => formatter.write_str("transitioning"),
            Self::ShuttingDown => formatter.write_str("shutting down"),
        }
    }
}

/// Errors produced by the connection-control layer.
#[derive(Debug, Error)]
pub enum ConnectionActorError {
    #[error("connection command queue is full")]
    QueueFull,

    #[error("connection actor is closed")]
    Closed,

    #[error("connection actor stopped before the request could run")]
    ActorStopped,

    #[error("connection actor stopped because a device job panicked")]
    JobPanicked,

    #[error("connection was lost before this queued request ran: {cause}")]
    ConnectionLost { cause: Arc<str> },

    #[error("connection work is not allowed while the connection is {current}")]
    ModeRejected { current: ConnectionState },

    #[error("CLI mode is available only over USB serial")]
    CliRequiresSerial,

    #[error("CLI command is {actual} bytes; limit is {limit}")]
    CliCommandBytesExceeded { actual: usize, limit: usize },

    #[error("CLI command contains a forbidden CR, LF, or NUL byte")]
    InvalidCliCommand,

    #[error("invalid screen input key {0}; expected 0 through 5")]
    InvalidScreenInputKey(i32),

    #[error("invalid screen input type {0}; expected 0 through 4")]
    InvalidScreenInputType(i32),

    #[error(
        "screen stream command {command_id} ended with status {command_status} while input was awaiting acknowledgement"
    )]
    ScreenStreamEndedDuringInput {
        command_id: u32,
        command_status: i32,
    },

    #[error("device request failed: {0}")]
    Device(#[from] FlipperError),

    #[error("connection protocol error: {0}")]
    Protocol(#[from] ConnectionProtocolError),

    #[error("failed to start connection actor thread: {0}")]
    ThreadSpawn(String),
}

/// Protocol violations that make the current byte stream unsafe to reuse.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConnectionProtocolError {
    #[error("timed out waiting for response to command {command_id}")]
    ResponseTimeout { command_id: u32 },

    #[error("command {command_id} exceeded its {deadline_ms} ms response-read deadline")]
    ResponseReadDeadlineExceeded { command_id: u32, deadline_ms: u64 },

    #[error("command {command_id} request is {actual} bytes; limit is {limit}")]
    RequestBytesExceeded {
        command_id: u32,
        actual: usize,
        limit: usize,
    },

    #[error("received command {received_id} while waiting for command {expected_id}")]
    ForeignCommandId { expected_id: u32, received_id: u32 },

    #[error("screen command {command_id} returned an unexpected response payload")]
    UnexpectedScreenResponse { command_id: u32 },

    #[error("CLI stop-session command {command_id} returned an unexpected response")]
    UnexpectedCliStopResponse { command_id: u32 },

    #[error("CLI-to-RPC handoff exceeded its {deadline_ms} ms deadline")]
    CliHandoffDeadlineExceeded { deadline_ms: u64 },

    #[error("CLI recovery ping command {command_id} returned an unexpected response")]
    UnexpectedCliPingResponse { command_id: u32 },

    #[error("command {command_id} exceeded {limit} response frames")]
    TooManyResponseFrames { command_id: u32, limit: usize },

    #[error("command {command_id} exceeded {limit} aggregate response bytes")]
    ResponseBytesExceeded { command_id: u32, limit: usize },
}

/// All matching protobuf frames belonging to one serialized RPC request.
#[derive(Debug)]
pub struct RpcResponse {
    pub command_id: u32,
    pub frames: Vec<pb::Main>,
}

type ActorResult<T> = std::result::Result<T, ConnectionActorError>;

trait RpcJob: Send {
    fn execute(self: Box<Self>, client: &mut FlipperClient) -> JobDisposition;
    fn reject(self: Box<Self>, error: ConnectionActorError);
}

struct TypedRpcJob<T, F> {
    work: F,
    reply: oneshot::Sender<ActorResult<T>>,
}

impl<T, F> RpcJob for TypedRpcJob<T, F>
where
    T: Send + 'static,
    F: FnOnce(&mut FlipperClient) -> FlipperResult<T> + Send + 'static,
{
    fn execute(self: Box<Self>, client: &mut FlipperClient) -> JobDisposition {
        let Self { work, reply } = *self;
        match panic::catch_unwind(AssertUnwindSafe(|| work(client))) {
            Ok(result) if result.as_ref().is_err_and(is_fatal_transport_error) => {
                let cause: Arc<str> = match result.as_ref() {
                    Err(error) => error.to_string().into(),
                    Ok(_) => unreachable!("fatal result must contain an error"),
                };
                JobDisposition::Fatal {
                    cause,
                    reply: Box::new(move || {
                        let _ = reply.send(result.map_err(ConnectionActorError::Device));
                    }),
                }
            }
            Ok(result) => {
                // Cancellation belongs to the caller. A dropped receiver must
                // never panic or take down the connection owner.
                let _ = reply.send(result.map_err(ConnectionActorError::Device));
                JobDisposition::Continue
            }
            Err(_) => JobDisposition::Fatal {
                cause: Arc::<str>::from("legacy device job panicked"),
                reply: Box::new(move || {
                    let _ = reply.send(Err(ConnectionActorError::JobPanicked));
                }),
            },
        }
    }

    fn reject(self: Box<Self>, error: ConnectionActorError) {
        let _ = self.reply.send(Err(error));
    }
}

enum JobDisposition {
    Continue,
    /// Publish only after the actor has closed admission, rejected pending
    /// work, dropped the client, and reported itself disconnected.
    Fatal {
        cause: Arc<str>,
        reply: Box<dyn FnOnce()>,
    },
}

struct ProtoRpcRequest {
    content: Content,
    reply: oneshot::Sender<ActorResult<RpcResponse>>,
}

impl ProtoRpcRequest {
    fn reject(self, error: ConnectionActorError) {
        let _ = self.reply.send(Err(error));
    }
}

struct StartScreenStreamRequest {
    reply: oneshot::Sender<ActorResult<()>>,
}

impl StartScreenStreamRequest {
    fn reject(self, error: ConnectionActorError) {
        let _ = self.reply.send(Err(error));
    }
}

struct ScreenInputRequest {
    key: i32,
    input_type: i32,
    reply: oneshot::Sender<ActorResult<()>>,
}

impl ScreenInputRequest {
    fn reject(self, error: ConnectionActorError) {
        let _ = self.reply.send(Err(error));
    }
}

struct StopScreenStreamRequest {
    reply: oneshot::Sender<ActorResult<()>>,
}

impl StopScreenStreamRequest {
    fn reject(self, error: ConnectionActorError) {
        let _ = self.reply.send(Err(error));
    }
}

struct EnterCliRequest {
    reply: oneshot::Sender<ActorResult<()>>,
}

impl EnterCliRequest {
    fn reject(self, error: ConnectionActorError) {
        let _ = self.reply.send(Err(error));
    }
}

struct CliSendRequest {
    command: String,
    reply: oneshot::Sender<ActorResult<()>>,
}

impl CliSendRequest {
    fn reject(self, error: ConnectionActorError) {
        let _ = self.reply.send(Err(error));
    }
}

struct CliInterruptRequest {
    reply: oneshot::Sender<ActorResult<()>>,
}

impl CliInterruptRequest {
    fn reject(self, error: ConnectionActorError) {
        let _ = self.reply.send(Err(error));
    }
}

struct ExitCliRequest {
    reply: oneshot::Sender<ActorResult<()>>,
}

impl ExitCliRequest {
    fn reject(self, error: ConnectionActorError) {
        let _ = self.reply.send(Err(error));
    }
}

enum ActorCommand {
    RunRpc(Box<dyn RpcJob>),
    ProtoRpc(ProtoRpcRequest),
    StartScreenStream(StartScreenStreamRequest),
    ScreenInput(ScreenInputRequest),
    StopScreenStream(StopScreenStreamRequest),
    EnterCli(EnterCliRequest),
    CliSend(CliSendRequest),
    CliInterrupt(CliInterruptRequest),
    ExitCli(ExitCliRequest),
    Shutdown,
    #[cfg(test)]
    ForcePanic,
}

/// Cloneable async-side handle for the single connection owner.
///
/// Cloning this type clones only the bounded command sender and state marker;
/// it never grants access to the client or transport.
#[derive(Clone)]
pub struct ConnectionHandle {
    commands: mpsc::Sender<ActorCommand>,
    /// Actor-owned actual lifecycle/protocol state. The handle reads this only
    /// to reconcile a failed transition admission with what really happened
    /// on the wire.
    state: Arc<AtomicU8>,
    /// Async-side desired/admission state. Transition preclaim changes this
    /// marker without changing the actor's actual protocol state.
    admission_state: Arc<AtomicU8>,
    admission_gate: Arc<Mutex<()>>,
    /// Monotonic actor-side screen-terminal publication attempts. Besides
    /// supporting deterministic race tests, this makes the publication point
    /// explicit in the shared transition protocol.
    #[cfg_attr(not(test), allow(dead_code))]
    screen_terminal_attempts: Arc<AtomicU64>,
    shutdown_complete: watch::Sender<bool>,
    screen_frames: watch::Receiver<Option<pb::Main>>,
    transport_kind: TransportKind,
    cli_output: Arc<Mutex<broadcast::Receiver<CliOutputEvent>>>,
}

impl ConnectionHandle {
    /// Start a dedicated blocking owner thread using the default queue bound.
    pub fn spawn(client: FlipperClient) -> ActorResult<Self> {
        Self::spawn_with_capacity(client, DEFAULT_QUEUE_CAPACITY)
    }

    fn spawn_with_capacity(client: FlipperClient, capacity: usize) -> ActorResult<Self> {
        assert!(
            capacity > 0,
            "connection actor queue must be bounded above zero"
        );

        Self::spawn_inner(
            client,
            capacity,
            ConnectionMode::Rpc,
            ActorConfig::default(),
        )
    }

    fn spawn_inner(
        client: FlipperClient,
        capacity: usize,
        initial_mode: ConnectionMode,
        config: ActorConfig,
    ) -> ActorResult<Self> {
        assert!(config.rpc_limits.max_frames > 0);
        assert!(config.rpc_limits.max_bytes > 0);
        assert!(config.rpc_limits.max_request_bytes > 0);
        let transport_kind = client.transport.kind();
        let (commands, receiver) = mpsc::channel(capacity);
        let state = Arc::new(AtomicU8::new(ConnectionState::from(initial_mode).as_byte()));
        let actor_state = Arc::clone(&state);
        let admission_state =
            Arc::new(AtomicU8::new(ConnectionState::from(initial_mode).as_byte()));
        let actor_admission_state = Arc::clone(&admission_state);
        let admission_gate = Arc::new(Mutex::new(()));
        let actor_admission_gate = Arc::clone(&admission_gate);
        let screen_terminal_attempts = Arc::new(AtomicU64::new(0));
        let actor_screen_terminal_attempts = Arc::clone(&screen_terminal_attempts);
        let (shutdown_complete, _) = watch::channel(false);
        let actor_shutdown_complete = shutdown_complete.clone();
        // A screen stream is conceptually a latest-value feed. Holding one
        // frame in a watch channel makes backpressure explicit: a slow UI sees
        // the newest complete frame instead of growing memory or disconnecting
        // the transport merely because older frames were not rendered.
        let (screen_frame_tx, screen_frame_rx) = watch::channel(None);
        // The actor owns the only Sender. This retained base Receiver gives
        // cloneable handles a place to call `resubscribe` without ever owning
        // transport state, while the fixed-capacity ring bounds memory.
        let (cli_output_tx, cli_output_rx) = broadcast::channel(DEFAULT_CLI_OUTPUT_CAPACITY);
        let cli_output = Arc::new(Mutex::new(cli_output_rx));

        std::thread::Builder::new()
            .name("flipper-connection".to_owned())
            .spawn(move || {
                actor_thread(
                    client,
                    receiver,
                    actor_state,
                    actor_admission_state,
                    actor_admission_gate,
                    actor_screen_terminal_attempts,
                    actor_shutdown_complete,
                    screen_frame_tx,
                    cli_output_tx,
                    config.rpc_limits,
                )
            })
            .map_err(|error| ConnectionActorError::ThreadSpawn(error.to_string()))?;

        Ok(Self {
            commands,
            state,
            admission_state,
            admission_gate,
            screen_terminal_attempts,
            shutdown_complete,
            screen_frames: screen_frame_rx,
            transport_kind,
            cli_output,
        })
    }

    #[cfg(test)]
    fn spawn_with_mode(
        client: FlipperClient,
        capacity: usize,
        initial_mode: ConnectionMode,
    ) -> ActorResult<Self> {
        assert!(capacity > 0);
        Self::spawn_inner(client, capacity, initial_mode, ActorConfig::default())
    }

    #[cfg(test)]
    fn spawn_with_config(
        client: FlipperClient,
        capacity: usize,
        initial_mode: ConnectionMode,
        config: ActorConfig,
    ) -> ActorResult<Self> {
        assert!(capacity > 0);
        Self::spawn_inner(client, capacity, initial_mode, config)
    }

    /// Return the latest async admission state without touching the transport
    /// or blocking the owner thread.
    ///
    /// A transition preclaim may report `Transitioning` while older FIFO work
    /// is still completing in the actor's actual wire state.
    pub fn state(&self) -> ConnectionState {
        ConnectionState::from_byte(self.admission_state.load(Ordering::Acquire))
    }

    /// Return the active protocol mode, or `None` while the actor is between
    /// modes, shutting down, or disconnected.
    pub fn mode(&self) -> Option<ConnectionMode> {
        match self.state() {
            ConnectionState::Rpc => Some(ConnectionMode::Rpc),
            ConnectionState::ScreenStreaming => Some(ConnectionMode::ScreenStreaming),
            ConnectionState::Cli => Some(ConnectionMode::Cli),
            ConnectionState::Disconnected
            | ConnectionState::Transitioning
            | ConnectionState::ShuttingDown => None,
        }
    }

    /// Physical transport kind retained by the actor-owned client.
    pub(crate) fn transport_kind(&self) -> TransportKind {
        self.transport_kind
    }

    /// True when two handles point at the same actor instance.
    pub(crate) fn same_connection(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    /// Wait until the actor has relinquished its client and published the
    /// disconnected state. Used by the AppState monitor to emit exactly one
    /// global fatal-disconnect event without introducing another reader.
    pub(crate) async fn wait_until_closed(&self) {
        let mut completion = self.shutdown_complete.subscribe();
        if *completion.borrow() {
            return;
        }
        while completion.changed().await.is_ok() {
            if *completion.borrow() {
                return;
            }
        }
    }

    /// Subscribe to the actor's coalescing screen-frame route.
    ///
    /// The watch channel retains only the newest complete frame. A slow
    /// consumer therefore skips stale frames by design. Each caller receives
    /// an independent subscription and no subscription grants transport
    /// access, so the actor remains the only reader.
    #[allow(dead_code)]
    pub(crate) fn subscribe_screen_frames(&self) -> watch::Receiver<Option<pb::Main>> {
        self.screen_frames.clone()
    }

    /// Subscribe to bounded, loss-aware actor-owned CLI output.
    ///
    /// `Lagged` is recoverable: consumers can call this method again to attach
    /// at the current tail. `Closed` means the actor has torn the connection
    /// down. Session boundary events prevent bytes from different CLI sessions
    /// being mistaken for one continuous stream.
    #[allow(dead_code)]
    pub(crate) fn subscribe_cli_output(&self) -> broadcast::Receiver<CliOutputEvent> {
        match self.cli_output.lock() {
            Ok(receiver) => receiver.resubscribe(),
            Err(poisoned) => {
                tracing::warn!("CLI output subscription gate was poisoned; recovering");
                poisoned.into_inner().resubscribe()
            }
        }
    }

    /// Stop the RPC session and enter actor-owned serial CLI mode.
    #[allow(dead_code)]
    pub(crate) async fn enter_cli(&self) -> ActorResult<()> {
        let receiver = self.submit_enter_cli()?;
        receiver
            .await
            .unwrap_or(Err(ConnectionActorError::ActorStopped))
    }

    /// Send one validated UTF-8 CLI command followed by exactly one CR byte.
    #[allow(dead_code)]
    pub(crate) async fn cli_send(&self, command: &str) -> ActorResult<()> {
        let receiver = self.submit_cli_send(command)?;
        receiver
            .await
            .unwrap_or(Err(ConnectionActorError::ActorStopped))
    }

    /// Send exactly one ETX byte to interrupt the active CLI command.
    #[allow(dead_code)]
    pub(crate) async fn cli_interrupt(&self) -> ActorResult<()> {
        let receiver = self.submit_cli_interrupt()?;
        receiver
            .await
            .unwrap_or(Err(ConnectionActorError::ActorStopped))
    }

    /// Leave CLI mode and publish RPC only after an exact recovery ping.
    #[allow(dead_code)]
    pub(crate) async fn exit_cli(&self) -> ActorResult<()> {
        let receiver = self.submit_exit_cli()?;
        receiver
            .await
            .unwrap_or(Err(ConnectionActorError::ActorStopped))
    }

    /// Acknowledge and enter actor-owned screen streaming mode.
    #[allow(dead_code)]
    pub(crate) async fn start_screen_stream(&self) -> ActorResult<()> {
        let receiver = self.submit_start_screen_stream()?;
        receiver
            .await
            .unwrap_or(Err(ConnectionActorError::ActorStopped))
    }

    /// Serialize one validated input gesture on the active screen stream.
    #[allow(dead_code)]
    pub(crate) async fn send_screen_input(&self, key: i32, input_type: i32) -> ActorResult<()> {
        let receiver = self.submit_screen_input(key, input_type)?;
        receiver
            .await
            .unwrap_or(Err(ConnectionActorError::ActorStopped))
    }

    /// Acknowledge and leave actor-owned screen streaming mode.
    #[allow(dead_code)]
    pub(crate) async fn stop_screen_stream(&self) -> ActorResult<()> {
        let receiver = self.submit_stop_screen_stream()?;
        receiver
            .await
            .unwrap_or(Err(ConnectionActorError::ActorStopped))
    }

    /// Submit one protobuf request to the actor-owned serialized dispatcher.
    ///
    /// The actor assigns the command ID, owns framing, collects all matching
    /// `has_next` response frames, and explicitly routes any interleaved screen
    /// frames. This is the preferred foundation for migrated commands.
    #[allow(dead_code)]
    pub(crate) async fn request_rpc(&self, content: Content) -> ActorResult<RpcResponse> {
        let receiver = self.submit_proto_rpc(content)?;
        receiver
            .await
            .unwrap_or(Err(ConnectionActorError::ActorStopped))
    }

    /// Execute one legacy blocking Flipper operation in RPC mode.
    ///
    /// This generic closure seam is temporary and crate-private. It exists only
    /// to migrate legacy commands incrementally; it is not the final typed
    /// command/response dispatcher and must not be exposed over IPC.
    /// Submission uses `try_send`, so saturation reports explicit backpressure.
    #[allow(dead_code)]
    pub(crate) async fn execute_legacy_rpc<T, F>(&self, work: F) -> ActorResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut FlipperClient) -> FlipperResult<T> + Send + 'static,
    {
        let reply = self.submit_rpc(work)?;
        reply
            .await
            .unwrap_or(Err(ConnectionActorError::ActorStopped))
    }

    fn submit_rpc<T, F>(&self, work: F) -> ActorResult<oneshot::Receiver<ActorResult<T>>>
    where
        T: Send + 'static,
        F: FnOnce(&mut FlipperClient) -> FlipperResult<T> + Send + 'static,
    {
        // Keep the admission check and queue insertion indivisible with
        // respect to shutdown. Recovering a poisoned gate is safe because the
        // lifecycle state remains the source of truth.
        let _admission = match self.admission_gate.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::warn!("connection admission gate was poisoned; recovering");
                poisoned.into_inner()
            }
        };

        if let Some(error) = self.admission_error() {
            return Err(error);
        }

        let (reply, receiver) = oneshot::channel();
        let command = ActorCommand::RunRpc(Box::new(TypedRpcJob { work, reply }));

        self.commands.try_send(command).map_err(map_send_error)?;
        Ok(receiver)
    }

    fn submit_proto_rpc(
        &self,
        content: Content,
    ) -> ActorResult<oneshot::Receiver<ActorResult<RpcResponse>>> {
        let _admission = match self.admission_gate.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::warn!("connection admission gate was poisoned; recovering");
                poisoned.into_inner()
            }
        };

        if let Some(error) = self.admission_error() {
            return Err(error);
        }

        let (reply, receiver) = oneshot::channel();
        self.commands
            .try_send(ActorCommand::ProtoRpc(ProtoRpcRequest { content, reply }))
            .map_err(map_send_error)?;
        Ok(receiver)
    }

    fn submit_start_screen_stream(&self) -> ActorResult<oneshot::Receiver<ActorResult<()>>> {
        let _admission = self.lock_admission();
        self.claim_transition(ConnectionState::Rpc)?;

        let (reply, receiver) = oneshot::channel();
        let command = ActorCommand::StartScreenStream(StartScreenStreamRequest { reply });
        if let Err(error) = self.commands.try_send(command) {
            self.revert_transition();
            return Err(map_send_error(error));
        }
        Ok(receiver)
    }

    fn submit_screen_input(
        &self,
        key: i32,
        input_type: i32,
    ) -> ActorResult<oneshot::Receiver<ActorResult<()>>> {
        if pb_gui::InputKey::try_from(key).is_err() {
            return Err(ConnectionActorError::InvalidScreenInputKey(key));
        }
        if pb_gui::InputType::try_from(input_type).is_err() {
            return Err(ConnectionActorError::InvalidScreenInputType(input_type));
        }

        let _admission = self.lock_admission();
        let current = self.state();
        if current != ConnectionState::ScreenStreaming {
            return Err(mode_error(current));
        }

        let (reply, receiver) = oneshot::channel();
        self.commands
            .try_send(ActorCommand::ScreenInput(ScreenInputRequest {
                key,
                input_type,
                reply,
            }))
            .map_err(map_send_error)?;
        Ok(receiver)
    }

    fn submit_stop_screen_stream(&self) -> ActorResult<oneshot::Receiver<ActorResult<()>>> {
        let _admission = self.lock_admission();
        self.claim_transition(ConnectionState::ScreenStreaming)?;

        let (reply, receiver) = oneshot::channel();
        let command = ActorCommand::StopScreenStream(StopScreenStreamRequest { reply });
        if let Err(error) = self.commands.try_send(command) {
            self.revert_transition();
            return Err(map_send_error(error));
        }
        Ok(receiver)
    }

    fn submit_enter_cli(&self) -> ActorResult<oneshot::Receiver<ActorResult<()>>> {
        // Physical capability rejection must precede every admission or wire
        // mutation so BLE callers cannot perturb an otherwise healthy RPC
        // connection.
        if self.transport_kind != TransportKind::Serial {
            return Err(ConnectionActorError::CliRequiresSerial);
        }

        let _admission = self.lock_admission();
        self.claim_transition(ConnectionState::Rpc)?;

        let (reply, receiver) = oneshot::channel();
        if let Err(error) = self
            .commands
            .try_send(ActorCommand::EnterCli(EnterCliRequest { reply }))
        {
            self.revert_transition();
            return Err(map_send_error(error));
        }
        Ok(receiver)
    }

    fn submit_cli_send(&self, command: &str) -> ActorResult<oneshot::Receiver<ActorResult<()>>> {
        validate_cli_command(command)?;
        let _admission = self.lock_admission();
        let current = self.state();
        if current != ConnectionState::Cli {
            return Err(mode_error(current));
        }

        let (reply, receiver) = oneshot::channel();
        self.commands
            .try_send(ActorCommand::CliSend(CliSendRequest {
                command: command.to_owned(),
                reply,
            }))
            .map_err(map_send_error)?;
        Ok(receiver)
    }

    fn submit_cli_interrupt(&self) -> ActorResult<oneshot::Receiver<ActorResult<()>>> {
        let _admission = self.lock_admission();
        let current = self.state();
        if current != ConnectionState::Cli {
            return Err(mode_error(current));
        }

        let (reply, receiver) = oneshot::channel();
        self.commands
            .try_send(ActorCommand::CliInterrupt(CliInterruptRequest { reply }))
            .map_err(map_send_error)?;
        Ok(receiver)
    }

    fn submit_exit_cli(&self) -> ActorResult<oneshot::Receiver<ActorResult<()>>> {
        let _admission = self.lock_admission();
        self.claim_transition(ConnectionState::Cli)?;

        let (reply, receiver) = oneshot::channel();
        if let Err(error) = self
            .commands
            .try_send(ActorCommand::ExitCli(ExitCliRequest { reply }))
        {
            self.revert_transition();
            return Err(map_send_error(error));
        }
        Ok(receiver)
    }

    fn lock_admission(&self) -> std::sync::MutexGuard<'_, ()> {
        lock_admission_gate(&self.admission_gate)
    }

    fn claim_transition(&self, expected: ConnectionState) -> ActorResult<()> {
        self.admission_state
            .compare_exchange(
                expected.as_byte(),
                ConnectionState::Transitioning.as_byte(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|current| mode_error(ConnectionState::from_byte(current)))
    }

    fn revert_transition(&self) {
        let target = self.state.load(Ordering::Acquire);
        let _ = self.admission_state.compare_exchange(
            ConnectionState::Transitioning.as_byte(),
            target,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// Atomically close admission, reject queued work, drop the client on its
    /// owner thread, and wait for completion.
    ///
    /// Concurrent calls share the same completion signal and are idempotent.
    pub fn shutdown(&self) -> impl std::future::Future<Output = ActorResult<()>> + Send + 'static {
        let mut completion = self.shutdown_complete.subscribe();
        // Closing admission and attempting to enqueue the wake-up command are
        // serialized with submit_rpc. The guard is intentionally released
        // before the returned future can wait.
        let admission = match self.admission_gate.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::warn!("connection admission gate was poisoned; recovering");
                poisoned.into_inner()
            }
        };

        let wait_for_completion = loop {
            let current = self.admission_state.load(Ordering::Acquire);
            let current_state = ConnectionState::from_byte(current);
            match current_state {
                ConnectionState::Disconnected => break false,
                ConnectionState::ShuttingDown => break true,
                ConnectionState::Rpc
                | ConnectionState::ScreenStreaming
                | ConnectionState::Cli
                | ConnectionState::Transitioning => {
                    if self
                        .admission_state
                        .compare_exchange(
                            current,
                            ConnectionState::ShuttingDown.as_byte(),
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        // A full queue already contains a wake-up command. The
                        // actor checks the shutdown state before executing it.
                        // A closed queue is finalized by the actor wrapper.
                        let _ = self.commands.try_send(ActorCommand::Shutdown);
                        break true;
                    }
                }
            }
        };
        drop(admission);

        async move {
            if !wait_for_completion {
                return Ok(());
            }

            while !*completion.borrow_and_update() {
                completion
                    .changed()
                    .await
                    .map_err(|_| ConnectionActorError::ActorStopped)?;
            }
            Ok(())
        }
    }

    fn admission_error(&self) -> Option<ConnectionActorError> {
        match self.state() {
            ConnectionState::Rpc => None,
            current @ (ConnectionState::ScreenStreaming
            | ConnectionState::Cli
            | ConnectionState::Transitioning) => {
                Some(ConnectionActorError::ModeRejected { current })
            }
            ConnectionState::Disconnected | ConnectionState::ShuttingDown => {
                Some(ConnectionActorError::Closed)
            }
        }
    }
}

fn lock_admission_gate(gate: &Mutex<()>) -> std::sync::MutexGuard<'_, ()> {
    match gate.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!("connection admission gate was poisoned; recovering");
            poisoned.into_inner()
        }
    }
}

fn mode_error(current: ConnectionState) -> ConnectionActorError {
    match current {
        ConnectionState::Disconnected | ConnectionState::ShuttingDown => {
            ConnectionActorError::Closed
        }
        current => ConnectionActorError::ModeRejected { current },
    }
}

fn map_send_error(error: mpsc::error::TrySendError<ActorCommand>) -> ConnectionActorError {
    match error {
        mpsc::error::TrySendError::Full(_) => ConnectionActorError::QueueFull,
        mpsc::error::TrySendError::Closed(_) => ConnectionActorError::Closed,
    }
}

fn validate_cli_command(command: &str) -> ActorResult<()> {
    let bytes = command.as_bytes();
    if bytes.len() > MAX_CLI_COMMAND_BYTES {
        return Err(ConnectionActorError::CliCommandBytesExceeded {
            actual: bytes.len(),
            limit: MAX_CLI_COMMAND_BYTES,
        });
    }
    if bytes
        .iter()
        .any(|byte| matches!(*byte, b'\r' | b'\n' | b'\0'))
    {
        return Err(ConnectionActorError::InvalidCliCommand);
    }
    Ok(())
}

enum ActorExit {
    ChannelClosed,
    Shutdown,
    Fatal {
        cause: Arc<str>,
        reply: Box<dyn FnOnce()>,
    },
}

enum PendingRequest {
    Legacy(Box<dyn RpcJob>),
    Proto(ProtoRpcRequest),
    StartScreen(StartScreenStreamRequest),
    ScreenInput(ScreenInputRequest),
    StopScreen(StopScreenStreamRequest),
    EnterCli(EnterCliRequest),
    CliSend(CliSendRequest),
    CliInterrupt(CliInterruptRequest),
    ExitCli(ExitCliRequest),
}

#[allow(clippy::too_many_arguments)]
fn actor_thread(
    client: FlipperClient,
    commands: mpsc::Receiver<ActorCommand>,
    state: Arc<AtomicU8>,
    admission_state: Arc<AtomicU8>,
    admission_gate: Arc<Mutex<()>>,
    screen_terminal_attempts: Arc<AtomicU64>,
    shutdown_complete: watch::Sender<bool>,
    screen_frames: watch::Sender<Option<pb::Main>>,
    cli_output: broadcast::Sender<CliOutputEvent>,
    rpc_limits: RpcDispatchLimits,
) {
    let mut client = Some(client);
    let mut commands = commands;
    let mut pending = Vec::new();
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        run_actor(
            &mut client,
            &mut commands,
            &state,
            &admission_state,
            &admission_gate,
            &screen_terminal_attempts,
            &mut pending,
            &screen_frames,
            &cli_output,
            rpc_limits,
        )
    }));

    state.store(ConnectionState::ShuttingDown.as_byte(), Ordering::Release);
    admission_state.store(ConnectionState::ShuttingDown.as_byte(), Ordering::Release);
    commands.close();

    let (rejection, fatal_reply) = match outcome {
        Ok(ActorExit::Shutdown) => (PendingRejection::Closed, None),
        Ok(ActorExit::Fatal { cause, reply }) => {
            (PendingRejection::ConnectionLost(cause), Some(reply))
        }
        Ok(ActorExit::ChannelClosed) => (PendingRejection::ActorStopped, None),
        Err(_) => {
            tracing::error!("connection actor panicked; forcing disconnect");
            (PendingRejection::ActorStopped, None)
        }
    };

    drain_pending(&mut commands, &mut pending);
    if panic::catch_unwind(AssertUnwindSafe(|| drop(client.take()))).is_err() {
        tracing::error!("Flipper client panicked while being dropped");
    }
    state.store(ConnectionState::Disconnected.as_byte(), Ordering::Release);
    admission_state.store(ConnectionState::Disconnected.as_byte(), Ordering::Release);
    // The actor is the sole Sender owner. Close all frame subscriptions before
    // any pending/fatal result can become observable.
    drop(screen_frames);
    drop(cli_output);

    // No shutdown-, panic-, or fatal-path reply becomes observable until the
    // actor has relinquished the transport, closed all subscriptions, and
    // reports itself disconnected.
    reject_pending(pending, rejection);

    let _ = shutdown_complete.send(true);

    // Fatal results are deliberately the final publication: observers cannot
    // receive one while the actor still admits work or owns the transport.
    if let Some(reply) = fatal_reply {
        reply();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_actor(
    client: &mut Option<FlipperClient>,
    commands: &mut mpsc::Receiver<ActorCommand>,
    state: &AtomicU8,
    admission_state: &AtomicU8,
    admission_gate: &Mutex<()>,
    screen_terminal_attempts: &AtomicU64,
    pending: &mut Vec<PendingRequest>,
    screen_frames: &watch::Sender<Option<pb::Main>>,
    cli_output: &broadcast::Sender<CliOutputEvent>,
    rpc_limits: RpcDispatchLimits,
) -> ActorExit {
    let mut next_cli_session_id = 1u64;
    while let Some(command) = commands.blocking_recv() {
        if ConnectionState::from_byte(admission_state.load(Ordering::Acquire))
            == ConnectionState::ShuttingDown
        {
            match command {
                ActorCommand::RunRpc(job) => pending.push(PendingRequest::Legacy(job)),
                ActorCommand::ProtoRpc(request) => {
                    pending.push(PendingRequest::Proto(request));
                }
                ActorCommand::StartScreenStream(request) => {
                    pending.push(PendingRequest::StartScreen(request));
                }
                ActorCommand::ScreenInput(request) => {
                    pending.push(PendingRequest::ScreenInput(request));
                }
                ActorCommand::StopScreenStream(request) => {
                    pending.push(PendingRequest::StopScreen(request));
                }
                ActorCommand::EnterCli(request) => {
                    pending.push(PendingRequest::EnterCli(request));
                }
                ActorCommand::CliSend(request) => {
                    pending.push(PendingRequest::CliSend(request));
                }
                ActorCommand::CliInterrupt(request) => {
                    pending.push(PendingRequest::CliInterrupt(request));
                }
                ActorCommand::ExitCli(request) => {
                    pending.push(PendingRequest::ExitCli(request));
                }
                ActorCommand::Shutdown => {}
                #[cfg(test)]
                ActorCommand::ForcePanic => {}
            }
            return ActorExit::Shutdown;
        }

        match command {
            ActorCommand::RunRpc(job) => {
                let current = ConnectionState::from_byte(state.load(Ordering::Acquire));
                if current != ConnectionState::Rpc {
                    job.reject(ConnectionActorError::ModeRejected { current });
                    continue;
                }

                let Some(client) = client.as_mut() else {
                    job.reject(ConnectionActorError::ActorStopped);
                    return ActorExit::ChannelClosed;
                };
                match job.execute(client) {
                    JobDisposition::Continue => {}
                    JobDisposition::Fatal { cause, reply } => {
                        return ActorExit::Fatal { cause, reply };
                    }
                }
            }
            ActorCommand::ProtoRpc(request) => {
                let current = ConnectionState::from_byte(state.load(Ordering::Acquire));
                if current != ConnectionState::Rpc {
                    request.reject(ConnectionActorError::ModeRejected { current });
                    continue;
                }

                let Some(client) = client.as_mut() else {
                    request.reject(ConnectionActorError::ActorStopped);
                    return ActorExit::ChannelClosed;
                };
                match execute_proto_rpc(client, request, screen_frames, rpc_limits) {
                    JobDisposition::Continue => {}
                    JobDisposition::Fatal { cause, reply } => {
                        return ActorExit::Fatal { cause, reply };
                    }
                }
            }
            ActorCommand::StartScreenStream(request) => {
                let current = ConnectionState::from_byte(state.load(Ordering::Acquire));
                let admission = ConnectionState::from_byte(admission_state.load(Ordering::Acquire));
                if current != ConnectionState::Rpc || admission != ConnectionState::Transitioning {
                    request.reject(mode_error(current));
                    let _ = admission_state.compare_exchange(
                        ConnectionState::Transitioning.as_byte(),
                        current.as_byte(),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    continue;
                }
                if state
                    .compare_exchange(
                        ConnectionState::Rpc.as_byte(),
                        ConnectionState::Transitioning.as_byte(),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    request.reject(mode_error(ConnectionState::from_byte(
                        state.load(Ordering::Acquire),
                    )));
                    continue;
                }

                let Some(client) = client.as_mut() else {
                    request.reject(ConnectionActorError::ActorStopped);
                    return ActorExit::ChannelClosed;
                };
                match execute_start_screen_stream(
                    client,
                    request,
                    state,
                    admission_state,
                    screen_frames,
                    rpc_limits,
                ) {
                    StartDisposition::Run { command_id, reply } => {
                        if state
                            .compare_exchange(
                                ConnectionState::Transitioning.as_byte(),
                                ConnectionState::ScreenStreaming.as_byte(),
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_err()
                        {
                            pending.push(PendingRequest::StartScreen(StartScreenStreamRequest {
                                reply,
                            }));
                            return ActorExit::Shutdown;
                        }
                        if admission_state
                            .compare_exchange(
                                ConnectionState::Transitioning.as_byte(),
                                ConnectionState::ScreenStreaming.as_byte(),
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_err()
                        {
                            pending.push(PendingRequest::StartScreen(StartScreenStreamRequest {
                                reply,
                            }));
                            return ActorExit::Shutdown;
                        }
                        let _ = reply.send(Ok(()));
                        match run_screen_stream(
                            client,
                            commands,
                            state,
                            admission_state,
                            admission_gate,
                            screen_terminal_attempts,
                            pending,
                            screen_frames,
                            command_id,
                            rpc_limits,
                        ) {
                            ScreenLoopExit::Rpc => {}
                            ScreenLoopExit::Actor(exit) => return exit,
                        }
                    }
                    StartDisposition::Continue => {}
                    StartDisposition::Shutdown(reply) => {
                        pending.push(PendingRequest::StartScreen(StartScreenStreamRequest {
                            reply,
                        }));
                        return ActorExit::Shutdown;
                    }
                    StartDisposition::Actor(exit) => return exit,
                }
            }
            ActorCommand::ScreenInput(request) => {
                let current = ConnectionState::from_byte(state.load(Ordering::Acquire));
                request.reject(ConnectionActorError::ModeRejected { current });
            }
            ActorCommand::StopScreenStream(request) => {
                let current = ConnectionState::from_byte(state.load(Ordering::Acquire));
                request.reject(ConnectionActorError::ModeRejected { current });
            }
            ActorCommand::EnterCli(request) => {
                let current = ConnectionState::from_byte(state.load(Ordering::Acquire));
                let admission = ConnectionState::from_byte(admission_state.load(Ordering::Acquire));
                if current != ConnectionState::Rpc || admission != ConnectionState::Transitioning {
                    request.reject(mode_error(current));
                    let _ = admission_state.compare_exchange(
                        ConnectionState::Transitioning.as_byte(),
                        current.as_byte(),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    continue;
                }
                if state
                    .compare_exchange(
                        ConnectionState::Rpc.as_byte(),
                        ConnectionState::Transitioning.as_byte(),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    request.reject(mode_error(ConnectionState::from_byte(
                        state.load(Ordering::Acquire),
                    )));
                    continue;
                }

                let Some(client) = client.as_mut() else {
                    request.reject(ConnectionActorError::ActorStopped);
                    return ActorExit::ChannelClosed;
                };
                let session_id = next_cli_session_id;
                next_cli_session_id = next_cli_session_id.wrapping_add(1).max(1);
                match execute_enter_cli(
                    client,
                    request,
                    commands,
                    state,
                    admission_state,
                    pending,
                    cli_output,
                    session_id,
                    rpc_limits,
                ) {
                    CliLoopExit::Rpc => {}
                    CliLoopExit::Actor(exit) => return exit,
                }
            }
            ActorCommand::CliSend(request) => {
                let current = ConnectionState::from_byte(state.load(Ordering::Acquire));
                request.reject(ConnectionActorError::ModeRejected { current });
            }
            ActorCommand::CliInterrupt(request) => {
                let current = ConnectionState::from_byte(state.load(Ordering::Acquire));
                request.reject(ConnectionActorError::ModeRejected { current });
            }
            ActorCommand::ExitCli(request) => {
                let current = ConnectionState::from_byte(state.load(Ordering::Acquire));
                request.reject(ConnectionActorError::ModeRejected { current });
            }
            ActorCommand::Shutdown => return ActorExit::Shutdown,
            #[cfg(test)]
            ActorCommand::ForcePanic => panic!("forced actor-thread panic"),
        }
    }

    ActorExit::ChannelClosed
}

enum EnterCliExecution {
    Established,
    Shutdown,
    Fatal(ConnectionActorError),
}

enum CliLoopExit {
    Rpc,
    Actor(ActorExit),
}

#[allow(clippy::too_many_arguments)]
fn execute_enter_cli(
    client: &mut FlipperClient,
    request: EnterCliRequest,
    commands: &mut mpsc::Receiver<ActorCommand>,
    state: &AtomicU8,
    admission_state: &AtomicU8,
    pending: &mut Vec<PendingRequest>,
    cli_output: &broadcast::Sender<CliOutputEvent>,
    session_id: u64,
    limits: RpcDispatchLimits,
) -> CliLoopExit {
    let EnterCliRequest { reply } = request;

    // Keep this defensive check actor-side as well as handle-side. It occurs
    // before StopSession, so an impossible/malformed BLE command remains a
    // recoverable admission error with no wire mutation.
    if client.transport.kind() != TransportKind::Serial {
        state.store(ConnectionState::Rpc.as_byte(), Ordering::Release);
        let _ = admission_state.compare_exchange(
            ConnectionState::Transitioning.as_byte(),
            ConnectionState::Rpc.as_byte(),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        let _ = reply.send(Err(ConnectionActorError::CliRequiresSerial));
        return CliLoopExit::Rpc;
    }

    let execution = panic::catch_unwind(AssertUnwindSafe(|| {
        perform_enter_cli(client, admission_state, limits)
    }));
    match execution {
        Err(_) => {
            return CliLoopExit::Actor(ActorExit::Fatal {
                cause: Arc::<str>::from("actor-owned CLI entry panicked"),
                reply: Box::new(move || {
                    let _ = reply.send(Err(ConnectionActorError::JobPanicked));
                }),
            });
        }
        Ok(EnterCliExecution::Shutdown) => {
            pending.push(PendingRequest::EnterCli(EnterCliRequest { reply }));
            return CliLoopExit::Actor(ActorExit::Shutdown);
        }
        Ok(EnterCliExecution::Fatal(error)) => {
            return CliLoopExit::Actor(fatal_unit_exit(Err(error), reply));
        }
        Ok(EnterCliExecution::Established) => {}
    }

    if state
        .compare_exchange(
            ConnectionState::Transitioning.as_byte(),
            ConnectionState::Cli.as_byte(),
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
        || admission_state
            .compare_exchange(
                ConnectionState::Transitioning.as_byte(),
                ConnectionState::Cli.as_byte(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
    {
        pending.push(PendingRequest::EnterCli(EnterCliRequest { reply }));
        return CliLoopExit::Actor(ActorExit::Shutdown);
    }

    let _ = cli_output.send(CliOutputEvent::SessionStarted { session_id });
    let _ = reply.send(Ok(()));
    run_cli_loop(
        client,
        commands,
        state,
        admission_state,
        pending,
        cli_output,
        session_id,
        limits,
    )
}

fn perform_enter_cli(
    client: &mut FlipperClient,
    admission_state: &AtomicU8,
    limits: RpcDispatchLimits,
) -> EnterCliExecution {
    let command_id = client.next_command_id();
    let request = pb::Main {
        command_id,
        command_status: 0,
        has_next: false,
        content: Some(Content::StopSession(pb::StopSession {})),
    };
    if let Err(error) = write_message(client.transport.as_mut(), &request) {
        return EnterCliExecution::Fatal(ConnectionActorError::Device(error));
    }

    let started = Instant::now();
    let deadline = started
        .checked_add(limits.response_deadline)
        .unwrap_or(started);
    loop {
        if ConnectionState::from_byte(admission_state.load(Ordering::Acquire))
            == ConnectionState::ShuttingDown
        {
            return EnterCliExecution::Shutdown;
        }
        let message =
            match read_message_until(client.transport.as_mut(), deadline, CLI_READ_TIMEOUT) {
                Ok(message) => message,
                Err(DeadlineReadError::DeadlineElapsed) => {
                    return EnterCliExecution::Fatal(ConnectionActorError::Protocol(
                        ConnectionProtocolError::ResponseReadDeadlineExceeded {
                            command_id,
                            deadline_ms: duration_millis_u64(limits.response_deadline),
                        },
                    ));
                }
                Err(DeadlineReadError::Flipper(FlipperError::Io(error)))
                    if is_cli_timeout_error(&error) =>
                {
                    continue;
                }
                Err(DeadlineReadError::Flipper(error)) => {
                    return EnterCliExecution::Fatal(ConnectionActorError::Device(error));
                }
            };

        if message.command_id != command_id {
            return EnterCliExecution::Fatal(foreign_id(command_id, message.command_id));
        }
        if message.command_status != 0
            || message.has_next
            || !matches!(message.content, Some(Content::Empty(_)))
        {
            return EnterCliExecution::Fatal(ConnectionActorError::Protocol(
                ConnectionProtocolError::UnexpectedCliStopResponse { command_id },
            ));
        }
        if let Err(error) = client.transport.set_timeout(CLI_READ_TIMEOUT) {
            return EnterCliExecution::Fatal(ConnectionActorError::Device(error.into()));
        }
        return EnterCliExecution::Established;
    }
}

#[allow(clippy::too_many_arguments)]
fn run_cli_loop(
    client: &mut FlipperClient,
    commands: &mut mpsc::Receiver<ActorCommand>,
    state: &AtomicU8,
    admission_state: &AtomicU8,
    pending: &mut Vec<PendingRequest>,
    cli_output: &broadcast::Sender<CliOutputEvent>,
    session_id: u64,
    limits: RpcDispatchLimits,
) -> CliLoopExit {
    loop {
        if ConnectionState::from_byte(admission_state.load(Ordering::Acquire))
            == ConnectionState::ShuttingDown
        {
            return CliLoopExit::Actor(ActorExit::Shutdown);
        }

        match commands.try_recv() {
            Ok(ActorCommand::CliSend(request)) => {
                if let Err(error) = validate_cli_command(&request.command) {
                    request.reject(error);
                } else {
                    let result = write_cli_command(client, &request.command);
                    match result {
                        Ok(()) => {
                            let _ = request.reply.send(Ok(()));
                        }
                        Err(error) => {
                            return CliLoopExit::Actor(fatal_unit_exit(Err(error), request.reply));
                        }
                    }
                }
            }
            Ok(ActorCommand::CliInterrupt(request)) => match write_cli_interrupt(client) {
                Ok(()) => {
                    let _ = request.reply.send(Ok(()));
                }
                Err(error) => {
                    return CliLoopExit::Actor(fatal_unit_exit(Err(error), request.reply));
                }
            },
            Ok(ActorCommand::ExitCli(request)) => {
                let current = ConnectionState::from_byte(state.load(Ordering::Acquire));
                let admission = ConnectionState::from_byte(admission_state.load(Ordering::Acquire));
                if current != ConnectionState::Cli || admission != ConnectionState::Transitioning {
                    request.reject(mode_error(current));
                } else if state
                    .compare_exchange(
                        ConnectionState::Cli.as_byte(),
                        ConnectionState::Transitioning.as_byte(),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    request.reject(mode_error(ConnectionState::from_byte(
                        state.load(Ordering::Acquire),
                    )));
                } else {
                    match perform_exit_cli(client, admission_state, cli_output, session_id, limits)
                    {
                        ExitCliExecution::Established => {
                            if state
                                .compare_exchange(
                                    ConnectionState::Transitioning.as_byte(),
                                    ConnectionState::Rpc.as_byte(),
                                    Ordering::AcqRel,
                                    Ordering::Acquire,
                                )
                                .is_err()
                                || admission_state
                                    .compare_exchange(
                                        ConnectionState::Transitioning.as_byte(),
                                        ConnectionState::Rpc.as_byte(),
                                        Ordering::AcqRel,
                                        Ordering::Acquire,
                                    )
                                    .is_err()
                            {
                                pending.push(PendingRequest::ExitCli(request));
                                return CliLoopExit::Actor(ActorExit::Shutdown);
                            }
                            let _ = cli_output.send(CliOutputEvent::SessionEnded { session_id });
                            let _ = request.reply.send(Ok(()));
                            return CliLoopExit::Rpc;
                        }
                        ExitCliExecution::Shutdown => {
                            pending.push(PendingRequest::ExitCli(request));
                            return CliLoopExit::Actor(ActorExit::Shutdown);
                        }
                        ExitCliExecution::Fatal(error) => {
                            return CliLoopExit::Actor(fatal_unit_exit(Err(error), request.reply));
                        }
                    }
                }
            }
            Ok(ActorCommand::Shutdown) => return CliLoopExit::Actor(ActorExit::Shutdown),
            Ok(ActorCommand::RunRpc(job)) => {
                job.reject(ConnectionActorError::ModeRejected {
                    current: ConnectionState::Cli,
                });
            }
            Ok(ActorCommand::ProtoRpc(request)) => {
                request.reject(ConnectionActorError::ModeRejected {
                    current: ConnectionState::Cli,
                });
            }
            Ok(ActorCommand::StartScreenStream(request)) => {
                request.reject(ConnectionActorError::ModeRejected {
                    current: ConnectionState::Cli,
                });
            }
            Ok(ActorCommand::ScreenInput(request)) => {
                request.reject(ConnectionActorError::ModeRejected {
                    current: ConnectionState::Cli,
                });
            }
            Ok(ActorCommand::StopScreenStream(request)) => {
                request.reject(ConnectionActorError::ModeRejected {
                    current: ConnectionState::Cli,
                });
            }
            Ok(ActorCommand::EnterCli(request)) => {
                request.reject(ConnectionActorError::ModeRejected {
                    current: ConnectionState::Cli,
                });
            }
            #[cfg(test)]
            Ok(ActorCommand::ForcePanic) => panic!("forced actor-thread panic"),
            Err(mpsc::error::TryRecvError::Empty) => {}
            Err(mpsc::error::TryRecvError::Disconnected) => {
                return CliLoopExit::Actor(ActorExit::ChannelClosed);
            }
        }

        // Exactly one bounded read after at most one queued command keeps both
        // output delivery and control (especially ETX) from starving.
        let deadline = Instant::now()
            .checked_add(CLI_READ_TIMEOUT)
            .unwrap_or_else(Instant::now);
        match read_cli_chunk_until(client, deadline) {
            Ok(Some(bytes)) => route_cli_output(cli_output, session_id, bytes),
            Ok(None) => {}
            Err(error) => return CliLoopExit::Actor(fatal_without_reply(error)),
        }
    }
}

fn write_cli_command(client: &mut FlipperClient, command: &str) -> ActorResult<()> {
    client
        .transport
        .write_all(command.as_bytes())
        .map_err(|error| ConnectionActorError::Device(error.into()))?;
    client
        .transport
        .write_all(b"\r")
        .map_err(|error| ConnectionActorError::Device(error.into()))?;
    client
        .transport
        .flush()
        .map_err(|error| ConnectionActorError::Device(error.into()))?;
    Ok(())
}

fn write_cli_interrupt(client: &mut FlipperClient) -> ActorResult<()> {
    client
        .transport
        .write_all(CLI_INTERRUPT)
        .map_err(|error| ConnectionActorError::Device(error.into()))?;
    client
        .transport
        .flush()
        .map_err(|error| ConnectionActorError::Device(error.into()))?;
    Ok(())
}

enum ExitCliExecution {
    Established,
    Shutdown,
    Fatal(ConnectionActorError),
}

struct CliHandoffMatcher {
    /// Bytes that are still an exact prefix of the handoff marker. This never
    /// grows beyond the marker itself; failed candidates are emitted as soon
    /// as they can no longer complete.
    candidate: Vec<u8>,
}

enum CliHandoffProgress {
    Pending { routed: Vec<u8> },
    Complete { routed: Vec<u8>, trailing: Vec<u8> },
}

impl CliHandoffMatcher {
    fn new() -> Self {
        Self {
            candidate: Vec::with_capacity(CLI_RPC_HANDOFF_MARKER.len()),
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> CliHandoffProgress {
        debug_assert!(bytes.len() <= CLI_READ_CHUNK_BYTES);
        let mut routed = Vec::with_capacity(bytes.len() + self.candidate.len());
        for (index, byte) in bytes.iter().copied().enumerate() {
            self.candidate.push(byte);
            while !CLI_RPC_HANDOFF_MARKER.starts_with(&self.candidate) {
                routed.push(self.candidate.remove(0));
            }
            if self.candidate.len() == CLI_RPC_HANDOFF_MARKER.len() {
                self.candidate.clear();
                return CliHandoffProgress::Complete {
                    routed,
                    trailing: bytes[index + 1..].to_vec(),
                };
            }
        }
        debug_assert!(self.candidate.len() < CLI_RPC_HANDOFF_MARKER.len());
        CliHandoffProgress::Pending { routed }
    }

    fn take_candidate(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.candidate)
    }
}

fn perform_exit_cli(
    client: &mut FlipperClient,
    admission_state: &AtomicU8,
    cli_output: &broadcast::Sender<CliOutputEvent>,
    session_id: u64,
    limits: RpcDispatchLimits,
) -> ExitCliExecution {
    let drain_started = Instant::now();
    let drain_deadline = drain_started
        .checked_add(CLI_PRE_EXIT_DRAIN_DEADLINE)
        .unwrap_or(drain_started);
    loop {
        if is_shutting_down(admission_state) {
            return ExitCliExecution::Shutdown;
        }
        match read_cli_chunk_until(client, drain_deadline) {
            Ok(Some(bytes)) => route_cli_output(cli_output, session_id, bytes),
            Ok(None) => break,
            Err(error) => return ExitCliExecution::Fatal(error),
        }
    }

    let started = Instant::now();
    let deadline = started
        .checked_add(limits.response_deadline)
        .unwrap_or(started);
    let Some(write_budget) = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
    else {
        return ExitCliExecution::Fatal(ConnectionActorError::Protocol(
            ConnectionProtocolError::CliHandoffDeadlineExceeded {
                deadline_ms: duration_millis_u64(limits.response_deadline),
            },
        ));
    };
    // The pre-drain deliberately used short read slices. Do not let its last
    // near-zero timeout govern the wire-changing write/flush: reset writes to
    // the remaining absolute handoff budget, capped at the normal transport
    // timeout, without creating a second deadline.
    if let Err(error) = client
        .transport
        .set_timeout(write_budget.min(SERIAL_TIMEOUT_NORMAL))
    {
        return ExitCliExecution::Fatal(ConnectionActorError::Device(error.into()));
    }
    if let Err(error) = client.transport.write_all(CLI_START_RPC_SESSION) {
        return ExitCliExecution::Fatal(ConnectionActorError::Device(error.into()));
    }
    if let Err(error) = client.transport.flush() {
        return ExitCliExecution::Fatal(ConnectionActorError::Device(error.into()));
    }

    // Firmware echoes the exact command plus LF immediately before handing
    // pipe ownership to RPC. Arbitrary newlines and prompts are ordinary CLI
    // output and cannot start the recovery ping.
    let mut matcher = CliHandoffMatcher::new();
    loop {
        if is_shutting_down(admission_state) {
            route_cli_output(cli_output, session_id, matcher.take_candidate());
            return ExitCliExecution::Shutdown;
        }
        let bytes = match read_cli_chunk_until(client, deadline) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                if Instant::now() >= deadline {
                    route_cli_output(cli_output, session_id, matcher.take_candidate());
                    return ExitCliExecution::Fatal(ConnectionActorError::Protocol(
                        ConnectionProtocolError::CliHandoffDeadlineExceeded {
                            deadline_ms: duration_millis_u64(limits.response_deadline),
                        },
                    ));
                }
                continue;
            }
            Err(error) => {
                route_cli_output(cli_output, session_id, matcher.take_candidate());
                return ExitCliExecution::Fatal(error);
            }
        };
        match matcher.feed(&bytes) {
            CliHandoffProgress::Pending { routed } => {
                route_cli_output(cli_output, session_id, routed);
            }
            CliHandoffProgress::Complete { routed, trailing } => {
                route_cli_output(cli_output, session_id, routed);
                if !trailing.is_empty() {
                    client.transport.unread(&trailing);
                }
                break;
            }
        }
    }

    if let Err(error) = client.transport.set_timeout(SERIAL_TIMEOUT_NORMAL) {
        return ExitCliExecution::Fatal(ConnectionActorError::Device(error.into()));
    }

    let command_id = client.next_command_id();
    let ping = pb::Main {
        command_id,
        command_status: 0,
        has_next: false,
        content: Some(Content::SystemPingRequest(pb_system::PingRequest {
            data: CLI_PING_PAYLOAD.to_vec(),
        })),
    };
    if let Err(error) = write_message(client.transport.as_mut(), &ping) {
        return ExitCliExecution::Fatal(ConnectionActorError::Device(error));
    }

    let response = loop {
        if is_shutting_down(admission_state) {
            return ExitCliExecution::Shutdown;
        }
        match read_message_until(client.transport.as_mut(), deadline, CLI_READ_TIMEOUT) {
            Ok(response) => break response,
            Err(DeadlineReadError::DeadlineElapsed) => {
                return ExitCliExecution::Fatal(ConnectionActorError::Protocol(
                    ConnectionProtocolError::ResponseReadDeadlineExceeded {
                        command_id,
                        deadline_ms: duration_millis_u64(limits.response_deadline),
                    },
                ));
            }
            Err(DeadlineReadError::Flipper(FlipperError::Io(error)))
                if is_cli_timeout_error(&error) =>
            {
                continue;
            }
            Err(DeadlineReadError::Flipper(error)) => {
                return ExitCliExecution::Fatal(ConnectionActorError::Device(error));
            }
        }
    };

    if response.command_id != command_id {
        return ExitCliExecution::Fatal(foreign_id(command_id, response.command_id));
    }
    let valid_ping = response.command_status == 0
        && !response.has_next
        && matches!(
            response.content,
            Some(Content::SystemPingResponse(pb_system::PingResponse { ref data }))
                if data.as_slice() == CLI_PING_PAYLOAD
        );
    if !valid_ping {
        return ExitCliExecution::Fatal(ConnectionActorError::Protocol(
            ConnectionProtocolError::UnexpectedCliPingResponse { command_id },
        ));
    }
    if let Err(error) = client.transport.set_timeout(SERIAL_TIMEOUT_NORMAL) {
        return ExitCliExecution::Fatal(ConnectionActorError::Device(error.into()));
    }
    ExitCliExecution::Established
}

fn read_cli_chunk_until(
    client: &mut FlipperClient,
    deadline: Instant,
) -> ActorResult<Option<Vec<u8>>> {
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return Ok(None);
    };
    if remaining.is_zero() {
        return Ok(None);
    }
    client
        .transport
        .set_timeout(remaining.min(CLI_READ_TIMEOUT))
        .map_err(|error| ConnectionActorError::Device(error.into()))?;
    let mut buffer = [0u8; CLI_READ_CHUNK_BYTES];
    match client.transport.read(&mut buffer) {
        Ok(0) => Err(ConnectionActorError::Device(
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "CLI transport returned zero bytes",
            )
            .into(),
        )),
        Ok(read) => Ok(Some(buffer[..read].to_vec())),
        Err(error) if is_cli_timeout_error(&error) => Ok(None),
        Err(error) => Err(ConnectionActorError::Device(error.into())),
    }
}

fn route_cli_output(
    cli_output: &broadcast::Sender<CliOutputEvent>,
    session_id: u64,
    bytes: Vec<u8>,
) {
    for chunk in bytes.chunks(CLI_READ_CHUNK_BYTES) {
        let _ = cli_output.send(CliOutputEvent::Data {
            session_id,
            bytes: chunk.to_vec(),
        });
    }
}

fn is_shutting_down(admission_state: &AtomicU8) -> bool {
    ConnectionState::from_byte(admission_state.load(Ordering::Acquire))
        == ConnectionState::ShuttingDown
}

fn is_cli_timeout_error(error: &io::Error) -> bool {
    is_timeout_kind(error.kind()) || error.raw_os_error() == Some(121)
}

enum StartExecution {
    Established(u32),
    Resolved(ActorResult<()>),
    Fatal(ActorResult<()>),
    Shutdown,
}

enum StartDisposition {
    Run {
        command_id: u32,
        reply: oneshot::Sender<ActorResult<()>>,
    },
    Continue,
    Shutdown(oneshot::Sender<ActorResult<()>>),
    Actor(ActorExit),
}

fn execute_start_screen_stream(
    client: &mut FlipperClient,
    request: StartScreenStreamRequest,
    state: &AtomicU8,
    admission_state: &AtomicU8,
    screen_frames: &watch::Sender<Option<pb::Main>>,
    limits: RpcDispatchLimits,
) -> StartDisposition {
    let StartScreenStreamRequest { reply } = request;
    let execution = panic::catch_unwind(AssertUnwindSafe(|| {
        let execution = perform_start_screen_stream(client, admission_state, screen_frames, limits);
        match execution {
            StartExecution::Established(command_id) => {
                match client
                    .transport
                    .set_timeout(screen_read_timeout(client.transport.kind()))
                {
                    Ok(()) => StartExecution::Established(command_id),
                    Err(error) => {
                        StartExecution::Fatal(Err(ConnectionActorError::Device(error.into())))
                    }
                }
            }
            StartExecution::Resolved(result) => {
                match client.transport.set_timeout(SERIAL_TIMEOUT_NORMAL) {
                    Ok(()) => StartExecution::Resolved(result),
                    Err(error) => {
                        StartExecution::Fatal(Err(ConnectionActorError::Device(error.into())))
                    }
                }
            }
            execution => execution,
        }
    }));

    match execution {
        Err(_) => StartDisposition::Actor(ActorExit::Fatal {
            cause: Arc::<str>::from("actor-owned screen start panicked"),
            reply: Box::new(move || {
                let _ = reply.send(Err(ConnectionActorError::JobPanicked));
            }),
        }),
        Ok(StartExecution::Established(command_id)) => StartDisposition::Run { command_id, reply },
        Ok(StartExecution::Resolved(result)) => {
            let actual_restored = state
                .compare_exchange(
                    ConnectionState::Transitioning.as_byte(),
                    ConnectionState::Rpc.as_byte(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok();
            let admission_restored = actual_restored
                && admission_state
                    .compare_exchange(
                        ConnectionState::Transitioning.as_byte(),
                        ConnectionState::Rpc.as_byte(),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok();
            if !admission_restored {
                StartDisposition::Shutdown(reply)
            } else {
                let _ = reply.send(result);
                StartDisposition::Continue
            }
        }
        Ok(StartExecution::Fatal(result)) => {
            StartDisposition::Actor(fatal_unit_exit(result, reply))
        }
        Ok(StartExecution::Shutdown) => StartDisposition::Shutdown(reply),
    }
}

fn perform_start_screen_stream(
    client: &mut FlipperClient,
    admission_state: &AtomicU8,
    screen_frames: &watch::Sender<Option<pb::Main>>,
    limits: RpcDispatchLimits,
) -> StartExecution {
    // A new stream session must never expose the previous session's retained
    // latest frame. Clear before the wire request so any ID-zero frames that
    // race with the acknowledgement belong to, and remain visible for, this
    // new session.
    screen_frames.send_replace(None);

    let command_id = client.next_command_id();
    let request = pb::Main {
        command_id,
        command_status: 0,
        has_next: false,
        content: Some(Content::GuiStartScreenStreamRequest(
            pb_gui::StartScreenStreamRequest {},
        )),
    };
    if let Err(error) = write_message(client.transport.as_mut(), &request) {
        return StartExecution::Fatal(Err(ConnectionActorError::Device(error)));
    }

    let started = Instant::now();
    let deadline = started
        .checked_add(limits.response_deadline)
        .unwrap_or(started);
    let read_slice = screen_read_timeout(client.transport.kind());

    loop {
        if ConnectionState::from_byte(admission_state.load(Ordering::Acquire))
            == ConnectionState::ShuttingDown
        {
            return StartExecution::Shutdown;
        }
        let message = match read_message_until(client.transport.as_mut(), deadline, read_slice) {
            Ok(message) => message,
            Err(DeadlineReadError::DeadlineElapsed) => {
                return StartExecution::Fatal(Err(ConnectionActorError::Protocol(
                    ConnectionProtocolError::ResponseReadDeadlineExceeded {
                        command_id,
                        deadline_ms: duration_millis_u64(limits.response_deadline),
                    },
                )));
            }
            Err(DeadlineReadError::Flipper(FlipperError::Io(error)))
                if is_timeout_kind(error.kind()) =>
            {
                // The transport-specific screen timeout is only a polling
                // slice. Recheck shutdown at the top of the loop and retain
                // the original absolute response deadline.
                continue;
            }
            Err(DeadlineReadError::Flipper(error)) => {
                return StartExecution::Fatal(Err(ConnectionActorError::Device(error)));
            }
        };

        if message.command_id == 0 {
            if matches!(&message.content, Some(Content::GuiScreenFrame(_))) {
                route_screen_frame(screen_frames, message);
                continue;
            }
            return StartExecution::Fatal(Err(foreign_id(command_id, 0)));
        }
        if message.command_id != command_id {
            return StartExecution::Fatal(Err(foreign_id(command_id, message.command_id)));
        }

        if let Err(error) = check_response(&message, command_id) {
            return if message.has_next {
                StartExecution::Fatal(Err(ConnectionActorError::Device(error)))
            } else {
                StartExecution::Resolved(Err(ConnectionActorError::Device(error)))
            };
        }

        match &message.content {
            Some(Content::Empty(_)) => {
                if !message.has_next {
                    // qFlipper's `rpc/abstractprotobufoperation.cpp` default
                    // contract acknowledges GuiStartScreenStreamOperation on
                    // a matching-ID terminal Empty response;
                    // `screenstreamer.cpp` receives frames as ID-zero
                    // broadcasts.
                    return StartExecution::Established(command_id);
                }
            }
            _ => {
                return StartExecution::Fatal(Err(ConnectionActorError::Protocol(
                    ConnectionProtocolError::UnexpectedScreenResponse { command_id },
                )));
            }
        }
    }
}

enum ScreenLoopExit {
    Rpc,
    Actor(ActorExit),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScreenTerminalPublication {
    /// The actor claimed `ScreenStreaming -> Transitioning`; it must drain all
    /// already-admitted screen requests before publishing RPC admission.
    ActorOwned,
    /// A successfully admitted Stop owns `Transitioning` and is already in the
    /// FIFO queue. It will perform the acknowledged transition to RPC.
    StopOwned,
    Shutdown,
}

fn publish_screen_terminal(
    state: &AtomicU8,
    admission_state: &AtomicU8,
    admission_gate: &Mutex<()>,
    attempts: &AtomicU64,
) -> ScreenTerminalPublication {
    // Increment before locking so deterministic tests can prove that a failed
    // async preclaim is actively blocking actor publication on this gate.
    attempts.fetch_add(1, Ordering::Release);
    let _admission = lock_admission_gate(admission_gate);
    let publication = match ConnectionState::from_byte(admission_state.load(Ordering::Acquire)) {
        ConnectionState::ScreenStreaming => {
            let claimed = admission_state
                .compare_exchange(
                    ConnectionState::ScreenStreaming.as_byte(),
                    ConnectionState::Transitioning.as_byte(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok();
            debug_assert!(claimed, "admission gate makes the screen claim exclusive");
            ScreenTerminalPublication::ActorOwned
        }
        ConnectionState::Transitioning => ScreenTerminalPublication::StopOwned,
        ConnectionState::ShuttingDown | ConnectionState::Disconnected => {
            ScreenTerminalPublication::Shutdown
        }
        current => {
            debug_assert!(
                false,
                "screen terminal observed with invalid admission state {current}"
            );
            ScreenTerminalPublication::Shutdown
        }
    };
    // Publish actual wire state under the same gate. A failed transition
    // enqueue therefore either rolls back completely before this store or
    // observes this completed publication; it cannot restore stale Screen.
    // Shutdown owns its own terminal publication in the actor wrapper, so do
    // not transiently advertise an actual RPC state after admission has closed.
    if matches!(
        publication,
        ScreenTerminalPublication::ActorOwned | ScreenTerminalPublication::StopOwned
    ) {
        state.store(ConnectionState::Rpc.as_byte(), Ordering::Release);
    }
    publication
}

#[allow(clippy::too_many_arguments)]
fn run_screen_stream(
    client: &mut FlipperClient,
    commands: &mut mpsc::Receiver<ActorCommand>,
    state: &AtomicU8,
    admission_state: &AtomicU8,
    admission_gate: &Mutex<()>,
    screen_terminal_attempts: &AtomicU64,
    pending: &mut Vec<PendingRequest>,
    screen_frames: &watch::Sender<Option<pb::Main>>,
    active_command_id: u32,
    limits: RpcDispatchLimits,
) -> ScreenLoopExit {
    let mut held_keys = [false; 6];
    let mut terminal: Option<StreamEnd> = None;
    let mut actor_owns_terminal_transition = false;
    loop {
        let admission = ConnectionState::from_byte(admission_state.load(Ordering::Acquire));
        if admission == ConnectionState::ShuttingDown {
            return ScreenLoopExit::Actor(ActorExit::Shutdown);
        }
        match commands.try_recv() {
            Ok(ActorCommand::RunRpc(job)) => {
                let current = ConnectionState::from_byte(state.load(Ordering::Acquire));
                job.reject(ConnectionActorError::ModeRejected { current });
            }
            Ok(ActorCommand::ProtoRpc(request)) => {
                let current = ConnectionState::from_byte(state.load(Ordering::Acquire));
                request.reject(ConnectionActorError::ModeRejected { current });
            }
            Ok(ActorCommand::StartScreenStream(request)) => {
                let current = ConnectionState::from_byte(state.load(Ordering::Acquire));
                request.reject(ConnectionActorError::ModeRejected { current });
            }
            Ok(ActorCommand::ScreenInput(request)) => {
                if let Some(ended) = terminal {
                    request.reject(ConnectionActorError::ScreenStreamEndedDuringInput {
                        command_id: ended.command_id,
                        command_status: ended.command_status,
                    });
                    continue;
                }
                let current = ConnectionState::from_byte(state.load(Ordering::Acquire));
                if current != ConnectionState::ScreenStreaming {
                    request.reject(mode_error(current));
                    continue;
                }
                match execute_screen_input(
                    client,
                    request,
                    state,
                    admission_state,
                    admission_gate,
                    screen_terminal_attempts,
                    screen_frames,
                    active_command_id,
                    limits,
                    &mut held_keys,
                ) {
                    ScreenCommandDisposition::Continue => {}
                    ScreenCommandDisposition::StreamEnded { ended, actor_owned } => {
                        terminal = Some(ended);
                        actor_owns_terminal_transition = actor_owned;
                    }
                    ScreenCommandDisposition::Shutdown(request) => {
                        pending.push(PendingRequest::ScreenInput(request));
                        return ScreenLoopExit::Actor(ActorExit::Shutdown);
                    }
                    ScreenCommandDisposition::Actor(exit) => {
                        return ScreenLoopExit::Actor(exit);
                    }
                }
            }
            Ok(ActorCommand::StopScreenStream(request)) => {
                let admission = ConnectionState::from_byte(admission_state.load(Ordering::Acquire));
                if admission != ConnectionState::Transitioning {
                    request.reject(mode_error(admission));
                    continue;
                }
                let actual = ConnectionState::from_byte(state.load(Ordering::Acquire));
                if !matches!(
                    actual,
                    ConnectionState::ScreenStreaming | ConnectionState::Rpc
                ) {
                    request.reject(mode_error(actual));
                    continue;
                }
                if state
                    .compare_exchange(
                        actual.as_byte(),
                        ConnectionState::Transitioning.as_byte(),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    request.reject(mode_error(ConnectionState::from_byte(
                        state.load(Ordering::Acquire),
                    )));
                    continue;
                }
                match execute_stop_screen_stream(
                    client,
                    request,
                    state,
                    admission_state,
                    screen_frames,
                    active_command_id,
                    limits,
                    &mut held_keys,
                ) {
                    StopDisposition::Rpc => return ScreenLoopExit::Rpc,
                    StopDisposition::Shutdown(request) => {
                        pending.push(PendingRequest::StopScreen(request));
                        return ScreenLoopExit::Actor(ActorExit::Shutdown);
                    }
                    StopDisposition::Actor(exit) => return ScreenLoopExit::Actor(exit),
                }
            }
            Ok(ActorCommand::EnterCli(request)) => {
                request.reject(ConnectionActorError::ModeRejected {
                    current: ConnectionState::ScreenStreaming,
                });
            }
            Ok(ActorCommand::CliSend(request)) => {
                request.reject(ConnectionActorError::ModeRejected {
                    current: ConnectionState::ScreenStreaming,
                });
            }
            Ok(ActorCommand::CliInterrupt(request)) => {
                request.reject(ConnectionActorError::ModeRejected {
                    current: ConnectionState::ScreenStreaming,
                });
            }
            Ok(ActorCommand::ExitCli(request)) => {
                request.reject(ConnectionActorError::ModeRejected {
                    current: ConnectionState::ScreenStreaming,
                });
            }
            Ok(ActorCommand::Shutdown) => {
                return ScreenLoopExit::Actor(ActorExit::Shutdown);
            }
            #[cfg(test)]
            Ok(ActorCommand::ForcePanic) => panic!("forced actor-thread panic"),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                return ScreenLoopExit::Actor(ActorExit::ChannelClosed);
            }
            Err(mpsc::error::TryRecvError::Empty) => {
                if terminal.is_some() {
                    if actor_owns_terminal_transition {
                        let _admission = lock_admission_gate(admission_gate);
                        if admission_state
                            .compare_exchange(
                                ConnectionState::Transitioning.as_byte(),
                                ConnectionState::Rpc.as_byte(),
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_ok()
                        {
                            return ScreenLoopExit::Rpc;
                        }
                        return ScreenLoopExit::Actor(ActorExit::Shutdown);
                    }
                    // A Stop-owned transition is inserted into the bounded
                    // FIFO before its submitter releases `admission_gate`.
                    // After actor publication acquires that same gate, an
                    // Empty observation can only be a transient receiver view.
                    std::thread::yield_now();
                    continue;
                }
                let read_slice = screen_read_timeout(client.transport.kind());
                let started = Instant::now();
                let deadline = started.checked_add(read_slice).unwrap_or(started);
                match read_message_until(client.transport.as_mut(), deadline, read_slice) {
                    Ok(message) => {
                        match process_stream_message(message, active_command_id, screen_frames) {
                            StreamMessage::Continue => {}
                            StreamMessage::Ended(ended) => {
                                let publication = publish_screen_terminal(
                                    state,
                                    admission_state,
                                    admission_gate,
                                    screen_terminal_attempts,
                                );
                                match release_all_held_keys(
                                    client,
                                    admission_state,
                                    active_command_id,
                                    screen_frames,
                                    limits,
                                    &mut held_keys,
                                ) {
                                    AckExecution::Resolved { result: Ok(()), .. } => {}
                                    AckExecution::Fatal(Err(error)) => {
                                        return ScreenLoopExit::Actor(fatal_without_reply(error));
                                    }
                                    AckExecution::Fatal(Ok(()))
                                    | AckExecution::Resolved { result: Err(_), .. } => {
                                        unreachable!("held-key cleanup failures are fatal")
                                    }
                                    AckExecution::Shutdown => {
                                        return ScreenLoopExit::Actor(ActorExit::Shutdown);
                                    }
                                }
                                if let Err(error) =
                                    client.transport.set_timeout(SERIAL_TIMEOUT_NORMAL)
                                {
                                    return ScreenLoopExit::Actor(fatal_without_reply(
                                        ConnectionActorError::Device(error.into()),
                                    ));
                                }
                                match publication {
                                    ScreenTerminalPublication::ActorOwned => {
                                        actor_owns_terminal_transition = true;
                                    }
                                    ScreenTerminalPublication::StopOwned => {
                                        actor_owns_terminal_transition = false;
                                    }
                                    ScreenTerminalPublication::Shutdown => {
                                        return ScreenLoopExit::Actor(ActorExit::Shutdown);
                                    }
                                }
                                terminal = Some(ended);
                            }
                            StreamMessage::Fatal(error) => {
                                return ScreenLoopExit::Actor(fatal_without_reply(error));
                            }
                        }
                    }
                    Err(DeadlineReadError::DeadlineElapsed) => {}
                    Err(DeadlineReadError::Flipper(FlipperError::Io(error)))
                        if is_timeout_kind(error.kind()) => {}
                    Err(DeadlineReadError::Flipper(error)) => {
                        return ScreenLoopExit::Actor(fatal_without_reply(
                            ConnectionActorError::Device(error),
                        ));
                    }
                }
            }
        }
    }
}

enum StreamMessage {
    Continue,
    Ended(StreamEnd),
    Fatal(ConnectionActorError),
}

fn process_stream_message(
    message: pb::Main,
    active_command_id: u32,
    screen_frames: &watch::Sender<Option<pb::Main>>,
) -> StreamMessage {
    if message.command_id == 0 {
        if matches!(&message.content, Some(Content::GuiScreenFrame(_))) {
            route_screen_frame(screen_frames, message);
            return StreamMessage::Continue;
        }
        return StreamMessage::Fatal(foreign_id(active_command_id, 0));
    }
    if message.command_id != active_command_id {
        return StreamMessage::Fatal(foreign_id(active_command_id, message.command_id));
    }

    if message.command_status != 0 && message.has_next {
        return StreamMessage::Fatal(ConnectionActorError::Device(FlipperError::Rpc {
            status: message.command_status,
            command_id: active_command_id,
        }));
    }

    match &message.content {
        Some(Content::Empty(_)) if !message.has_next => StreamMessage::Ended(StreamEnd {
            command_id: active_command_id,
            command_status: message.command_status,
        }),
        Some(Content::Empty(_)) => StreamMessage::Continue,
        _ => StreamMessage::Fatal(ConnectionActorError::Protocol(
            ConnectionProtocolError::UnexpectedScreenResponse {
                command_id: active_command_id,
            },
        )),
    }
}

enum AckExecution {
    Resolved {
        result: ActorResult<()>,
        stream_end: Option<StreamEnd>,
    },
    Fatal(ActorResult<()>),
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StreamEnd {
    command_id: u32,
    command_status: i32,
}

enum ScreenCommandDisposition {
    Continue,
    StreamEnded { ended: StreamEnd, actor_owned: bool },
    Shutdown(ScreenInputRequest),
    Actor(ActorExit),
}

#[allow(clippy::too_many_arguments)]
fn execute_screen_input(
    client: &mut FlipperClient,
    request: ScreenInputRequest,
    state: &AtomicU8,
    admission_state: &AtomicU8,
    admission_gate: &Mutex<()>,
    screen_terminal_attempts: &AtomicU64,
    screen_frames: &watch::Sender<Option<pb::Main>>,
    active_command_id: u32,
    limits: RpcDispatchLimits,
    held_keys: &mut [bool; 6],
) -> ScreenCommandDisposition {
    let ScreenInputRequest {
        key,
        input_type,
        reply,
    } = request;
    let execution = panic::catch_unwind(AssertUnwindSafe(|| {
        // Preserve one-shot SHORT/LONG compatibility, but do not synthesize a
        // nested PRESS/RELEASE when the frontend is already driving the full
        // qFlipper lifecycle for a held keyboard or pointer key.
        if matches!(input_type, INPUT_SHORT | INPUT_LONG) && !held_keys[key as usize] {
            perform_expanded_screen_input(
                client,
                admission_state,
                key,
                input_type,
                active_command_id,
                screen_frames,
                limits,
                held_keys,
            )
        } else {
            match perform_screen_ack_command(
                client,
                admission_state,
                Content::GuiSendInputEventRequest(pb_gui::SendInputEventRequest {
                    key,
                    r#type: input_type,
                }),
                active_command_id,
                screen_frames,
                limits,
            ) {
                AckExecution::Resolved { result, stream_end } => {
                    if result.is_ok() {
                        if input_type == INPUT_PRESS {
                            held_keys[key as usize] = true;
                        } else if input_type == INPUT_RELEASE {
                            held_keys[key as usize] = false;
                        }
                    }
                    finish_input_after_ack(
                        client,
                        admission_state,
                        active_command_id,
                        screen_frames,
                        limits,
                        held_keys,
                        result,
                        stream_end,
                    )
                }
                execution => execution,
            }
        }
    }));

    let execution = match execution {
        Ok(AckExecution::Resolved { result, stream_end }) => {
            let timeout = if stream_end.is_some() {
                SERIAL_TIMEOUT_NORMAL
            } else {
                screen_read_timeout(client.transport.kind())
            };
            match client.transport.set_timeout(timeout) {
                Ok(()) => AckExecution::Resolved { result, stream_end },
                Err(error) => AckExecution::Fatal(Err(ConnectionActorError::Device(error.into()))),
            }
        }
        Ok(execution) => execution,
        Err(_) => {
            return ScreenCommandDisposition::Actor(ActorExit::Fatal {
                cause: Arc::<str>::from("actor-owned screen input panicked"),
                reply: Box::new(move || {
                    let _ = reply.send(Err(ConnectionActorError::JobPanicked));
                }),
            });
        }
    };

    match execution {
        AckExecution::Resolved { result, stream_end } => {
            if let Some(ended) = stream_end {
                let publication = publish_screen_terminal(
                    state,
                    admission_state,
                    admission_gate,
                    screen_terminal_attempts,
                );
                if publication == ScreenTerminalPublication::Shutdown {
                    return ScreenCommandDisposition::Shutdown(ScreenInputRequest {
                        key,
                        input_type,
                        reply,
                    });
                }
                let _ = reply.send(result);
                ScreenCommandDisposition::StreamEnded {
                    ended,
                    actor_owned: publication == ScreenTerminalPublication::ActorOwned,
                }
            } else if ConnectionState::from_byte(admission_state.load(Ordering::Acquire))
                == ConnectionState::ShuttingDown
            {
                ScreenCommandDisposition::Shutdown(ScreenInputRequest {
                    key,
                    input_type,
                    reply,
                })
            } else {
                let _ = reply.send(result);
                ScreenCommandDisposition::Continue
            }
        }
        AckExecution::Fatal(result) => {
            ScreenCommandDisposition::Actor(fatal_unit_exit(result, reply))
        }
        AckExecution::Shutdown => ScreenCommandDisposition::Shutdown(ScreenInputRequest {
            key,
            input_type,
            reply,
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn perform_expanded_screen_input(
    client: &mut FlipperClient,
    admission_state: &AtomicU8,
    key: i32,
    input_type: i32,
    active_command_id: u32,
    screen_frames: &watch::Sender<Option<pb::Main>>,
    limits: RpcDispatchLimits,
    held_keys: &mut [bool; 6],
) -> AckExecution {
    let key_index = key as usize;
    let mut stream_end = match perform_screen_ack_command(
        client,
        admission_state,
        Content::GuiSendInputEventRequest(pb_gui::SendInputEventRequest {
            key,
            r#type: INPUT_PRESS,
        }),
        active_command_id,
        screen_frames,
        limits,
    ) {
        AckExecution::Resolved {
            result: Ok(()),
            stream_end,
        } => {
            held_keys[key_index] = true;
            stream_end
        }
        AckExecution::Resolved {
            result: Err(error),
            stream_end: None,
        } => {
            return AckExecution::Resolved {
                result: Err(error),
                stream_end: None,
            };
        }
        AckExecution::Resolved {
            stream_end: Some(ended),
            ..
        } => Some(ended),
        execution => return execution,
    };

    let mut result = Ok(());
    // Once PRESS is acknowledged, RELEASE is mandatory. If the stream ended
    // with the PRESS ack, skip the SHORT/LONG action but still balance it.
    if stream_end.is_none() {
        match perform_screen_ack_command(
            client,
            admission_state,
            Content::GuiSendInputEventRequest(pb_gui::SendInputEventRequest {
                key,
                r#type: input_type,
            }),
            active_command_id,
            screen_frames,
            limits,
        ) {
            AckExecution::Resolved {
                result: action_result,
                stream_end: observed,
            } => {
                result = action_result;
                stream_end = observed;
            }
            execution => return execution,
        }
    }

    match release_held_key(
        client,
        admission_state,
        active_command_id,
        screen_frames,
        limits,
        held_keys,
        key_index,
    ) {
        AckExecution::Resolved {
            result: Ok(()),
            stream_end: observed,
        } => stream_end = stream_end.or(observed),
        execution => return execution,
    }

    finish_input_after_ack(
        client,
        admission_state,
        active_command_id,
        screen_frames,
        limits,
        held_keys,
        result,
        stream_end,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_input_after_ack(
    client: &mut FlipperClient,
    admission_state: &AtomicU8,
    active_command_id: u32,
    screen_frames: &watch::Sender<Option<pb::Main>>,
    limits: RpcDispatchLimits,
    held_keys: &mut [bool; 6],
    result: ActorResult<()>,
    stream_end: Option<StreamEnd>,
) -> AckExecution {
    let Some(ended) = stream_end else {
        return AckExecution::Resolved {
            result,
            stream_end: None,
        };
    };
    match release_all_held_keys(
        client,
        admission_state,
        active_command_id,
        screen_frames,
        limits,
        held_keys,
    ) {
        AckExecution::Resolved { result: Ok(()), .. } => AckExecution::Resolved {
            result: Err(ConnectionActorError::ScreenStreamEndedDuringInput {
                command_id: ended.command_id,
                command_status: ended.command_status,
            }),
            stream_end: Some(ended),
        },
        execution => execution,
    }
}

enum StopDisposition {
    Rpc,
    Shutdown(StopScreenStreamRequest),
    Actor(ActorExit),
}

#[allow(clippy::too_many_arguments)]
fn execute_stop_screen_stream(
    client: &mut FlipperClient,
    request: StopScreenStreamRequest,
    state: &AtomicU8,
    admission_state: &AtomicU8,
    screen_frames: &watch::Sender<Option<pb::Main>>,
    active_command_id: u32,
    limits: RpcDispatchLimits,
    held_keys: &mut [bool; 6],
) -> StopDisposition {
    let StopScreenStreamRequest { reply } = request;
    let execution = panic::catch_unwind(AssertUnwindSafe(|| {
        match release_all_held_keys(
            client,
            admission_state,
            active_command_id,
            screen_frames,
            limits,
            held_keys,
        ) {
            AckExecution::Resolved { result: Ok(()), .. } => {}
            execution => return execution,
        }
        let execution = perform_screen_ack_command(
            client,
            admission_state,
            Content::GuiStopScreenStreamRequest(pb_gui::StopScreenStreamRequest {}),
            active_command_id,
            screen_frames,
            limits,
        );
        match execution {
            AckExecution::Resolved { result, stream_end } => {
                match client.transport.set_timeout(SERIAL_TIMEOUT_NORMAL) {
                    Ok(()) => AckExecution::Resolved { result, stream_end },
                    Err(error) => {
                        AckExecution::Fatal(Err(ConnectionActorError::Device(error.into())))
                    }
                }
            }
            execution => execution,
        }
    }));

    let execution = match execution {
        Ok(execution) => execution,
        Err(_) => {
            return StopDisposition::Actor(ActorExit::Fatal {
                cause: Arc::<str>::from("actor-owned screen stop panicked"),
                reply: Box::new(move || {
                    let _ = reply.send(Err(ConnectionActorError::JobPanicked));
                }),
            });
        }
    };

    match execution {
        AckExecution::Resolved { result, .. } => {
            let actual_restored = state
                .compare_exchange(
                    ConnectionState::Transitioning.as_byte(),
                    ConnectionState::Rpc.as_byte(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok();
            let admission_restored = actual_restored
                && admission_state
                    .compare_exchange(
                        ConnectionState::Transitioning.as_byte(),
                        ConnectionState::Rpc.as_byte(),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok();
            if !admission_restored {
                StopDisposition::Shutdown(StopScreenStreamRequest { reply })
            } else {
                let _ = reply.send(result);
                StopDisposition::Rpc
            }
        }
        AckExecution::Fatal(result) => StopDisposition::Actor(fatal_unit_exit(result, reply)),
        AckExecution::Shutdown => StopDisposition::Shutdown(StopScreenStreamRequest { reply }),
    }
}

#[allow(clippy::too_many_arguments)]
fn release_held_key(
    client: &mut FlipperClient,
    admission_state: &AtomicU8,
    active_command_id: u32,
    screen_frames: &watch::Sender<Option<pb::Main>>,
    limits: RpcDispatchLimits,
    held_keys: &mut [bool; 6],
    key_index: usize,
) -> AckExecution {
    if !held_keys[key_index] {
        return AckExecution::Resolved {
            result: Ok(()),
            stream_end: None,
        };
    }
    match perform_screen_ack_command(
        client,
        admission_state,
        Content::GuiSendInputEventRequest(pb_gui::SendInputEventRequest {
            key: key_index as i32,
            r#type: INPUT_RELEASE,
        }),
        active_command_id,
        screen_frames,
        limits,
    ) {
        AckExecution::Resolved {
            result: Ok(()),
            stream_end,
        } => {
            held_keys[key_index] = false;
            AckExecution::Resolved {
                result: Ok(()),
                stream_end,
            }
        }
        // A synthesized release without a successful matching ack leaves the
        // firmware key state indeterminate. Force session teardown so the
        // transport drop becomes the final cleanup barrier.
        AckExecution::Resolved {
            result: Err(error), ..
        } => AckExecution::Fatal(Err(error)),
        execution => execution,
    }
}

fn release_all_held_keys(
    client: &mut FlipperClient,
    admission_state: &AtomicU8,
    active_command_id: u32,
    screen_frames: &watch::Sender<Option<pb::Main>>,
    limits: RpcDispatchLimits,
    held_keys: &mut [bool; 6],
) -> AckExecution {
    let mut stream_end = None;
    for key_index in 0..held_keys.len() {
        if !held_keys[key_index] {
            continue;
        }
        match release_held_key(
            client,
            admission_state,
            active_command_id,
            screen_frames,
            limits,
            held_keys,
            key_index,
        ) {
            AckExecution::Resolved {
                result: Ok(()),
                stream_end: observed,
            } => stream_end = stream_end.or(observed),
            execution => return execution,
        }
    }
    AckExecution::Resolved {
        result: Ok(()),
        stream_end,
    }
}

fn perform_screen_ack_command(
    client: &mut FlipperClient,
    admission_state: &AtomicU8,
    content: Content,
    active_command_id: u32,
    screen_frames: &watch::Sender<Option<pb::Main>>,
    limits: RpcDispatchLimits,
) -> AckExecution {
    let command_id = client.next_command_id();
    let request = pb::Main {
        command_id,
        command_status: 0,
        has_next: false,
        content: Some(content),
    };
    if let Err(error) = write_message(client.transport.as_mut(), &request) {
        return AckExecution::Fatal(Err(ConnectionActorError::Device(error)));
    }

    let started = Instant::now();
    let deadline = started
        .checked_add(limits.response_deadline)
        .unwrap_or(started);
    let read_slice = screen_read_timeout(client.transport.kind());
    let mut stream_end = None;
    loop {
        if ConnectionState::from_byte(admission_state.load(Ordering::Acquire))
            == ConnectionState::ShuttingDown
        {
            return AckExecution::Shutdown;
        }
        let message = match read_message_until(client.transport.as_mut(), deadline, read_slice) {
            Ok(message) => message,
            Err(DeadlineReadError::DeadlineElapsed) => {
                return AckExecution::Fatal(Err(ConnectionActorError::Protocol(
                    ConnectionProtocolError::ResponseReadDeadlineExceeded {
                        command_id,
                        deadline_ms: duration_millis_u64(limits.response_deadline),
                    },
                )));
            }
            Err(DeadlineReadError::Flipper(FlipperError::Io(error)))
                if is_timeout_kind(error.kind()) =>
            {
                // A screen timeout is a polling tick, not the request
                // deadline. The loop retains the original absolute deadline
                // and rechecks shutdown before the next read.
                continue;
            }
            Err(DeadlineReadError::Flipper(error)) => {
                return AckExecution::Fatal(Err(ConnectionActorError::Device(error)));
            }
        };

        if message.command_id == command_id {
            if let Err(error) = check_response(&message, command_id) {
                return if message.has_next {
                    AckExecution::Fatal(Err(ConnectionActorError::Device(error)))
                } else {
                    AckExecution::Resolved {
                        result: Err(ConnectionActorError::Device(error)),
                        stream_end,
                    }
                };
            }
            if !matches!(&message.content, Some(Content::Empty(_))) {
                return AckExecution::Fatal(Err(ConnectionActorError::Protocol(
                    ConnectionProtocolError::UnexpectedScreenResponse { command_id },
                )));
            }
            if !message.has_next {
                // qFlipper's stop and input operations share the default
                // matching-ID terminal Empty acknowledgement contract.
                return AckExecution::Resolved {
                    result: Ok(()),
                    stream_end,
                };
            }
            continue;
        }

        if message.command_id == 0 && matches!(&message.content, Some(Content::GuiScreenFrame(_))) {
            route_screen_frame(screen_frames, message);
            continue;
        }

        if message.command_id == active_command_id {
            if message.command_status != 0 && message.has_next {
                return AckExecution::Fatal(Err(ConnectionActorError::Device(FlipperError::Rpc {
                    status: message.command_status,
                    command_id: active_command_id,
                })));
            }
            match &message.content {
                Some(Content::Empty(_)) if !message.has_next => {
                    // A continuous-start terminal may race with the stop ack.
                    // It is byte-aligned and known, but does not acknowledge
                    // the distinct stop/input command.
                    stream_end = Some(StreamEnd {
                        command_id: active_command_id,
                        command_status: message.command_status,
                    });
                    continue;
                }
                Some(Content::Empty(_)) => continue,
                _ => {
                    return AckExecution::Fatal(Err(ConnectionActorError::Protocol(
                        ConnectionProtocolError::UnexpectedScreenResponse {
                            command_id: active_command_id,
                        },
                    )));
                }
            }
        }

        return AckExecution::Fatal(Err(foreign_id(command_id, message.command_id)));
    }
}

fn screen_read_timeout(kind: TransportKind) -> Duration {
    match kind {
        TransportKind::Serial => SERIAL_TIMEOUT_SCREEN,
        TransportKind::Ble => BLE_TIMEOUT_SCREEN,
    }
}

fn is_timeout_kind(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::Interrupted
    )
}

fn foreign_id(expected_id: u32, received_id: u32) -> ConnectionActorError {
    ConnectionActorError::Protocol(ConnectionProtocolError::ForeignCommandId {
        expected_id,
        received_id,
    })
}

fn fatal_unit_exit(result: ActorResult<()>, reply: oneshot::Sender<ActorResult<()>>) -> ActorExit {
    let cause: Arc<str> = match result.as_ref() {
        Err(error) => error.to_string().into(),
        Ok(()) => unreachable!("fatal unit result must contain an error"),
    };
    ActorExit::Fatal {
        cause,
        reply: Box::new(move || {
            let _ = reply.send(result);
        }),
    }
}

fn fatal_without_reply(error: ConnectionActorError) -> ActorExit {
    ActorExit::Fatal {
        cause: error.to_string().into(),
        reply: Box::new(|| {}),
    }
}

fn execute_proto_rpc(
    client: &mut FlipperClient,
    request: ProtoRpcRequest,
    screen_frames: &watch::Sender<Option<pb::Main>>,
    limits: RpcDispatchLimits,
) -> JobDisposition {
    let ProtoRpcRequest { content, reply } = request;
    let command_id = client.next_command_id();
    let message = pb::Main {
        command_id,
        command_status: 0,
        has_next: false,
        content: Some(content),
    };

    // Keep `reply` outside the unwind boundary. A transport/framing panic must
    // not close the oneshot before the actor has torn down the connection.
    let execution = panic::catch_unwind(AssertUnwindSafe(|| {
        match perform_proto_rpc(client, message, command_id, screen_frames, limits) {
            RpcExecution::Resolved(result) => {
                match client.transport.set_timeout(SERIAL_TIMEOUT_NORMAL) {
                    Ok(()) => RpcExecution::Resolved(result),
                    Err(error) => {
                        RpcExecution::Fatal(Err(ConnectionActorError::Device(error.into())))
                    }
                }
            }
            execution => execution,
        }
    }));
    let execution = match execution {
        Ok(execution) => execution,
        Err(_) => {
            return JobDisposition::Fatal {
                cause: Arc::<str>::from("actor-owned protobuf request panicked"),
                reply: Box::new(move || {
                    let _ = reply.send(Err(ConnectionActorError::JobPanicked));
                }),
            };
        }
    };
    match execution {
        RpcExecution::Rejected(result) | RpcExecution::Resolved(result) => {
            let _ = reply.send(result);
            JobDisposition::Continue
        }
        RpcExecution::Fatal(result) => {
            let cause: Arc<str> = match result.as_ref() {
                Err(error) => error.to_string().into(),
                Ok(_) => unreachable!("fatal RPC result must contain an error"),
            };
            JobDisposition::Fatal {
                cause,
                reply: Box::new(move || {
                    let _ = reply.send(result);
                }),
            }
        }
    }
}

enum RpcExecution {
    /// Rejected before any transport write/read; no timeout restoration needed.
    Rejected(ActorResult<RpcResponse>),
    Resolved(ActorResult<RpcResponse>),
    Fatal(ActorResult<RpcResponse>),
}

fn perform_proto_rpc(
    client: &mut FlipperClient,
    request: pb::Main,
    command_id: u32,
    screen_frames: &watch::Sender<Option<pb::Main>>,
    limits: RpcDispatchLimits,
) -> RpcExecution {
    let request_bytes = request.encoded_len();
    if request_bytes > limits.max_request_bytes {
        return RpcExecution::Rejected(Err(ConnectionActorError::Protocol(
            ConnectionProtocolError::RequestBytesExceeded {
                command_id,
                actual: request_bytes,
                limit: limits.max_request_bytes,
            },
        )));
    }
    if let Err(error) = write_message(client.transport.as_mut(), &request) {
        return RpcExecution::Fatal(Err(ConnectionActorError::Device(error)));
    }
    let started = Instant::now();
    let deadline = started
        .checked_add(limits.response_deadline)
        .unwrap_or(started);

    let mut frames = Vec::new();
    let mut response_bytes = 0_usize;
    let mut response_frame_count = 0_usize;
    loop {
        let message =
            match read_message_until(client.transport.as_mut(), deadline, SERIAL_TIMEOUT_NORMAL) {
                Ok(message) => message,
                Err(DeadlineReadError::DeadlineElapsed) => {
                    return RpcExecution::Fatal(Err(ConnectionActorError::Protocol(
                        ConnectionProtocolError::ResponseReadDeadlineExceeded {
                            command_id,
                            deadline_ms: duration_millis_u64(limits.response_deadline),
                        },
                    )));
                }
                Err(DeadlineReadError::Flipper(FlipperError::Io(error)))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    return RpcExecution::Fatal(Err(ConnectionActorError::Protocol(
                        ConnectionProtocolError::ResponseTimeout { command_id },
                    )));
                }
                Err(DeadlineReadError::Flipper(error)) => {
                    return RpcExecution::Fatal(Err(ConnectionActorError::Device(error)));
                }
            };

        match classify_inbound(message, command_id) {
            InboundRoute::Screen(frame) => route_screen_frame(screen_frames, frame),
            InboundRoute::Foreign(received_id) => {
                return RpcExecution::Fatal(Err(ConnectionActorError::Protocol(
                    ConnectionProtocolError::ForeignCommandId {
                        expected_id: command_id,
                        received_id,
                    },
                )));
            }
            InboundRoute::Matching(frame) => {
                response_frame_count = match response_frame_count.checked_add(1) {
                    Some(total) if total <= limits.max_frames => total,
                    _ => {
                        return RpcExecution::Fatal(Err(ConnectionActorError::Protocol(
                            ConnectionProtocolError::TooManyResponseFrames {
                                command_id,
                                limit: limits.max_frames,
                            },
                        )));
                    }
                };
                response_bytes = match response_bytes.checked_add(frame.encoded_len()) {
                    Some(total) if total <= limits.max_bytes => total,
                    _ => {
                        return RpcExecution::Fatal(Err(ConnectionActorError::Protocol(
                            ConnectionProtocolError::ResponseBytesExceeded {
                                command_id,
                                limit: limits.max_bytes,
                            },
                        )));
                    }
                };
                if let Err(error) = check_response(&frame, command_id) {
                    return if matches!(error, FlipperError::Rpc { .. }) && !frame.has_next {
                        RpcExecution::Resolved(Err(ConnectionActorError::Device(error)))
                    } else {
                        RpcExecution::Fatal(Err(ConnectionActorError::Device(error)))
                    };
                }

                let has_next = frame.has_next;
                frames.push(frame);
                if !has_next {
                    return RpcExecution::Resolved(Ok(RpcResponse { command_id, frames }));
                }
            }
        }
    }
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

enum InboundRoute {
    Matching(pb::Main),
    Screen(pb::Main),
    Foreign(u32),
}

fn classify_inbound(message: pb::Main, expected_id: u32) -> InboundRoute {
    if message.command_id == expected_id {
        InboundRoute::Matching(message)
    } else if message.command_id == 0
        && matches!(&message.content, Some(Content::GuiScreenFrame(_)))
    {
        InboundRoute::Screen(message)
    } else {
        InboundRoute::Foreign(message.command_id)
    }
}

fn route_screen_frame(screen_frames: &watch::Sender<Option<pb::Main>>, frame: pb::Main) {
    // `send_replace` retains the latest value even when no receiver currently
    // exists. Continuous screen traffic is intentionally coalesced rather than
    // turning a slow/closed UI route into a transport-fatal condition.
    screen_frames.send_replace(Some(frame));
}

enum PendingRejection {
    Closed,
    ActorStopped,
    ConnectionLost(Arc<str>),
}

fn drain_pending(commands: &mut mpsc::Receiver<ActorCommand>, pending: &mut Vec<PendingRequest>) {
    while let Some(command) = commands.blocking_recv() {
        match command {
            ActorCommand::RunRpc(job) => pending.push(PendingRequest::Legacy(job)),
            ActorCommand::ProtoRpc(request) => pending.push(PendingRequest::Proto(request)),
            ActorCommand::StartScreenStream(request) => {
                pending.push(PendingRequest::StartScreen(request));
            }
            ActorCommand::ScreenInput(request) => {
                pending.push(PendingRequest::ScreenInput(request));
            }
            ActorCommand::StopScreenStream(request) => {
                pending.push(PendingRequest::StopScreen(request));
            }
            ActorCommand::EnterCli(request) => pending.push(PendingRequest::EnterCli(request)),
            ActorCommand::CliSend(request) => pending.push(PendingRequest::CliSend(request)),
            ActorCommand::CliInterrupt(request) => {
                pending.push(PendingRequest::CliInterrupt(request));
            }
            ActorCommand::ExitCli(request) => pending.push(PendingRequest::ExitCli(request)),
            ActorCommand::Shutdown => {}
            #[cfg(test)]
            ActorCommand::ForcePanic => {}
        }
    }
}

fn reject_pending(pending: Vec<PendingRequest>, reason: PendingRejection) {
    for request in pending {
        let error = match &reason {
            PendingRejection::Closed => ConnectionActorError::Closed,
            PendingRejection::ActorStopped => ConnectionActorError::ActorStopped,
            PendingRejection::ConnectionLost(cause) => ConnectionActorError::ConnectionLost {
                cause: Arc::clone(cause),
            },
        };
        match request {
            PendingRequest::Legacy(job) => job.reject(error),
            PendingRequest::Proto(request) => request.reject(error),
            PendingRequest::StartScreen(request) => request.reject(error),
            PendingRequest::ScreenInput(request) => request.reject(error),
            PendingRequest::StopScreen(request) => request.reject(error),
            PendingRequest::EnterCli(request) => request.reject(error),
            PendingRequest::CliSend(request) => request.reject(error),
            PendingRequest::CliInterrupt(request) => request.reject(error),
            PendingRequest::ExitCli(request) => request.reject(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::io;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{mpsc as std_mpsc, Arc, Condvar, Mutex};
    use std::thread::ThreadId;
    use std::time::{Duration, Instant};

    use prost::Message;

    use super::*;
    use crate::flipper::transport::{Transport, TransportKind};
    use crate::pb_gui;
    use tokio::sync::Barrier;

    const TEST_TIMEOUT: Duration = Duration::from_secs(2);
    type Writes = Arc<Mutex<Vec<(ThreadId, Vec<u8>)>>>;

    struct ScriptProbe {
        written: Arc<Mutex<Vec<u8>>>,
        read_threads: Arc<Mutex<Vec<ThreadId>>>,
        read_calls: Arc<AtomicUsize>,
        remaining: Arc<AtomicUsize>,
        dropped: Arc<AtomicBool>,
        timeouts: Arc<Mutex<Vec<Duration>>>,
        current_timeout: Arc<Mutex<Option<Duration>>>,
        write_timeouts: Arc<Mutex<Vec<Option<Duration>>>>,
    }

    struct ScriptedTransport {
        input: VecDeque<u8>,
        unread: VecDeque<u8>,
        fail_first_read: bool,
        fail_normal_timeout: bool,
        probe: ScriptProbe,
    }

    impl ScriptedTransport {
        fn new(messages: Vec<pb::Main>, fail_first_read: bool) -> (Self, ScriptProbe) {
            let input: VecDeque<u8> = messages
                .into_iter()
                .flat_map(|message| frame_bytes(&message))
                .collect();
            let probe = ScriptProbe {
                written: Arc::new(Mutex::new(Vec::new())),
                read_threads: Arc::new(Mutex::new(Vec::new())),
                read_calls: Arc::new(AtomicUsize::new(0)),
                remaining: Arc::new(AtomicUsize::new(input.len())),
                dropped: Arc::new(AtomicBool::new(false)),
                timeouts: Arc::new(Mutex::new(Vec::new())),
                current_timeout: Arc::new(Mutex::new(None)),
                write_timeouts: Arc::new(Mutex::new(Vec::new())),
            };
            let actor_probe = probe.clone();
            (
                Self {
                    input,
                    unread: VecDeque::new(),
                    fail_first_read,
                    fail_normal_timeout: false,
                    probe: actor_probe,
                },
                probe,
            )
        }

        fn available(&self) -> usize {
            self.unread.len() + self.input.len()
        }

        fn pop_byte(&mut self) -> Option<u8> {
            self.unread.pop_front().or_else(|| self.input.pop_front())
        }
    }

    impl Clone for ScriptProbe {
        fn clone(&self) -> Self {
            Self {
                written: Arc::clone(&self.written),
                read_threads: Arc::clone(&self.read_threads),
                read_calls: Arc::clone(&self.read_calls),
                remaining: Arc::clone(&self.remaining),
                dropped: Arc::clone(&self.dropped),
                timeouts: Arc::clone(&self.timeouts),
                current_timeout: Arc::clone(&self.current_timeout),
                write_timeouts: Arc::clone(&self.write_timeouts),
            }
        }
    }

    impl Drop for ScriptedTransport {
        fn drop(&mut self) {
            self.probe.dropped.store(true, Ordering::Release);
        }
    }

    impl Transport for ScriptedTransport {
        fn read_exact(&mut self, buffer: &mut [u8]) -> io::Result<()> {
            self.probe.read_calls.fetch_add(1, Ordering::SeqCst);
            self.probe
                .read_threads
                .lock()
                .unwrap()
                .push(std::thread::current().id());
            if self.fail_first_read {
                self.fail_first_read = false;
                return Err(io::ErrorKind::TimedOut.into());
            }
            if self.available() < buffer.len() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            for byte in buffer {
                *byte = self.pop_byte().expect("availability was checked");
            }
            self.probe
                .remaining
                .store(self.input.len(), Ordering::Release);
            Ok(())
        }

        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if buffer.is_empty() {
                return Ok(0);
            }
            self.read_exact(&mut buffer[..1])?;
            Ok(1)
        }

        fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.probe.written.lock().unwrap().extend_from_slice(bytes);
            let timeout = *self.probe.current_timeout.lock().unwrap();
            self.probe.write_timeouts.lock().unwrap().push(timeout);
            Ok(())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn set_timeout(&mut self, duration: Duration) -> io::Result<()> {
            *self.probe.current_timeout.lock().unwrap() = Some(duration);
            self.probe.timeouts.lock().unwrap().push(duration);
            if self.fail_normal_timeout && duration == SERIAL_TIMEOUT_NORMAL {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "scripted normal-timeout restoration failure",
                ));
            }
            Ok(())
        }

        fn unread(&mut self, bytes: &[u8]) {
            for byte in bytes.iter().rev() {
                self.unread.push_front(*byte);
            }
        }

        fn kind(&self) -> TransportKind {
            TransportKind::Serial
        }
    }

    #[derive(Clone)]
    struct InteractiveProbe {
        input: Arc<(Mutex<VecDeque<u8>>, Condvar)>,
        written: Arc<Mutex<Vec<u8>>>,
        write_chunks: Arc<Mutex<Vec<Vec<u8>>>>,
        write_timeouts: Arc<Mutex<Vec<Duration>>>,
        read_calls: Arc<AtomicUsize>,
        waiting_reads: Arc<AtomicUsize>,
        block_empty_reads: Arc<AtomicBool>,
        blocked_read_entries: Arc<AtomicUsize>,
        read_threads: Arc<Mutex<Vec<ThreadId>>>,
        timeouts: Arc<Mutex<Vec<Duration>>>,
        flushes: Arc<AtomicUsize>,
        dropped: Arc<AtomicBool>,
    }

    impl InteractiveProbe {
        fn push(&self, message: pb::Main) {
            self.push_bytes(frame_bytes(&message));
        }

        fn push_bytes(&self, bytes: impl IntoIterator<Item = u8>) {
            let (input, available) = &*self.input;
            input.lock().unwrap().extend(bytes);
            available.notify_all();
        }

        fn decoded_writes(&self) -> Vec<pb::Main> {
            decode_messages(&self.written.lock().unwrap())
        }

        fn write_chunks(&self) -> Vec<Vec<u8>> {
            self.write_chunks.lock().unwrap().clone()
        }
    }

    struct InteractiveTransport {
        probe: InteractiveProbe,
        unread: VecDeque<u8>,
        timeout: Duration,
        kind: TransportKind,
    }

    impl InteractiveTransport {
        fn new(messages: Vec<pb::Main>, kind: TransportKind) -> (Self, InteractiveProbe) {
            let input: VecDeque<u8> = messages
                .into_iter()
                .flat_map(|message| frame_bytes(&message))
                .collect();
            let probe = InteractiveProbe {
                input: Arc::new((Mutex::new(input), Condvar::new())),
                written: Arc::new(Mutex::new(Vec::new())),
                write_chunks: Arc::new(Mutex::new(Vec::new())),
                write_timeouts: Arc::new(Mutex::new(Vec::new())),
                read_calls: Arc::new(AtomicUsize::new(0)),
                waiting_reads: Arc::new(AtomicUsize::new(0)),
                block_empty_reads: Arc::new(AtomicBool::new(false)),
                blocked_read_entries: Arc::new(AtomicUsize::new(0)),
                read_threads: Arc::new(Mutex::new(Vec::new())),
                timeouts: Arc::new(Mutex::new(Vec::new())),
                flushes: Arc::new(AtomicUsize::new(0)),
                dropped: Arc::new(AtomicBool::new(false)),
            };
            (
                Self {
                    probe: probe.clone(),
                    unread: VecDeque::new(),
                    timeout: SERIAL_TIMEOUT_NORMAL,
                    kind,
                },
                probe,
            )
        }
    }

    impl Drop for InteractiveTransport {
        fn drop(&mut self) {
            self.probe.dropped.store(true, Ordering::Release);
            self.probe.input.1.notify_all();
        }
    }

    impl Transport for InteractiveTransport {
        fn read_exact(&mut self, buffer: &mut [u8]) -> io::Result<()> {
            let mut filled = 0;
            while filled < buffer.len() {
                match self.read(&mut buffer[filled..]) {
                    Ok(0) => {
                        self.unread(&buffer[..filled]);
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "interactive transport returned zero bytes",
                        ));
                    }
                    Ok(read) => filled += read,
                    Err(error) => {
                        self.unread(&buffer[..filled]);
                        return Err(error);
                    }
                }
            }
            Ok(())
        }

        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if buffer.is_empty() {
                return Ok(0);
            }
            self.probe.read_calls.fetch_add(1, Ordering::SeqCst);
            self.probe
                .read_threads
                .lock()
                .unwrap()
                .push(std::thread::current().id());

            if !self.unread.is_empty() {
                let take = buffer.len().min(self.unread.len());
                for slot in &mut buffer[..take] {
                    *slot = self.unread.pop_front().unwrap();
                }
                return Ok(take);
            }

            let (input, available) = &*self.probe.input;
            let mut input = input.lock().unwrap();
            if input.is_empty() {
                self.probe.waiting_reads.fetch_add(1, Ordering::SeqCst);
                if self.probe.block_empty_reads.load(Ordering::Acquire) {
                    self.probe
                        .blocked_read_entries
                        .fetch_add(1, Ordering::SeqCst);
                    while input.is_empty() && self.probe.block_empty_reads.load(Ordering::Acquire) {
                        input = available.wait(input).unwrap();
                    }
                    self.probe.waiting_reads.fetch_sub(1, Ordering::SeqCst);
                } else {
                    let (next, timeout) = available.wait_timeout(input, self.timeout).unwrap();
                    input = next;
                    self.probe.waiting_reads.fetch_sub(1, Ordering::SeqCst);
                    if timeout.timed_out() && input.is_empty() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "interactive read timeout",
                        ));
                    }
                }
            }
            let take = buffer.len().min(input.len());
            for slot in &mut buffer[..take] {
                *slot = input.pop_front().unwrap();
            }
            Ok(take)
        }

        fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.probe.written.lock().unwrap().extend_from_slice(bytes);
            self.probe.write_chunks.lock().unwrap().push(bytes.to_vec());
            self.probe.write_timeouts.lock().unwrap().push(self.timeout);
            Ok(())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.probe.flushes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn set_timeout(&mut self, duration: Duration) -> io::Result<()> {
            self.timeout = duration;
            self.probe.timeouts.lock().unwrap().push(duration);
            Ok(())
        }

        fn unread(&mut self, bytes: &[u8]) {
            for byte in bytes.iter().rev() {
                self.unread.push_front(*byte);
            }
        }

        fn kind(&self) -> TransportKind {
            self.kind
        }
    }

    struct FatalReadTransport {
        started: Option<std_mpsc::Sender<()>>,
        release: std_mpsc::Receiver<()>,
        dropped: Arc<AtomicBool>,
        written: Vec<u8>,
    }

    struct PanickingReadTransport {
        started: Option<std_mpsc::Sender<()>>,
        release: std_mpsc::Receiver<()>,
        dropped: Arc<AtomicBool>,
    }

    impl Drop for PanickingReadTransport {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    impl Transport for PanickingReadTransport {
        fn read_exact(&mut self, _buffer: &mut [u8]) -> io::Result<()> {
            if let Some(started) = self.started.take() {
                started.send(()).unwrap();
            }
            self.release.recv_timeout(TEST_TIMEOUT).unwrap();
            panic!("scripted typed transport panic");
        }

        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.read_exact(buffer)?;
            Ok(buffer.len())
        }

        fn write_all(&mut self, _bytes: &[u8]) -> io::Result<()> {
            Ok(())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn set_timeout(&mut self, _duration: Duration) -> io::Result<()> {
            Ok(())
        }

        fn unread(&mut self, _bytes: &[u8]) {}

        fn kind(&self) -> TransportKind {
            TransportKind::Serial
        }
    }

    impl Drop for FatalReadTransport {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    impl Transport for FatalReadTransport {
        fn read_exact(&mut self, _buffer: &mut [u8]) -> io::Result<()> {
            if let Some(started) = self.started.take() {
                started.send(()).unwrap();
            }
            self.release.recv_timeout(TEST_TIMEOUT).unwrap();
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "scripted fatal read",
            ))
        }

        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.read_exact(buffer)?;
            Ok(buffer.len())
        }

        fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.written.extend_from_slice(bytes);
            Ok(())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn set_timeout(&mut self, _duration: Duration) -> io::Result<()> {
            Ok(())
        }

        fn unread(&mut self, _bytes: &[u8]) {}

        fn kind(&self) -> TransportKind {
            TransportKind::Serial
        }
    }

    fn frame_bytes(message: &pb::Main) -> Vec<u8> {
        let body = message.encode_to_vec();
        let mut framed = encode_varint(body.len() as u64);
        framed.extend_from_slice(&body);
        framed
    }

    fn encode_varint(mut value: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            bytes.push(byte);
            if value == 0 {
                return bytes;
            }
        }
    }

    fn decode_first_message(bytes: &[u8]) -> pb::Main {
        let mut length = 0_u64;
        let mut shift = 0_u32;
        let mut prefix = 0_usize;
        for byte in bytes {
            prefix += 1;
            length |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        pb::Main::decode(&bytes[prefix..prefix + length as usize]).unwrap()
    }

    fn decode_messages(mut bytes: &[u8]) -> Vec<pb::Main> {
        let mut messages = Vec::new();
        while !bytes.is_empty() {
            let mut length = 0_u64;
            let mut shift = 0_u32;
            let mut prefix = 0_usize;
            loop {
                let byte = bytes[prefix];
                prefix += 1;
                length |= u64::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            let end = prefix + length as usize;
            messages.push(pb::Main::decode(&bytes[prefix..end]).unwrap());
            bytes = &bytes[end..];
        }
        messages
    }

    fn empty_content() -> Content {
        Content::Empty(pb::Empty {})
    }

    fn response(command_id: u32, has_next: bool) -> pb::Main {
        response_with_status(command_id, has_next, 0)
    }

    fn response_with_status(command_id: u32, has_next: bool, command_status: i32) -> pb::Main {
        pb::Main {
            command_id,
            command_status,
            has_next,
            content: Some(empty_content()),
        }
    }

    fn ping_response(command_id: u32, data: &[u8]) -> pb::Main {
        pb::Main {
            command_id,
            command_status: 0,
            has_next: false,
            content: Some(Content::SystemPingResponse(pb_system::PingResponse {
                data: data.to_vec(),
            })),
        }
    }

    fn screen_frame(marker: u8) -> pb::Main {
        screen_frame_with_id(0, marker)
    }

    fn screen_frame_with_id(command_id: u32, marker: u8) -> pb::Main {
        stream_frame_with_id(command_id, marker, false)
    }

    fn stream_frame_with_id(command_id: u32, marker: u8, has_next: bool) -> pb::Main {
        pb::Main {
            command_id,
            command_status: 0,
            has_next,
            content: Some(Content::GuiScreenFrame(pb_gui::ScreenFrame {
                data: vec![marker],
                orientation: 0,
                bg_color: 0,
                fg_color: 1,
            })),
        }
    }

    fn screen_content_with_request_len(target: usize) -> Content {
        let mut data_len = target;
        for _ in 0..8 {
            let content = Content::GuiScreenFrame(pb_gui::ScreenFrame {
                data: vec![0x5a; data_len],
                orientation: 0,
                bg_color: 0,
                fg_color: 1,
            });
            let encoded_len = pb::Main {
                command_id: 1,
                command_status: 0,
                has_next: false,
                content: Some(content.clone()),
            }
            .encoded_len();
            if encoded_len == target {
                return content;
            }
            if encoded_len > target {
                data_len -= encoded_len - target;
            } else {
                data_len += target - encoded_len;
            }
        }
        panic!("could not build an RPC request with encoded length {target}");
    }

    fn scripted_handle(
        messages: Vec<pb::Main>,
        fail_first_read: bool,
    ) -> (ConnectionHandle, ScriptProbe) {
        scripted_handle_with_config(messages, fail_first_read, ActorConfig::default())
    }

    fn scripted_handle_with_config(
        messages: Vec<pb::Main>,
        fail_first_read: bool,
        config: ActorConfig,
    ) -> (ConnectionHandle, ScriptProbe) {
        let (transport, probe) = ScriptedTransport::new(messages, fail_first_read);
        let client = FlipperClient::new(Box::new(transport));
        (
            ConnectionHandle::spawn_with_config(client, 4, ConnectionMode::Rpc, config).unwrap(),
            probe,
        )
    }

    fn interactive_handle(
        messages: Vec<pb::Main>,
        kind: TransportKind,
    ) -> (ConnectionHandle, InteractiveProbe) {
        interactive_handle_with_capacity(messages, kind, 8)
    }

    fn interactive_handle_with_capacity(
        messages: Vec<pb::Main>,
        kind: TransportKind,
        capacity: usize,
    ) -> (ConnectionHandle, InteractiveProbe) {
        let (transport, probe) = InteractiveTransport::new(messages, kind);
        let client = FlipperClient::new(Box::new(transport));
        (
            ConnectionHandle::spawn_with_capacity(client, capacity).unwrap(),
            probe,
        )
    }

    struct RecordingTransport {
        writes: Writes,
        dropped: Option<Arc<AtomicBool>>,
    }

    impl RecordingTransport {
        fn new() -> (Self, Writes) {
            let writes = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    writes: Arc::clone(&writes),
                    dropped: None,
                },
                writes,
            )
        }

        fn drop_aware() -> (Self, Arc<AtomicBool>) {
            let dropped = Arc::new(AtomicBool::new(false));
            (
                Self {
                    writes: Arc::new(Mutex::new(Vec::new())),
                    dropped: Some(Arc::clone(&dropped)),
                },
                dropped,
            )
        }
    }

    impl Drop for RecordingTransport {
        fn drop(&mut self) {
            if let Some(dropped) = &self.dropped {
                dropped.store(true, Ordering::Release);
            }
        }
    }

    impl Transport for RecordingTransport {
        fn read_exact(&mut self, _buf: &mut [u8]) -> io::Result<()> {
            Err(io::ErrorKind::TimedOut.into())
        }

        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::TimedOut.into())
        }

        fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.writes
                .lock()
                .unwrap()
                .push((std::thread::current().id(), bytes.to_vec()));
            Ok(())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn set_timeout(&mut self, _duration: Duration) -> io::Result<()> {
            Ok(())
        }

        fn unread(&mut self, _bytes: &[u8]) {}

        fn kind(&self) -> TransportKind {
            TransportKind::Serial
        }
    }

    fn test_handle(capacity: usize) -> (ConnectionHandle, Writes) {
        let (transport, writes) = RecordingTransport::new();
        let client = FlipperClient::new(Box::new(transport));
        (
            ConnectionHandle::spawn_with_capacity(client, capacity).unwrap(),
            writes,
        )
    }

    fn test_handle_in_mode(capacity: usize, mode: ConnectionMode) -> (ConnectionHandle, Writes) {
        let (transport, writes) = RecordingTransport::new();
        let client = FlipperClient::new(Box::new(transport));
        (
            ConnectionHandle::spawn_with_mode(client, capacity, mode).unwrap(),
            writes,
        )
    }

    fn drop_aware_handle(capacity: usize) -> (ConnectionHandle, Arc<AtomicBool>) {
        let (transport, dropped) = RecordingTransport::drop_aware();
        let client = FlipperClient::new(Box::new(transport));
        (
            ConnectionHandle::spawn_with_capacity(client, capacity).unwrap(),
            dropped,
        )
    }

    async fn bounded<F: Future>(future: F) -> F::Output {
        tokio::time::timeout(TEST_TIMEOUT, future)
            .await
            .expect("operation exceeded deterministic test timeout")
    }

    async fn next_screen_frame(
        receiver: &mut watch::Receiver<Option<pb::Main>>,
    ) -> Option<pb::Main> {
        loop {
            receiver.changed().await.ok()?;
            if let Some(frame) = receiver.borrow_and_update().clone() {
                return Some(frame);
            }
        }
    }

    async fn receive<T>(receiver: oneshot::Receiver<ActorResult<T>>) -> ActorResult<T> {
        bounded(receiver)
            .await
            .unwrap_or(Err(ConnectionActorError::ActorStopped))
    }

    async fn shutdown(handle: &ConnectionHandle) {
        bounded(handle.shutdown()).await.unwrap();
    }

    async fn wait_for_write_chunk(probe: &InteractiveProbe, expected: &[u8]) {
        bounded(async {
            loop {
                if probe
                    .write_chunks
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|chunk| chunk.as_slice() == expected)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
    }

    async fn wait_for_input_empty(probe: &InteractiveProbe) {
        bounded(async {
            loop {
                if probe.input.0.lock().unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
    }

    async fn enter_interactive_cli(
        handle: &ConnectionHandle,
        output: &mut broadcast::Receiver<CliOutputEvent>,
    ) -> u64 {
        bounded(handle.enter_cli()).await.unwrap();
        match bounded(output.recv()).await.unwrap() {
            CliOutputEvent::SessionStarted { session_id } => session_id,
            event => panic!("expected CLI session start, got {event:?}"),
        }
    }

    async fn finish_interactive_cli_exit(
        handle: &ConnectionHandle,
        probe: &InteractiveProbe,
        ping_command_id: u32,
    ) -> ActorResult<()> {
        let prior_starts = probe
            .write_chunks
            .lock()
            .unwrap()
            .iter()
            .filter(|chunk| chunk.as_slice() == CLI_START_RPC_SESSION)
            .count();
        let exit = handle.submit_exit_cli()?;
        bounded(async {
            loop {
                let starts = probe
                    .write_chunks
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|chunk| chunk.as_slice() == CLI_START_RPC_SESSION)
                    .count();
                if starts > prior_starts {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        let mut handoff = CLI_RPC_HANDOFF_MARKER.to_vec();
        handoff.extend(frame_bytes(&ping_response(
            ping_command_id,
            CLI_PING_PAYLOAD,
        )));
        probe.push_bytes(handoff);
        receive(exit).await
    }

    async fn wait_for_state(handle: &ConnectionHandle, expected: ConnectionState) {
        bounded(async {
            while handle.state() != expected {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }

    async fn wait_for_screen_subscription_closed(receiver: &mut watch::Receiver<Option<pb::Main>>) {
        bounded(async {
            while receiver.changed().await.is_ok() {
                receiver.borrow_and_update();
            }
        })
        .await;
    }

    async fn wait_for_count(counter: &AtomicUsize, expected: usize) {
        bounded(async {
            while counter.load(Ordering::Acquire) < expected {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }

    struct EmptyReadBlock {
        probe: InteractiveProbe,
    }

    impl Drop for EmptyReadBlock {
        fn drop(&mut self) {
            self.probe.block_empty_reads.store(false, Ordering::Release);
            self.probe.input.1.notify_all();
        }
    }

    async fn block_next_empty_read(probe: &InteractiveProbe) -> EmptyReadBlock {
        let expected_entry = probe.blocked_read_entries.load(Ordering::Acquire) + 1;
        probe.block_empty_reads.store(true, Ordering::Release);
        wait_for_count(&probe.blocked_read_entries, expected_entry).await;
        EmptyReadBlock {
            probe: probe.clone(),
        }
    }

    fn wait_for_screen_terminal_attempt(handle: &ConnectionHandle, expected: u64) {
        let deadline = Instant::now() + TEST_TIMEOUT;
        while handle.screen_terminal_attempts.load(Ordering::Acquire) < expected {
            assert!(
                Instant::now() < deadline,
                "actor did not reach screen-terminal publication"
            );
            std::thread::yield_now();
        }
    }

    fn assert_terminal_input_rejected_before_rpc(
        handle: &ConnectionHandle,
        input: &mut oneshot::Receiver<ActorResult<()>>,
        active_id: u32,
    ) {
        assert_eq!(handle.state(), ConnectionState::Rpc);
        assert!(matches!(
            input.try_recv(),
            Ok(Err(ConnectionActorError::ScreenStreamEndedDuringInput {
                command_id,
                command_status: 14
            })) if command_id == active_id
        ));
    }

    async fn acknowledge_start(handle: &ConnectionHandle, probe: &InteractiveProbe) -> u32 {
        let expected_flushes = probe.flushes.load(Ordering::Acquire) + 1;
        let receiver = handle.submit_start_screen_stream().unwrap();
        wait_for_count(&probe.flushes, expected_flushes).await;
        let command_id = probe.decoded_writes().last().unwrap().command_id;
        probe.push(response(command_id, false));
        receive(receiver).await.unwrap();
        command_id
    }

    async fn acknowledge_stop(handle: &ConnectionHandle, probe: &InteractiveProbe) {
        let mut expected_flushes = probe.flushes.load(Ordering::Acquire) + 1;
        let receiver = handle.submit_stop_screen_stream().unwrap();
        loop {
            wait_for_count(&probe.flushes, expected_flushes).await;
            let command = probe.decoded_writes().last().unwrap().clone();
            let is_stop = matches!(
                &command.content,
                Some(Content::GuiStopScreenStreamRequest(_))
            );
            assert!(
                is_stop
                    || matches!(
                        &command.content,
                        Some(Content::GuiSendInputEventRequest(
                            pb_gui::SendInputEventRequest {
                                r#type: INPUT_RELEASE,
                                ..
                            }
                        ))
                    ),
                "Stop cleanup may write only held-key RELEASE commands before Stop"
            );
            probe.push(response(command.command_id, false));
            if is_stop {
                break;
            }
            expected_flushes += 1;
        }
        receive(receiver).await.unwrap();
    }

    #[tokio::test]
    async fn client_has_one_owner_thread_and_jobs_execute_serially() {
        let main_thread = std::thread::current().id();
        let (handle, writes) = test_handle(4);

        let first = handle.submit_rpc(|client| {
            client.transport.write_all(b"first")?;
            Ok(std::thread::current().id())
        });
        let second = handle.submit_rpc(|client| {
            client.transport.write_all(b"second")?;
            Ok(std::thread::current().id())
        });

        let first_thread = receive(first.unwrap()).await.unwrap();
        let second_thread = receive(second.unwrap()).await.unwrap();
        assert_ne!(first_thread, main_thread);
        assert_eq!(first_thread, second_thread);
        {
            let recorded = writes.lock().unwrap();
            assert_eq!(recorded[0].0, first_thread);
            assert_eq!(recorded[1].0, first_thread);
            assert_eq!(recorded[0].1, b"first");
            assert_eq!(recorded[1].1, b"second");
        }

        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn bounded_queue_reports_backpressure_without_blocking_async_worker() {
        let (handle, _) = test_handle(1);
        let (started_tx, started_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();

        let running = handle
            .submit_rpc(move |_| {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(TEST_TIMEOUT).unwrap();
                Ok(1_u8)
            })
            .unwrap();
        started_rx.recv_timeout(TEST_TIMEOUT).unwrap();

        let queued = handle.submit_rpc(|_| Ok(2_u8)).unwrap();
        let rejected = bounded(handle.execute_legacy_rpc(|_| Ok(3_u8))).await;
        assert!(matches!(rejected, Err(ConnectionActorError::QueueFull)));

        release_tx.send(()).unwrap();
        assert_eq!(receive(running).await.unwrap(), 1);
        assert_eq!(receive(queued).await.unwrap(), 2);
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn retryable_error_classification_alone_preserves_actor_availability() {
        let (handle, writes) = test_handle(2);

        let first = handle.execute_legacy_rpc::<(), _>(|_| {
            Err(FlipperError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "scripted timeout",
            )))
        });
        let first = bounded(first).await;
        assert!(matches!(first, Err(ConnectionActorError::Device(_))));
        assert_eq!(handle.state(), ConnectionState::Rpc);

        let retry = handle.execute_legacy_rpc(|client| {
            client.transport.write_all(b"retry")?;
            Ok(())
        });
        bounded(retry).await.unwrap();
        assert_eq!(writes.lock().unwrap().len(), 1);
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn fatal_failure_stops_actor_and_fails_queued_and_later_work() {
        let (handle, dropped) = drop_aware_handle(2);
        let (started_tx, started_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();

        let fatal = handle
            .submit_rpc(move |_| -> FlipperResult<()> {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(TEST_TIMEOUT).unwrap();
                Err(FlipperError::Io(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "scripted disconnect",
                )))
            })
            .unwrap();
        started_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        let pending = handle.submit_rpc(|_| Ok(())).unwrap();
        release_tx.send(()).unwrap();

        assert!(matches!(
            receive(fatal).await,
            Err(ConnectionActorError::Device(_))
        ));
        assert_eq!(handle.state(), ConnectionState::Disconnected);
        assert!(dropped.load(Ordering::Acquire));
        assert!(matches!(
            receive(pending).await,
            Err(ConnectionActorError::ConnectionLost { cause })
                if cause.contains("scripted disconnect")
        ));
        assert_eq!(handle.state(), ConnectionState::Disconnected);
        assert!(matches!(
            bounded(handle.execute_legacy_rpc(|_| Ok(()))).await,
            Err(ConnectionActorError::Closed)
        ));
    }

    #[tokio::test]
    async fn dropped_reply_receiver_does_not_stop_actor() {
        let (handle, writes) = test_handle(2);
        let (executed_tx, executed_rx) = std_mpsc::channel();
        let dropped = handle
            .submit_rpc(move |client| {
                client.transport.write_all(b"cancelled-caller")?;
                executed_tx.send(()).unwrap();
                Ok(())
            })
            .unwrap();
        drop(dropped);
        executed_rx.recv_timeout(TEST_TIMEOUT).unwrap();

        let still_alive = handle.execute_legacy_rpc(|client| {
            client.transport.write_all(b"still-alive")?;
            Ok(())
        });
        bounded(still_alive).await.unwrap();
        assert_eq!(writes.lock().unwrap().len(), 2);
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn cli_and_screen_modes_reciprocally_reject_rpc_before_wire_io() {
        for (mode, expected_state) in [
            (ConnectionMode::Cli, ConnectionState::Cli),
            (
                ConnectionMode::ScreenStreaming,
                ConnectionState::ScreenStreaming,
            ),
        ] {
            let (handle, writes) = test_handle_in_mode(2, mode);
            let calls = Arc::new(AtomicUsize::new(0));
            let calls_for_job = Arc::clone(&calls);
            let result = bounded(handle.execute_legacy_rpc(move |client| {
                calls_for_job.fetch_add(1, Ordering::SeqCst);
                client.transport.write_all(b"must-not-run")?;
                Ok(())
            }))
            .await;

            assert!(matches!(
                result,
                Err(ConnectionActorError::ModeRejected { current })
                    if current == expected_state
            ));
            assert_eq!(handle.mode(), Some(mode));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(writes.lock().unwrap().is_empty());
            shutdown(&handle).await;
        }
    }

    #[tokio::test]
    async fn screen_controls_reject_illegal_rpc_cli_and_screen_states_before_wire_io() {
        for mode in [ConnectionMode::Cli, ConnectionMode::ScreenStreaming] {
            let (handle, writes) = test_handle_in_mode(2, mode);
            let result = bounded(handle.start_screen_stream()).await;
            assert!(matches!(
                result,
                Err(ConnectionActorError::ModeRejected { current })
                    if current == ConnectionState::from(mode)
            ));
            assert!(writes.lock().unwrap().is_empty());
            shutdown(&handle).await;
        }

        let (handle, writes) = test_handle(2);
        assert!(matches!(
            bounded(handle.stop_screen_stream()).await,
            Err(ConnectionActorError::ModeRejected {
                current: ConnectionState::Rpc
            })
        ));
        assert!(matches!(
            bounded(handle.send_screen_input(0, INPUT_PRESS)).await,
            Err(ConnectionActorError::ModeRejected {
                current: ConnectionState::Rpc
            })
        ));
        assert!(writes.lock().unwrap().is_empty());
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn screen_start_claims_transition_before_wire_and_waits_for_matching_ack() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        let mut frames = handle.subscribe_screen_frames();
        let mut start = handle.submit_start_screen_stream().unwrap();

        assert_eq!(handle.state(), ConnectionState::Transitioning);
        assert!(matches!(
            bounded(handle.request_rpc(empty_content())).await,
            Err(ConnectionActorError::ModeRejected {
                current: ConnectionState::Transitioning
            })
        ));
        wait_for_count(&probe.flushes, 1).await;
        assert!(matches!(
            start.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        probe.push(screen_frame(0x21));
        let routed = bounded(next_screen_frame(&mut frames)).await.unwrap();
        assert!(matches!(
            routed.content,
            Some(Content::GuiScreenFrame(pb_gui::ScreenFrame { data, .. })) if data == [0x21]
        ));
        assert!(matches!(
            start.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        probe.push(response(1, false));
        receive(start).await.unwrap();
        assert_eq!(handle.state(), ConnectionState::ScreenStreaming);

        acknowledge_stop(&handle, &probe).await;
        assert_eq!(handle.state(), ConnectionState::Rpc);
        let writes = probe.decoded_writes();
        assert!(matches!(
            writes[0].content,
            Some(Content::GuiStartScreenStreamRequest(_))
        ));
        assert!(matches!(
            writes[1].content,
            Some(Content::GuiStopScreenStreamRequest(_))
        ));
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn full_control_queue_reverts_screen_start_transition_without_wire_io() {
        let (handle, _) = test_handle(1);
        let (started_tx, started_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let running = handle
            .submit_rpc(move |_| {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(TEST_TIMEOUT).unwrap();
                Ok(())
            })
            .unwrap();
        started_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        let queued = handle.submit_rpc(|_| Ok(())).unwrap();

        let rejected = bounded(handle.start_screen_stream()).await;

        assert!(matches!(rejected, Err(ConnectionActorError::QueueFull)));
        assert_eq!(handle.state(), ConnectionState::Rpc);
        release_tx.send(()).unwrap();
        receive(running).await.unwrap();
        receive(queued).await.unwrap();
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn start_preclaim_preserves_older_rpc_work_in_fifo_order() {
        let (handle, probe) =
            interactive_handle_with_capacity(Vec::new(), TransportKind::Serial, 4);
        let (running_tx, running_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let (queued_tx, queued_rx) = std_mpsc::channel();

        let running = handle
            .submit_rpc(move |_| {
                running_tx.send(()).unwrap();
                release_rx.recv_timeout(TEST_TIMEOUT).unwrap();
                Ok(1_u8)
            })
            .unwrap();
        running_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        let queued = handle
            .submit_rpc(move |_| {
                queued_tx.send(()).unwrap();
                Ok(2_u8)
            })
            .unwrap();
        let start = handle.submit_start_screen_stream().unwrap();
        assert_eq!(handle.state(), ConnectionState::Transitioning);
        assert_eq!(
            ConnectionState::from_byte(handle.state.load(Ordering::Acquire)),
            ConnectionState::Rpc,
            "admission intent must not alter the actor's actual FIFO mode"
        );

        release_tx.send(()).unwrap();
        assert_eq!(receive(running).await.unwrap(), 1);
        assert_eq!(receive(queued).await.unwrap(), 2);
        queued_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        wait_for_count(&probe.flushes, 1).await;

        let start_id = probe.decoded_writes().last().unwrap().command_id;
        probe.push(response(start_id, false));
        receive(start).await.unwrap();
        assert_eq!(handle.state(), ConnectionState::ScreenStreaming);
        acknowledge_stop(&handle, &probe).await;
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn full_control_queue_reverts_screen_stop_transition_and_stream_continues() {
        let (handle, probe) = interactive_handle_with_capacity(Vec::new(), TransportKind::Ble, 1);
        acknowledge_start(&handle, &probe).await;
        let reads = probe.read_calls.load(Ordering::Acquire);
        wait_for_count(&probe.read_calls, reads + 1).await;

        let input = handle.submit_screen_input(0, INPUT_PRESS).unwrap();
        let rejected = bounded(handle.stop_screen_stream()).await;

        assert!(matches!(rejected, Err(ConnectionActorError::QueueFull)));
        assert_eq!(handle.state(), ConnectionState::ScreenStreaming);

        wait_for_count(&probe.flushes, 2).await;
        let input_id = probe.decoded_writes().last().unwrap().command_id;
        probe.push(response(input_id, false));
        receive(input).await.unwrap();
        acknowledge_stop(&handle, &probe).await;
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn failed_stop_rollback_before_idle_terminal_publication_preserves_terminal_cause() {
        let (handle, probe) =
            interactive_handle_with_capacity(Vec::new(), TransportKind::Serial, 1);
        let active_id = acknowledge_start(&handle, &probe).await;
        let read_block = block_next_empty_read(&probe).await;

        // Fill the one-slot queue, then complete the failed Stop rollback
        // while the actor is still inside its idle transport read. This
        // deterministically forces rollback to win the shared admission gate.
        let mut queued_input = handle.submit_screen_input(2, INPUT_PRESS).unwrap();
        let admission = handle.lock_admission();
        handle
            .claim_transition(ConnectionState::ScreenStreaming)
            .unwrap();
        let (stop_reply, _) = oneshot::channel();
        let full =
            handle
                .commands
                .try_send(ActorCommand::StopScreenStream(StopScreenStreamRequest {
                    reply: stop_reply,
                }));
        assert!(matches!(full, Err(mpsc::error::TrySendError::Full(_))));
        handle.revert_transition();
        assert_eq!(handle.state(), ConnectionState::ScreenStreaming);
        drop(admission);

        probe.push(response_with_status(active_id, false, 14));
        wait_for_state(&handle, ConnectionState::Rpc).await;
        assert_terminal_input_rejected_before_rpc(&handle, &mut queued_input, active_id);
        assert_eq!(probe.decoded_writes().len(), 1, "no queued input after end");
        assert!(!probe.dropped.load(Ordering::Acquire));
        drop(read_block);
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn failed_stop_rollback_blocking_idle_terminal_publication_preserves_terminal_cause() {
        let (handle, probe) =
            interactive_handle_with_capacity(Vec::new(), TransportKind::Serial, 1);
        let active_id = acknowledge_start(&handle, &probe).await;
        let read_block = block_next_empty_read(&probe).await;

        let mut queued_input = handle.submit_screen_input(2, INPUT_PRESS).unwrap();
        let admission = handle.lock_admission();
        handle
            .claim_transition(ConnectionState::ScreenStreaming)
            .unwrap();
        let (stop_reply, _) = oneshot::channel();
        let full =
            handle
                .commands
                .try_send(ActorCommand::StopScreenStream(StopScreenStreamRequest {
                    reply: stop_reply,
                }));
        assert!(matches!(full, Err(mpsc::error::TrySendError::Full(_))));

        // Wake the actor while the failed submitter still owns admission. The
        // attempt counter increments immediately before the actor blocks on
        // the same gate, forcing the formerly unsafe publication/rollback
        // interleaving without scheduler timing or transport-mutex pinning.
        let expected_attempt = handle.screen_terminal_attempts.load(Ordering::Acquire) + 1;
        probe.push(response_with_status(active_id, false, 14));
        wait_for_screen_terminal_attempt(&handle, expected_attempt);
        assert_eq!(
            ConnectionState::from_byte(handle.state.load(Ordering::Acquire)),
            ConnectionState::ScreenStreaming,
            "actor publication must not pass the failed submitter's admission gate"
        );
        handle.revert_transition();
        assert_eq!(handle.state(), ConnectionState::ScreenStreaming);
        drop(admission);

        wait_for_state(&handle, ConnectionState::Rpc).await;
        assert_terminal_input_rejected_before_rpc(&handle, &mut queued_input, active_id);
        assert_eq!(probe.decoded_writes().len(), 1, "no queued input after end");
        assert!(!probe.dropped.load(Ordering::Acquire));
        drop(read_block);
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn stop_preclaim_preserves_older_screen_input_in_fifo_order() {
        let (handle, probe) = interactive_handle_with_capacity(Vec::new(), TransportKind::Ble, 4);
        acknowledge_start(&handle, &probe).await;
        let idle_reads = probe.read_calls.load(Ordering::Acquire);
        wait_for_count(&probe.read_calls, idle_reads + 1).await;

        let base_flushes = probe.flushes.load(Ordering::Acquire);
        let input = handle.submit_screen_input(3, INPUT_PRESS).unwrap();
        let stop = handle.submit_stop_screen_stream().unwrap();
        assert_eq!(handle.state(), ConnectionState::Transitioning);
        assert_eq!(
            ConnectionState::from_byte(handle.state.load(Ordering::Acquire)),
            ConnectionState::ScreenStreaming,
            "Stop preclaim must not alter actual mode before older input runs"
        );

        wait_for_count(&probe.flushes, base_flushes + 1).await;
        let input_command = probe.decoded_writes().last().unwrap().clone();
        assert!(matches!(
            input_command.content,
            Some(Content::GuiSendInputEventRequest(_))
        ));
        probe.push(response(input_command.command_id, false));
        receive(input).await.unwrap();

        wait_for_count(&probe.flushes, base_flushes + 2).await;
        let release_command = probe.decoded_writes().last().unwrap().clone();
        assert!(matches!(
            release_command.content,
            Some(Content::GuiSendInputEventRequest(
                pb_gui::SendInputEventRequest {
                    key: 3,
                    r#type: INPUT_RELEASE
                }
            ))
        ));
        probe.push(response(release_command.command_id, false));

        wait_for_count(&probe.flushes, base_flushes + 3).await;
        assert_eq!(handle.state(), ConnectionState::Transitioning);
        let stop_command = probe.decoded_writes().last().unwrap().clone();
        assert!(matches!(
            stop_command.content,
            Some(Content::GuiStopScreenStreamRequest(_))
        ));
        probe.push(response(stop_command.command_id, false));
        receive(stop).await.unwrap();
        assert_eq!(handle.state(), ConnectionState::Rpc);
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn queue_full_transition_revert_race_never_rejects_pre_admitted_rpc() {
        for _ in 0..32 {
            let (handle, probe) =
                interactive_handle_with_capacity(Vec::new(), TransportKind::Serial, 1);
            let (running_tx, running_rx) = std_mpsc::channel();
            let (release_tx, release_rx) = std_mpsc::channel();
            let queued_runs = Arc::new(AtomicUsize::new(0));

            let running = handle
                .submit_rpc(move |_| {
                    running_tx.send(()).unwrap();
                    release_rx.recv_timeout(TEST_TIMEOUT).unwrap();
                    Ok(())
                })
                .unwrap();
            running_rx.recv_timeout(TEST_TIMEOUT).unwrap();
            let queued_runs_for_job = Arc::clone(&queued_runs);
            let queued = handle
                .submit_rpc(move |_| {
                    queued_runs_for_job.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .unwrap();

            let barrier = Arc::new(std::sync::Barrier::new(2));
            let race_handle = handle.clone();
            let race_barrier = Arc::clone(&barrier);
            let starter = std::thread::spawn(move || {
                race_barrier.wait();
                race_handle.submit_start_screen_stream()
            });
            barrier.wait();
            release_tx.send(()).unwrap();

            receive(running).await.unwrap();
            receive(queued).await.unwrap();
            assert_eq!(queued_runs.load(Ordering::SeqCst), 1);

            match starter.join().unwrap() {
                Err(ConnectionActorError::QueueFull) => {
                    assert_eq!(handle.state(), ConnectionState::Rpc);
                }
                Ok(start) => {
                    wait_for_count(&probe.flushes, 1).await;
                    let start_id = probe.decoded_writes().last().unwrap().command_id;
                    probe.push(response(start_id, false));
                    receive(start).await.unwrap();
                    acknowledge_stop(&handle, &probe).await;
                }
                Err(error) => panic!("unexpected start race result: {error}"),
            }
            shutdown(&handle).await;
        }
    }

    #[tokio::test]
    async fn matching_id_screen_frame_is_not_a_start_ack() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        let frames = handle.subscribe_screen_frames();
        let start = handle.submit_start_screen_stream().unwrap();
        wait_for_count(&probe.flushes, 1).await;

        probe.push(stream_frame_with_id(1, 0x31, true));

        assert!(matches!(
            receive(start).await,
            Err(ConnectionActorError::Protocol(
                ConnectionProtocolError::UnexpectedScreenResponse { command_id: 1 }
            ))
        ));
        assert!(frames.borrow().is_none());
        assert_eq!(handle.state(), ConnectionState::Disconnected);
        assert!(probe.dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn continuous_screen_frames_coalesce_for_a_slow_subscriber() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        let frames = handle.subscribe_screen_frames();
        acknowledge_start(&handle, &probe).await;

        probe.push(screen_frame(1));
        probe.push(screen_frame(2));
        probe.push(screen_frame(3));
        bounded(async {
            loop {
                let latest = frames.borrow().clone();
                if matches!(
                    latest.and_then(|frame| frame.content),
                    Some(Content::GuiScreenFrame(pb_gui::ScreenFrame { data, .. }))
                        if data == [3]
                ) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;

        assert_eq!(handle.state(), ConnectionState::ScreenStreaming);
        acknowledge_stop(&handle, &probe).await;
        assert_eq!(handle.state(), ConnectionState::Rpc);
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn new_screen_session_clears_stale_frame_and_allows_resubscription() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        let mut first = handle.subscribe_screen_frames();
        acknowledge_start(&handle, &probe).await;
        probe.push(screen_frame(0x61));
        let first_frame = bounded(next_screen_frame(&mut first)).await.unwrap();
        assert!(matches!(
            first_frame.content,
            Some(Content::GuiScreenFrame(pb_gui::ScreenFrame { data, .. })) if data == [0x61]
        ));
        acknowledge_stop(&handle, &probe).await;
        drop(first);

        let stale_observer = handle.subscribe_screen_frames();
        assert!(stale_observer.borrow().is_some());
        let expected_flushes = probe.flushes.load(Ordering::Acquire) + 1;
        let start = handle.submit_start_screen_stream().unwrap();
        wait_for_count(&probe.flushes, expected_flushes).await;
        assert!(stale_observer.borrow().is_none());

        let start_id = probe.decoded_writes().last().unwrap().command_id;
        probe.push(response(start_id, false));
        receive(start).await.unwrap();
        drop(stale_observer);

        let mut replacement = handle.subscribe_screen_frames();
        assert!(replacement.borrow().is_none());
        probe.push(screen_frame(0x62));
        let replacement_frame = bounded(next_screen_frame(&mut replacement)).await.unwrap();
        assert!(matches!(
            replacement_frame.content,
            Some(Content::GuiScreenFrame(pb_gui::ScreenFrame { data, .. })) if data == [0x62]
        ));
        drop(replacement);

        let late_subscriber = handle.subscribe_screen_frames();
        assert!(matches!(
            late_subscriber.borrow().clone().and_then(|frame| frame.content),
            Some(Content::GuiScreenFrame(pb_gui::ScreenFrame { data, .. })) if data == [0x62]
        ));
        acknowledge_stop(&handle, &probe).await;
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn screen_input_expands_short_and_long_and_serializes_matching_acks() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        let frames = handle.subscribe_screen_frames();
        acknowledge_start(&handle, &probe).await;

        for (input_type, expected_types) in [
            (INPUT_SHORT, [INPUT_PRESS, INPUT_SHORT, INPUT_RELEASE]),
            (INPUT_LONG, [INPUT_PRESS, INPUT_LONG, INPUT_RELEASE]),
        ] {
            let base_flushes = probe.flushes.load(Ordering::Acquire);
            let input = handle.submit_screen_input(4, input_type).unwrap();
            for (index, expected_type) in expected_types.into_iter().enumerate() {
                wait_for_count(&probe.flushes, base_flushes + index + 1).await;
                let command = probe.decoded_writes().last().unwrap().clone();
                assert!(matches!(
                    &command.content,
                    Some(Content::GuiSendInputEventRequest(
                        pb_gui::SendInputEventRequest { key: 4, r#type }
                    )) if *r#type == expected_type
                ));
                probe.push(screen_frame(command.command_id as u8));
                probe.push(response(command.command_id, false));
            }
            receive(input).await.unwrap();
        }

        let latest = frames.borrow().clone().unwrap();
        assert!(matches!(
            latest.content,
            Some(Content::GuiScreenFrame(pb_gui::ScreenFrame { data, .. })) if data == [7]
        ));
        assert_eq!(handle.state(), ConnectionState::ScreenStreaming);
        acknowledge_stop(&handle, &probe).await;
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn held_screen_input_forwards_every_key_lifecycle_event_once() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        acknowledge_start(&handle, &probe).await;
        let repeat = pb_gui::InputType::Repeat as i32;

        for key in 0..6 {
            for input_type in [INPUT_PRESS, INPUT_LONG, repeat, INPUT_RELEASE] {
                let base_flushes = probe.flushes.load(Ordering::Acquire);
                let input = handle.submit_screen_input(key, input_type).unwrap();
                wait_for_count(&probe.flushes, base_flushes + 1).await;
                let command = probe.decoded_writes().last().unwrap().clone();
                assert!(matches!(
                    &command.content,
                    Some(Content::GuiSendInputEventRequest(
                        pb_gui::SendInputEventRequest { key: sent_key, r#type }
                    )) if *sent_key == key && *r#type == input_type
                ));
                probe.push(response(command.command_id, false));
                receive(input).await.unwrap();
                assert_eq!(probe.flushes.load(Ordering::Acquire), base_flushes + 1);
            }
        }

        assert_eq!(handle.state(), ConnectionState::ScreenStreaming);
        acknowledge_stop(&handle, &probe).await;
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn terminal_stream_during_input_ack_returns_nonfatal_end_and_stops_expansion() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        let active_id = acknowledge_start(&handle, &probe).await;
        let base_flushes = probe.flushes.load(Ordering::Acquire);
        let input = handle.submit_screen_input(4, INPUT_SHORT).unwrap();
        wait_for_count(&probe.flushes, base_flushes + 1).await;
        let input_id = probe.decoded_writes().last().unwrap().command_id;

        probe.push(response_with_status(active_id, false, 14));
        probe.push(response(input_id, false));

        wait_for_count(&probe.flushes, base_flushes + 2).await;
        let release = probe.decoded_writes().last().unwrap().clone();
        assert!(matches!(
            release.content,
            Some(Content::GuiSendInputEventRequest(
                pb_gui::SendInputEventRequest {
                    key: 4,
                    r#type: INPUT_RELEASE
                }
            ))
        ));
        probe.push(response(release.command_id, false));

        assert!(matches!(
            receive(input).await,
            Err(ConnectionActorError::ScreenStreamEndedDuringInput {
                command_id,
                command_status: 14
            }) if command_id == active_id
        ));
        assert_eq!(handle.state(), ConnectionState::Rpc);
        let input_writes: Vec<_> = probe
            .decoded_writes()
            .into_iter()
            .filter_map(|message| match message.content {
                Some(Content::GuiSendInputEventRequest(event)) => Some(event),
                _ => None,
            })
            .collect();
        assert_eq!(input_writes.len(), 2);
        assert_eq!(input_writes[0].r#type, INPUT_PRESS);
        assert_eq!(input_writes[1].r#type, INPUT_RELEASE);
        assert_eq!(
            probe.timeouts.lock().unwrap().last(),
            Some(&SERIAL_TIMEOUT_NORMAL)
        );
        assert!(!probe.dropped.load(Ordering::Acquire));
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn input_ack_terminal_rejects_later_queued_input_without_wire_io() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        let active_id = acknowledge_start(&handle, &probe).await;
        let base_flushes = probe.flushes.load(Ordering::Acquire);
        let first = handle.submit_screen_input(4, INPUT_SHORT).unwrap();
        wait_for_count(&probe.flushes, base_flushes + 1).await;
        let press_id = probe.decoded_writes().last().unwrap().command_id;
        let queued = handle.submit_screen_input(2, INPUT_PRESS).unwrap();

        probe.push(response_with_status(active_id, false, 14));
        probe.push(response(press_id, false));
        wait_for_count(&probe.flushes, base_flushes + 2).await;
        let release = probe.decoded_writes().last().unwrap().clone();
        assert!(matches!(
            release.content,
            Some(Content::GuiSendInputEventRequest(
                pb_gui::SendInputEventRequest {
                    key: 4,
                    r#type: INPUT_RELEASE
                }
            ))
        ));
        probe.push(response(release.command_id, false));

        assert!(matches!(
            receive(first).await,
            Err(ConnectionActorError::ScreenStreamEndedDuringInput {
                command_id,
                command_status: 14
            }) if command_id == active_id
        ));
        assert!(matches!(
            receive(queued).await,
            Err(ConnectionActorError::ScreenStreamEndedDuringInput {
                command_id,
                command_status: 14
            }) if command_id == active_id
        ));
        assert_eq!(handle.state(), ConnectionState::Rpc);
        let input_events: Vec<_> = probe
            .decoded_writes()
            .into_iter()
            .filter_map(|message| match message.content {
                Some(Content::GuiSendInputEventRequest(event)) => Some(event),
                _ => None,
            })
            .collect();
        assert_eq!(input_events.len(), 2);
        assert_eq!(input_events[0].r#type, INPUT_PRESS);
        assert_eq!(input_events[1].r#type, INPUT_RELEASE);
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn queued_stop_keeps_its_ack_after_terminal_stream_during_input() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        let active_id = acknowledge_start(&handle, &probe).await;
        let idle_reads = probe.read_calls.load(Ordering::Acquire);
        wait_for_count(&probe.read_calls, idle_reads + 1).await;
        let base_flushes = probe.flushes.load(Ordering::Acquire);

        let input = handle.submit_screen_input(4, INPUT_SHORT).unwrap();
        let stop = handle.submit_stop_screen_stream().unwrap();
        wait_for_count(&probe.flushes, base_flushes + 1).await;
        let input_id = probe.decoded_writes().last().unwrap().command_id;
        probe.push(response_with_status(active_id, false, 14));
        probe.push(response(input_id, false));

        wait_for_count(&probe.flushes, base_flushes + 2).await;
        let release = probe.decoded_writes().last().unwrap().clone();
        assert!(matches!(
            release.content,
            Some(Content::GuiSendInputEventRequest(
                pb_gui::SendInputEventRequest {
                    key: 4,
                    r#type: INPUT_RELEASE
                }
            ))
        ));
        probe.push(response(release.command_id, false));

        assert!(matches!(
            receive(input).await,
            Err(ConnectionActorError::ScreenStreamEndedDuringInput {
                command_id,
                command_status: 14
            }) if command_id == active_id
        ));
        wait_for_count(&probe.flushes, base_flushes + 3).await;
        let writes = probe.decoded_writes();
        assert_eq!(
            writes
                .iter()
                .filter(|message| matches!(
                    message.content,
                    Some(Content::GuiSendInputEventRequest(_))
                ))
                .count(),
            2
        );
        let stop_id = writes.last().unwrap().command_id;
        assert!(matches!(
            writes.last().unwrap().content,
            Some(Content::GuiStopScreenStreamRequest(_))
        ));
        probe.push(response(stop_id, false));
        receive(stop).await.unwrap();
        assert_eq!(handle.state(), ConnectionState::Rpc);
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn start_input_and_stop_acks_survive_multiple_usb_and_ble_poll_timeouts() {
        for kind in [TransportKind::Serial, TransportKind::Ble] {
            let (handle, probe) = interactive_handle(Vec::new(), kind);

            let start = handle.submit_start_screen_stream().unwrap();
            wait_for_count(&probe.flushes, 1).await;
            let reads = probe.read_calls.load(Ordering::Acquire);
            wait_for_count(&probe.read_calls, reads + 2).await;
            let start_id = probe.decoded_writes().last().unwrap().command_id;
            probe.push(response(start_id, false));
            receive(start).await.unwrap();

            let input = handle.submit_screen_input(2, INPUT_PRESS).unwrap();
            wait_for_count(&probe.flushes, 2).await;
            let reads = probe.read_calls.load(Ordering::Acquire);
            wait_for_count(&probe.read_calls, reads + 2).await;
            let input_id = probe.decoded_writes().last().unwrap().command_id;
            probe.push(response(input_id, false));
            receive(input).await.unwrap();

            let stop = handle.submit_stop_screen_stream().unwrap();
            wait_for_count(&probe.flushes, 3).await;
            let reads = probe.read_calls.load(Ordering::Acquire);
            wait_for_count(&probe.read_calls, reads + 2).await;
            let release_id = probe.decoded_writes().last().unwrap().command_id;
            probe.push(response(release_id, false));

            wait_for_count(&probe.flushes, 4).await;
            let reads = probe.read_calls.load(Ordering::Acquire);
            wait_for_count(&probe.read_calls, reads + 2).await;
            let stop_id = probe.decoded_writes().last().unwrap().command_id;
            probe.push(response(stop_id, false));
            receive(stop).await.unwrap();

            assert_eq!(handle.state(), ConnectionState::Rpc);
            assert!(!probe.dropped.load(Ordering::Acquire));
            shutdown(&handle).await;
        }
    }

    #[tokio::test]
    async fn invalid_screen_input_is_rejected_before_control_or_wire_admission() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        acknowledge_start(&handle, &probe).await;
        let flushes = probe.flushes.load(Ordering::Acquire);

        assert!(matches!(
            bounded(handle.send_screen_input(6, INPUT_PRESS)).await,
            Err(ConnectionActorError::InvalidScreenInputKey(6))
        ));
        assert!(matches!(
            bounded(handle.send_screen_input(0, 5)).await,
            Err(ConnectionActorError::InvalidScreenInputType(5))
        ));
        assert_eq!(probe.flushes.load(Ordering::Acquire), flushes);

        acknowledge_stop(&handle, &probe).await;
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn screen_stop_routes_outstanding_frames_until_its_terminal_ack() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        let frames = handle.subscribe_screen_frames();
        let active_id = acknowledge_start(&handle, &probe).await;
        let stop = handle.submit_stop_screen_stream().unwrap();
        wait_for_count(&probe.flushes, 2).await;
        let stop_id = probe.decoded_writes().last().unwrap().command_id;

        probe.push(screen_frame(0x41));
        probe.push(response_with_status(active_id, false, 14));
        probe.push(response(stop_id, false));

        receive(stop).await.unwrap();
        assert_eq!(handle.state(), ConnectionState::Rpc);
        let latest = frames.borrow().clone().unwrap();
        assert!(matches!(
            latest.content,
            Some(Content::GuiScreenFrame(pb_gui::ScreenFrame { data, .. })) if data == [0x41]
        ));
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn spontaneous_terminal_stream_response_returns_safely_to_rpc() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        let active_id = acknowledge_start(&handle, &probe).await;

        probe.push(response_with_status(active_id, false, 14));
        wait_for_state(&handle, ConnectionState::Rpc).await;

        assert!(!probe.dropped.load(Ordering::Acquire));
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn spontaneous_terminal_releases_acknowledged_held_key_before_rpc() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        let active_id = acknowledge_start(&handle, &probe).await;
        let press = handle.submit_screen_input(5, INPUT_PRESS).unwrap();
        wait_for_count(&probe.flushes, 2).await;
        let press_id = probe.decoded_writes().last().unwrap().command_id;
        probe.push(response(press_id, false));
        receive(press).await.unwrap();

        probe.push(response_with_status(active_id, false, 14));
        wait_for_count(&probe.flushes, 3).await;
        let release = probe.decoded_writes().last().unwrap().clone();
        assert!(matches!(
            release.content,
            Some(Content::GuiSendInputEventRequest(
                pb_gui::SendInputEventRequest {
                    key: 5,
                    r#type: INPUT_RELEASE
                }
            ))
        ));
        assert_eq!(handle.state(), ConnectionState::Transitioning);
        probe.push(response(release.command_id, false));
        wait_for_state(&handle, ConnectionState::Rpc).await;

        assert!(!probe.dropped.load(Ordering::Acquire));
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn failed_terminal_held_key_release_forces_session_teardown() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        let active_id = acknowledge_start(&handle, &probe).await;
        let press = handle.submit_screen_input(1, INPUT_PRESS).unwrap();
        wait_for_count(&probe.flushes, 2).await;
        let press_id = probe.decoded_writes().last().unwrap().command_id;
        probe.push(response(press_id, false));
        receive(press).await.unwrap();

        probe.push(response_with_status(active_id, false, 14));
        wait_for_count(&probe.flushes, 3).await;
        let release_id = probe.decoded_writes().last().unwrap().command_id;
        probe.push(response_with_status(release_id, false, 9));
        wait_for_state(&handle, ConnectionState::Disconnected).await;

        assert!(probe.dropped.load(Ordering::Acquire));
        assert!(matches!(
            bounded(handle.send_screen_input(1, INPUT_RELEASE)).await,
            Err(ConnectionActorError::Closed)
        ));
    }

    #[tokio::test]
    async fn screen_timeout_polling_keeps_stream_alive_and_services_stop() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        acknowledge_start(&handle, &probe).await;
        let reads = probe.read_calls.load(Ordering::Acquire);

        wait_for_count(&probe.read_calls, reads + 2).await;

        assert_eq!(handle.state(), ConnectionState::ScreenStreaming);
        assert!(!probe.dropped.load(Ordering::Acquire));
        acknowledge_stop(&handle, &probe).await;
        assert_eq!(handle.state(), ConnectionState::Rpc);
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn transactional_idle_poll_services_stop_and_shutdown_during_partial_frame() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        let mut frames = handle.subscribe_screen_frames();
        acknowledge_start(&handle, &probe).await;
        let framed = frame_bytes(&screen_frame(0x71));
        let split = 2.min(framed.len() - 1);
        let reads = probe.read_calls.load(Ordering::Acquire);
        probe.push_bytes(framed[..split].iter().copied());
        wait_for_count(&probe.read_calls, reads + 3).await;

        let expected_flushes = probe.flushes.load(Ordering::Acquire) + 1;
        let stop = handle.submit_stop_screen_stream().unwrap();
        wait_for_count(&probe.flushes, expected_flushes).await;
        let stop_id = probe.decoded_writes().last().unwrap().command_id;
        probe.push_bytes(framed[split..].iter().copied());
        probe.push(response(stop_id, false));

        receive(stop).await.unwrap();
        let recovered = bounded(next_screen_frame(&mut frames)).await.unwrap();
        assert!(matches!(
            recovered.content,
            Some(Content::GuiScreenFrame(pb_gui::ScreenFrame { data, .. })) if data == [0x71]
        ));
        assert_eq!(handle.state(), ConnectionState::Rpc);
        shutdown(&handle).await;

        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        acknowledge_start(&handle, &probe).await;
        let framed = frame_bytes(&screen_frame(0x72));
        let split = 2.min(framed.len() - 1);
        let reads = probe.read_calls.load(Ordering::Acquire);
        probe.push_bytes(framed[..split].iter().copied());
        wait_for_count(&probe.read_calls, reads + 3).await;

        bounded(handle.shutdown()).await.unwrap();
        assert_eq!(handle.state(), ConnectionState::Disconnected);
        assert!(probe.dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn foreign_nonbroadcast_id_during_stream_is_fatal() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        acknowledge_start(&handle, &probe).await;

        probe.push(response(77, false));
        wait_for_state(&handle, ConnectionState::Disconnected).await;

        assert!(probe.dropped.load(Ordering::Acquire));
        assert!(matches!(
            bounded(handle.send_screen_input(0, INPUT_PRESS)).await,
            Err(ConnectionActorError::Closed)
        ));
    }

    #[tokio::test]
    async fn shutdown_during_stream_drops_transport_before_completion() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        acknowledge_start(&handle, &probe).await;

        bounded(handle.shutdown()).await.unwrap();

        assert_eq!(handle.state(), ConnectionState::Disconnected);
        assert!(probe.dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn shutdown_interrupts_start_input_and_stop_ack_polling() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        let start = handle.submit_start_screen_stream().unwrap();
        wait_for_count(&probe.flushes, 1).await;
        let reads = probe.read_calls.load(Ordering::Acquire);
        wait_for_count(&probe.read_calls, reads + 2).await;
        bounded(handle.shutdown()).await.unwrap();
        assert!(matches!(
            receive(start).await,
            Err(ConnectionActorError::Closed)
        ));
        assert!(probe.dropped.load(Ordering::Acquire));

        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        acknowledge_start(&handle, &probe).await;
        let input = handle.submit_screen_input(1, INPUT_PRESS).unwrap();
        wait_for_count(&probe.flushes, 2).await;
        let reads = probe.read_calls.load(Ordering::Acquire);
        wait_for_count(&probe.read_calls, reads + 2).await;
        bounded(handle.shutdown()).await.unwrap();
        assert!(matches!(
            receive(input).await,
            Err(ConnectionActorError::Closed)
        ));
        assert!(probe.dropped.load(Ordering::Acquire));

        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        acknowledge_start(&handle, &probe).await;
        let stop = handle.submit_stop_screen_stream().unwrap();
        wait_for_count(&probe.flushes, 2).await;
        let reads = probe.read_calls.load(Ordering::Acquire);
        wait_for_count(&probe.read_calls, reads + 2).await;
        bounded(handle.shutdown()).await.unwrap();
        assert!(matches!(
            receive(stop).await,
            Err(ConnectionActorError::Closed)
        ));
        assert!(probe.dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn usb_and_ble_streams_use_kind_specific_timeouts_on_one_reader_thread() {
        for (kind, expected_timeout) in [
            (TransportKind::Serial, SERIAL_TIMEOUT_SCREEN),
            (TransportKind::Ble, BLE_TIMEOUT_SCREEN),
        ] {
            let caller = std::thread::current().id();
            let (handle, probe) = interactive_handle(Vec::new(), kind);
            acknowledge_start(&handle, &probe).await;
            probe.push(screen_frame(0x51));
            wait_for_count(&probe.read_calls, 3).await;
            acknowledge_stop(&handle, &probe).await;

            assert!(probe.timeouts.lock().unwrap().contains(&expected_timeout));
            let readers = probe.read_threads.lock().unwrap().clone();
            assert!(!readers.is_empty());
            assert_ne!(readers[0], caller);
            assert!(readers.iter().all(|thread| *thread == readers[0]));
            shutdown(&handle).await;
        }
    }

    #[tokio::test]
    async fn shutdown_with_a_full_capacity_one_queue_rejects_queued_work() {
        let (handle, _) = test_handle(1);
        let (started_tx, started_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let queued_calls = Arc::new(AtomicUsize::new(0));

        let running = handle
            .submit_rpc(move |_| {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(TEST_TIMEOUT).unwrap();
                Ok(1_u8)
            })
            .unwrap();
        started_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        let queued_calls_for_job = Arc::clone(&queued_calls);
        let queued = handle
            .submit_rpc(move |_| {
                queued_calls_for_job.fetch_add(1, Ordering::SeqCst);
                Ok(2_u8)
            })
            .unwrap();

        let shutdown_future = handle.shutdown();
        assert_eq!(handle.state(), ConnectionState::ShuttingDown);
        let shutdown_task = tokio::spawn(shutdown_future);

        assert!(matches!(
            bounded(handle.execute_legacy_rpc(|_| Ok(()))).await,
            Err(ConnectionActorError::Closed)
        ));
        release_tx.send(()).unwrap();
        assert_eq!(receive(running).await.unwrap(), 1);
        assert!(matches!(
            receive(queued).await,
            Err(ConnectionActorError::Closed)
        ));
        assert_eq!(queued_calls.load(Ordering::SeqCst), 0);
        bounded(shutdown_task).await.unwrap().unwrap();
        assert_eq!(handle.state(), ConnectionState::Disconnected);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_submission_race_never_executes_work_after_shutdown_wins() {
        const ITERATIONS: usize = 32;

        for _ in 0..ITERATIONS {
            let (handle, _) = test_handle(2);
            let (started_tx, started_rx) = std_mpsc::channel();
            let (release_tx, release_rx) = std_mpsc::channel();
            let running = handle
                .submit_rpc(move |_| {
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(TEST_TIMEOUT).unwrap();
                    Ok(())
                })
                .unwrap();
            started_rx.recv_timeout(TEST_TIMEOUT).unwrap();

            let barrier = Arc::new(Barrier::new(3));
            let executed = Arc::new(AtomicUsize::new(0));
            let shutdown_handle = handle.clone();
            let shutdown_barrier = Arc::clone(&barrier);
            let shutdown_task = tokio::spawn(async move {
                tokio::time::timeout(TEST_TIMEOUT, shutdown_barrier.wait())
                    .await
                    .expect("shutdown racer did not reach the barrier");
                // shutdown performs admission closure synchronously; waiting
                // for teardown is deliberately left to the async test.
                drop(shutdown_handle.shutdown());
            });

            let submission_handle = handle.clone();
            let submission_barrier = Arc::clone(&barrier);
            let submission_executed = Arc::clone(&executed);
            let submission_task = tokio::spawn(async move {
                tokio::time::timeout(TEST_TIMEOUT, submission_barrier.wait())
                    .await
                    .expect("submission racer did not reach the barrier");
                submission_handle.submit_rpc(move |_| {
                    submission_executed.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            });

            bounded(barrier.wait()).await;
            bounded(shutdown_task).await.unwrap();
            let _submission_result = bounded(submission_task).await.unwrap();

            release_tx.send(()).unwrap();
            receive(running).await.unwrap();
            shutdown(&handle).await;

            assert_eq!(executed.load(Ordering::SeqCst), 0);
            assert_eq!(handle.state(), ConnectionState::Disconnected);
        }
    }

    #[tokio::test]
    async fn shutdown_acknowledges_only_after_transport_drop() {
        let (handle, dropped) = drop_aware_handle(2);

        shutdown(&handle).await;

        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(handle.state(), ConnectionState::Disconnected);
    }

    #[tokio::test]
    async fn concurrent_shutdown_is_idempotent() {
        let (handle, _) = test_handle(2);
        let (started_tx, started_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let running = handle
            .submit_rpc(move |_| {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(TEST_TIMEOUT).unwrap();
                Ok(())
            })
            .unwrap();
        started_rx.recv_timeout(TEST_TIMEOUT).unwrap();

        let shutdowns: Vec<_> = (0..4)
            .map(|_| {
                let handle = handle.clone();
                tokio::spawn(async move { handle.shutdown().await })
            })
            .collect();
        wait_for_state(&handle, ConnectionState::ShuttingDown).await;
        release_tx.send(()).unwrap();
        receive(running).await.unwrap();

        for shutdown_task in shutdowns {
            bounded(shutdown_task).await.unwrap().unwrap();
        }
        shutdown(&handle).await;
        assert_eq!(handle.state(), ConnectionState::Disconnected);
    }

    #[tokio::test]
    async fn panicking_job_disconnects_and_fails_pending_work() {
        let (handle, dropped) = drop_aware_handle(2);
        let (started_tx, started_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let panicking = handle
            .submit_rpc::<(), _>(move |_| {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(TEST_TIMEOUT).unwrap();
                panic!("scripted legacy job panic")
            })
            .unwrap();
        started_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        let pending = handle.submit_rpc(|_| Ok(())).unwrap();
        release_tx.send(()).unwrap();

        assert!(matches!(
            receive(panicking).await,
            Err(ConnectionActorError::JobPanicked)
        ));
        assert_eq!(handle.state(), ConnectionState::Disconnected);
        assert!(dropped.load(Ordering::Acquire));
        assert!(matches!(
            receive(pending).await,
            Err(ConnectionActorError::ConnectionLost { cause })
                if cause.contains("legacy device job panicked")
        ));
    }

    #[tokio::test]
    async fn actor_owned_rpc_matches_single_frame_and_is_the_only_reader() {
        let caller_thread = std::thread::current().id();
        let (handle, probe) = scripted_handle(vec![response(1, false)], false);

        let rpc = bounded(handle.request_rpc(empty_content())).await.unwrap();

        assert_eq!(rpc.command_id, 1);
        assert_eq!(rpc.frames.len(), 1);
        assert_eq!(rpc.frames[0].command_id, 1);
        let written = decode_first_message(&probe.written.lock().unwrap());
        assert_eq!(written.command_id, 1);
        assert!(matches!(written.content, Some(Content::Empty(_))));
        {
            let read_threads = probe.read_threads.lock().unwrap();
            assert!(!read_threads.is_empty());
            assert_ne!(read_threads[0], caller_thread);
            assert!(read_threads.iter().all(|thread| *thread == read_threads[0]));
        }
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn actor_owned_rpc_collects_all_matching_multi_frame_responses() {
        let (handle, _) = scripted_handle(
            vec![response(1, true), response(1, true), response(1, false)],
            false,
        );

        let rpc = bounded(handle.request_rpc(empty_content())).await.unwrap();

        assert_eq!(rpc.frames.len(), 3);
        assert!(rpc.frames[0].has_next);
        assert!(rpc.frames[1].has_next);
        assert!(!rpc.frames[2].has_next);
        assert!(rpc.frames.iter().all(|frame| frame.command_id == 1));
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn interleaved_screen_frame_is_routed_exactly_once() {
        let (handle, _) = scripted_handle(vec![screen_frame(0x5a), response(1, false)], false);
        let mut screen_frames = handle.subscribe_screen_frames();
        let second_screen_frames = handle.subscribe_screen_frames();

        let rpc = bounded(handle.request_rpc(empty_content())).await.unwrap();
        let routed = bounded(next_screen_frame(&mut screen_frames))
            .await
            .unwrap();

        assert_eq!(rpc.frames.len(), 1);
        assert!(matches!(
            routed.content,
            Some(Content::GuiScreenFrame(pb_gui::ScreenFrame { data, .. })) if data == [0x5a]
        ));
        assert!(matches!(
            second_screen_frames.borrow().clone().and_then(|frame| frame.content),
            Some(Content::GuiScreenFrame(pb_gui::ScreenFrame { data, .. })) if data == [0x5a]
        ));
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn foreign_command_id_is_explicit_and_disconnects_stream() {
        let (handle, probe) = scripted_handle(vec![response(77, false)], false);

        let result = bounded(handle.request_rpc(empty_content())).await;

        assert!(matches!(
            result,
            Err(ConnectionActorError::Protocol(
                ConnectionProtocolError::ForeignCommandId {
                    expected_id: 1,
                    received_id: 77
                }
            ))
        ));
        assert_eq!(handle.state(), ConnectionState::Disconnected);
        assert!(probe.dropped.load(Ordering::Acquire));
        assert!(matches!(
            bounded(handle.request_rpc(empty_content())).await,
            Err(ConnectionActorError::Closed)
        ));
    }

    #[tokio::test]
    async fn response_timeout_disconnects_before_late_frame_can_shift_next_request() {
        let (handle, probe) = scripted_handle(vec![response(1, false)], true);
        let late_frame_bytes = probe.remaining.load(Ordering::Acquire);

        let result = bounded(handle.request_rpc(empty_content())).await;

        assert!(matches!(
            result,
            Err(ConnectionActorError::Protocol(
                ConnectionProtocolError::ResponseTimeout { command_id: 1 }
            ))
        ));
        assert_eq!(probe.read_calls.load(Ordering::Acquire), 1);
        assert_eq!(probe.remaining.load(Ordering::Acquire), late_frame_bytes);
        assert!(late_frame_bytes > 0);
        assert_eq!(handle.state(), ConnectionState::Disconnected);
        assert!(matches!(
            bounded(handle.request_rpc(empty_content())).await,
            Err(ConnectionActorError::Closed)
        ));
    }

    #[tokio::test]
    async fn typed_rpc_fatal_read_fans_out_without_running_queued_request() {
        let (started_tx, started_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let transport = FatalReadTransport {
            started: Some(started_tx),
            release: release_rx,
            dropped: Arc::clone(&dropped),
            written: Vec::new(),
        };
        let handle =
            ConnectionHandle::spawn_with_capacity(FlipperClient::new(Box::new(transport)), 2)
                .unwrap();
        let fatal = handle.submit_proto_rpc(empty_content()).unwrap();
        started_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        let pending = handle.submit_proto_rpc(empty_content()).unwrap();
        release_tx.send(()).unwrap();

        assert!(matches!(
            receive(fatal).await,
            Err(ConnectionActorError::Device(FlipperError::Io(_)))
        ));
        assert_eq!(handle.state(), ConnectionState::Disconnected);
        assert!(dropped.load(Ordering::Acquire));
        assert!(matches!(
            receive(pending).await,
            Err(ConnectionActorError::ConnectionLost { cause })
                if cause.contains("scripted fatal read")
        ));
    }

    #[tokio::test]
    async fn typed_transport_panic_replies_only_after_disconnect_and_drop() {
        let (started_tx, started_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let transport = PanickingReadTransport {
            started: Some(started_tx),
            release: release_rx,
            dropped: Arc::clone(&dropped),
        };
        let handle =
            ConnectionHandle::spawn_with_capacity(FlipperClient::new(Box::new(transport)), 2)
                .unwrap();
        let panicking = handle.submit_proto_rpc(empty_content()).unwrap();
        started_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        let pending = handle.submit_proto_rpc(empty_content()).unwrap();
        release_tx.send(()).unwrap();

        assert!(matches!(
            receive(panicking).await,
            Err(ConnectionActorError::JobPanicked)
        ));
        assert_eq!(handle.state(), ConnectionState::Disconnected);
        assert!(dropped.load(Ordering::Acquire));
        assert!(matches!(
            receive(pending).await,
            Err(ConnectionActorError::ConnectionLost { cause })
                if cause.contains("actor-owned protobuf request panicked")
        ));
    }

    #[tokio::test]
    async fn aggregate_response_byte_budget_accepts_boundary_and_rejects_next_byte() {
        let frame = response(1, false);
        let encoded_len = frame.encoded_len();
        let exact_config = ActorConfig {
            rpc_limits: RpcDispatchLimits {
                max_bytes: encoded_len,
                ..RpcDispatchLimits::default()
            },
        };
        let (exact_handle, _) =
            scripted_handle_with_config(vec![frame.clone()], false, exact_config);

        let exact = bounded(exact_handle.request_rpc(empty_content()))
            .await
            .unwrap();
        assert_eq!(exact.frames.len(), 1);
        shutdown(&exact_handle).await;

        let over_config = ActorConfig {
            rpc_limits: RpcDispatchLimits {
                max_bytes: encoded_len - 1,
                ..RpcDispatchLimits::default()
            },
        };
        let (over_handle, _) = scripted_handle_with_config(vec![frame], false, over_config);
        let over = bounded(over_handle.request_rpc(empty_content())).await;
        assert!(matches!(
            over,
            Err(ConnectionActorError::Protocol(
                ConnectionProtocolError::ResponseBytesExceeded {
                    command_id: 1,
                    limit
                }
            )) if limit == encoded_len - 1
        ));
        assert_eq!(over_handle.state(), ConnectionState::Disconnected);
    }

    #[tokio::test]
    async fn response_frame_budget_accepts_boundary_and_rejects_extra_frame() {
        let exact_config = ActorConfig {
            rpc_limits: RpcDispatchLimits {
                max_frames: 2,
                ..RpcDispatchLimits::default()
            },
        };
        let (exact_handle, _) = scripted_handle_with_config(
            vec![response(1, true), response(1, false)],
            false,
            exact_config,
        );
        let exact = bounded(exact_handle.request_rpc(empty_content()))
            .await
            .unwrap();
        assert_eq!(exact.frames.len(), 2);
        shutdown(&exact_handle).await;

        let over_config = ActorConfig {
            rpc_limits: RpcDispatchLimits {
                max_frames: 1,
                ..RpcDispatchLimits::default()
            },
        };
        let (over_handle, _) = scripted_handle_with_config(
            vec![response(1, true), response(1, false)],
            false,
            over_config,
        );
        let over = bounded(over_handle.request_rpc(empty_content())).await;
        assert!(matches!(
            over,
            Err(ConnectionActorError::Protocol(
                ConnectionProtocolError::TooManyResponseFrames {
                    command_id: 1,
                    limit: 1
                }
            ))
        ));
        assert_eq!(over_handle.state(), ConnectionState::Disconnected);
    }

    #[tokio::test]
    async fn response_read_deadline_disconnects_without_reading_late_frame() {
        let config = ActorConfig {
            rpc_limits: RpcDispatchLimits {
                response_deadline: Duration::ZERO,
                ..RpcDispatchLimits::default()
            },
        };
        let (handle, probe) = scripted_handle_with_config(vec![response(1, false)], false, config);

        let result = bounded(handle.request_rpc(empty_content())).await;

        assert!(matches!(
            result,
            Err(ConnectionActorError::Protocol(
                ConnectionProtocolError::ResponseReadDeadlineExceeded {
                    command_id: 1,
                    deadline_ms: 0
                }
            ))
        ));
        assert!(!probe.written.lock().unwrap().is_empty());
        assert_eq!(probe.read_calls.load(Ordering::Acquire), 0);
        assert!(probe.remaining.load(Ordering::Acquire) > 0);
        assert_eq!(handle.state(), ConnectionState::Disconnected);
    }

    #[tokio::test]
    async fn terminal_rpc_status_is_resolved_and_next_request_remains_aligned() {
        let (handle, _) = scripted_handle(
            vec![response_with_status(1, false, 9), response(2, false)],
            false,
        );

        let terminal_error = bounded(handle.request_rpc(empty_content())).await;
        assert!(matches!(
            terminal_error,
            Err(ConnectionActorError::Device(FlipperError::Rpc {
                status: 9,
                command_id: 1
            }))
        ));
        assert_eq!(handle.state(), ConnectionState::Rpc);

        let next = bounded(handle.request_rpc(empty_content())).await.unwrap();
        assert_eq!(next.command_id, 2);
        assert_eq!(next.frames.len(), 1);
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn typed_success_and_terminal_status_restore_normal_timeout_before_legacy_work() {
        for response in [response(1, false), response_with_status(1, false, 9)] {
            let config = ActorConfig {
                rpc_limits: RpcDispatchLimits {
                    response_deadline: Duration::from_secs(1),
                    ..RpcDispatchLimits::default()
                },
            };
            let (handle, probe) = scripted_handle_with_config(vec![response], false, config);
            let typed = bounded(handle.request_rpc(empty_content())).await;
            assert!(typed.is_ok() || matches!(typed, Err(ConnectionActorError::Device(_))));

            let recorded_timeouts = probe.timeouts.lock().unwrap().clone();
            assert!(recorded_timeouts[..recorded_timeouts.len() - 1]
                .iter()
                .all(|timeout| *timeout < SERIAL_TIMEOUT_NORMAL));
            assert_eq!(
                recorded_timeouts.last(),
                Some(&SERIAL_TIMEOUT_NORMAL),
                "typed completion must restore the known normal RPC timeout"
            );

            bounded(handle.execute_legacy_rpc(|client| {
                client.transport.write_all(b"legacy-after-typed")?;
                Ok(())
            }))
            .await
            .unwrap();

            assert_eq!(
                probe.write_timeouts.lock().unwrap().last(),
                Some(&Some(SERIAL_TIMEOUT_NORMAL))
            );
            assert_eq!(
                *probe.current_timeout.lock().unwrap(),
                Some(SERIAL_TIMEOUT_NORMAL)
            );
            shutdown(&handle).await;
        }
    }

    #[tokio::test]
    async fn normal_timeout_restoration_failure_is_fatal_before_typed_reply() {
        let (mut transport, probe) = ScriptedTransport::new(vec![response(1, false)], false);
        transport.fail_normal_timeout = true;
        let config = ActorConfig {
            rpc_limits: RpcDispatchLimits {
                response_deadline: Duration::from_secs(1),
                ..RpcDispatchLimits::default()
            },
        };
        let handle = ConnectionHandle::spawn_with_config(
            FlipperClient::new(Box::new(transport)),
            2,
            ConnectionMode::Rpc,
            config,
        )
        .unwrap();

        let result = bounded(handle.request_rpc(empty_content())).await;

        assert!(matches!(
            result,
            Err(ConnectionActorError::Device(FlipperError::Io(error)))
                if error.to_string().contains("restoration failure")
        ));
        assert_eq!(handle.state(), ConnectionState::Disconnected);
        assert!(probe.dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn outbound_request_budget_accepts_boundary_and_rejects_one_over_before_wire_io() {
        let content = Content::GuiScreenFrame(pb_gui::ScreenFrame {
            data: vec![0x5a; 64],
            orientation: 0,
            bg_color: 0,
            fg_color: 1,
        });
        let request_size = pb::Main {
            command_id: 1,
            command_status: 0,
            has_next: false,
            content: Some(content.clone()),
        }
        .encoded_len();

        let exact_config = ActorConfig {
            rpc_limits: RpcDispatchLimits {
                max_request_bytes: request_size,
                ..RpcDispatchLimits::default()
            },
        };
        let (exact_handle, _) =
            scripted_handle_with_config(vec![response(1, false)], false, exact_config);
        bounded(exact_handle.request_rpc(content.clone()))
            .await
            .unwrap();
        shutdown(&exact_handle).await;

        let over_config = ActorConfig {
            rpc_limits: RpcDispatchLimits {
                max_request_bytes: request_size - 1,
                ..RpcDispatchLimits::default()
            },
        };
        let (over_handle, probe) =
            scripted_handle_with_config(vec![response(2, false)], false, over_config);
        let rejected = bounded(over_handle.request_rpc(content)).await;
        assert!(matches!(
            rejected,
            Err(ConnectionActorError::Protocol(
                ConnectionProtocolError::RequestBytesExceeded {
                    command_id: 1,
                    actual,
                    limit
                }
            )) if actual == request_size && limit == request_size - 1
        ));
        assert!(probe.written.lock().unwrap().is_empty());
        assert_eq!(over_handle.state(), ConnectionState::Rpc);

        let next = bounded(over_handle.request_rpc(empty_content()))
            .await
            .unwrap();
        assert_eq!(next.command_id, 2);
        assert!(!probe.written.lock().unwrap().is_empty());
        shutdown(&over_handle).await;
    }

    #[tokio::test]
    async fn default_outbound_one_mib_cap_accepts_exact_and_rejects_one_over() {
        let exact_content = screen_content_with_request_len(MAX_RPC_REQUEST_BYTES);
        let (exact_handle, exact_probe) = scripted_handle(vec![response(1, false)], false);

        bounded(exact_handle.request_rpc(exact_content))
            .await
            .unwrap();
        assert!(!exact_probe.written.lock().unwrap().is_empty());
        shutdown(&exact_handle).await;

        let over_content = screen_content_with_request_len(MAX_RPC_REQUEST_BYTES + 1);
        let (over_handle, over_probe) = scripted_handle(Vec::new(), false);
        let rejected = bounded(over_handle.request_rpc(over_content)).await;

        assert!(matches!(
            rejected,
            Err(ConnectionActorError::Protocol(
                ConnectionProtocolError::RequestBytesExceeded {
                    command_id: 1,
                    actual,
                    limit: MAX_RPC_REQUEST_BYTES
                }
            )) if actual == MAX_RPC_REQUEST_BYTES + 1
        ));
        assert!(over_probe.written.lock().unwrap().is_empty());
        assert_eq!(over_handle.state(), ConnectionState::Rpc);
        shutdown(&over_handle).await;
    }

    #[tokio::test]
    async fn nonterminal_rpc_status_is_fatal_instead_of_leaving_unread_frames() {
        let (handle, _) = scripted_handle(
            vec![response_with_status(1, true, 9), response(1, false)],
            false,
        );

        let result = bounded(handle.request_rpc(empty_content())).await;

        assert!(matches!(
            result,
            Err(ConnectionActorError::Device(FlipperError::Rpc {
                status: 9,
                command_id: 1
            }))
        ));
        assert_eq!(handle.state(), ConnectionState::Disconnected);
        assert!(matches!(
            bounded(handle.request_rpc(empty_content())).await,
            Err(ConnectionActorError::Closed)
        ));
    }

    #[tokio::test]
    async fn matching_id_screen_content_is_the_rpc_response_not_unsolicited_screen() {
        let (handle, _) = scripted_handle(vec![screen_frame_with_id(1, 0x41)], false);
        let screen_frames = handle.subscribe_screen_frames();

        let rpc = bounded(handle.request_rpc(empty_content())).await.unwrap();

        assert_eq!(rpc.frames.len(), 1);
        assert!(matches!(
            &rpc.frames[0].content,
            Some(Content::GuiScreenFrame(pb_gui::ScreenFrame { data, .. }))
                if data == &[0x41]
        ));
        assert!(screen_frames.borrow().is_none());
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn nonzero_foreign_screen_content_is_foreign_not_unsolicited_screen() {
        let (handle, _) = scripted_handle(vec![screen_frame_with_id(44, 0x42)], false);

        let result = bounded(handle.request_rpc(empty_content())).await;

        assert!(matches!(
            result,
            Err(ConnectionActorError::Protocol(
                ConnectionProtocolError::ForeignCommandId {
                    expected_id: 1,
                    received_id: 44
                }
            ))
        ));
        assert_eq!(handle.state(), ConnectionState::Disconnected);
    }

    #[tokio::test]
    async fn screen_route_coalesces_to_latest_frame_without_disconnect() {
        let (handle, _) = scripted_handle(
            vec![screen_frame(1), screen_frame(2), response(1, false)],
            false,
        );
        let mut screen_frames = handle.subscribe_screen_frames();

        bounded(handle.request_rpc(empty_content())).await.unwrap();
        let routed = bounded(next_screen_frame(&mut screen_frames))
            .await
            .unwrap();

        assert!(matches!(
            routed.content,
            Some(Content::GuiScreenFrame(pb_gui::ScreenFrame { data, .. })) if data == [2]
        ));
        assert_eq!(handle.state(), ConnectionState::Rpc);
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn closed_screen_route_does_not_disconnect_rpc() {
        let (handle, _) = scripted_handle(vec![screen_frame(1), response(1, false)], false);
        drop(handle.subscribe_screen_frames());

        bounded(handle.request_rpc(empty_content())).await.unwrap();

        assert_eq!(handle.state(), ConnectionState::Rpc);
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn screen_subscriptions_close_before_shutdown_completion() {
        let (handle, _) = test_handle(2);
        let mut frames = handle.subscribe_screen_frames();

        bounded(handle.shutdown()).await.unwrap();
        assert!(
            frames.has_changed().is_err(),
            "shutdown completion must not precede screen subscription closure"
        );
        wait_for_screen_subscription_closed(&mut frames).await;

        assert_eq!(handle.state(), ConnectionState::Disconnected);
    }

    #[tokio::test]
    async fn screen_subscriptions_close_before_fatal_result_publication() {
        let (handle, _) = scripted_handle(vec![response(77, false)], false);
        let mut frames = handle.subscribe_screen_frames();

        assert!(matches!(
            bounded(handle.request_rpc(empty_content())).await,
            Err(ConnectionActorError::Protocol(
                ConnectionProtocolError::ForeignCommandId {
                    expected_id: 1,
                    received_id: 77
                }
            ))
        ));
        assert!(
            frames.has_changed().is_err(),
            "fatal result publication must not precede screen subscription closure"
        );
        wait_for_screen_subscription_closed(&mut frames).await;

        assert_eq!(handle.state(), ConnectionState::Disconnected);
    }

    #[test]
    fn cli_handoff_matcher_accepts_exact_marker_split_at_every_boundary() {
        let trailing = b"protobuf-after-marker";
        for boundary in 0..=CLI_RPC_HANDOFF_MARKER.len() {
            let mut matcher = CliHandoffMatcher::new();
            let first = matcher.feed(&CLI_RPC_HANDOFF_MARKER[..boundary]);
            if boundary == CLI_RPC_HANDOFF_MARKER.len() {
                assert!(matches!(
                    first,
                    CliHandoffProgress::Complete { routed, trailing }
                        if routed.is_empty() && trailing.is_empty()
                ));
                continue;
            }
            assert!(matches!(
                first,
                CliHandoffProgress::Pending { routed } if routed.is_empty()
            ));

            let mut remainder = CLI_RPC_HANDOFF_MARKER[boundary..].to_vec();
            remainder.extend_from_slice(trailing);
            assert!(matches!(
                matcher.feed(&remainder),
                CliHandoffProgress::Complete {
                    routed,
                    trailing: observed,
                } if routed.is_empty() && observed == trailing
            ));
        }
    }

    #[tokio::test]
    async fn cli_handoff_matcher_routes_overlap_and_max_chunk_fallback_once_and_bounded() {
        let mut overlap = CliHandoffMatcher::new();
        let mut overlap_bytes = b"start_".to_vec();
        overlap_bytes.extend_from_slice(CLI_RPC_HANDOFF_MARKER);
        overlap_bytes.extend_from_slice(b"post");
        assert!(matches!(
            overlap.feed(&overlap_bytes),
            CliHandoffProgress::Complete { routed, trailing }
                if routed == b"start_" && trailing == b"post"
        ));

        let mut fallback = CliHandoffMatcher::new();
        assert!(matches!(
            fallback.feed(&CLI_RPC_HANDOFF_MARKER[..CLI_RPC_HANDOFF_MARKER.len() - 1]),
            CliHandoffProgress::Pending { routed } if routed.is_empty()
        ));
        let mismatch = vec![b'x'; CLI_READ_CHUNK_BYTES];
        let routed = match fallback.feed(&mismatch) {
            CliHandoffProgress::Pending { routed } => routed,
            CliHandoffProgress::Complete { .. } => panic!("mismatch cannot complete marker"),
        };
        let mut expected = CLI_RPC_HANDOFF_MARKER[..CLI_RPC_HANDOFF_MARKER.len() - 1].to_vec();
        expected.extend_from_slice(&mismatch);
        assert_eq!(routed, expected);
        assert!(fallback.candidate.is_empty());

        let (sender, mut receiver) = broadcast::channel(4);
        route_cli_output(&sender, 9, routed);
        let mut observed = Vec::new();
        while let Ok(CliOutputEvent::Data { session_id, bytes }) = receiver.try_recv() {
            assert_eq!(session_id, 9);
            assert!(bytes.len() <= CLI_READ_CHUNK_BYTES);
            observed.extend(bytes);
        }
        assert_eq!(observed, expected);

        route_cli_output(&sender, 9, Vec::new());
        assert!(matches!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn cli_is_serial_only_and_ble_rejection_has_no_state_or_wire_side_effect() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Ble);

        assert!(matches!(
            bounded(handle.enter_cli()).await,
            Err(ConnectionActorError::CliRequiresSerial)
        ));
        assert_eq!(handle.state(), ConnectionState::Rpc);
        assert!(probe.write_chunks().is_empty());
        assert_eq!(probe.read_calls.load(Ordering::Acquire), 0);

        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn cli_entry_waits_across_poll_slices_for_exact_terminal_empty_ack() {
        let (handle, probe) = interactive_handle(Vec::new(), TransportKind::Serial);
        let mut output = handle.subscribe_cli_output();
        let entry = handle.submit_enter_cli().unwrap();
        bounded(async {
            while probe.flushes.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        tokio::time::sleep(CLI_READ_TIMEOUT + CLI_READ_TIMEOUT).await;
        probe.push(response(1, false));

        receive(entry).await.unwrap();
        assert_eq!(handle.state(), ConnectionState::Cli);
        assert!(probe.read_calls.load(Ordering::Acquire) >= 2);
        assert!(matches!(
            bounded(output.recv()).await.unwrap(),
            CliOutputEvent::SessionStarted { session_id: 1 }
        ));
        let stop = decode_first_message(&probe.written.lock().unwrap());
        assert_eq!(stop.command_id, 1);
        assert!(matches!(stop.content, Some(Content::StopSession(_))));

        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn cli_entry_rejects_wrong_stop_ack_content_status_terminality_and_id_fatally() {
        let wrong_content = pb::Main {
            command_id: 1,
            command_status: 0,
            has_next: false,
            content: Some(Content::SystemPingResponse(pb_system::PingResponse {
                data: Vec::new(),
            })),
        };
        let cases = [
            (wrong_content, false),
            (response_with_status(1, false, 7), false),
            (response(1, true), false),
            (response(77, false), true),
        ];

        for (message, foreign_id_expected) in cases {
            let (handle, probe) = interactive_handle(vec![message], TransportKind::Serial);
            let result = bounded(handle.enter_cli()).await;
            if foreign_id_expected {
                assert!(matches!(
                    result,
                    Err(ConnectionActorError::Protocol(
                        ConnectionProtocolError::ForeignCommandId {
                            expected_id: 1,
                            received_id: 77
                        }
                    ))
                ));
            } else {
                assert!(matches!(
                    result,
                    Err(ConnectionActorError::Protocol(
                        ConnectionProtocolError::UnexpectedCliStopResponse { command_id: 1 }
                    ))
                ));
            }
            assert_eq!(handle.state(), ConnectionState::Disconnected);
            assert!(probe.dropped.load(Ordering::Acquire));
        }
    }

    #[tokio::test]
    async fn cli_send_validates_before_wire_and_writes_utf8_cr_and_etx_exactly() {
        let (handle, probe) = interactive_handle(vec![response(1, false)], TransportKind::Serial);
        let mut output = handle.subscribe_cli_output();
        enter_interactive_cli(&handle, &mut output).await;
        let baseline_chunks = probe.write_chunks().len();
        let baseline_flushes = probe.flushes.load(Ordering::Acquire);

        for invalid in ["bad\rcommand", "bad\ncommand", "bad\0command"] {
            assert!(matches!(
                bounded(handle.cli_send(invalid)).await,
                Err(ConnectionActorError::InvalidCliCommand)
            ));
        }
        let oversized = "x".repeat(MAX_CLI_COMMAND_BYTES + 1);
        assert!(matches!(
            bounded(handle.cli_send(&oversized)).await,
            Err(ConnectionActorError::CliCommandBytesExceeded { actual, limit })
                if actual == MAX_CLI_COMMAND_BYTES + 1 && limit == MAX_CLI_COMMAND_BYTES
        ));
        assert_eq!(probe.write_chunks().len(), baseline_chunks);

        let boundary = "x".repeat(MAX_CLI_COMMAND_BYTES);
        bounded(handle.cli_send(&boundary)).await.unwrap();
        bounded(handle.cli_send("héllo")).await.unwrap();
        bounded(handle.cli_interrupt()).await.unwrap();
        let chunks = probe.write_chunks();
        assert_eq!(chunks[baseline_chunks], boundary.as_bytes());
        assert_eq!(chunks[baseline_chunks + 1], b"\r");
        assert_eq!(chunks[baseline_chunks + 2], "héllo".as_bytes());
        assert_eq!(chunks[baseline_chunks + 3], b"\r");
        assert_eq!(chunks[baseline_chunks + 4], CLI_INTERRUPT);
        assert_eq!(probe.flushes.load(Ordering::Acquire), baseline_flushes + 3);

        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn cli_entry_preclaim_preserves_older_rpc_fifo_order() {
        let (handle, probe) = interactive_handle(vec![response(1, false)], TransportKind::Serial);
        let (started_tx, started_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let older = handle
            .submit_rpc(move |client| {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(TEST_TIMEOUT).unwrap();
                client.transport.write_all(b"older-rpc")?;
                Ok(())
            })
            .unwrap();
        started_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        let entry = handle.submit_enter_cli().unwrap();
        assert_eq!(handle.state(), ConnectionState::Transitioning);
        assert!(probe.write_chunks().is_empty());

        release_tx.send(()).unwrap();
        receive(older).await.unwrap();
        receive(entry).await.unwrap();

        let chunks = probe.write_chunks();
        assert_eq!(
            chunks.first().map(Vec::as_slice),
            Some(b"older-rpc".as_slice())
        );
        let stop = decode_first_message(&chunks[1..].concat());
        assert!(matches!(stop.content, Some(Content::StopSession(_))));
        assert_eq!(handle.state(), ConnectionState::Cli);
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn cli_exit_preserves_prior_send_fifo_and_requires_exact_echoed_marker_and_ping() {
        let (handle, probe) = interactive_handle(vec![response(1, false)], TransportKind::Serial);
        let mut output = handle.subscribe_cli_output();
        let session_id = enter_interactive_cli(&handle, &mut output).await;

        let send = handle.submit_cli_send("log").unwrap();
        let exit = handle.submit_exit_cli().unwrap();
        assert_eq!(handle.state(), ConnectionState::Transitioning);
        receive(send).await.unwrap();
        wait_for_write_chunk(&probe, CLI_START_RPC_SESSION).await;
        let writes_before_marker = probe.write_chunks().len();
        let raw_part_one = b"log output\nunrelated\r";
        let raw_part_two = b"\nstart_rpc_sessXstart_";
        let mut raw_before = raw_part_one.to_vec();
        raw_before.extend_from_slice(raw_part_two);
        let marker_split = CLI_RPC_HANDOFF_MARKER.len() - 1;
        probe.push_bytes(raw_part_one.iter().copied());
        wait_for_input_empty(&probe).await;
        tokio::time::sleep(CLI_READ_TIMEOUT + CLI_READ_TIMEOUT).await;
        assert_eq!(
            probe.write_chunks().len(),
            writes_before_marker,
            "an unrelated LF and the first half of unrelated CRLF must not start the ping"
        );
        let mut first_raw = raw_part_two.to_vec();
        first_raw.extend_from_slice(&CLI_RPC_HANDOFF_MARKER[..marker_split]);
        probe.push_bytes(first_raw);
        wait_for_input_empty(&probe).await;
        tokio::time::sleep(CLI_READ_TIMEOUT + CLI_READ_TIMEOUT).await;
        assert_eq!(
            probe.write_chunks().len(),
            writes_before_marker,
            "unrelated LF/CRLF and a partial marker must not start the ping"
        );
        let mut handoff = CLI_RPC_HANDOFF_MARKER[marker_split..].to_vec();
        handoff.extend(frame_bytes(&ping_response(2, CLI_PING_PAYLOAD)));
        probe.push_bytes(handoff);
        receive(exit).await.unwrap();

        assert_eq!(handle.state(), ConnectionState::Rpc);
        let chunks = probe.write_chunks();
        let start_index = chunks
            .iter()
            .position(|chunk| chunk.as_slice() == CLI_START_RPC_SESSION)
            .unwrap();
        assert_eq!(chunks[start_index - 2], b"log");
        assert_eq!(chunks[start_index - 1], b"\r");
        assert_eq!(
            probe.write_timeouts.lock().unwrap()[start_index],
            SERIAL_TIMEOUT_NORMAL,
            "the pre-drain short timeout must be reset before the handoff write"
        );
        let ping = decode_first_message(&chunks[start_index + 1..].concat());
        assert_eq!(ping.command_id, 2);
        assert!(matches!(
            ping.content,
            Some(Content::SystemPingRequest(pb_system::PingRequest { data }))
                if data.as_slice() == CLI_PING_PAYLOAD
        ));
        assert_eq!(
            probe.timeouts.lock().unwrap().last(),
            Some(&SERIAL_TIMEOUT_NORMAL)
        );

        let mut routed = Vec::new();
        loop {
            match bounded(output.recv()).await.unwrap() {
                CliOutputEvent::Data {
                    session_id: event_session,
                    bytes,
                } => {
                    assert_eq!(event_session, session_id);
                    assert!(bytes.len() <= CLI_READ_CHUNK_BYTES);
                    routed.extend(bytes);
                }
                CliOutputEvent::SessionEnded {
                    session_id: event_session,
                } => {
                    assert_eq!(event_session, session_id);
                    break;
                }
                event => panic!("unexpected CLI output event: {event:?}"),
            }
        }
        assert_eq!(routed, raw_before);

        probe.push(response(3, false));
        assert_eq!(
            bounded(handle.request_rpc(empty_content()))
                .await
                .unwrap()
                .command_id,
            3
        );
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn cli_output_routes_split_chunks_and_interrupt_is_not_starved_by_output() {
        let (handle, probe) = interactive_handle(vec![response(1, false)], TransportKind::Serial);
        let mut output = handle.subscribe_cli_output();
        let session_id = enter_interactive_cli(&handle, &mut output).await;

        probe.push_bytes(b"hel".iter().copied());
        assert_eq!(
            bounded(output.recv()).await.unwrap(),
            CliOutputEvent::Data {
                session_id,
                bytes: b"hel".to_vec()
            }
        );
        probe.push_bytes(b"lo".iter().copied());
        assert_eq!(
            bounded(output.recv()).await.unwrap(),
            CliOutputEvent::Data {
                session_id,
                bytes: b"lo".to_vec()
            }
        );

        probe.push_bytes(vec![b'x'; CLI_READ_CHUNK_BYTES * 3]);
        bounded(handle.cli_interrupt()).await.unwrap();
        assert!(probe
            .write_chunks()
            .iter()
            .any(|chunk| chunk.as_slice() == CLI_INTERRUPT));
        assert_eq!(handle.state(), ConnectionState::Cli);
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn cli_output_lag_is_explicit_and_resubscribe_recovers_at_current_tail() {
        let (handle, probe) = interactive_handle(vec![response(1, false)], TransportKind::Serial);
        let mut slow = handle.subscribe_cli_output();
        bounded(handle.enter_cli()).await.unwrap();

        for marker in 0..(DEFAULT_CLI_OUTPUT_CAPACITY + 8) {
            probe.push_bytes([marker as u8]);
            wait_for_input_empty(&probe).await;
            tokio::task::yield_now().await;
        }
        assert!(matches!(
            bounded(slow.recv()).await,
            Err(broadcast::error::RecvError::Lagged(skipped)) if skipped > 0
        ));

        let mut resumed = handle.subscribe_cli_output();
        probe.push_bytes([0xF5]);
        assert!(matches!(
            bounded(resumed.recv()).await.unwrap(),
            CliOutputEvent::Data { session_id: 1, bytes } if bytes == [0xF5]
        ));
        assert_eq!(handle.state(), ConnectionState::Cli);
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn cli_session_events_reset_generation_and_new_subscription_has_no_stale_replay() {
        let (handle, probe) = interactive_handle(vec![response(1, false)], TransportKind::Serial);
        let mut first_output = handle.subscribe_cli_output();
        let first_session = enter_interactive_cli(&handle, &mut first_output).await;
        finish_interactive_cli_exit(&handle, &probe, 2)
            .await
            .unwrap();
        loop {
            if matches!(
                bounded(first_output.recv()).await.unwrap(),
                CliOutputEvent::SessionEnded { session_id } if session_id == first_session
            ) {
                break;
            }
        }

        let mut second_output = handle.subscribe_cli_output();
        probe.push(response(3, false));
        let second_session = enter_interactive_cli(&handle, &mut second_output).await;
        assert_ne!(first_session, second_session);
        assert_eq!(second_session, 2);
        finish_interactive_cli_exit(&handle, &probe, 4)
            .await
            .unwrap();
        shutdown(&handle).await;
    }

    #[tokio::test]
    async fn cli_exit_uses_one_absolute_deadline_and_timeout_is_fatal() {
        let config = ActorConfig {
            rpc_limits: RpcDispatchLimits {
                response_deadline: Duration::from_millis(90),
                ..RpcDispatchLimits::default()
            },
        };
        let (transport, probe) =
            InteractiveTransport::new(vec![response(1, false)], TransportKind::Serial);
        let handle = ConnectionHandle::spawn_with_config(
            FlipperClient::new(Box::new(transport)),
            4,
            ConnectionMode::Rpc,
            config,
        )
        .unwrap();
        bounded(handle.enter_cli()).await.unwrap();

        let started = Instant::now();
        let exit = handle.submit_exit_cli().unwrap();
        wait_for_write_chunk(&probe, CLI_START_RPC_SESSION).await;
        // A prompt is not a firmware handoff. Only the exact echoed command
        // marker followed by an exact protobuf ping may publish RPC.
        probe.push_bytes(b">: ".iter().copied());
        let result = receive(exit).await;
        assert!(matches!(
            result,
            Err(ConnectionActorError::Protocol(
                ConnectionProtocolError::CliHandoffDeadlineExceeded { deadline_ms: 90 }
            ))
        ));
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(handle.state(), ConnectionState::Disconnected);
        assert!(probe.dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn failed_cli_recovery_ping_is_fatal_and_closes_output_before_reply() {
        let (handle, probe) = interactive_handle(vec![response(1, false)], TransportKind::Serial);
        bounded(handle.enter_cli()).await.unwrap();
        let mut output = handle.subscribe_cli_output();
        let exit = handle.submit_exit_cli().unwrap();
        wait_for_write_chunk(&probe, CLI_START_RPC_SESSION).await;
        let mut handoff = CLI_RPC_HANDOFF_MARKER.to_vec();
        handoff.extend(frame_bytes(&ping_response(2, b"wrong")));
        probe.push_bytes(handoff);

        assert!(matches!(
            receive(exit).await,
            Err(ConnectionActorError::Protocol(
                ConnectionProtocolError::UnexpectedCliPingResponse { command_id: 2 }
            ))
        ));
        assert_eq!(handle.state(), ConnectionState::Disconnected);
        loop {
            match output.try_recv() {
                Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Closed) => break,
                Err(broadcast::error::TryRecvError::Empty) => {
                    panic!("fatal reply became visible before CLI output closed")
                }
            }
        }
    }

    #[tokio::test]
    async fn cli_queue_full_exit_reverts_admission_and_shutdown_closes_output_after_drop() {
        let (handle, probe) =
            interactive_handle_with_capacity(vec![response(1, false)], TransportKind::Serial, 1);
        bounded(handle.enter_cli()).await.unwrap();
        let mut output = handle.subscribe_cli_output();

        let baseline_reads = probe.read_calls.load(Ordering::Acquire);
        bounded(async {
            while probe.read_calls.load(Ordering::Acquire) == baseline_reads {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let input_guard = probe.input.0.lock().unwrap();
        let queued_send = handle.submit_cli_send("queued").unwrap();
        let exit = handle.submit_exit_cli();
        assert!(matches!(exit, Err(ConnectionActorError::QueueFull)));
        assert_eq!(handle.state(), ConnectionState::Cli);
        drop(input_guard);
        receive(queued_send).await.unwrap();

        bounded(handle.shutdown()).await.unwrap();
        assert!(probe.dropped.load(Ordering::Acquire));
        assert!(matches!(
            output.try_recv(),
            Err(broadcast::error::TryRecvError::Closed)
        ));
        assert_eq!(handle.state(), ConnectionState::Disconnected);
    }

    #[tokio::test]
    async fn actor_thread_panic_is_contained_and_pending_work_fails() {
        let (handle, dropped) = drop_aware_handle(3);
        let (started_tx, started_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let running = handle
            .submit_rpc(move |_| {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(TEST_TIMEOUT).unwrap();
                Ok(())
            })
            .unwrap();
        started_rx.recv_timeout(TEST_TIMEOUT).unwrap();
        handle.commands.try_send(ActorCommand::ForcePanic).unwrap();
        let pending = handle.submit_rpc(|_| Ok(())).unwrap();
        release_tx.send(()).unwrap();

        receive(running).await.unwrap();
        assert!(matches!(
            receive(pending).await,
            Err(ConnectionActorError::ActorStopped)
        ));
        wait_for_state(&handle, ConnectionState::Disconnected).await;
        assert!(dropped.load(Ordering::Acquire));
    }
}
