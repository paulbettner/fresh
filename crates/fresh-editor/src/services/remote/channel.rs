//! Agent communication channel
//!
//! Handles request/response multiplexing over SSH stdin/stdout.
//! Supports transport hot-swapping for automatic reconnection:
//! the read/write tasks survive connection drops and resume when
//! a new transport is provided via `replace_transport()`.

use crate::services::remote::protocol::{AgentRequest, AgentResponse};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

/// Default capacity for the per-request streaming data channel.
const DEFAULT_DATA_CHANNEL_CAPACITY: usize = 64;

/// Default timeout for remote requests. If a response is not received within
/// this duration, the request fails with `ChannelError::Timeout` and the
/// connection is marked as disconnected.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Test-only: microseconds to sleep in the consumer loop between chunks.
/// Set to a non-zero value from tests to simulate a slow consumer and
/// deterministically reproduce channel backpressure scenarios.
/// Always compiled (not cfg(test)) because integration tests need access.
pub static TEST_RECV_DELAY_US: AtomicU64 = AtomicU64::new(0);

/// Error type for channel operations
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Channel closed")]
    ChannelClosed,

    #[error("Request cancelled")]
    Cancelled,

    #[error("Request timed out")]
    Timeout,

    #[error("Remote error: {0}")]
    Remote(String),
}

/// Pending request state
struct PendingRequest {
    generation: u64,
    /// Channel for streaming data
    data_tx: mpsc::Sender<serde_json::Value>,
    /// Channel for final result
    result_tx: oneshot::Sender<Result<serde_json::Value, String>>,
}

/// One request admitted against an exact transport generation.
struct OutboundMessage {
    generation: u64,
    id: u64,
    json: String,
}

#[derive(Debug, Clone, Copy)]
struct AdmissionState {
    connected: bool,
    generation: u64,
    replacing: bool,
}

/// Boxed async reader type used by the read task.
type BoxedReader = Box<dyn AsyncBufRead + Unpin + Send>;
/// Boxed async writer type used by the write task.
type BoxedWriter = Box<dyn AsyncWrite + Unpin + Send>;

struct ReplacementReader {
    reader: BoxedReader,
    generation: u64,
    installed: oneshot::Sender<()>,
}

/// Writer half of a transport hot-swap. The acknowledgement closes the gap
/// where the reader could publish `connected` while requests still targeted
/// the old writer.
struct ReplacementWriter {
    writer: BoxedWriter,
    generation: u64,
    installed: oneshot::Sender<()>,
}

/// Process-global source of stable per-channel ids. Lets the editor map an
/// `AsyncMessage::RemoteReconnected` back to the window whose authority owns
/// this channel, without the channel knowing anything about windows.
static NEXT_CHANNEL_ID: AtomicU64 = AtomicU64::new(1);

/// Communication channel with the remote agent
pub struct AgentChannel {
    /// Stable identity for this channel, assigned at creation. Survives
    /// transport hot-swaps (the channel object is reused across reconnects),
    /// so it's a durable key for "this remote session".
    id: u64,
    /// Notified once each time the transport is hot-swapped back in
    /// (`replace_transport`). The editor spawns a forwarder that turns each
    /// notification into an `AsyncMessage::RemoteReconnected`, so a silent
    /// background reconnect reaches the app event-driven rather than by
    /// polling `is_connected()`.
    reconnect_notify: Arc<tokio::sync::Notify>,
    /// Monotonic transport hot-swap generation. Consumers use it to ignore a
    /// duplicate notification without confusing it with a later reconnect.
    reconnect_generation: Arc<AtomicU64>,
    /// Sender to the write task
    write_tx: mpsc::Sender<OutboundMessage>,
    /// Admission fence pairing the connected check with the generation enqueue.
    admission: Arc<tokio::sync::Mutex<AdmissionState>>,
    /// Pending requests awaiting responses
    pending: Arc<Mutex<HashMap<u64, PendingRequest>>>,
    /// Next request ID
    next_id: AtomicU64,
    /// Whether the channel is connected
    connected: Arc<std::sync::atomic::AtomicBool>,
    /// Runtime handle for blocking operations
    runtime_handle: tokio::runtime::Handle,
    /// Capacity for per-request streaming data channels
    data_channel_capacity: usize,
    /// Timeout for individual requests (stored as milliseconds for atomic access)
    request_timeout_ms: AtomicU64,
    /// Sender to deliver a new reader to the read task after reconnection
    new_reader_tx: mpsc::Sender<ReplacementReader>,
    /// Sender to deliver a new writer to the write task after reconnection
    new_writer_tx: mpsc::Sender<ReplacementWriter>,
}

impl AgentChannel {
    /// Create a new channel from async read/write handles
    ///
    /// Must be called from within a Tokio runtime context.
    pub fn new(
        reader: tokio::io::BufReader<tokio::process::ChildStdout>,
        writer: tokio::process::ChildStdin,
    ) -> Self {
        Self::with_capacity(reader, writer, DEFAULT_DATA_CHANNEL_CAPACITY)
    }

    /// Create a new channel with a custom data channel capacity.
    ///
    /// Lower capacity makes channel overflow more likely if `try_send` is used,
    /// which is useful for stress-testing backpressure handling.
    pub fn with_capacity(
        reader: tokio::io::BufReader<tokio::process::ChildStdout>,
        writer: tokio::process::ChildStdin,
        data_channel_capacity: usize,
    ) -> Self {
        Self::from_transport(reader, writer, data_channel_capacity)
    }

    /// Create a new channel from any async reader/writer pair.
    ///
    /// This is the generic constructor used by both production code (via
    /// `new`/`with_capacity`) and tests (via arbitrary `AsyncBufRead`/`AsyncWrite`
    /// implementations like `DuplexStream`).
    ///
    /// Must be called from within a Tokio runtime context.
    pub fn from_transport<R, W>(reader: R, writer: W, data_channel_capacity: usize) -> Self
    where
        R: AsyncBufRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let pending: Arc<Mutex<HashMap<u64, PendingRequest>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let connected = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let admission = Arc::new(tokio::sync::Mutex::new(AdmissionState {
            connected: true,
            generation: 0,
            replacing: false,
        }));
        let reconnect_notify = Arc::new(tokio::sync::Notify::new());
        let reconnect_generation = Arc::new(AtomicU64::new(0));
        let runtime_handle = tokio::runtime::Handle::current();

        // Channel for outgoing requests (lives for the lifetime of the AgentChannel)
        let (write_tx, write_rx) = mpsc::channel::<OutboundMessage>(64);

        // Channels for delivering replacement transports on reconnection.
        // Capacity 1: at most one pending reconnection at a time.
        let (new_reader_tx, new_reader_rx) = mpsc::channel::<ReplacementReader>(1);
        let (new_writer_tx, new_writer_rx) = mpsc::channel::<ReplacementWriter>(1);

        // Spawn write task (lives for the lifetime of the AgentChannel)
        tokio::spawn(Self::write_task(
            Box::new(writer),
            write_rx,
            new_writer_rx,
            Arc::clone(&pending),
            Arc::clone(&connected),
            Arc::clone(&admission),
        ));

        // Spawn read task (lives for the lifetime of the AgentChannel)
        tokio::spawn(Self::read_task(
            Box::new(reader),
            new_reader_rx,
            Arc::clone(&pending),
            Arc::clone(&connected),
            Arc::clone(&admission),
            Arc::clone(&reconnect_generation),
            Arc::clone(&reconnect_notify),
        ));

        Self {
            id: NEXT_CHANNEL_ID.fetch_add(1, Ordering::Relaxed),
            reconnect_notify,
            reconnect_generation,
            write_tx,
            admission,
            pending,
            next_id: AtomicU64::new(1),
            connected,
            runtime_handle,
            data_channel_capacity,
            request_timeout_ms: AtomicU64::new(DEFAULT_REQUEST_TIMEOUT.as_millis() as u64),
            new_reader_tx,
            new_writer_tx,
        }
    }

    /// Mark one exact transport generation disconnected. A delayed timeout or
    /// EOF from an older transport must not tear down its replacement.
    async fn mark_disconnected(
        admission: &Arc<tokio::sync::Mutex<AdmissionState>>,
        connected: &Arc<std::sync::atomic::AtomicBool>,
        generation: u64,
    ) {
        let mut state = admission.lock().await;
        if state.generation == generation {
            state.connected = false;
            connected.store(false, Ordering::SeqCst);
        }
    }

    fn fail_outbound(
        pending: &Arc<Mutex<HashMap<u64, PendingRequest>>>,
        message: &OutboundMessage,
    ) {
        let request = {
            let mut pending = pending.lock().unwrap();
            match pending.get(&message.id) {
                Some(request) if request.generation == message.generation => {
                    pending.remove(&message.id)
                }
                _ => None,
            }
        };
        if let Some(request) = request {
            #[allow(clippy::let_underscore_must_use)]
            let _ = request.result_tx.send(Err(
                "connection replaced before request was sent".to_string()
            ));
        }
    }

    fn install_replacement_writer(
        writer: &mut BoxedWriter,
        writer_generation: &mut u64,
        replacement: ReplacementWriter,
        write_rx: &mut mpsc::Receiver<OutboundMessage>,
        pending: &Arc<Mutex<HashMap<u64, PendingRequest>>>,
    ) {
        *writer = replacement.writer;
        *writer_generation = replacement.generation;

        // Request admission is fenced while replacement is in progress, so
        // every message already queued here belongs to the retired writer.
        // Purge them before acknowledging installation: otherwise a caller can
        // receive "connection closed" and still mutate the replacement tenant.
        while let Ok(stale) = write_rx.try_recv() {
            debug_assert_ne!(stale.generation, *writer_generation);
            Self::fail_outbound(pending, &stale);
        }
        #[allow(clippy::let_underscore_must_use)]
        let _ = replacement.installed.send(());
    }

    /// Long-lived write task. Every queued request is tagged with the transport
    /// generation captured by the admission fence; stale generations are never
    /// written to a replacement transport.
    async fn write_task(
        mut writer: BoxedWriter,
        mut write_rx: mpsc::Receiver<OutboundMessage>,
        mut new_writer_rx: mpsc::Receiver<ReplacementWriter>,
        pending: Arc<Mutex<HashMap<u64, PendingRequest>>>,
        connected: Arc<std::sync::atomic::AtomicBool>,
        admission: Arc<tokio::sync::Mutex<AdmissionState>>,
    ) {
        let mut writer_generation = 0;
        loop {
            tokio::select! {
                biased;
                replacement = new_writer_rx.recv() => {
                    match replacement {
                        Some(replacement) => Self::install_replacement_writer(
                            &mut writer,
                            &mut writer_generation,
                            replacement,
                            &mut write_rx,
                            &pending,
                        ),
                        None => break,
                    }
                }
                message = write_rx.recv() => {
                    let Some(message) = message else { break };
                    if message.generation != writer_generation {
                        Self::fail_outbound(&pending, &message);
                        continue;
                    }

                    let write_ok = writer.write_all(message.json.as_bytes()).await.is_ok()
                        && writer.flush().await.is_ok();
                    if !write_ok {
                        Self::mark_disconnected(&admission, &connected, writer_generation).await;
                        match new_writer_rx.recv().await {
                            Some(replacement) => Self::install_replacement_writer(
                                &mut writer,
                                &mut writer_generation,
                                replacement,
                                &mut write_rx,
                                &pending,
                            ),
                            None => break,
                        }
                    }
                }
            }
        }
    }

    async fn install_replacement_reader(
        reader: &mut BoxedReader,
        replacement: ReplacementReader,
        pending: &Arc<Mutex<HashMap<u64, PendingRequest>>>,
        connected: &Arc<std::sync::atomic::AtomicBool>,
        admission: &Arc<tokio::sync::Mutex<AdmissionState>>,
        reconnect_generation: &Arc<AtomicU64>,
        reconnect_notify: &Arc<tokio::sync::Notify>,
    ) {
        Self::drain_pending(pending);
        *reader = replacement.reader;
        let generation = replacement.generation;
        let published = {
            let mut state = admission.lock().await;
            if state.replacing && state.generation == generation {
                state.connected = true;
                state.replacing = false;
                true
            } else {
                false
            }
        };
        if published {
            reconnect_generation.store(generation, Ordering::SeqCst);
            connected.store(true, Ordering::SeqCst);
            reconnect_notify.notify_one();
            #[allow(clippy::let_underscore_must_use)]
            let _ = replacement.installed.send(());
        }
    }

    /// Long-lived read task. Reads responses from the current transport and
    /// publishes a replacement only after its writer half is installed.
    async fn read_task(
        mut reader: BoxedReader,
        mut new_reader_rx: mpsc::Receiver<ReplacementReader>,
        pending: Arc<Mutex<HashMap<u64, PendingRequest>>>,
        connected: Arc<std::sync::atomic::AtomicBool>,
        admission: Arc<tokio::sync::Mutex<AdmissionState>>,
        reconnect_generation: Arc<AtomicU64>,
        reconnect_notify: Arc<tokio::sync::Notify>,
    ) {
        let mut reader_generation = 0;
        let mut line = String::new();

        loop {
            line.clear();
            tokio::select! {
                read_result = reader.read_line(&mut line) => {
                    match read_result {
                        Ok(0) | Err(_) => {
                            Self::mark_disconnected(
                                &admission,
                                &connected,
                                reader_generation,
                            ).await;
                            Self::drain_pending(&pending);
                            match new_reader_rx.recv().await {
                                Some(replacement) => {
                                    reader_generation = replacement.generation;
                                    Self::install_replacement_reader(
                                        &mut reader,
                                        replacement,
                                        &pending,
                                        &connected,
                                        &admission,
                                        &reconnect_generation,
                                        &reconnect_notify,
                                    ).await;
                                }
                                None => break,
                            }
                        }
                        Ok(_) => {
                            if let Ok(resp) = serde_json::from_str::<AgentResponse>(&line) {
                                Self::handle_response(&pending, resp).await;
                            }
                        }
                    }
                }
                replacement = new_reader_rx.recv() => {
                    match replacement {
                        Some(replacement) => {
                            reader_generation = replacement.generation;
                            Self::install_replacement_reader(
                                &mut reader,
                                replacement,
                                &pending,
                                &connected,
                                &admission,
                                &reconnect_generation,
                                &reconnect_notify,
                            ).await;
                        }
                        None => break,
                    }
                }
            }
        }
    }

    /// Fail all pending requests with "connection closed" so callers don't hang.
    fn drain_pending(pending: &Arc<Mutex<HashMap<u64, PendingRequest>>>) {
        let mut pending = pending.lock().unwrap();
        for (id, req) in pending.drain() {
            match req.result_tx.send(Err("connection closed".to_string())) {
                Ok(()) => {}
                Err(_) => {
                    warn!("request {id}: receiver dropped during disconnect cleanup");
                }
            }
        }
    }

    /// Handle an incoming response.
    ///
    /// For streaming data, uses `send().await` to apply backpressure when the
    /// consumer is slower than the producer. This prevents silent data loss
    /// that occurred with `try_send` (#1059).
    async fn handle_response(
        pending: &Arc<Mutex<HashMap<u64, PendingRequest>>>,
        resp: AgentResponse,
    ) {
        // Send streaming data without holding the mutex (send().await may yield)
        if let Some(data) = resp.data {
            let data_tx = {
                let pending = pending.lock().unwrap();
                pending.get(&resp.id).map(|req| req.data_tx.clone())
            };
            if let Some(tx) = data_tx {
                // send().await blocks until the consumer drains a slot, providing
                // backpressure instead of silently dropping data.
                if tx.send(data).await.is_err() {
                    // Receiver was dropped — this is unexpected since callers
                    // should hold data_rx until the stream ends. Clean up the
                    // pending entry to avoid leaking the dead request.
                    warn!("request {}: data receiver dropped mid-stream", resp.id);
                    let mut pending = pending.lock().unwrap();
                    pending.remove(&resp.id);
                    return;
                }
            }
        }

        // Handle final result/error
        if resp.result.is_some() || resp.error.is_some() {
            let mut pending = pending.lock().unwrap();
            if let Some(req) = pending.remove(&resp.id) {
                let outcome = if let Some(result) = resp.result {
                    req.result_tx.send(Ok(result))
                } else if let Some(error) = resp.error {
                    req.result_tx.send(Err(error))
                } else {
                    // resp matched the outer condition (result or error is Some)
                    // but neither branch fired — unreachable by construction.
                    return;
                };
                match outcome {
                    Ok(()) => {}
                    Err(_) => {
                        // Receiver was dropped — this is unexpected since
                        // callers should hold result_rx until they get a result.
                        warn!("request {}: result receiver dropped", resp.id);
                    }
                }
            }
        }
    }

    /// Check if the channel is connected
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Replace the underlying transport with a new reader/writer pair.
    ///
    /// Request admission is closed and advanced to a new generation before
    /// either half is handed off. The writer installs first, purges every
    /// queued old-generation request, and acknowledges; only then may the
    /// reader publish the generation as connected.
    pub async fn replace_transport<R, W>(&self, reader: R, writer: W)
    where
        R: AsyncBufRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let generation = {
            let mut state = self.admission.lock().await;
            if state.replacing {
                warn!("replace_transport: replacement already in progress");
                return;
            }
            state.connected = false;
            state.replacing = true;
            state.generation = state.generation.saturating_add(1);
            self.connected.store(false, Ordering::SeqCst);
            state.generation
        };

        let (writer_installed_tx, writer_installed_rx) = oneshot::channel();
        let replacement = ReplacementWriter {
            writer: Box::new(writer),
            generation,
            installed: writer_installed_tx,
        };
        if self.new_writer_tx.send(replacement).await.is_err() || writer_installed_rx.await.is_err()
        {
            warn!("replace_transport: write task could not install replacement");
            let mut state = self.admission.lock().await;
            if state.generation == generation {
                state.replacing = false;
            }
            return;
        }

        let (reader_installed_tx, reader_installed_rx) = oneshot::channel();
        let replacement = ReplacementReader {
            reader: Box::new(reader),
            generation,
            installed: reader_installed_tx,
        };
        if self.new_reader_tx.send(replacement).await.is_err() || reader_installed_rx.await.is_err()
        {
            warn!("replace_transport: read task could not publish replacement");
            let mut state = self.admission.lock().await;
            if state.generation == generation {
                state.replacing = false;
            }
        }
    }

    /// Stable identity for this channel (see the `id` field).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// A handle that is notified once per successful transport hot-swap. The
    /// editor awaits it to drive event-driven reconnect handling.
    pub fn reconnect_notify(&self) -> Arc<tokio::sync::Notify> {
        self.reconnect_notify.clone()
    }

    /// Current transport hot-swap generation.
    pub fn reconnect_generation(&self) -> u64 {
        self.reconnect_generation.load(Ordering::SeqCst)
    }

    /// Shared generation counter for the editor's reconnect forwarder. This
    /// does not retain the channel or filesystem after a window closes.
    pub fn reconnect_generation_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.reconnect_generation)
    }

    /// Replace the underlying transport (blocking version for non-async contexts).
    ///
    /// Sends the new transport to the tasks and waits until the channel is
    /// marked as connected (i.e., the read task has drained stale requests
    /// and is ready to receive responses on the new reader).
    pub fn replace_transport_blocking<R, W>(&self, reader: R, writer: W)
    where
        R: AsyncBufRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        self.block_on_request(self.replace_transport(reader, writer));

        // `replace_transport` returns only after the read task has published
        // the replacement generation as connected.
        debug_assert!(self.is_connected());
    }

    /// Set the request timeout duration.
    ///
    /// Requests that don't receive a response within this duration will fail
    /// with `ChannelError::Timeout` and the connection will be marked as
    /// disconnected.
    pub fn set_request_timeout(&self, timeout: Duration) {
        self.request_timeout_ms
            .store(timeout.as_millis() as u64, Ordering::SeqCst);
    }

    /// Get the current request timeout duration.
    fn request_timeout(&self) -> Duration {
        Duration::from_millis(self.request_timeout_ms.load(Ordering::SeqCst))
    }

    /// Send a request and wait for the final result (ignoring streaming data)
    pub async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ChannelError> {
        let (generation, mut data_rx, result_rx) =
            self.request_streaming_in_generation(method, params).await?;
        let timeout = self.request_timeout();

        let result = tokio::time::timeout(timeout, async {
            while data_rx.recv().await.is_some() {}
            result_rx
                .await
                .map_err(|_| ChannelError::ChannelClosed)?
                .map_err(ChannelError::Remote)
        })
        .await;

        match result {
            Ok(inner) => inner,
            Err(_elapsed) => {
                warn!("request '{}' timed out after {:?}", method, timeout);
                Self::mark_disconnected(&self.admission, &self.connected, generation).await;
                Err(ChannelError::Timeout)
            }
        }
    }

    async fn request_streaming_in_generation(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<
        (
            u64,
            mpsc::Receiver<serde_json::Value>,
            oneshot::Receiver<Result<serde_json::Value, String>>,
        ),
        ChannelError,
    > {
        // Reserve queue capacity before taking the admission fence. Holding
        // the fence while awaiting a full queue would deadlock reconnect: the
        // failed writer waits for a replacement that cannot advance admission.
        let permit = self
            .write_tx
            .reserve()
            .await
            .map_err(|_| ChannelError::ChannelClosed)?;
        let admission = self.admission.lock().await;
        if !admission.connected {
            return Err(ChannelError::ChannelClosed);
        }
        let generation = admission.generation;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (data_tx, data_rx) = mpsc::channel(self.data_channel_capacity);
        let (result_tx, result_rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(
            id,
            PendingRequest {
                generation,
                data_tx,
                result_tx,
            },
        );

        let request = AgentRequest::new(id, method, params);
        let message = OutboundMessage {
            generation,
            id,
            json: request.to_json_line(),
        };
        permit.send(message);
        drop(admission);
        Ok((generation, data_rx, result_rx))
    }

    /// Send a request that may stream data.
    pub async fn request_streaming(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<
        (
            mpsc::Receiver<serde_json::Value>,
            oneshot::Receiver<Result<serde_json::Value, String>>,
        ),
        ChannelError,
    > {
        let (_, data_rx, result_rx) = self.request_streaming_in_generation(method, params).await?;
        Ok((data_rx, result_rx))
    }

    /// Block on `fut` using the channel's runtime, safe to call whether or
    /// not the caller is already inside a Tokio runtime.
    ///
    /// The blocking wrappers (`request_blocking`, …) are reached from the
    /// plugin thread, which drives its own `current_thread` runtime while
    /// servicing a synchronous plugin call (e.g. a remote `read_dir` from the
    /// Orchestrator dock). A plain `Handle::block_on` there panics with
    /// "Cannot start a runtime from within a runtime" — the crash reported
    /// when arrowing onto an unreachable SSH workspace. When an ambient
    /// runtime is detected, drive the future on a scratch OS thread that
    /// carries no runtime context of its own; the channel's runtime (where
    /// the read/write tasks live) still services the I/O, and the caller's
    /// thread simply waits on the join. Outside any runtime, block directly.
    fn block_on_request<F, T>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T> + Send,
        T: Send,
    {
        if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::scope(|scope| {
                scope
                    .spawn(|| self.runtime_handle.block_on(fut))
                    .join()
                    .expect("remote channel block_on scratch thread panicked")
            })
        } else {
            self.runtime_handle.block_on(fut)
        }
    }

    /// Send a request synchronously (blocking).
    ///
    /// Safe to call from within or outside a Tokio runtime (see
    /// [`Self::block_on_request`]).
    pub fn request_blocking(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ChannelError> {
        self.block_on_request(self.request(method, params))
    }

    /// Send a request and collect all streaming data along with the final result
    pub async fn request_with_data(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(Vec<serde_json::Value>, serde_json::Value), ChannelError> {
        let (generation, mut data_rx, result_rx) =
            self.request_streaming_in_generation(method, params).await?;

        // Idle deadline, reset on every chunk — NOT a cap on total transfer
        // time. A large file streaming over a slow link (e.g. a bandwidth-
        // throttled SSH host) makes steady progress but can take minutes in
        // total; a single `timeout(total)` around the whole collection killed
        // those healthy reads at the first deadline (and flipped `connected`
        // to false, falsely marking the session dead). Here the timeout only
        // fires when *no* chunk arrives for `idle_timeout` — a genuine stall.
        let idle_timeout = self.request_timeout();

        // Collect all streaming data, bounding each await on progress.
        let mut data = Vec::new();
        loop {
            match tokio::time::timeout(idle_timeout, data_rx.recv()).await {
                Ok(Some(chunk)) => {
                    data.push(chunk);

                    // Test hook: simulate slow consumer for backpressure testing.
                    // Zero-cost in production (atomic load + branch-not-taken).
                    let delay_us = TEST_RECV_DELAY_US.load(Ordering::Relaxed);
                    if delay_us > 0 {
                        tokio::time::sleep(tokio::time::Duration::from_micros(delay_us)).await;
                    }
                }
                Ok(None) => break, // stream closed: all data received
                Err(_elapsed) => {
                    warn!("streaming request stalled: no data for {:?}", idle_timeout);
                    Self::mark_disconnected(&self.admission, &self.connected, generation).await;
                    return Err(ChannelError::Timeout);
                }
            }
        }

        // Wait for the final result, also bounded by the idle deadline: once
        // the stream has closed the result should follow promptly, so a hang
        // here is a stall, not slow progress.
        match tokio::time::timeout(idle_timeout, result_rx).await {
            Ok(result) => {
                let result = result
                    .map_err(|_| ChannelError::ChannelClosed)?
                    .map_err(ChannelError::Remote)?;
                Ok((data, result))
            }
            Err(_elapsed) => {
                warn!(
                    "streaming request stalled awaiting result after {:?}",
                    idle_timeout
                );
                Self::mark_disconnected(&self.admission, &self.connected, generation).await;
                Err(ChannelError::Timeout)
            }
        }
    }

    /// Send a request with streaming data, synchronously (blocking).
    ///
    /// Safe to call from within or outside a Tokio runtime (see
    /// [`Self::block_on_request`]).
    pub fn request_with_data_blocking(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(Vec<serde_json::Value>, serde_json::Value), ChannelError> {
        self.block_on_request(self.request_with_data(method, params))
    }

    /// Send a streaming request synchronously, returning receivers for
    /// incremental processing.
    ///
    /// Unlike `request_with_data_blocking` which collects all data into
    /// memory, this returns the raw receivers so callers can process each
    /// chunk as it arrives (e.g., for `walk_files` where the server sends
    /// file paths in batches).
    ///
    /// Use `data_rx.blocking_recv()` to receive chunks from a sync context.
    #[allow(clippy::type_complexity)]
    pub fn request_streaming_blocking(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<
        (
            mpsc::Receiver<serde_json::Value>,
            oneshot::Receiver<Result<serde_json::Value, String>>,
        ),
        ChannelError,
    > {
        self.block_on_request(self.request_streaming(method, params))
    }

    /// Cancel a request
    pub async fn cancel(&self, request_id: u64) -> Result<(), ChannelError> {
        use crate::services::remote::protocol::cancel_params;
        self.request("cancel", cancel_params(request_id)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::AtomicBool;
    use std::task::{Context, Poll};
    use tokio::io::{duplex, BufReader, DuplexStream};

    struct GateWriter {
        inner: DuplexStream,
        dropped: Arc<AtomicBool>,
        write_started: Option<oneshot::Sender<()>>,
        release: oneshot::Receiver<()>,
        released: bool,
    }

    impl Drop for GateWriter {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl AsyncWrite for GateWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            if let Some(write_started) = this.write_started.take() {
                let _ = write_started.send(());
            }
            if !this.released {
                match Pin::new(&mut this.release).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(())) => this.released = true,
                    Poll::Ready(Err(_)) => {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "writer-install gate dropped",
                        )));
                    }
                }
            }
            Pin::new(&mut this.inner).poll_write(cx, buf)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
        }
    }

    struct CaptureWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl AsyncWrite for CaptureWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.bytes.lock().unwrap().extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn eof_waiting_reader_installs_writer_before_publishing_reconnect() {
        let (initial_reader, initial_reader_peer) = duplex(64);
        let (initial_writer, _initial_writer_peer) = duplex(64);
        let old_writer_dropped = Arc::new(AtomicBool::new(false));
        let (write_started_tx, write_started_rx) = oneshot::channel();
        let (release_writer_tx, release_writer_rx) = oneshot::channel();
        let channel = AgentChannel::from_transport(
            BufReader::new(initial_reader),
            GateWriter {
                inner: initial_writer,
                dropped: Arc::clone(&old_writer_dropped),
                write_started: Some(write_started_tx),
                release: release_writer_rx,
                released: false,
            },
            4,
        );

        let _request = channel
            .request_streaming("test", serde_json::json!({}))
            .await
            .expect("initial request must reach the old writer");
        write_started_rx
            .await
            .expect("old writer must block inside its write");
        drop(initial_reader_peer);
        while channel.is_connected() {
            tokio::task::yield_now().await;
        }

        let notify = channel.reconnect_notify();
        let notification = notify.notified();
        tokio::pin!(notification);
        let (replacement_reader, _replacement_reader_peer) = duplex(64);
        let (replacement_writer, _replacement_writer_peer) = duplex(64);
        let mut replacement = std::pin::pin!(
            channel.replace_transport(BufReader::new(replacement_reader), replacement_writer,)
        );
        std::future::poll_fn(|cx| match replacement.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(()) => panic!("reconnect must wait for the writer-install gate"),
        })
        .await;

        assert_eq!(channel.reconnect_generation(), 0);
        std::future::poll_fn(|cx| match notification.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(()) => {
                panic!("reconnect notification published before writer installation")
            }
        })
        .await;

        release_writer_tx
            .send(())
            .expect("writer-install gate must still be closed");
        replacement.await;
        notification.await;

        assert!(old_writer_dropped.load(Ordering::SeqCst));
        assert_eq!(channel.reconnect_generation(), 1);
        assert!(channel.is_connected());
        let second_notification = notify.notified();
        tokio::pin!(second_notification);
        std::future::poll_fn(|cx| match second_notification.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(()) => panic!("reconnect published more than one notification"),
        })
        .await;
    }

    #[tokio::test]
    async fn queued_old_generation_request_is_not_written_after_replacement() {
        let (initial_reader, initial_reader_peer) = duplex(64);
        let (initial_writer, _initial_writer_peer) = duplex(1024);
        let (write_started_tx, write_started_rx) = oneshot::channel();
        let (release_writer_tx, release_writer_rx) = oneshot::channel();
        let channel = AgentChannel::from_transport(
            BufReader::new(initial_reader),
            GateWriter {
                inner: initial_writer,
                dropped: Arc::new(AtomicBool::new(false)),
                write_started: Some(write_started_tx),
                release: release_writer_rx,
                released: false,
            },
            4,
        );

        let _in_flight = channel
            .request_streaming("write", serde_json::json!({"path": "/old"}))
            .await
            .expect("first request reaches old writer");
        write_started_rx.await.expect("old writer is blocked");
        let _queued = channel
            .request_streaming("rm", serde_json::json!({"path": "/must-not-run"}))
            .await
            .expect("second old-generation request is queued");

        drop(initial_reader_peer);
        while channel.is_connected() {
            tokio::task::yield_now().await;
        }

        let captured = Arc::new(Mutex::new(Vec::new()));
        let (replacement_reader, _replacement_reader_peer) = duplex(64);
        let replacement = channel.replace_transport(
            BufReader::new(replacement_reader),
            CaptureWriter {
                bytes: Arc::clone(&captured),
            },
        );
        tokio::pin!(replacement);
        std::future::poll_fn(|cx| match replacement.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(()) => panic!("replacement must wait for the blocked old writer"),
        })
        .await;
        release_writer_tx.send(()).expect("release old writer");
        replacement.await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        assert!(
            captured.lock().unwrap().is_empty(),
            "queued old-generation mutation must be purged before replacement publication"
        );
    }
}
