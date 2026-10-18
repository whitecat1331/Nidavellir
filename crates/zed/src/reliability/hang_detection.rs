use std::collections::VecDeque;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use client::Client;
use gpui::profiler::hang::{HangTrigger, SerializedHangIncident};
use gpui::{AppContext, TasksIncluded, profiler};
use hang_telemetry::HangTelemetry;
use ui::App;

use crate::STARTUP_TIME;

mod logging;
mod task_traces;

/// Minimum spacing between GPUI-incident-triggered trace writes. A burst of
/// incidents would otherwise write a trace per monitor tick.
const INCIDENT_TRACE_INTERVAL: Duration = Duration::from_secs(10);

/// Sliding window over which [`MAX_INCIDENT_TRACES_PER_HOUR`] applies.
const INCIDENT_TRACE_BUDGET_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Maximum incident traces written per hour. Even a genuinely hang-prone
/// session must not turn trace writing itself into a disk/memory storm.
const MAX_INCIDENT_TRACES_PER_HOUR: usize = 20;

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
    // process can be killed. `save_incident` bypasses the profiler gate, so the
    // observer gates saves itself: threshold-triggered incidents only,
    // rate-limited, and budgeted per hour.
    let mut incident_trace_saves: VecDeque<Instant> = VecDeque::new();
    let telemetry = HangTelemetry::new(startup, telemetry::send_event).with_incident_observer(
        move |incidents| {
            if should_save_incident_trace(incidents, Instant::now(), &mut incident_trace_saves)
                && let Some(path) = task_traces::save_incident(foreground_thread)
            {
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

/// Decides whether an incident poll warrants a task trace on disk.
///
/// Only threshold-triggered incidents count: a budget incident means an
/// interval's total foreground spend crossed the frame budget, which happens
/// constantly on any busy machine (see `hang_telemetry::frame_budget`) and is
/// a responsiveness signal, not crash evidence. Even then, saves are
/// rate-limited ([`INCIDENT_TRACE_INTERVAL`]) and capped
/// ([`MAX_INCIDENT_TRACES_PER_HOUR`] per [`INCIDENT_TRACE_BUDGET_WINDOW`]) so
/// a hang-prone session cannot turn trace writing itself into a stall.
fn should_save_incident_trace(
    incidents: &[SerializedHangIncident],
    now: Instant,
    recent_saves: &mut VecDeque<Instant>,
) -> bool {
    if !incidents
        .iter()
        .any(|incident| matches!(incident.trigger, HangTrigger::Threshold))
    {
        return false;
    }
    while recent_saves
        .front()
        .is_some_and(|save| now.saturating_duration_since(*save) > INCIDENT_TRACE_BUDGET_WINDOW)
    {
        recent_saves.pop_front();
    }
    if recent_saves.len() >= MAX_INCIDENT_TRACES_PER_HOUR {
        return false;
    }
    if recent_saves
        .back()
        .is_some_and(|last| now.saturating_duration_since(*last) < INCIDENT_TRACE_INTERVAL)
    {
        return false;
    }
    recent_saves.push_back(now);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::profiler::hang::MEASUREMENT_VERSION;

    fn incident(trigger: HangTrigger, stall_ms: f64) -> SerializedHangIncident {
        SerializedHangIncident {
            measurement_version: MEASUREMENT_VERSION,
            phase: "steady",
            trigger,
            start_ms: 0.0,
            active_ms: stall_ms,
            stall_ms,
            dirty_to_present_ms: None,
            sealed_by: "idle",
            busy_fraction: 1.0,
            event_count: 1,
            small_poll_count: 0,
            small_poll_total_ms: 0.0,
            dropped_events: 0,
            journal_discontinuous: false,
            contributors: Vec::new(),
            contributors_elided: 0,
        }
    }

    #[test]
    fn budget_incidents_are_never_saved() {
        let now = Instant::now();
        let mut recent_saves = VecDeque::new();

        assert!(!should_save_incident_trace(
            &[incident(HangTrigger::Budget, 5.0)],
            now,
            &mut recent_saves,
        ));
        assert!(recent_saves.is_empty());
    }

    #[test]
    fn threshold_incident_saves_and_records_the_time() {
        let now = Instant::now();
        let mut recent_saves = VecDeque::new();

        assert!(should_save_incident_trace(
            &[incident(HangTrigger::Threshold, 150.0)],
            now,
            &mut recent_saves,
        ));
        assert_eq!(recent_saves.len(), 1);
    }

    #[test]
    fn mixed_poll_with_a_threshold_incident_saves() {
        let now = Instant::now();
        let mut recent_saves = VecDeque::new();
        let incidents = vec![
            incident(HangTrigger::Budget, 5.0),
            incident(HangTrigger::Threshold, 150.0),
            incident(HangTrigger::Budget, 8.0),
        ];

        assert!(should_save_incident_trace(
            &incidents,
            now,
            &mut recent_saves
        ));
    }

    #[test]
    fn saves_are_rate_limited() {
        let start = Instant::now();
        let mut recent_saves = VecDeque::new();

        assert!(should_save_incident_trace(
            &[incident(HangTrigger::Threshold, 150.0)],
            start,
            &mut recent_saves,
        ));
        assert!(!should_save_incident_trace(
            &[incident(HangTrigger::Threshold, 150.0)],
            start + Duration::from_secs(5),
            &mut recent_saves,
        ));
        assert!(should_save_incident_trace(
            &[incident(HangTrigger::Threshold, 150.0)],
            start + INCIDENT_TRACE_INTERVAL + Duration::from_secs(1),
            &mut recent_saves,
        ));
    }

    #[test]
    fn hourly_budget_caps_saves_until_oldest_ages_out() {
        let start = Instant::now();
        let mut recent_saves = VecDeque::new();
        for index in 0..MAX_INCIDENT_TRACES_PER_HOUR {
            recent_saves.push_back(start + INCIDENT_TRACE_INTERVAL * (index as u32));
        }
        let cap_hit = start + INCIDENT_TRACE_INTERVAL * (MAX_INCIDENT_TRACES_PER_HOUR as u32);

        assert!(!should_save_incident_trace(
            &[incident(HangTrigger::Threshold, 150.0)],
            cap_hit,
            &mut recent_saves,
        ));

        let after_window = cap_hit + INCIDENT_TRACE_BUDGET_WINDOW + INCIDENT_TRACE_INTERVAL;
        assert!(should_save_incident_trace(
            &[incident(HangTrigger::Threshold, 150.0)],
            after_window,
            &mut recent_saves,
        ));
    }

    #[test]
    fn old_saves_age_out_of_the_window() {
        let now = Instant::now();
        let mut recent_saves = VecDeque::new();
        recent_saves.push_back(now - INCIDENT_TRACE_BUDGET_WINDOW - Duration::from_secs(60));

        assert!(should_save_incident_trace(
            &[incident(HangTrigger::Threshold, 150.0)],
            now,
            &mut recent_saves,
        ));
        assert_eq!(recent_saves.len(), 1);
        assert_eq!(recent_saves[0], now);
    }
}
