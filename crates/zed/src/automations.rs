use std::time::Duration as StdDuration;

use agent::automations::{Automation, AutomationsStore, Schedule};
use chrono::Utc;
use gpui::App;

/// Arms the persisted automation schedule for the lifetime of the app. Each
/// enabled automation fires on its interval, dispatching the Phase 7 headless
/// runner (`eval-cli`) and recording the run.
pub fn init(cx: &mut App) {
    let entries = match AutomationsStore::load(AutomationsStore::default_path()) {
        Ok(store) => store
            .automations()
            .entries()
            .iter()
            .filter(|entry| entry.enabled)
            .cloned()
            .collect::<Vec<_>>(),
        Err(error) => {
            log::error!("[AUTOMATION] failed to load schedule: {error:#}");
            return;
        }
    };

    for entry in entries {
        cx.spawn(async move |cx| run_on_schedule(entry, cx).await)
            .detach();
    }
}

async fn run_on_schedule(mut entry: Automation, cx: &mut gpui::AsyncApp) {
    loop {
        let Some(delay) = next_delay(&entry) else {
            log::warn!(
                "[AUTOMATION] {} has an invalid schedule {:?}; stopping",
                entry.id,
                entry.schedule
            );
            return;
        };

        cx.background_executor().timer(delay).await;

        let started = Utc::now();
        log::info!("[AUTOMATION] {} firing: {}", entry.id, entry.prompt);
        dispatch(&entry).await;

        entry.last_run = Some(started);
        record_run(&entry.id, started);
    }
}

fn next_delay(entry: &Automation) -> Option<StdDuration> {
    let schedule = Schedule::parse(&entry.schedule).ok()?;
    let now = Utc::now();
    let due = schedule.next_fire(now, entry.last_run);
    (due - now).to_std().ok()
}

async fn dispatch(entry: &Automation) {
    let result = smol::process::Command::new(eval_cli_command())
        .arg("--prompt")
        .arg(&entry.prompt)
        .arg("--model")
        .arg(&entry.model)
        .arg("--print-response")
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => {
            log::info!(
                "[AUTOMATION] {} completed: {}",
                entry.id,
                String::from_utf8_lossy(&output.stdout).trim()
            );
        }
        Ok(output) => {
            log::error!(
                "[AUTOMATION] {} exited {}: {}",
                entry.id,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Err(error) => log::error!("[AUTOMATION] {} failed to dispatch: {error}", entry.id),
    }
}

/// Resolves the `eval-cli` binary: the sibling next to this executable first,
/// then the bare command (PATH).
fn eval_cli_command() -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let name = if cfg!(windows) { "eval-cli.exe" } else { "eval-cli" };
            let candidate = dir.join(name);
            if candidate.exists() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    "eval-cli".to_string()
}

fn record_run(id: &str, at: chrono::DateTime<Utc>) {
    let result = AutomationsStore::load(AutomationsStore::default_path()).and_then(|mut store| {
        store.automations_mut().record_run(id, at);
        store.save()
    });
    if let Err(error) = result {
        log::error!("[AUTOMATION] failed to record run for {id}: {error:#}");
    }
}
