use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use client::Client;
use gpui::{AppContext, TasksIncluded, profiler};
use hang_telemetry::HangTelemetry;
use ui::App;

use crate::STARTUP_TIME;

mod logging;
mod task_traces;

/// Minimum spacing between GPUI-incident-triggered trace writes. A burst of
/// incidents would otherwise write a trace per monitor tick.
const INCIDENT_TRACE_INTERVAL: Duration = Duration::from_secs(10);

gpui::actions!(
    dev,
    [
        /// Causes a performance hang to test performance monitoring
        HangAction,
        /// Causes a performance hang to test performance monitoring
        HangBackground,
        /// Causes a performance hang to test performance monitoring
        HangForeground,
    ]
);

pub(crate) fn start(client: Arc<Client>, cx: &mut App) {
    let hang_time = hang_telemetry::hang_threshold();

    if cfg!(debug_assertions) {
        log::warn!("debug build, only reporting hangs longer then {hang_time:?}");
    }

    start_hang_detection(hang_time, client, cx);

    cx.on_action(move |_: &HangAction, _| {
        log::warn!(
            "Hanging the foreground for {hang_time:?} by blocking in an action. \
            Zed will be unresponsive for that time. This should trigger a report in the log",
        );
        thread::sleep(hang_time + Duration::from_micros(1));
        log::warn!("Hang ended");
    });
    cx.on_action(move |_: &HangBackground, cx| {
        cx.background_spawn(async move {
            log::warn!(
                "Hanging one background executor for {hang_time:?}. \
                This should trigger a report in the log",
            );
            thread::sleep(hang_time + Duration::from_micros(1));
            log::warn!("Hang ended");
        })
        .detach();
    });
    cx.on_action(move |_: &HangForeground, cx| {
        cx.spawn(async move |_| {
            log::warn!(
                "Hanging the foreground executor for {hang_time:?} seconds to test \
                performance monitoring! Zed will be unresponsive for that time. \
                This should trigger a report in the log"
            );
            thread::sleep(hang_time + Duration::from_micros(1));
            log::warn!("Hang ended");
        })
        .detach();
    });
}

fn start_hang_detection(report_longer_then: Duration, client: Arc<Client>, cx: &mut App) {
    let foreground_thread = thread::current().id();
    let monitor_interval = Duration::from_secs(1);
    let started = Instant::now();
    let startup = *STARTUP_TIME.get().unwrap_or(&started);
    // GPUI's final `Flush` poll runs during shutdown, concurrently with this
    // handler and within `SHUTDOWN_TIMEOUT`, so the last batch may miss this
    // flush.
    // GPUI's hang monitor observes foreground stalls through its own journal,
    // independently of the reporter loop below. Capture a task trace from that
    // path too, so a hang that starves the loop still lands on disk before the
    // process can be killed. Rate-limited, and `save_any` caps the trace files.
    let mut last_incident_trace: Option<Instant> = None;
    let telemetry = HangTelemetry::new(startup, telemetry::send_event).with_incident_observer(
        move |incidents| {
            if incidents.is_empty() {
                return;
            }
            let now = Instant::now();
            if last_incident_trace
                .is_some_and(|last| now.duration_since(last) < INCIDENT_TRACE_INTERVAL)
            {
                return;
            }
            last_incident_trace = Some(now);
            if let Some(path) = task_traces::save_any(foreground_thread) {
                log::info!("Hang incident trace saved to: {}", path.display());
            }
        },
    );
    match telemetry.start(cx) {
        Ok(()) => cx
            .on_app_quit(move |_| client.telemetry().flush_events())
            .detach(),
        Err(error) => log::error!("failed to start hang reporting: {error}"),
    }

    let mut log = logging::Reporter::new(monitor_interval, report_longer_then, foreground_thread);
    // An OS thread keeps the legacy hang logs and task traces working while
    // the foreground or background executors are hung.
    thread::Builder::new()
        .name("HangLogging".to_string())
        .spawn(move || {
            // allow "bad" tasks during startup. Not because we should but since here
            // they are not observed by the user and to lower on clutter from the reporter
            thread::sleep(Duration::from_millis(200));
            loop {
                thread::sleep(monitor_interval);
                let task_stats = profiler::take_all_stats(TasksIncluded::CompletedAndRunning);
                let action_stats = profiler::take_action_stats();

                let should_write_trace = log.check_and_report(&task_stats, &action_stats);
                if should_write_trace {
                    if let Some(path) = task_traces::save_any(foreground_thread) {
                        log::info!("Task trace has been saved to: {}", path.display());
                    }
                }
            }
        })
        .expect("App can always spawn threads");
}
