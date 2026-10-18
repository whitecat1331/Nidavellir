use std::path::PathBuf;
use std::thread::ThreadId;

use anyhow::Context;
use gpui::{SerializedThreadTaskTimings, TasksIncluded, profiler};
use util::ResultExt;

use crate::STARTUP_TIME;

/// Longest task polls kept per thread in a trace. Bounds the trace to about a
/// megabyte even after a long session accumulates millions of timings; the
/// full profile lives in the in-memory profiler, and a trace only needs the
/// stalls that explain the hang.
const MAX_TASKS_PER_THREAD: usize = 512;

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

            let serialized =
                SerializedThreadTaskTimings::convert(*STARTUP_TIME.get().unwrap(), timings);
            trim_serialized_timings(serialized)
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

/// Truncates a serialized thread's timings to the longest [`MAX_TASKS_PER_THREAD`]
/// polls. Keeping only the longest polls is what keeps an incident trace small
/// enough that writing it cannot itself stall a busy session. The trim runs on
/// the serialized form because converting each timing is cheap (the location is
/// a static borrow) while the serialized structs are public, which keeps the
/// cap unit-testable without the profiler's private spawn types.
fn trim_serialized_timings(
    mut timings: SerializedThreadTaskTimings,
) -> SerializedThreadTaskTimings {
    timings
        .timings
        .sort_unstable_by_key(|timing| std::cmp::Reverse(timing.duration));
    timings.timings.truncate(MAX_TASKS_PER_THREAD);
    timings
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::SharedString;
    use gpui::profiler::{SerializedLocation, SerializedTaskTiming};

    fn timing(file: &'static str, line: u32, duration: u128) -> SerializedTaskTiming {
        SerializedTaskTiming {
            location: SerializedLocation {
                file: SharedString::from(file),
                line,
                column: 0,
            },
            start: 0,
            duration,
        }
    }

    #[test]
    fn trim_keeps_longest_polls_per_thread() {
        let extra = 100u128;
        let timings = (0..(MAX_TASKS_PER_THREAD as u128 + extra))
            .map(|index| timing("crates/zed/src/lib.rs", index as u32, index + 1))
            .collect::<Vec<_>>();

        let trimmed = trim_serialized_timings(SerializedThreadTaskTimings {
            thread_name: Some("main".to_string()),
            thread_id: 1,
            timings,
        });

        assert_eq!(trimmed.timings.len(), MAX_TASKS_PER_THREAD);
        assert_eq!(trimmed.thread_name.as_deref(), Some("main"));
        assert!(
            trimmed
                .timings
                .windows(2)
                .all(|pair| pair[0].duration >= pair[1].duration)
        );
        // The longest poll is kept and sorted first.
        assert_eq!(
            trimmed.timings[0].duration,
            MAX_TASKS_PER_THREAD as u128 + extra
        );
        // The shortest kept poll is the (extra + 1) ms one: the `extra`
        // shortest source polls were dropped.
        assert_eq!(trimmed.timings.last().unwrap().duration, extra + 1);
    }
}
