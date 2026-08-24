use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use uuid::Uuid;

use crate::db::Database;
use crate::importer::{
    build_destination, execute_import, import_request_units, result_to_record, stat_source,
    ImportRequest, ImportResult,
};

/// One file within an import job. `status` is one of:
/// queued | running | imported | skipped | preview | failed | cancelled.
#[derive(Clone, Serialize)]
pub struct JobItem {
    pub index: usize,
    pub name: String,
    pub source_path: String,
    pub destination: String,
    pub status: String,
    pub bytes: u64,
    pub total: u64,
    /// What `bytes`/`total` count: "bytes" for copy/move, "items" for hardlink
    /// and test, which complete instantly and are measured as 1 unit each. The
    /// UI needs this to avoid rendering an item count as a file size.
    pub unit: &'static str,
    pub error: Option<String>,
}

/// Progress unit for an action: copies and moves stream bytes, hardlinks and
/// dry runs are instantaneous and counted as whole items.
fn unit_for(action: &str) -> &'static str {
    if action == "copy" || action == "move" {
        "bytes"
    } else {
        "items"
    }
}

#[derive(Default)]
struct JobState {
    completed_units: u64,
    total_units: u64,
    total_items: usize,
    state: String,
    label: String,
    media_type: String,
    cancel_all: bool,
    cancel_items: HashSet<usize>,
    items: Vec<JobItem>,
    // The work queue, parallel to `items`. The worker pulls from here by index
    // and may grow while running (a second import appended to the same queue).
    requests: Vec<ImportRequest>,
    results: Vec<ImportResult>,
    error: Option<String>,
}

pub struct Job {
    pub id: String,
    pub seq: u64,
    state: Arc<Mutex<JobState>>,
}

#[derive(Serialize)]
pub struct JobSnapshot {
    pub id: String,
    pub seq: u64,
    pub label: String,
    pub state: String,
    pub percent: u32,
    pub completed: u64,
    pub total: u64,
    pub total_items: usize,
    pub completed_items: usize,
    pub failed_items: usize,
    pub cancelled_items: usize,
    pub active: bool,
    /// Aggregate unit over every item: "bytes", "items", or "mixed" when the
    /// job combines both and the totals cannot be rendered as one quantity.
    pub unit: &'static str,
    pub error: Option<String>,
    pub items: Vec<JobItem>,
}

fn build_item(index: usize, request: &ImportRequest) -> JobItem {
    JobItem {
        index,
        name: request
            .source_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        source_path: request.source_path.to_string_lossy().to_string(),
        // Planned destination name, shown before the copy runs; updated to the
        // actual final path once the file completes.
        destination: build_destination(request)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        status: "queued".to_string(),
        bytes: 0,
        total: import_request_units(request),
        unit: unit_for(&request.action),
        error: None,
    }
}

/// The unit shared by every item, or "mixed" when a job combines byte-measured
/// copies with item-counted hardlinks — in that case the summed totals are not
/// one quantity and the UI renders them as plain numbers.
fn aggregate_unit(items: &[JobItem]) -> &'static str {
    let mut iter = items.iter();
    match iter.next() {
        None => "items",
        Some(first) => {
            if iter.all(|item| item.unit == first.unit) {
                first.unit
            } else {
                "mixed"
            }
        }
    }
}

fn make_label(media_type: &str, total_items: usize) -> String {
    let media = if media_type.is_empty() {
        "media"
    } else {
        media_type
    };
    format!(
        "{media} · {total_items} item{}",
        if total_items == 1 { "" } else { "s" }
    )
}

impl Job {
    fn lock(&self) -> std::sync::MutexGuard<'_, JobState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn snapshot(&self) -> JobSnapshot {
        let s = self.lock();
        let percent = if s.total_units == 0 {
            100
        } else {
            ((s.completed_units as f64 / s.total_units as f64) * 100.0).min(100.0) as u32
        };
        let mut completed_items = 0;
        let mut failed_items = 0;
        let mut cancelled_items = 0;
        for item in &s.items {
            match item.status.as_str() {
                "imported" | "skipped" | "preview" => completed_items += 1,
                "failed" => failed_items += 1,
                "cancelled" => cancelled_items += 1,
                _ => {}
            }
        }
        JobSnapshot {
            id: self.id.clone(),
            seq: self.seq,
            label: s.label.clone(),
            state: s.state.clone(),
            percent,
            completed: s.completed_units,
            total: s.total_units,
            total_items: s.total_items,
            completed_items,
            failed_items,
            cancelled_items,
            active: s.state == "running",
            unit: aggregate_unit(&s.items),
            error: s.error.clone(),
            items: s.items.clone(),
        }
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.lock().state.as_str(), "done" | "cancelled" | "failed")
    }

    /// A job that finished with every file imported/skipped and no failures or
    /// cancellations — safe to clear from the list.
    pub fn is_clean_completed(&self) -> bool {
        let s = self.lock();
        s.state == "done"
            && !s
                .items
                .iter()
                .any(|it| it.status == "failed" || it.status == "cancelled")
    }

    pub fn results(&self) -> Vec<ImportResult> {
        self.lock().results.clone()
    }

    /// Append more files to a still-running job's queue so they import after the
    /// current ones, on the same worker (no concurrent disk access). Returns
    /// false if the job is no longer running, so the caller starts a fresh job.
    fn try_append(&self, requests: &[ImportRequest]) -> bool {
        let mut s = self.lock();
        // `cancel_all` is set while the state is still "running" — the worker
        // only flips it once it has drained the queue. Appending in that window
        // would hand the new files straight to the cancel check and silently
        // cancel them, so refuse and let the caller open a fresh job.
        if s.state != "running" || s.cancel_all {
            return false;
        }
        for request in requests {
            // Items keep their own media_type/action/root and execute correctly,
            // but a job spanning more than one media type can't be labelled with a
            // single one — fall back to the generic "media" label in that case.
            if !s.media_type.is_empty() && request.media_type != s.media_type {
                s.media_type.clear();
            }
            let index = s.items.len() + 1;
            let item = build_item(index, request);
            s.total_units += item.total;
            s.total_items += 1;
            s.items.push(item);
            s.requests.push(request.clone());
        }
        let media = s.media_type.clone();
        let total = s.total_items;
        s.label = make_label(&media, total);
        true
    }

    /// Cancel the whole job: the running file is aborted and every queued file
    /// is skipped.
    pub fn request_cancel(&self) {
        let mut s = self.lock();
        if s.state == "running" {
            s.cancel_all = true;
        }
    }

    /// Cancel a single file by its 1-based index. A queued file is skipped; the
    /// file currently copying is aborted while the rest of the job continues.
    pub fn request_cancel_item(&self, index: usize) {
        let mut s = self.lock();
        if s.state != "running" {
            return;
        }
        let still_active = s
            .items
            .get(index.wrapping_sub(1))
            .map(|it| it.status == "queued" || it.status == "running")
            .unwrap_or(false);
        if still_active {
            s.cancel_items.insert(index);
        }
    }
}

/// How many finished jobs to keep once a new one starts.
///
/// "Clear finished" only drops jobs that completed cleanly, so a job with a
/// single failed file used to live for the lifetime of the process — and every
/// 1s poll re-serialized its whole item list. Active jobs are never pruned.
const MAX_FINISHED_JOBS: usize = 20;

/// Drop the oldest finished jobs past [`MAX_FINISHED_JOBS`], newest kept.
fn prune_finished(jobs: &mut Vec<Arc<Job>>) {
    let finished: usize = jobs.iter().filter(|job| job.is_finished()).count();
    if finished <= MAX_FINISHED_JOBS {
        return;
    }
    let mut to_drop = finished - MAX_FINISHED_JOBS;
    // `jobs` is push-ordered, so the oldest are at the front.
    jobs.retain(|job| {
        if to_drop > 0 && job.is_finished() {
            to_drop -= 1;
            false
        } else {
            true
        }
    });
}

#[derive(Clone, Default)]
pub struct JobManager {
    jobs: Arc<Mutex<Vec<Arc<Job>>>>,
    seq: Arc<AtomicU64>,
}

impl JobManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Job>> {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|j| j.id == id)
            .cloned()
    }

    /// Remove finished jobs that completed cleanly (all imported/skipped, no
    /// failures or cancellations). Returns how many were cleared.
    pub fn clear_completed(&self) -> usize {
        let mut guard = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let before = guard.len();
        guard.retain(|job| !job.is_clean_completed());
        before - guard.len()
    }

    /// All jobs, newest first.
    pub fn list(&self) -> Vec<Arc<Job>> {
        let mut jobs: Vec<Arc<Job>> = self.jobs.lock().unwrap_or_else(|e| e.into_inner()).clone();
        jobs.sort_by_key(|j| std::cmp::Reverse(j.seq));
        jobs
    }

    /// Enqueue an import. If a job is already running, the files are appended to
    /// its queue (single serial worker, no concurrent disk access). Otherwise a
    /// new job is created and its worker started.
    pub fn start(
        &self,
        requests: Vec<ImportRequest>,
        db: Database,
        copy_rate_limit_mbps: Option<f64>,
    ) -> Arc<Job> {
        let mut guard = self.jobs.lock().unwrap_or_else(|e| e.into_inner());

        // Append to the active job if there is one.
        for job in guard.iter() {
            if job.try_append(&requests) {
                return job.clone();
            }
        }

        // Otherwise start a fresh job + worker.
        let total_units: u64 = requests.iter().map(import_request_units).sum();
        let items: Vec<JobItem> = requests
            .iter()
            .enumerate()
            .map(|(i, request)| build_item(i + 1, request))
            .collect();
        let media_type = requests
            .first()
            .map(|r| r.media_type.clone())
            .unwrap_or_default();
        let label = make_label(&media_type, requests.len());

        let job = Arc::new(Job {
            id: Uuid::new_v4().simple().to_string(),
            seq: self.seq.fetch_add(1, Ordering::SeqCst),
            state: Arc::new(Mutex::new(JobState {
                state: "running".to_string(),
                total_units,
                total_items: requests.len(),
                label,
                media_type,
                items,
                requests,
                ..Default::default()
            })),
        });
        guard.push(job.clone());
        prune_finished(&mut guard);
        drop(guard);

        let worker_state = job.state.clone();
        tokio::task::spawn_blocking(move || {
            run_job(worker_state, db, copy_rate_limit_mbps);
        });

        job
    }
}

fn lock(state: &Arc<Mutex<JobState>>) -> std::sync::MutexGuard<'_, JobState> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// Mark every still-queued/running file as cancelled (whole-job stop).
fn cancel_remaining(s: &mut JobState) {
    for item in s.items.iter_mut() {
        if item.status == "queued" || item.status == "running" {
            item.status = "cancelled".to_string();
        }
    }
}

fn run_job(state: Arc<Mutex<JobState>>, db: Database, copy_rate_limit_mbps: Option<f64>) {
    let mut index = 0usize; // zero-based cursor into the queue

    loop {
        let request;
        let item_units;
        let item_start;
        {
            let mut s = lock(&state);
            if s.cancel_all {
                cancel_remaining(&mut s);
                s.state = "cancelled".to_string();
                return;
            }
            // Queue drained: finish. New work appended after this point starts a
            // fresh job (the queue is empty, so nothing runs concurrently).
            if index >= s.requests.len() {
                s.completed_units = s.total_units;
                s.state = "done".to_string();
                return;
            }
            let item_index = index + 1;
            // File cancelled while still queued: skip it, keep going.
            if s.cancel_items.contains(&item_index) {
                let units = s.items.get(index).map(|it| it.total).unwrap_or(0);
                if let Some(item) = s.items.get_mut(index) {
                    item.status = "cancelled".to_string();
                }
                s.completed_units += units;
                index += 1;
                continue;
            }
            request = s.requests[index].clone();
            item_units = import_request_units(&request);
            item_start = s.completed_units;
            if let Some(item) = s.items.get_mut(index) {
                item.status = "running".to_string();
                item.bytes = 0;
                item.total = item_units;
            }
        }

        let cursor = index;
        let progress_state = state.clone();
        let progress = move |copied: u64, total: u64| {
            let mut s = progress_state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(item) = s.items.get_mut(cursor) {
                item.bytes = copied;
                item.total = total.max(item_units);
            }
            s.completed_units = if total == 0 {
                item_start + item_units
            } else {
                item_start + ((copied as f64 / total as f64) * item_units as f64) as u64
            };
        };

        let cancel_state = state.clone();
        let item_index = index + 1;
        let cancel = move || {
            let s = cancel_state.lock().unwrap_or_else(|e| e.into_inner());
            s.cancel_all || s.cancel_items.contains(&item_index)
        };

        // Capture source stat before the import runs: a `move` deletes the
        // source, so it must be read up front to be recorded.
        let source_stat = stat_source(&request.source_path);
        let result = execute_import(
            request,
            Some(&progress),
            copy_rate_limit_mbps,
            Some(&cancel),
        );
        db.insert_import(&result_to_record(&result, source_stat));

        {
            let mut s = lock(&state);
            let status = result.result.clone();
            let final_path = result.final_path.clone();
            let error = result.error.clone();
            s.results.push(result);
            if let Some(item) = s.items.get_mut(cursor) {
                item.status = status;
                item.destination = final_path;
                item.error = error;
                item.bytes = item.total;
            }
            s.completed_units = item_start + item_units;

            if s.cancel_all {
                cancel_remaining(&mut s);
                s.state = "cancelled".to_string();
                return;
            }
        }

        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finished_job(seq: u64, state: &str) -> Arc<Job> {
        Arc::new(Job {
            id: format!("job{seq}"),
            seq,
            state: Arc::new(Mutex::new(JobState {
                state: state.to_string(),
                ..Default::default()
            })),
        })
    }

    #[test]
    fn prune_keeps_active_jobs_and_the_newest_finished_ones() {
        // Oldest first, as `start` pushes them.
        let mut jobs: Vec<Arc<Job>> = (0..MAX_FINISHED_JOBS as u64 + 5)
            .map(|seq| finished_job(seq, "failed"))
            .collect();
        // An active job interleaved partway through must survive regardless.
        jobs.insert(2, finished_job(999, "running"));

        prune_finished(&mut jobs);

        let finished: Vec<u64> = jobs
            .iter()
            .filter(|j| j.is_finished())
            .map(|j| j.seq)
            .collect();
        assert_eq!(finished.len(), MAX_FINISHED_JOBS);
        // The five oldest finished jobs were dropped, newest kept.
        assert_eq!(finished.first(), Some(&5));
        assert!(
            jobs.iter().any(|j| j.seq == 999 && !j.is_finished()),
            "an active job must never be pruned"
        );
    }

    #[test]
    fn prune_is_a_noop_below_the_cap() {
        let mut jobs: Vec<Arc<Job>> = (0..3).map(|seq| finished_job(seq, "done")).collect();
        prune_finished(&mut jobs);
        assert_eq!(jobs.len(), 3);
    }

    #[test]
    fn progress_unit_follows_the_action() {
        assert_eq!(unit_for("copy"), "bytes");
        assert_eq!(unit_for("move"), "bytes");
        assert_eq!(unit_for("hardlink"), "items");
        assert_eq!(unit_for("test"), "items");
    }
}
