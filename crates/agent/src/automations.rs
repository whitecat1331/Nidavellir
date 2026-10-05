use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};

/// A single scheduled automation: a prompt to run on a recurring schedule.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Automation {
    /// Stable identifier for this automation.
    pub id: String,
    /// The prompt handed to the headless runner on each fire.
    pub prompt: String,
    /// The schedule spec (`@hourly`, `@daily`, `@weekly`, or `<n>s/m/h/d`).
    pub schedule: String,
    /// The model in `provider/model` form.
    pub model: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// When this automation last fired, if ever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<DateTime<Utc>>,
}

fn default_enabled() -> bool {
    true
}

/// A parsed schedule: a fixed interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    Every(Duration),
}

impl Schedule {
    pub fn parse(spec: &str) -> Result<Schedule> {
        let interval = match spec.trim() {
            "@hourly" => Duration::hours(1),
            "@daily" => Duration::days(1),
            "@weekly" => Duration::weeks(1),
            other => parse_interval(other)?,
        };
        Ok(Schedule::Every(interval))
    }

    pub fn interval(self) -> Duration {
        match self {
            Schedule::Every(duration) => duration,
        }
    }

    /// The next time this schedule should fire, given `now` and the last run.
    /// Falls back to `now` when the schedule has never run or is overdue.
    pub fn next_fire(self, now: DateTime<Utc>, last_run: Option<DateTime<Utc>>) -> DateTime<Utc> {
        let due = last_run.unwrap_or(now) + self.interval();
        if due <= now {
            now
        } else {
            due
        }
    }
}

fn parse_interval(spec: &str) -> Result<Duration> {
    if spec.is_empty() {
        anyhow::bail!("empty schedule spec");
    }
    let (number, unit) = spec.split_at(spec.len() - 1);
    let value: i64 = number
        .parse()
        .with_context(|| format!("invalid interval value {number:?}"))?;
    if value <= 0 {
        anyhow::bail!("interval must be positive");
    }
    match unit {
        "s" => Ok(Duration::seconds(value)),
        "m" => Ok(Duration::minutes(value)),
        "h" => Ok(Duration::hours(value)),
        "d" => Ok(Duration::days(value)),
        _ => anyhow::bail!("invalid interval unit {unit:?} (expected s, m, h, or d)"),
    }
}

/// The set of automations, persisted as JSON.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Automations {
    #[serde(default)]
    entries: Vec<Automation>,
}

impl Automations {
    pub fn entries(&self) -> &[Automation] {
        &self.entries
    }

    pub fn get(&self, id: &str) -> Option<&Automation> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    pub fn upsert(&mut self, automation: Automation) {
        match self.entries.iter_mut().find(|entry| entry.id == automation.id) {
            Some(existing) => *existing = automation,
            None => self.entries.push(automation),
        }
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != id);
        self.entries.len() != before
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> bool {
        match self.entries.iter_mut().find(|entry| entry.id == id) {
            Some(entry) => {
                entry.enabled = enabled;
                true
            }
            None => false,
        }
    }

    pub fn record_run(&mut self, id: &str, at: DateTime<Utc>) -> bool {
        match self.entries.iter_mut().find(|entry| entry.id == id) {
            Some(entry) => {
                entry.last_run = Some(at);
                true
            }
            None => false,
        }
    }
}

/// Loads and persists [`Automations`] to a JSON file, mirroring agent memory.
#[derive(Debug)]
pub struct AutomationsStore {
    path: PathBuf,
    automations: Automations,
}

impl AutomationsStore {
    pub fn default_path() -> PathBuf {
        paths::data_dir().join("automations.json")
    }

    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            automations: Automations::default(),
        }
    }

    pub fn load(path: PathBuf) -> Result<Self> {
        let automations = match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("parsing automations file {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Automations::default(),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading automations file {}", path.display()));
            }
        };
        Ok(Self { path, automations })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn automations(&self) -> &Automations {
        &self.automations
    }

    pub fn automations_mut(&mut self) -> &mut Automations {
        &mut self.automations
    }

    pub fn save(&self) -> Result<()> {
        let contents =
            serde_json::to_vec_pretty(&self.automations).context("serializing automations")?;
        crate::memory::atomic_write(&self.path, &contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    fn sample() -> Automation {
        Automation {
            id: "prtg-sync".into(),
            prompt: "run the PRTG sync".into(),
            schedule: "@hourly".into(),
            model: "deepseek/deepseek-v4-pro".into(),
            enabled: true,
            last_run: None,
        }
    }

    #[test]
    fn schedule_parses_known_specs() {
        assert_eq!(Schedule::parse("@hourly").unwrap().interval(), Duration::hours(1));
        assert_eq!(Schedule::parse("@daily").unwrap().interval(), Duration::days(1));
        assert_eq!(Schedule::parse("@weekly").unwrap().interval(), Duration::weeks(1));
        assert_eq!(Schedule::parse("30s").unwrap().interval(), Duration::seconds(30));
        assert_eq!(Schedule::parse("5m").unwrap().interval(), Duration::minutes(5));
        assert_eq!(Schedule::parse("2h").unwrap().interval(), Duration::hours(2));
        assert_eq!(Schedule::parse("1d").unwrap().interval(), Duration::days(1));
        assert!(Schedule::parse("nonsense").is_err());
        assert!(Schedule::parse("0s").is_err());
        assert!(Schedule::parse("").is_err());
    }

    #[test]
    fn next_fire_advances_from_last_run_and_fires_overdue_now() {
        let schedule = Schedule::parse("@hourly").unwrap();
        let now = ts(1_700_000_000);

        // Never run: fires an hour from now.
        assert_eq!(schedule.next_fire(now, None), now + Duration::hours(1));
        // Ran 30 minutes ago: fires 30 minutes from now.
        assert_eq!(
            schedule.next_fire(now, Some(now - Duration::minutes(30))),
            now + Duration::minutes(30)
        );
        // Ran two hours ago: overdue, fires now.
        assert_eq!(schedule.next_fire(now, Some(now - Duration::hours(2))), now);
    }

    #[test]
    fn upsert_remove_and_record_run() {
        let mut automations = Automations::default();
        let automation = sample();
        automations.upsert(automation.clone());
        assert_eq!(automations.entries().len(), 1);

        // Upsert replaces by id.
        let mut changed = automation;
        changed.prompt = "run the DST audit".into();
        automations.upsert(changed);
        assert_eq!(automations.get("prtg-sync").unwrap().prompt, "run the DST audit");
        assert_eq!(automations.entries().len(), 1);

        let at = ts(1_700_000_000);
        assert!(automations.record_run("prtg-sync", at));
        assert_eq!(automations.get("prtg-sync").unwrap().last_run, Some(at));

        assert!(automations.set_enabled("prtg-sync", false));
        assert!(!automations.get("prtg-sync").unwrap().enabled);

        assert!(automations.remove("prtg-sync"));
        assert!(automations.entries().is_empty());
    }

    #[test]
    fn store_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automations.json");

        {
            let mut store = AutomationsStore::new(path.clone());
            store.automations_mut().upsert(sample());
            store.save().unwrap();
        }

        let store = AutomationsStore::load(path).unwrap();
        assert_eq!(store.automations().entries(), &[sample()]);
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = AutomationsStore::load(dir.path().join("missing.json")).unwrap();
        assert!(store.automations().entries().is_empty());
    }
}
