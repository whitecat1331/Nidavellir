use std::path::PathBuf;
use std::thread::ThreadId;

use anyhow::Context;
use gpui::{SerializedThreadTaskTimings, TasksIncluded, profiler};
use util::ResultExt;

use crate::STARTUP_TIME;

/// Writes a hang trace unless the performance profiler is enabled or tracing.
///
/// Routine hangs (task polls, actions) skip the write while the profiler is on:
/// the profiler already samples those timings, and on the dev/nightly channels
/// the profiler is enabled by default, so writing on every routine hang would
/// churn the three-file trace directory with short-hang noise. Serious hangs
/// take the [`save_incident`] path instead.
pub fn save_any(main_thread_id: ThreadId) -> Option<PathBuf> {
    save(main_thread_id, false)
}

/// Writes a hang trace even when the performance profiler is enabled or
/// tracing.
///
/// Used by the GPUI hang-incident path: an incident is a foreground stall that
/// starves the reporter loop, and the in-memory profiler trace is lost if the
/// process is killed before the user reads it — so the trace must land on disk
/// regardless of the profiler setting (which is on by default on dev/nightly).
pub fn save_incident(main_thread_id: ThreadId) -> Option<PathBuf> {
    save(main_thread_id, true)
}

fn save(main_thread_id: ThreadId, force: bool) -> Option<PathBuf> {
    cleanup_old_hang_traces();
    let thread_timings = gpui::profiler::get_all_timings(TasksIncluded::CompletedAndRunning);

    let thread_timings = thread_timings
        .into_iter()
        .map(|mut timings| {
            if timings.thread_id == main_thread_id {
                timings.thread_name = Some("main".to_string());
            }

            SerializedThreadTaskTimings::convert(*STARTUP_TIME.get().unwrap(), timings)
        })
        .collect::<Vec<_>>();

    let Some(timings) = serde_json::to_string(&thread_timings)
        .context("hang timings serialization")
        .log_err()
    else {
        return None;
    };

    if profiler::trace_enabled() && !force {
        None
    } else {
        cleanup_old_hang_traces();
        let trace_path = paths::hang_traces_dir().join(&format!(
            "hang-{}.miniprof.json",
            chrono::Local::now().format("%Y-%m-%d_%H-%M-%S")
        ));
        std::fs::write(&trace_path, timings)
            .context("hang trace file writing")
            .log_err();
        Some(trace_path)
    }
}

pub fn cleanup_old_hang_traces() {
    if let Ok(entries) = std::fs::read_dir(paths::hang_traces_dir()) {
        let mut files: Vec<_> = entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "json" || ext == "miniprof")
            })
            .collect();

        const MAX_HANG_TRACES: usize = 3;
        if files.len() > MAX_HANG_TRACES {
            files.sort_by_key(|entry| entry.file_name());
            for entry in files.iter().take(files.len() - MAX_HANG_TRACES) {
                std::fs::remove_file(entry.path()).log_err();
            }
        }
    }
}
