//! One background writer for every persisted document.
//!
//! The UI thread hands the writer an owned, typed payload and a path; it never
//! encodes JSON or touches the filesystem for a saved document. The writer owns
//! a single FIFO worker, so documents written by one handler keep their order,
//! while a document changed again before the worker reaches it keeps only its
//! latest state.
//!
//! Writes report back through [`WriteReport`]s. A report names the document and
//! the generation of the payload that was written, so a slow write that lands
//! after a newer schedule cannot clear the newer pending state.
//!
//! [`Persistence::flush`] is a barrier: every job queued before the call is
//! written before the returned task completes, and jobs queued afterwards are
//! not part of it. Callers may put a deadline on the wait; a timed-out flush
//! leaves the write running in the background.

use std::collections::{HashMap, VecDeque};
use std::future::{Future, poll_fn};
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Poll;
use std::time::Duration;

use async_channel::{Receiver, Sender, TrySendError};
use gpui::{BackgroundExecutor, Task};
use posthaste_core::cookies::CookieWrite;
use posthaste_core::history::{HistoryEntry, HistoryId};
use posthaste_core::model::{Collection, Environment, RequestId, RequestSpec, Workspace};
use posthaste_core::preferences::Preferences;
use posthaste_core::recents::RecentWorkspaces;
use posthaste_core::session::SessionDocument;
use posthaste_core::storage::{JsonStore, StorageError};

/// How many wake-ups may wait for the worker. A wake-up names a document with
/// pending state, so the queue never needs more entries than there are
/// documents.
const COMMAND_QUEUE: usize = 64;

/// Which persisted document a job belongs to.
///
/// The identity is the coalescing key: two schedules for the same document
/// replace one another, while schedules for different documents keep their
/// order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Document {
    Preferences,
    Recents,
    Session,
    Cookies,
    Workspace,
    Collection { slug: String },
    Environment { slug: String },
    Request { slug: String, id: RequestId },
    HistoryEntry { id: HistoryId },
}

impl Document {
    /// A short name used by logs and test failures.
    pub fn label(&self) -> String {
        match self {
            Self::Preferences => "preferences".to_owned(),
            Self::Recents => "recents".to_owned(),
            Self::Session => "session".to_owned(),
            Self::Cookies => "cookies".to_owned(),
            Self::Workspace => "workspace".to_owned(),
            Self::Collection { slug } => format!("collection:{slug}"),
            Self::Environment { slug } => format!("environment:{slug}"),
            Self::Request { slug, id } => format!("request:{slug}/{id}"),
            Self::HistoryEntry { id } => format!("history:{id}"),
        }
    }
}

/// The owned value a job writes. The worker encodes it; the UI thread only
/// clones or moves the current in-memory state.
#[derive(Debug)]
pub enum DocumentPayload {
    Preferences(Preferences),
    Recents(RecentWorkspaces),
    Session(SessionDocument),
    Workspace(Workspace),
    Collection(Box<Collection>),
    Environment(Box<Environment>),
    Request(Box<RequestSpec>),
    HistoryEntry(Box<HistoryEntry>),
    Cookies(CookieWrite),
}

/// One unit of work: a document identity, the generation of this payload, the
/// destination, and the owned value.
#[derive(Debug)]
pub struct WriteJob {
    pub document: Document,
    pub generation: u64,
    pub path: PathBuf,
    pub payload: DocumentPayload,
}

/// The result of one write.
#[derive(Debug, Clone)]
pub struct WriteReport {
    pub document: Document,
    pub generation: u64,
    pub path: PathBuf,
    /// `Err` carries a user-facing description of the failure.
    pub result: Result<(), String>,
}

/// The visible state of one document's latest write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentState {
    /// Nothing has been written or queued in this session.
    Idle,
    /// A newer payload is waiting for the writer.
    Pending,
    /// The latest write succeeded.
    Saved,
    /// The latest write failed, with the reason.
    Failed(String),
}

/// One document a flush could not write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushFailure {
    pub document: Document,
    pub path: PathBuf,
    pub message: String,
}

/// What a flush wrote, and whether it finished in time.
#[derive(Debug, Clone, Default)]
pub struct FlushOutcome {
    pub written: Vec<PathBuf>,
    pub failures: Vec<FlushFailure>,
    /// `true` when the caller's deadline elapsed before the barrier completed.
    pub timed_out: bool,
}

impl FlushOutcome {
    /// Whether every job the barrier covered was written.
    pub fn is_success(&self) -> bool {
        !self.timed_out && self.failures.is_empty()
    }

    fn timed_out() -> Self {
        Self {
            written: Vec::new(),
            failures: Vec::new(),
            timed_out: true,
        }
    }
}

/// A barrier command. Barriers travel on their own channel: a flush must not
/// wait for room among the wake-ups, least of all during close.
#[derive(Debug)]
enum Barrier {
    Flush {
        jobs: Vec<WriteJob>,
        ack: Sender<FlushOutcome>,
    },
    FlushBlocking {
        jobs: Vec<WriteJob>,
        ack: std::sync::mpsc::SyncSender<FlushOutcome>,
    },
}

#[derive(Debug)]
enum Command {
    Work,
}

#[derive(Debug, Default)]
struct Pending {
    jobs: HashMap<Document, WriteJob>,
    order: VecDeque<Document>,
}

impl Pending {
    fn lock(pending: &Mutex<Self>) -> MutexGuard<'_, Self> {
        pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn insert(&mut self, job: WriteJob) -> bool {
        let first = !self.jobs.contains_key(&job.document);
        if first {
            self.order.push_back(job.document.clone());
        }
        self.jobs.insert(job.document.clone(), job);
        first
    }

    fn take_next(&mut self) -> Option<WriteJob> {
        while let Some(document) = self.order.pop_front() {
            if let Some(job) = self.jobs.remove(&document) {
                return Some(job);
            }
        }
        None
    }

    fn take_all(&mut self) -> Vec<WriteJob> {
        let mut jobs = Vec::with_capacity(self.jobs.len());
        while let Some(document) = self.order.pop_front() {
            if let Some(job) = self.jobs.remove(&document) {
                jobs.push(job);
            }
        }
        jobs
    }

    fn contains(&self, document: &Document) -> bool {
        self.jobs.contains_key(document)
    }

    fn len(&self) -> usize {
        self.jobs.len()
    }
}

struct Shared {
    pending: Mutex<Pending>,
    last: Mutex<HashMap<Document, WriteReport>>,
    generation: AtomicU64,
    writes: AtomicUsize,
}

impl Shared {
    fn last_writes(&self) -> MutexGuard<'_, HashMap<Document, WriteReport>> {
        self.last.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct Inner {
    shared: Arc<Shared>,
    commands: Option<Sender<Command>>,
    barriers: Option<Sender<Barrier>>,
    reports: Sender<WriteReport>,
    report_receiver: Receiver<WriteReport>,
    executor: BackgroundExecutor,
    synchronous: bool,
}

/// The app's writer. Cloning shares the same worker and pending state.
#[derive(Clone)]
pub struct Persistence {
    inner: Arc<Inner>,
}

impl Persistence {
    /// Starts the single background worker on `executor`.
    pub fn background(executor: BackgroundExecutor) -> Self {
        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending::default()),
            last: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(1),
            writes: AtomicUsize::new(0),
        });
        let (commands, command_receiver) = async_channel::bounded(COMMAND_QUEUE);
        let (barriers, barrier_receiver) = async_channel::unbounded();
        let (reports, report_receiver) = async_channel::unbounded();
        let worker_shared = shared.clone();
        let worker_reports = reports.clone();
        executor
            .spawn(async move {
                run_worker(
                    worker_shared,
                    command_receiver,
                    barrier_receiver,
                    worker_reports,
                )
                .await;
            })
            .detach();
        Self {
            inner: Arc::new(Inner {
                shared,
                commands: Some(commands),
                barriers: Some(barriers),
                reports,
                report_receiver,
                executor,
                synchronous: false,
            }),
        }
    }

    /// A writer that performs each write where it is scheduled. Only the test
    /// suite uses it, so tests that read a file after advancing a timer keep
    /// working without pumping the background executor.
    pub fn synchronous(executor: BackgroundExecutor) -> Self {
        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending::default()),
            last: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(1),
            writes: AtomicUsize::new(0),
        });
        let (reports, report_receiver) = async_channel::unbounded();
        Self {
            inner: Arc::new(Inner {
                shared,
                commands: None,
                barriers: None,
                reports,
                report_receiver,
                executor,
                synchronous: true,
            }),
        }
    }

    pub fn is_synchronous(&self) -> bool {
        self.inner.synchronous
    }

    /// A receiver for every write result, in write order.
    pub fn report_receiver(&self) -> Receiver<WriteReport> {
        self.inner.report_receiver.clone()
    }

    /// The generation the next schedule will receive.
    pub fn next_generation(&self) -> u64 {
        self.inner.shared.generation.load(Ordering::SeqCst)
    }

    /// How many writes this writer performed, including synchronous ones.
    pub fn write_count(&self) -> usize {
        self.inner.shared.writes.load(Ordering::SeqCst)
    }

    /// Queues `payload` as the latest state of `document`.
    ///
    /// In the normal writer this only takes a lock and sends a wake-up; the
    /// encode and filesystem write happen on the worker.
    pub fn schedule(&self, document: Document, path: PathBuf, payload: DocumentPayload) {
        let generation = self.inner.shared.generation.fetch_add(1, Ordering::SeqCst);
        let job = WriteJob {
            document: document.clone(),
            generation,
            path,
            payload,
        };
        if self.inner.synchronous {
            let report = perform_write(&job);
            self.record(report);
            return;
        }
        let first = Pending::lock(&self.inner.shared.pending).insert(job);
        if first {
            self.wake();
        }
    }

    /// Schedules a write and returns its result only in the test adapter.
    ///
    /// The synchronous adapter finishes inline, so a caller that projects a
    /// result without pumping tasks (the cookie jar) can use it directly. The
    /// report is still delivered; the background writer returns `None`.
    pub fn schedule_reporting(
        &self,
        document: Document,
        path: PathBuf,
        payload: DocumentPayload,
    ) -> Option<Result<(), String>> {
        if !self.inner.synchronous {
            self.schedule(document, path, payload);
            return None;
        }
        let generation = self.inner.shared.generation.fetch_add(1, Ordering::SeqCst);
        let job = WriteJob {
            document,
            generation,
            path,
            payload,
        };
        let report = perform_write(&job);
        let result = report.result.clone();
        self.record(report);
        Some(result)
    }

    /// Queues a write and waits for it to finish.
    pub fn flush(&self) -> Task<FlushOutcome> {
        if self.inner.synchronous {
            return Task::ready(FlushOutcome::default());
        }
        let jobs = Pending::lock(&self.inner.shared.pending).take_all();
        let Some(barriers) = self.inner.barriers.clone() else {
            return Task::ready(FlushOutcome::timed_out());
        };
        // Enqueue before returning: a caller's deadline must never drop a
        // barrier whose jobs already left the pending map.
        let (ack, outcome) = async_channel::bounded(1);
        if barriers.try_send(Barrier::Flush { jobs, ack }).is_err() {
            return Task::ready(FlushOutcome::timed_out());
        }
        self.inner.executor.spawn(async move {
            outcome
                .recv()
                .await
                .unwrap_or_else(|_| FlushOutcome::timed_out())
        })
    }

    /// [`Self::flush`] with a caller-side deadline.
    ///
    /// The deadline only bounds the wait: a timed-out flush keeps writing, and
    /// its reports still arrive.
    pub fn flush_with_deadline(&self, deadline: Duration) -> Task<FlushOutcome> {
        if self.inner.synchronous {
            return Task::ready(FlushOutcome::default());
        }
        let flush = self.flush();
        let timer = self.inner.executor.timer(deadline);
        self.inner.executor.spawn(async move {
            let mut flush = pin!(flush);
            let mut timer = pin!(timer);
            poll_fn(move |cx| {
                if let Poll::Ready(outcome) = flush.as_mut().poll(cx) {
                    return Poll::Ready(outcome);
                }
                if timer.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(FlushOutcome::timed_out());
                }
                Poll::Pending
            })
            .await
        })
    }

    /// [`Self::flush`] for a synchronous caller such as window close.
    ///
    /// The barrier travels on its own unbounded channel. When the deadline
    /// elapses the caller gets a timed-out outcome and the worker keeps going.
    pub fn flush_blocking(&self, deadline: Duration) -> FlushOutcome {
        if self.inner.synchronous {
            return FlushOutcome::default();
        }
        let jobs = Pending::lock(&self.inner.shared.pending).take_all();
        let Some(barriers) = self.inner.barriers.clone() else {
            return FlushOutcome::timed_out();
        };
        let (ack, outcome) = std::sync::mpsc::sync_channel(1);
        if barriers
            .try_send(Barrier::FlushBlocking { jobs, ack })
            .is_err()
        {
            return FlushOutcome::timed_out();
        }
        outcome
            .recv_timeout(deadline)
            .unwrap_or_else(|_| FlushOutcome::timed_out())
    }

    /// The visible state of `document`'s latest write.
    pub fn document_state(&self, document: &Document) -> DocumentState {
        if Pending::lock(&self.inner.shared.pending).contains(document) {
            return DocumentState::Pending;
        }
        match self.inner.shared.last_writes().get(document) {
            Some(report) => match &report.result {
                Ok(()) => DocumentState::Saved,
                Err(message) => DocumentState::Failed(message.clone()),
            },
            None => DocumentState::Idle,
        }
    }

    /// Whether any document has a newer payload waiting.
    pub fn has_pending(&self) -> bool {
        Pending::lock(&self.inner.shared.pending).len() > 0
    }

    fn wake(&self) {
        let Some(commands) = self.inner.commands.clone() else {
            return;
        };
        match commands.try_send(Command::Work) {
            Ok(()) => {}
            Err(TrySendError::Full(command)) => {
                // The worker is behind. Wait for room off the UI thread; the
                // pending state already holds the latest payload.
                let executor = self.inner.executor.clone();
                executor
                    .spawn(async move {
                        let _ = commands.send(command).await;
                    })
                    .detach();
            }
            Err(TrySendError::Closed(_)) => {}
        }
    }

    fn record(&self, report: WriteReport) {
        self.inner
            .shared
            .last_writes()
            .insert(report.document.clone(), report.clone());
        let _ = self.inner.reports.try_send(report);
    }
}

async fn run_worker(
    shared: Arc<Shared>,
    commands: Receiver<Command>,
    barriers: Receiver<Barrier>,
    reports: Sender<WriteReport>,
) {
    loop {
        let Some(next) = next_work(&commands, &barriers).await else {
            break;
        };
        match next {
            Next::Command(Command::Work) => {
                let Some(job) = Pending::lock(&shared.pending).take_next() else {
                    continue;
                };
                let report = perform_write(&job);
                deliver(&shared, &reports, report).await;
            }
            Next::Barrier(Barrier::Flush { jobs, ack }) => {
                let outcome = run_flush(&shared, &reports, jobs).await;
                let _ = ack.send(outcome).await;
            }
            Next::Barrier(Barrier::FlushBlocking { jobs, ack }) => {
                let outcome = run_flush(&shared, &reports, jobs).await;
                let _ = ack.try_send(outcome);
            }
        }
    }
}

enum Next {
    Command(Command),
    Barrier(Barrier),
}

/// Waits for either a wake-up or a barrier, preferring barriers so a close-time
/// flush is never starved by autosaves.
async fn next_work(commands: &Receiver<Command>, barriers: &Receiver<Barrier>) -> Option<Next> {
    let mut command = pin!(commands.recv());
    let mut barrier = pin!(barriers.recv());
    poll_fn(move |cx| {
        if let Poll::Ready(result) = barrier.as_mut().poll(cx) {
            return match result {
                Ok(barrier) => Poll::Ready(Some(Next::Barrier(barrier))),
                Err(_) => Poll::Ready(None),
            };
        }
        if let Poll::Ready(result) = command.as_mut().poll(cx) {
            return match result {
                Ok(command) => Poll::Ready(Some(Next::Command(command))),
                Err(_) => Poll::Ready(None),
            };
        }
        Poll::Pending
    })
    .await
}

async fn run_flush(
    shared: &Shared,
    reports: &Sender<WriteReport>,
    jobs: Vec<WriteJob>,
) -> FlushOutcome {
    let mut outcome = FlushOutcome::default();
    for job in jobs {
        let path = job.path.clone();
        let report = perform_write(&job);
        let succeeded = report.result.is_ok();
        if let Err(message) = &report.result {
            outcome.failures.push(FlushFailure {
                document: job.document.clone(),
                path: path.clone(),
                message: message.clone(),
            });
        }
        deliver(shared, reports, report).await;
        if succeeded {
            outcome.written.push(path);
        }
    }
    outcome
}

async fn deliver(shared: &Shared, reports: &Sender<WriteReport>, report: WriteReport) {
    shared
        .last_writes()
        .insert(report.document.clone(), report.clone());
    let _ = reports.send(report).await;
}

/// Encodes and writes one job. Runs only on the worker in the normal writer.
fn perform_write(job: &WriteJob) -> WriteReport {
    let result = write_payload(&job.path, &job.payload);
    WriteReport {
        document: job.document.clone(),
        generation: job.generation,
        path: job.path.clone(),
        result,
    }
}

fn write_payload(path: &Path, payload: &DocumentPayload) -> Result<(), String> {
    if let DocumentPayload::Cookies(CookieWrite::Remove { .. }) = payload {
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("could not remove the cookie file: {error}")),
        };
    }
    let store = JsonStore::at(path);
    let bytes = match payload {
        DocumentPayload::Preferences(value) => store.encode(value),
        DocumentPayload::Recents(value) => store.encode(value),
        DocumentPayload::Session(value) => store.encode(value),
        DocumentPayload::Workspace(value) => store.encode(value),
        DocumentPayload::Collection(value) => store.encode(value.as_ref()),
        DocumentPayload::Environment(value) => store.encode(value.as_ref()),
        DocumentPayload::Request(value) => store.encode(value.as_ref()),
        DocumentPayload::HistoryEntry(value) => store.encode(value.as_ref()),
        DocumentPayload::Cookies(write) => {
            write.encode().map_err(|message| StorageError::Serialize {
                path: path.to_path_buf(),
                message,
            })
        }
    }
    .map_err(|error| error.to_string())?;
    store
        .write_encoded(&bytes)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn background(cx: &mut gpui::TestAppContext) -> Persistence {
        Persistence::background(cx.background_executor.clone())
    }

    #[gpui::test]
    async fn three_schedules_leave_only_the_latest_state_on_disk(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("workspace.json");
        let persistence = background(cx);
        let document = Document::Workspace;

        for value in 0..3 {
            persistence.schedule(
                document.clone(),
                path.clone(),
                DocumentPayload::Workspace(Workspace::new(format!("workspace-{value}"))),
            );
        }
        assert!(persistence.has_pending(), "the write is queued");

        let outcome = persistence.flush().await;
        assert!(outcome.is_success(), "{outcome:?}");
        let loaded: Workspace = JsonStore::at(&path)
            .load()
            .expect("load workspace")
            .into_loaded()
            .expect("written");
        assert_eq!(loaded.name, "workspace-2", "the third state won");
        assert_eq!(persistence.write_count(), 1, "the first two were coalesced");
    }

    #[gpui::test]
    async fn scheduling_does_not_touch_the_filesystem_until_the_worker_runs(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("preferences.json");
        let persistence = background(cx);

        persistence.schedule(
            Document::Preferences,
            path.clone(),
            DocumentPayload::Preferences(Preferences::default()),
        );

        assert!(!path.exists(), "the UI thread only queued the payload");
        persistence.flush().await;
        assert!(path.is_file(), "the worker wrote it");
    }

    #[gpui::test]
    async fn a_flush_covers_jobs_queued_before_it_not_after(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().expect("dir");
        let before = dir.path().join("before.json");
        let after = dir.path().join("after.json");
        let persistence = background(cx);

        persistence.schedule(
            Document::Preferences,
            before.clone(),
            DocumentPayload::Preferences(Preferences::default()),
        );
        let flush = persistence.flush();
        persistence.schedule(
            Document::Recents,
            after.clone(),
            DocumentPayload::Recents(RecentWorkspaces::default()),
        );

        let outcome = flush.await;
        assert!(outcome.is_success(), "{outcome:?}");
        assert_eq!(outcome.written, vec![before.clone()]);
        assert!(before.is_file());

        let outcome = persistence.flush().await;
        assert_eq!(outcome.written, vec![after.clone()]);
    }

    #[gpui::test]
    async fn a_finished_flush_lets_a_dependent_read_see_the_write(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("recents.json");
        let persistence = background(cx);
        let mut recents = RecentWorkspaces::default();
        recents.record(dir.path());

        persistence.schedule(
            Document::Recents,
            path.clone(),
            DocumentPayload::Recents(recents.clone()),
        );
        let outcome = persistence.flush().await;

        assert!(outcome.is_success(), "{outcome:?}");
        let loaded = JsonStore::at(&path)
            .load::<RecentWorkspaces>()
            .expect("load")
            .into_loaded()
            .expect("written");
        assert_eq!(loaded, recents);
    }

    #[gpui::test]
    async fn a_flush_deadline_returns_without_cancelling_the_write(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("session.json");
        let persistence = background(cx);
        persistence.schedule(
            Document::Session,
            path.clone(),
            DocumentPayload::Preferences(Preferences::default()),
        );

        let outcome = persistence.flush_with_deadline(Duration::ZERO).await;
        assert!(outcome.timed_out, "{outcome:?}");

        // The barrier remains queued; a later flush waits for it and the
        // worker still writes.
        let outcome = persistence.flush().await;
        assert!(outcome.is_success(), "{outcome:?}");
        assert!(path.is_file());
    }

    #[gpui::test]
    async fn reports_name_the_document_and_generation(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("workspace.json");
        let persistence = background(cx);
        let generation = persistence.next_generation();
        let reports = persistence.report_receiver();

        persistence.schedule(
            Document::Workspace,
            path.clone(),
            DocumentPayload::Workspace(Workspace::new("reported")),
        );
        persistence.flush().await;

        let report = reports.recv().await.expect("report");
        assert_eq!(report.document, Document::Workspace);
        assert_eq!(report.generation, generation);
        assert_eq!(report.path, path);
        assert!(report.result.is_ok());
        assert_eq!(
            persistence.document_state(&Document::Workspace),
            DocumentState::Saved
        );
    }

    #[gpui::test]
    async fn reports_project_pending_success_and_failure_without_reading_the_file(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().expect("dir");
        let good = dir.path().join("preferences.json");
        let persistence = background(cx);

        persistence.schedule(
            Document::Preferences,
            good.clone(),
            DocumentPayload::Preferences(Preferences::default()),
        );
        assert_eq!(
            persistence.document_state(&Document::Preferences),
            DocumentState::Pending,
            "a queued document is pending before the worker runs"
        );

        let outcome = persistence.flush().await;
        assert!(outcome.is_success(), "{outcome:?}");
        assert_eq!(
            persistence.document_state(&Document::Preferences),
            DocumentState::Saved
        );

        // A parent that is a file makes the write fail after encoding.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("blocker");
        persistence.schedule(
            Document::Recents,
            blocker.join("recents.json"),
            DocumentPayload::Recents(RecentWorkspaces::default()),
        );
        assert_eq!(
            persistence.document_state(&Document::Recents),
            DocumentState::Pending
        );
        let outcome = persistence.flush().await;
        assert!(!outcome.is_success(), "{outcome:?}");
        assert!(matches!(
            persistence.document_state(&Document::Recents),
            DocumentState::Failed(message) if !message.is_empty()
        ));
    }

    #[gpui::test]
    async fn a_blocking_flush_reports_a_deadline_instead_of_waiting(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("session.json");
        let persistence = background(cx);
        persistence.schedule(
            Document::Session,
            path.clone(),
            DocumentPayload::Preferences(Preferences::default()),
        );

        let outcome = persistence.flush_blocking(Duration::ZERO);
        assert!(outcome.timed_out, "{outcome:?}");
        assert!(
            !path.exists(),
            "the caller returns before the worker can write"
        );

        // Returning on the deadline does not lose the queued job.
        let outcome = persistence.flush().await;
        assert!(outcome.is_success(), "{outcome:?}");
        assert!(path.is_file());
    }

    #[gpui::test]
    async fn the_synchronous_writer_reports_without_a_worker(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("preferences.json");
        let persistence = Persistence::synchronous(cx.background_executor.clone());
        assert!(persistence.is_synchronous());

        persistence.schedule(
            Document::Preferences,
            path.clone(),
            DocumentPayload::Preferences(Preferences::default()),
        );

        assert!(path.is_file(), "the synchronous writer already wrote it");
        assert!(!persistence.has_pending());
        assert_eq!(
            persistence.document_state(&Document::Preferences),
            DocumentState::Saved
        );
        assert!(persistence.flush().await.is_success());
    }

    trait Loaded<T> {
        fn into_loaded(self) -> Option<T>;
    }

    impl<T> Loaded<T> for posthaste_core::storage::LoadOutcome<T> {
        fn into_loaded(self) -> Option<T> {
            match self {
                posthaste_core::storage::LoadOutcome::Absent => None,
                posthaste_core::storage::LoadOutcome::Loaded(value) => Some(value),
            }
        }
    }
}
