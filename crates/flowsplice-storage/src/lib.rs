#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
};

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::signature::EcdsaKeyPair;
use flowsplice_core::{
    protocol::Role,
    statistics::{
        FIVE_MINUTE_SECS, MetricValue, STATISTICS_METRIC_VERSION, STATISTICS_REPORT_VERSION,
        SignedStatisticsReport, StatisticsReportPayload, VerifiedStatisticsReport,
        five_minute_bucket_start,
    },
};
use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::sync::oneshot;

pub const SCHEMA_VERSION: u32 = 1;
pub const LATENCY_HISTOGRAM_BOUNDS_MS: [u64; 12] =
    [1, 5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000];

const METADATA: TableDefinition<&[u8], &[u8]> = TableDefinition::new("metadata");
const TRAVEL_CONTROL_STATE: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("travel_control_state");
const RELAY_HISTORY: TableDefinition<&[u8], &[u8]> = TableDefinition::new("relay_history");
const METRIC_5M: TableDefinition<&[u8], &[u8]> = TableDefinition::new("metric_5m");
const METRIC_DAILY: TableDefinition<&[u8], &[u8]> = TableDefinition::new("metric_daily");
const REPORT_OUTBOX: TableDefinition<&[u8], &[u8]> = TableDefinition::new("report_outbox");
const ACCEPTED_REPORTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("accepted_reports");
const ENROLLMENT_OUTBOX: TableDefinition<&[u8], &[u8]> = TableDefinition::new("enrollment_outbox");
const ENROLLMENT_INBOX: TableDefinition<&[u8], &[u8]> = TableDefinition::new("enrollment_inbox");
const HOME_ENROLLMENT_INBOX: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("home_enrollment_inbox");

const SCHEMA_VERSION_KEY: &[u8] = b"schema_version";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Table {
    Metadata,
    TravelControlState,
    RelayHistory,
    Metric5m,
    MetricDaily,
    ReportOutbox,
    AcceptedReports,
    EnrollmentOutbox,
    EnrollmentInbox,
    HomeEnrollmentInbox,
}

impl Table {
    const fn definition(self) -> TableDefinition<'static, &'static [u8], &'static [u8]> {
        match self {
            Self::Metadata => METADATA,
            Self::TravelControlState => TRAVEL_CONTROL_STATE,
            Self::RelayHistory => RELAY_HISTORY,
            Self::Metric5m => METRIC_5M,
            Self::MetricDaily => METRIC_DAILY,
            Self::ReportOutbox => REPORT_OUTBOX,
            Self::AcceptedReports => ACCEPTED_REPORTS,
            Self::EnrollmentOutbox => ENROLLMENT_OUTBOX,
            Self::EnrollmentInbox => ENROLLMENT_INBOX,
            Self::HomeEnrollmentInbox => HOME_ENROLLMENT_INBOX,
        }
    }
}

#[derive(Clone, Debug)]
pub enum WriteOperation {
    Put {
        table: Table,
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Delete {
        table: Table,
        key: Vec<u8>,
    },
}

#[derive(Clone, Debug, Default)]
pub struct WriteBatch {
    operations: Vec<WriteOperation>,
}

impl WriteBatch {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn put(mut self, table: Table, key: Vec<u8>, value: Vec<u8>) -> Self {
        self.operations
            .push(WriteOperation::Put { table, key, value });
        self
    }

    /// Adds one JSON value to this batch.
    ///
    /// # Errors
    ///
    /// Returns an error when JSON serialization fails.
    pub fn put_json<T: Serialize>(self, table: Table, key: Vec<u8>, value: &T) -> Result<Self> {
        Ok(self.put(
            table,
            key,
            serde_json::to_vec(value).context("failed to encode redb value")?,
        ))
    }

    #[must_use]
    pub fn delete(mut self, table: Table, key: Vec<u8>) -> Self {
        self.operations.push(WriteOperation::Delete { table, key });
        self
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }
}

#[derive(Clone)]
pub struct StateStore {
    database: Arc<Database>,
    path: Arc<PathBuf>,
}

impl StateStore {
    /// Opens or creates a protected, versioned `redb` state store.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe path, filesystem failure, database failure, or unsupported
    /// schema version.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        validate_store_path(path)?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create state directory {}", parent.display())
            })?;
        }
        let database = Database::create(path)
            .with_context(|| format!("failed to open redb state store {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to protect state store {}", path.display()))?;
        let store = Self {
            database: Arc::new(database),
            path: Arc::new(path.to_path_buf()),
        };
        store.initialize_schema()?;
        Ok(store)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn initialize_schema(&self) -> Result<()> {
        let mut write = self.database.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        for table in [
            Table::Metadata,
            Table::TravelControlState,
            Table::RelayHistory,
            Table::Metric5m,
            Table::MetricDaily,
            Table::ReportOutbox,
            Table::AcceptedReports,
            Table::EnrollmentOutbox,
            Table::EnrollmentInbox,
            Table::HomeEnrollmentInbox,
        ] {
            drop(write.open_table(table.definition())?);
        }
        {
            let mut metadata = write.open_table(METADATA)?;
            let stored_version = metadata
                .get(SCHEMA_VERSION_KEY)?
                .map(|value| value.value().to_vec());
            match stored_version {
                Some(bytes) => {
                    let version = bytes
                        .as_slice()
                        .try_into()
                        .map(u32::from_be_bytes)
                        .map_err(|_| anyhow!("redb schema version has an invalid length"))?;
                    if version > SCHEMA_VERSION {
                        bail!(
                            "state store schema {version} is newer than supported {SCHEMA_VERSION}"
                        );
                    }
                    if version < SCHEMA_VERSION {
                        bail!("state store schema {version} requires an unavailable migration");
                    }
                }
                None => {
                    metadata.insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION.to_be_bytes().as_slice())?;
                }
            }
        }
        write.commit()?;
        Ok(())
    }

    /// Reads one raw value.
    ///
    /// # Errors
    ///
    /// Returns an error when the database transaction or table read fails.
    pub fn get(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let read = self.database.begin_read()?;
        let table = read.open_table(table.definition())?;
        Ok(table.get(key)?.map(|value| value.value().to_vec()))
    }

    /// Reads and decodes one JSON value.
    ///
    /// # Errors
    ///
    /// Returns an error when the database read or JSON decoding fails.
    pub fn get_json<T: DeserializeOwned>(&self, table: Table, key: &[u8]) -> Result<Option<T>> {
        self.get(table, key)?
            .map(|bytes| serde_json::from_slice(&bytes).context("failed to decode redb value"))
            .transpose()
    }

    /// Returns every raw key/value pair whose key starts with `prefix`.
    ///
    /// # Errors
    ///
    /// Returns an error when the database transaction, table, or iterator fails.
    pub fn scan_prefix(&self, table: Table, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let read = self.database.begin_read()?;
        let table = read.open_table(table.definition())?;
        let mut values = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            if key.value().starts_with(prefix) {
                values.push((key.value().to_vec(), value.value().to_vec()));
            }
        }
        Ok(values)
    }

    /// Commits one batch with immediate durability.
    ///
    /// # Errors
    ///
    /// Returns an error when a transaction, table mutation, or durable commit fails.
    pub fn apply_immediate(&self, batch: WriteBatch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let mut write = self.database.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        for operation in batch.operations {
            match operation {
                WriteOperation::Put { table, key, value } => {
                    write
                        .open_table(table.definition())?
                        .insert(key.as_slice(), value.as_slice())?;
                }
                WriteOperation::Delete { table, key } => {
                    write
                        .open_table(table.definition())?
                        .remove(key.as_slice())?;
                }
            }
        }
        write.commit()?;
        Ok(())
    }
}

fn validate_store_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("state_store must be non-empty");
    }
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.file_type().is_file())
    {
        bail!(
            "state_store {} must be a regular non-symlink file",
            path.display()
        );
    }
    Ok(())
}

struct WriterCommand {
    batch: WriteBatch,
    reply: oneshot::Sender<Result<()>>,
}

#[derive(Clone)]
pub struct StorageWriter {
    sender: mpsc::SyncSender<WriterCommand>,
}

impl StorageWriter {
    /// Starts a bounded dedicated writer thread.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero-sized queue or when the writer thread cannot be created.
    pub fn start(store: StateStore, queue_capacity: usize) -> Result<Self> {
        if queue_capacity == 0 {
            bail!("storage writer queue capacity must be positive");
        }
        let (sender, receiver) = mpsc::sync_channel::<WriterCommand>(queue_capacity);
        std::thread::Builder::new()
            .name("flowsplice-redb-writer".to_owned())
            .spawn(move || {
                while let Ok(command) = receiver.recv() {
                    let _ = command.reply.send(store.apply_immediate(command.batch));
                }
            })
            .context("failed to start redb writer thread")?;
        Ok(Self { sender })
    }

    /// Queues and waits for one durable batch.
    ///
    /// # Errors
    ///
    /// Returns an error when the queue is unavailable, the writer stops, or the commit fails.
    pub async fn write(&self, batch: WriteBatch) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.sender
            .try_send(WriterCommand { batch, reply })
            .map_err(|error| anyhow!("redb writer queue unavailable: {error}"))?;
        result.await.map_err(|_| anyhow!("redb writer stopped"))??;
        Ok(())
    }

    /// Attempts to queue one batch without waiting for capacity.
    ///
    /// # Errors
    ///
    /// Returns an error when the bounded writer queue is unavailable.
    pub fn try_write(&self, batch: WriteBatch) -> Result<oneshot::Receiver<Result<()>>> {
        let (reply, result) = oneshot::channel();
        self.sender
            .try_send(WriterCommand { batch, reply })
            .map_err(|error| anyhow!("redb writer queue unavailable: {error}"))?;
        Ok(result)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricIdentity {
    pub metric_family: String,
    pub dimensions: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricPoint {
    pub bucket_start_unix_secs: u64,
    pub identity: MetricIdentity,
    pub value: MetricValue,
}

#[derive(Clone, Debug, Default)]
struct MetricDelta {
    count: u64,
    sum: u64,
    min: u64,
    max: u64,
    histogram: Vec<u64>,
}

#[derive(Clone)]
pub struct LocalStatistics {
    store: StateStore,
    accumulator: Arc<Mutex<BTreeMap<(u64, MetricIdentity), MetricDelta>>>,
    dropped_events: Arc<AtomicU64>,
    #[cfg(feature = "fault-injection")]
    injected_flush_failures: Arc<AtomicU64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedReport {
    pub digest_sha256: String,
    pub payload: StatisticsReportPayload,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricRollup {
    pub metric_family: String,
    pub dimensions: BTreeMap<String, String>,
    pub bucket_count: u64,
    pub count: u64,
    pub sum: u64,
    pub min: u64,
    pub max: u64,
    pub weighted_average: f64,
    pub average_per_five_minutes: f64,
    pub histogram: Vec<u64>,
}

#[derive(Default)]
struct RollupAccumulator {
    bucket_count: u64,
    count: u64,
    sum: u64,
    min: u64,
    max: u64,
    histogram: Vec<u64>,
}

/// Builds weighted window summaries from five-minute points.
///
/// `retain_dimensions=false` produces one overview row per metric family. When it is true, the
/// full bounded canonical dimensions are preserved for role-specific breakdown tables.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn summarize_metric_points(
    points: &[MetricPoint],
    retain_dimensions: bool,
) -> Vec<MetricRollup> {
    let mut grouped = BTreeMap::<(String, BTreeMap<String, String>), RollupAccumulator>::new();
    for point in points {
        let dimensions = if retain_dimensions {
            point.identity.dimensions.clone()
        } else {
            BTreeMap::new()
        };
        let aggregate = grouped
            .entry((point.identity.metric_family.clone(), dimensions))
            .or_default();
        aggregate.bucket_count = aggregate.bucket_count.saturating_add(1);
        let prior_count = aggregate.count;
        aggregate.count = aggregate.count.saturating_add(point.value.count);
        aggregate.sum = aggregate.sum.saturating_add(point.value.sum);
        if prior_count == 0 {
            aggregate.min = point.value.min;
            aggregate.max = point.value.max;
        } else if point.value.count > 0 {
            aggregate.min = aggregate.min.min(point.value.min);
            aggregate.max = aggregate.max.max(point.value.max);
        }
        if aggregate.histogram.len() < point.value.histogram.len() {
            aggregate.histogram.resize(point.value.histogram.len(), 0);
        }
        for (target, value) in aggregate.histogram.iter_mut().zip(&point.value.histogram) {
            *target = target.saturating_add(*value);
        }
    }
    grouped
        .into_iter()
        .map(|((metric_family, dimensions), aggregate)| MetricRollup {
            metric_family,
            dimensions,
            bucket_count: aggregate.bucket_count,
            count: aggregate.count,
            sum: aggregate.sum,
            min: aggregate.min,
            max: aggregate.max,
            weighted_average: if aggregate.count == 0 {
                0.0
            } else {
                aggregate.sum as f64 / aggregate.count as f64
            },
            average_per_five_minutes: if aggregate.bucket_count == 0 {
                0.0
            } else {
                aggregate.sum as f64 / aggregate.bucket_count as f64
            },
            histogram: aggregate.histogram,
        })
        .collect()
}

#[must_use]
pub fn accepted_reports_as_metric_points(reports: &[AcceptedReport]) -> Vec<MetricPoint> {
    reports
        .iter()
        .map(|report| MetricPoint {
            bucket_start_unix_secs: report.payload.bucket_start_unix_secs,
            identity: MetricIdentity {
                metric_family: report.payload.metric_family.clone(),
                dimensions: report.payload.dimensions.clone(),
            },
            value: report.payload.value.clone(),
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportAcceptance {
    Inserted,
    Replaced,
    Idempotent,
    Stale,
}

impl LocalStatistics {
    #[must_use]
    pub fn new(store: StateStore) -> Self {
        Self {
            store,
            accumulator: Arc::new(Mutex::new(BTreeMap::new())),
            dropped_events: Arc::new(AtomicU64::new(0)),
            #[cfg(feature = "fault-injection")]
            injected_flush_failures: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Arms deterministic write failures for end-to-end fault testing.
    ///
    /// This API is absent from production builds. Each injected failure occurs after pending
    /// deltas have been detached and exercises the same restoration path as a redb write error.
    #[cfg(feature = "fault-injection")]
    pub fn inject_flush_failures(&self, count: u64) {
        self.injected_flush_failures.store(count, Ordering::Release);
    }

    #[cfg(feature = "fault-injection")]
    #[must_use]
    pub fn injected_flush_failures_remaining(&self) -> u64 {
        self.injected_flush_failures.load(Ordering::Acquire)
    }

    pub fn record(
        &self,
        now: u64,
        metric_family: impl Into<String>,
        dimensions: BTreeMap<String, String>,
        value: u64,
        histogram_sample: Option<u64>,
    ) {
        let identity = MetricIdentity {
            metric_family: metric_family.into(),
            dimensions,
        };
        let Ok(mut accumulator) = self.accumulator.try_lock() else {
            self.dropped_events.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let delta = accumulator
            .entry((five_minute_bucket_start(now), identity))
            .or_default();
        delta.count = delta.count.saturating_add(1);
        delta.sum = delta.sum.saturating_add(value);
        if delta.count == 1 {
            delta.min = value;
            delta.max = value;
        } else {
            delta.min = delta.min.min(value);
            delta.max = delta.max.max(value);
        }
        if let Some(sample) = histogram_sample {
            if delta.histogram.is_empty() {
                delta.histogram = vec![0; LATENCY_HISTOGRAM_BOUNDS_MS.len() + 1];
            }
            let index = LATENCY_HISTOGRAM_BOUNDS_MS
                .iter()
                .position(|bound| sample <= *bound)
                .unwrap_or(LATENCY_HISTOGRAM_BOUNDS_MS.len());
            delta.histogram[index] = delta.histogram[index].saturating_add(1);
        }
    }

    /// Atomically merges pending deltas and stages signed outbox reports.
    ///
    /// # Errors
    ///
    /// Returns an error when locking, database access, serialization, signing, or commit fails.
    pub fn flush_and_stage(
        &self,
        deployment_id: &str,
        reporter_role: Role,
        reporter_id: &str,
        certificate_pem: &str,
        signer: &EcdsaKeyPair,
    ) -> Result<usize> {
        let deltas = {
            let mut accumulator = self
                .accumulator
                .lock()
                .map_err(|_| anyhow!("statistics accumulator lock is poisoned"))?;
            std::mem::take(&mut *accumulator)
        };
        if deltas.is_empty() {
            return Ok(0);
        }
        #[cfg(feature = "fault-injection")]
        if self
            .injected_flush_failures
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                (remaining > 0).then(|| remaining - 1)
            })
            .is_ok()
        {
            self.restore_deltas(deltas);
            bail!("injected statistics redb write failure");
        }
        let result = self
            .build_flush_batch(
                &deltas,
                deployment_id,
                reporter_role,
                reporter_id,
                certificate_pem,
                signer,
            )
            .and_then(|batch| self.store.apply_immediate(batch));
        match result {
            Ok(()) => Ok(deltas.len()),
            Err(error) => {
                self.restore_deltas(deltas);
                Err(error)
            }
        }
    }

    fn build_flush_batch(
        &self,
        deltas: &BTreeMap<(u64, MetricIdentity), MetricDelta>,
        deployment_id: &str,
        reporter_role: Role,
        reporter_id: &str,
        certificate_pem: &str,
        signer: &EcdsaKeyPair,
    ) -> Result<WriteBatch> {
        let mut batch = WriteBatch::new();
        for ((bucket_start, identity), delta) in deltas {
            let key = metric_key(*bucket_start, identity)?;
            let mut value = self
                .store
                .get_json::<MetricPoint>(Table::Metric5m, &key)?
                .map_or_else(empty_metric_value, |point| point.value);
            merge_delta(&mut value, delta);
            value.revision = value.revision.saturating_add(1);
            let point = MetricPoint {
                bucket_start_unix_secs: *bucket_start,
                identity: identity.clone(),
                value: value.clone(),
            };
            let payload = StatisticsReportPayload {
                version: STATISTICS_REPORT_VERSION,
                deployment_id: deployment_id.to_owned(),
                reporter_role,
                reporter_id: reporter_id.to_owned(),
                bucket_start_unix_secs: *bucket_start,
                bucket_end_unix_secs: bucket_start.saturating_add(FIVE_MINUTE_SECS),
                metric_family: identity.metric_family.clone(),
                dimensions: identity.dimensions.clone(),
                report_sequence: value.revision,
                value,
            };
            let report = SignedStatisticsReport::sign(&payload, certificate_pem, signer)?;
            batch = batch
                .put_json(Table::Metric5m, key.clone(), &point)?
                .put_json(Table::ReportOutbox, key, &report)?;
        }
        Ok(batch)
    }

    fn restore_deltas(&self, deltas: BTreeMap<(u64, MetricIdentity), MetricDelta>) {
        if let Ok(mut accumulator) = self.accumulator.lock() {
            for (key, delta) in deltas {
                merge_pending_delta(accumulator.entry(key).or_default(), delta);
            }
        } else {
            self.dropped_events.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Queries local five-minute points in a half-open time range.
    ///
    /// # Errors
    ///
    /// Returns an error when the database scan fails.
    pub fn query(&self, from: u64, to: u64) -> Result<Vec<MetricPoint>> {
        let mut points = self
            .store
            .scan_prefix(Table::Metric5m, b"")?
            .into_iter()
            .filter_map(|(_, value)| serde_json::from_slice::<MetricPoint>(&value).ok())
            .filter(|point| {
                point.bucket_start_unix_secs >= from && point.bucket_start_unix_secs < to
            })
            .collect::<Vec<_>>();
        points.sort_by(|left, right| {
            left.bucket_start_unix_secs
                .cmp(&right.bucket_start_unix_secs)
                .then_with(|| left.identity.cmp(&right.identity))
        });
        Ok(points)
    }

    /// Returns at most `limit` durable outbox reports.
    ///
    /// # Errors
    ///
    /// Returns an error when scanning or decoding an outbox record fails.
    pub fn pending_reports(&self, limit: usize) -> Result<Vec<(Vec<u8>, SignedStatisticsReport)>> {
        self.store
            .scan_prefix(Table::ReportOutbox, b"")?
            .into_iter()
            .take(limit)
            .map(|(key, value)| {
                serde_json::from_slice(&value)
                    .context("statistics report outbox contains an invalid value")
                    .map(|report| (key, report))
            })
            .collect()
    }

    /// Removes an acknowledged outbox report only when its digest matches.
    ///
    /// # Errors
    ///
    /// Returns an error when reading, decoding, hashing, or committing fails.
    pub fn acknowledge_report(&self, key: &[u8], digest_sha256: &str) -> Result<bool> {
        let Some(report) = self
            .store
            .get_json::<SignedStatisticsReport>(Table::ReportOutbox, key)?
        else {
            return Ok(false);
        };
        if report.digest_sha256()? != digest_sha256 {
            return Ok(false);
        }
        self.store
            .apply_immediate(WriteBatch::new().delete(Table::ReportOutbox, key.to_vec()))?;
        Ok(true)
    }

    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.dropped_events.load(Ordering::Relaxed)
    }
}

impl StateStore {
    /// Idempotently accepts one verified reporter revision.
    ///
    /// # Errors
    ///
    /// Returns an error for a conflicting same-revision report or any storage/encoding failure.
    pub fn accept_statistics_report(
        &self,
        report: &VerifiedStatisticsReport,
    ) -> Result<ReportAcceptance> {
        let key = accepted_report_key(&report.payload)?;
        let mut write = self.database.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let acceptance = {
            let mut table = write.open_table(ACCEPTED_REPORTS)?;
            let existing = table
                .get(key.as_slice())?
                .map(|value| {
                    serde_json::from_slice::<AcceptedReport>(value.value())
                        .context("failed to decode accepted statistics report")
                })
                .transpose()?;
            let acceptance = match existing {
                Some(existing)
                    if existing.payload.value.revision > report.payload.value.revision =>
                {
                    ReportAcceptance::Stale
                }
                Some(existing)
                    if existing.payload.value.revision == report.payload.value.revision =>
                {
                    if existing.digest_sha256 != report.digest_sha256 {
                        bail!("conflicting statistics reports use the same revision");
                    }
                    ReportAcceptance::Idempotent
                }
                Some(_) => ReportAcceptance::Replaced,
                None => ReportAcceptance::Inserted,
            };
            if matches!(
                acceptance,
                ReportAcceptance::Inserted | ReportAcceptance::Replaced
            ) {
                let accepted = AcceptedReport {
                    digest_sha256: report.digest_sha256.clone(),
                    payload: report.payload.clone(),
                };
                let encoded = serde_json::to_vec(&accepted)
                    .context("failed to encode accepted statistics report")?;
                table.insert(key.as_slice(), encoded.as_slice())?;
            }
            acceptance
        };
        write.commit()?;
        Ok(acceptance)
    }

    /// Queries accepted reports in a half-open time range.
    ///
    /// # Errors
    ///
    /// Returns an error when the database scan fails.
    pub fn query_accepted_reports(&self, from: u64, to: u64) -> Result<Vec<AcceptedReport>> {
        let mut reports = self
            .scan_prefix(Table::AcceptedReports, b"")?
            .into_iter()
            .filter_map(|(_, value)| serde_json::from_slice::<AcceptedReport>(&value).ok())
            .filter(|report| {
                report.payload.bucket_start_unix_secs >= from
                    && report.payload.bucket_start_unix_secs < to
            })
            .collect::<Vec<_>>();
        reports.sort_by(|left, right| {
            left.payload
                .bucket_start_unix_secs
                .cmp(&right.payload.bucket_start_unix_secs)
                .then_with(|| left.payload.reporter_id.cmp(&right.payload.reporter_id))
        });
        Ok(reports)
    }
}

/// Encodes a timestamp-leading stable key with bounded string parts.
///
/// # Errors
///
/// Returns an error when a part exceeds 65,535 bytes.
pub fn ordered_key(parts: &[&str], timestamp: u64) -> Result<Vec<u8>> {
    let mut key = timestamp.to_be_bytes().to_vec();
    for part in parts {
        let bytes = part.as_bytes();
        let len = u16::try_from(bytes.len()).context("ordered key part exceeds 65535 bytes")?;
        key.extend_from_slice(&len.to_be_bytes());
        key.extend_from_slice(bytes);
    }
    Ok(key)
}

fn empty_metric_value() -> MetricValue {
    MetricValue {
        version: STATISTICS_METRIC_VERSION,
        revision: 0,
        count: 0,
        sum: 0,
        min: 0,
        max: 0,
        histogram: Vec::new(),
    }
}

fn merge_delta(value: &mut MetricValue, delta: &MetricDelta) {
    let previous_count = value.count;
    value.count = value.count.saturating_add(delta.count);
    value.sum = value.sum.saturating_add(delta.sum);
    if previous_count == 0 {
        value.min = delta.min;
        value.max = delta.max;
    } else if delta.count > 0 {
        value.min = value.min.min(delta.min);
        value.max = value.max.max(delta.max);
    }
    if !delta.histogram.is_empty() {
        if value.histogram.len() < delta.histogram.len() {
            value.histogram.resize(delta.histogram.len(), 0);
        }
        for (target, source) in value.histogram.iter_mut().zip(&delta.histogram) {
            *target = target.saturating_add(*source);
        }
    }
}

fn merge_pending_delta(target: &mut MetricDelta, source: MetricDelta) {
    let previous_count = target.count;
    target.count = target.count.saturating_add(source.count);
    target.sum = target.sum.saturating_add(source.sum);
    if previous_count == 0 {
        target.min = source.min;
        target.max = source.max;
    } else if source.count > 0 {
        target.min = target.min.min(source.min);
        target.max = target.max.max(source.max);
    }
    if target.histogram.len() < source.histogram.len() {
        target.histogram.resize(source.histogram.len(), 0);
    }
    for (left, right) in target.histogram.iter_mut().zip(source.histogram) {
        *left = left.saturating_add(right);
    }
}

fn metric_key(bucket_start: u64, identity: &MetricIdentity) -> Result<Vec<u8>> {
    let identity = serde_json::to_string(identity).context("failed to encode metric identity")?;
    ordered_key(&[&identity], bucket_start)
}

fn accepted_report_key(payload: &StatisticsReportPayload) -> Result<Vec<u8>> {
    let dimensions =
        serde_json::to_string(&payload.dimensions).context("failed to encode report dimensions")?;
    ordered_key(
        &[
            &payload.deployment_id,
            &format!("{:?}", payload.reporter_role),
            &payload.reporter_id,
            &payload.metric_family,
            &dimensions,
        ],
        payload.bucket_start_unix_secs,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};

    #[test]
    fn immediate_batch_survives_reopen_and_prefix_scan() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("state.redb");
        let store = StateStore::open(&path)?;
        store.apply_immediate(
            WriteBatch::new()
                .put(Table::RelayHistory, b"relay/one".to_vec(), b"1".to_vec())
                .put(Table::RelayHistory, b"relay/two".to_vec(), b"2".to_vec()),
        )?;
        drop(store);
        let reopened = StateStore::open(&path)?;
        assert_eq!(
            reopened.get(Table::RelayHistory, b"relay/one")?,
            Some(b"1".to_vec())
        );
        assert_eq!(
            reopened.scan_prefix(Table::RelayHistory, b"relay/")?.len(),
            2
        );
        Ok(())
    }

    #[test]
    fn bucket_and_ordered_keys_are_stable() -> Result<()> {
        assert_eq!(five_minute_bucket_start(599), 300);
        assert!(ordered_key(&["relay-1"], 300)? < ordered_key(&["relay-1"], 600)?);
        Ok(())
    }

    #[test]
    fn statistics_flush_reopen_and_server_deduplication() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = StateStore::open(directory.path().join("state.redb"))?;
        let statistics = LocalStatistics::new(store.clone());
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)?;
        statistics.record(
            301,
            "relay_transport_upload_bytes",
            BTreeMap::from([("home_id".to_owned(), "home-1".to_owned())]),
            42,
            None,
        );
        assert_eq!(
            statistics.flush_and_stage(
                "deployment-1",
                Role::Relay,
                "relay-1",
                "certificate",
                &key,
            )?,
            1
        );
        let pending = statistics.pending_reports(8)?;
        let [(outbox_key, signed)] = pending.as_slice() else {
            bail!("expected exactly one staged report");
        };
        let verified = signed.verify(key.public_key().as_ref())?;
        assert_eq!(
            store.accept_statistics_report(&verified)?,
            ReportAcceptance::Inserted
        );
        assert_eq!(
            store.accept_statistics_report(&verified)?,
            ReportAcceptance::Idempotent
        );
        assert!(statistics.acknowledge_report(outbox_key, &verified.digest_sha256)?);
        drop(statistics);
        drop(store);
        let reopened = StateStore::open(directory.path().join("state.redb"))?;
        assert_eq!(reopened.query_accepted_reports(300, 600)?.len(), 1);
        Ok(())
    }

    #[test]
    fn concurrent_identical_statistics_reports_are_atomic_and_idempotent() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = StateStore::open(directory.path().join("state.redb"))?;
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)?;
        let payload = StatisticsReportPayload {
            version: STATISTICS_REPORT_VERSION,
            deployment_id: "deployment-1".to_owned(),
            reporter_role: Role::Relay,
            reporter_id: "relay-1".to_owned(),
            bucket_start_unix_secs: 300,
            bucket_end_unix_secs: 600,
            metric_family: "relay_transport_upload_bytes".to_owned(),
            dimensions: BTreeMap::from([("home_id".to_owned(), "home-1".to_owned())]),
            report_sequence: 1,
            value: MetricValue {
                version: STATISTICS_METRIC_VERSION,
                revision: 1,
                count: 1,
                sum: 42,
                min: 42,
                max: 42,
                histogram: Vec::new(),
            },
        };
        let report = Arc::new(
            SignedStatisticsReport::sign(&payload, "certificate", &key)?
                .verify(key.public_key().as_ref())?,
        );
        let mut threads = Vec::new();
        for _ in 0..16 {
            let store = store.clone();
            let report = Arc::clone(&report);
            threads.push(std::thread::spawn(move || {
                store.accept_statistics_report(&report)
            }));
        }
        let results = threads
            .into_iter()
            .map(|thread| {
                thread
                    .join()
                    .map_err(|_| anyhow!("statistics writer panicked"))?
            })
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == ReportAcceptance::Inserted)
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == ReportAcceptance::Idempotent)
                .count(),
            15
        );
        assert_eq!(store.query_accepted_reports(300, 600)?.len(), 1);
        Ok(())
    }
}
