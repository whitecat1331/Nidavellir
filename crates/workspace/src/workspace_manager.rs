use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use db::{
    sqlez::{
        bindable::{Bind, Column, StaticColumnCount},
        statement::Statement,
    },
    sqlez_macros::sql,
};
use gpui::App;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{WorkspaceDb, path_list::PathList};

/// A stable, minted identifier for a managed workspace.
///
/// Unlike the legacy [`crate::WorkspaceId`] (an auto-increment `i64` keyed by a
/// path set), this id is minted once on `create` and never changes, even as
/// projects are added to or removed from the workspace. It is stored as its
/// hyphenated string form, so it round-trips as a readable `TEXT` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ManagedWorkspaceId(Uuid);

impl ManagedWorkspaceId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Stable, hyphenated string form suitable for use as a key.
    pub fn to_key_string(&self) -> String {
        self.0.hyphenated().to_string()
    }

    pub fn from_key_string(key: &str) -> Result<Self> {
        Ok(Self(Uuid::parse_str(key)?))
    }

    /// The raw [`Uuid`], used as the layout `workspaces.workspace_uuid` key.
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl StaticColumnCount for ManagedWorkspaceId {}

impl Bind for ManagedWorkspaceId {
    fn bind(&self, statement: &Statement, start_index: i32) -> Result<i32> {
        self.to_key_string().bind(statement, start_index)
    }
}

impl Column for ManagedWorkspaceId {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (key, next_index) = String::column(statement, start_index)?;
        Ok((Self::from_key_string(&key)?, next_index))
    }
}

/// A managed workspace's identity row (name + timestamps), keyed by
/// [`ManagedWorkspaceId`].
#[derive(Debug, Clone)]
pub struct ManagedWorkspace {
    pub workspace_id: ManagedWorkspaceId,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One project (root folder) belonging to a managed workspace.
#[derive(Debug, Clone)]
pub struct ManagedWorkspaceProject {
    pub workspace_id: ManagedWorkspaceId,
    pub path: PathBuf,
    pub position: i64,
    pub remote_connection_id: Option<i64>,
}

/// A managed workspace that contains a folder being opened — one option in the
/// open-folder ask.
#[derive(Debug, Clone)]
pub struct AskCandidate {
    pub workspace_id: ManagedWorkspaceId,
    pub name: String,
    pub project_count: usize,
}

/// The single source of truth for workspace identity: a thin wrapper over
/// [`WorkspaceDb`] that owns the `managed_workspaces` /
/// `managed_workspace_projects` tables (distinct from the legacy path-keyed
/// `workspaces` layout table).
#[derive(Clone)]
pub struct WorkspaceManager(WorkspaceDb);

impl std::ops::Deref for WorkspaceManager {
    type Target = WorkspaceDb;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl WorkspaceManager {
    pub fn new(db: WorkspaceDb) -> Self {
        Self(db)
    }

    pub fn global(cx: &App) -> Self {
        Self(WorkspaceDb::global(cx))
    }

    /// Comma-joined last path components — the default display name.
    pub fn derive_name(project_paths: &[PathBuf]) -> String {
        project_paths
            .iter()
            .filter_map(|path| path.file_name())
            .filter_map(|name| name.to_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Mint a new managed workspace id and record its membership.
    pub async fn create(
        &self,
        name: String,
        project_paths: Vec<PathBuf>,
    ) -> Result<ManagedWorkspaceId> {
        let workspace_id = ManagedWorkspaceId::new();
        let now = Utc::now().to_rfc3339();
        let key = workspace_id.to_key_string();

        self.write(move |connection| -> Result<()> {
            connection.exec_bound::<(&str, &str, &str, &str)>(sql! {
                INSERT INTO managed_workspaces (workspace_id, name, created_at, updated_at)
                VALUES (?, ?, ?, ?)
            })?((key.as_str(), name.as_str(), now.as_str(), now.as_str()))?;

            for (position, path) in project_paths.into_iter().enumerate() {
                let path = path.to_string_lossy().into_owned();
                connection.exec_bound::<(&str, &str, i64)>(sql! {
                    INSERT INTO managed_workspace_projects (workspace_id, path, position)
                    VALUES (?, ?, ?)
                })?((key.as_str(), path.as_str(), position as i64))?;
            }
            Ok(())
        })
        .await?;

        Ok(workspace_id)
    }

    pub fn get(&self, workspace_id: ManagedWorkspaceId) -> Result<Option<ManagedWorkspace>> {
        let key = workspace_id.to_key_string();
        let row = self.select_row_bound::<&str, (String, String, String)>(sql! {
            SELECT name, created_at, updated_at
            FROM managed_workspaces
            WHERE workspace_id = ?
        })?(key.as_str())?;

        Ok(row.map(|(name, created_at, updated_at)| ManagedWorkspace {
            workspace_id,
            name,
            created_at: parse_timestamp(&created_at),
            updated_at: parse_timestamp(&updated_at),
        }))
    }

    pub fn projects(
        &self,
        workspace_id: ManagedWorkspaceId,
    ) -> Result<Vec<ManagedWorkspaceProject>> {
        let key = workspace_id.to_key_string();
        let rows = self.select_bound::<&str, (String, i64, Option<i64>)>(sql! {
            SELECT path, position, remote_connection_id
            FROM managed_workspace_projects
            WHERE workspace_id = ?
            ORDER BY position
        })?(key.as_str())?;

        Ok(rows
            .into_iter()
            .map(
                |(path, position, remote_connection_id)| ManagedWorkspaceProject {
                    workspace_id,
                    path: PathBuf::from(path),
                    position,
                    remote_connection_id,
                },
            )
            .collect())
    }

    /// Resolve the managed workspace whose project membership is exactly the
    /// given folder set, if any. This is the look-up used to tag a thread with
    /// its workspace id and to keep backfill idempotent.
    pub fn workspace_id_for_paths(&self, paths: &PathList) -> Result<Option<ManagedWorkspaceId>> {
        if paths.is_empty() {
            return Ok(None);
        }

        let mut target = paths
            .paths()
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        target.sort();

        let rows = self
            .select_bound::<(), (String, String)>(sql! {
                SELECT workspace_id, path FROM managed_workspace_projects ORDER BY workspace_id, position
            })?
            (())?;

        let mut by_workspace: collections::HashMap<String, Vec<String>> =
            collections::HashMap::default();
        for (workspace_id, path) in rows {
            by_workspace.entry(workspace_id).or_default().push(path);
        }

        for (workspace_id, mut project_paths) in by_workspace {
            project_paths.sort();
            if project_paths == target {
                return Ok(Some(ManagedWorkspaceId::from_key_string(&workspace_id)?));
            }
        }

        Ok(None)
    }

    /// Resolve the managed workspace that best represents the given folder
    /// set. Prefers an exact membership match (see
    /// [`Self::workspace_id_for_paths`]), then the workspace whose membership
    /// is a *superset* of the set (closest by extra-folder count), then the
    /// workspace with the strongest path overlap.
    ///
    /// The overlap step keeps a thread attached after its workspace changes
    /// shape: the thread's saved paths then name worktrees the workspace has
    /// since removed, so it is neither an exact match nor a subset.
    pub fn workspace_id_for_paths_relaxed(
        &self,
        paths: &PathList,
    ) -> Result<Option<ManagedWorkspaceId>> {
        if let Some(id) = self.workspace_id_for_paths(paths)? {
            return Ok(Some(id));
        }
        if paths.is_empty() {
            return Ok(None);
        }

        let target = paths
            .paths()
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<collections::HashSet<_>>();

        let rows = self
            .select_bound::<(), (String, String)>(sql! {
                SELECT workspace_id, path FROM managed_workspace_projects ORDER BY workspace_id, position
            })?
            (())?;

        let mut by_workspace: collections::HashMap<String, collections::HashSet<String>> =
            collections::HashMap::default();
        for (workspace_id, path) in rows {
            by_workspace.entry(workspace_id).or_default().insert(path);
        }

        let mut closest: Option<(usize, ManagedWorkspaceId)> = None;
        for (workspace_id, members) in &by_workspace {
            if members.len() < target.len() || !target.is_subset(members) {
                continue;
            }
            let extra = members.len() - target.len();
            let id = ManagedWorkspaceId::from_key_string(workspace_id)?;
            if closest.map_or(true, |(closest_extra, _)| extra < closest_extra) {
                closest = Some((extra, id));
            }
        }
        if let Some((_, id)) = closest {
            return Ok(Some(id));
        }

        best_overlap_workspace(&target, &by_workspace)
            .map(|workspace_id| ManagedWorkspaceId::from_key_string(&workspace_id))
            .transpose()
    }

    /// Create a managed workspace for the given paths unless one with identical
    /// membership already exists, in which case reuse (and return) its existing
    /// id. Keeps `NewManagedWorkspace` and the `--workspace` create-if-absent
    /// path from minting a duplicate of an already-known folder set.
    pub async fn create_or_reuse(
        &self,
        name: String,
        project_paths: Vec<PathBuf>,
    ) -> Result<ManagedWorkspaceId> {
        if let Some(existing) =
            self.workspace_id_for_paths(&PathList::new(project_paths.as_slice()))?
        {
            return Ok(existing);
        }
        self.create(name, project_paths).await
    }

    /// Managed workspaces whose membership includes the given project path —
    /// the disambiguation set for the open-folder ask.
    pub fn workspaces_for_path(&self, path: &Path) -> Result<Vec<ManagedWorkspaceId>> {
        let key = path.to_string_lossy().into_owned();
        let rows = self.select_bound::<&str, String>(sql! {
            SELECT workspace_id FROM managed_workspace_projects WHERE path = ?
        })?(key.as_str())?;
        rows.into_iter()
            .map(|id| ManagedWorkspaceId::from_key_string(&id))
            .collect()
    }

    /// The disambiguation options for the open-folder ask: the managed
    /// workspaces (with more than one project) that contain `path`. A folder
    /// that is the *only* member of a workspace is suppressed, since "open that
    /// workspace" and "open alone" are equivalent.
    pub fn ask_candidates(&self, path: &Path) -> Result<Vec<AskCandidate>> {
        let mut candidates = Vec::new();
        for workspace_id in self.workspaces_for_path(path)? {
            let project_count = self.projects(workspace_id)?.len();
            if project_count <= 1 {
                continue;
            }
            let Some(workspace) = self.get(workspace_id)? else {
                continue;
            };
            candidates.push(AskCandidate {
                workspace_id,
                name: workspace.name,
                project_count,
            });
        }
        Ok(candidates)
    }

    /// Resolve a workspace by its hyphenated id or its user-giveable name.
    pub fn resolve(&self, id_or_name: &str) -> Result<Option<ManagedWorkspaceId>> {
        if let Ok(id) = ManagedWorkspaceId::from_key_string(id_or_name)
            && self.get(id)?.is_some()
        {
            return Ok(Some(id));
        }

        let rows = self.select_bound::<&str, String>(sql! {
            SELECT workspace_id FROM managed_workspaces WHERE name = ?
        })?(id_or_name)?;

        rows.into_iter()
            .next()
            .map(|id| ManagedWorkspaceId::from_key_string(&id))
            .transpose()
    }

    /// The project paths (membership) of a workspace, in position order.
    pub fn project_paths(&self, workspace_id: ManagedWorkspaceId) -> Result<Vec<PathBuf>> {
        Ok(self
            .projects(workspace_id)?
            .into_iter()
            .map(|project| project.path)
            .collect())
    }

    /// The project paths to open for a workspace, in position order — the
    /// UI-facing "open by id" entry point. Callers feed the result to
    /// [`crate::open_paths`] to actually open the workspace.
    pub fn open(&self, workspace_id: ManagedWorkspaceId) -> Result<Vec<PathBuf>> {
        self.project_paths(workspace_id)
    }

    /// The full managed-workspace list, ordered by name — backing for the home
    /// page and the workspace switcher.
    pub fn all(&self) -> Result<Vec<ManagedWorkspace>> {
        let rows = self.select_bound::<(), (String, String, String, String)>(sql! {
            SELECT workspace_id, name, created_at, updated_at
            FROM managed_workspaces
            ORDER BY name
        })?(())?;

        rows.into_iter()
            .map(|(workspace_id, name, created_at, updated_at)| {
                Ok(ManagedWorkspace {
                    workspace_id: ManagedWorkspaceId::from_key_string(&workspace_id)?,
                    name,
                    created_at: parse_timestamp(&created_at),
                    updated_at: parse_timestamp(&updated_at),
                })
            })
            .collect()
    }

    /// Rename a managed workspace, bumping its `updated_at`.
    pub async fn rename(&self, workspace_id: ManagedWorkspaceId, name: String) -> Result<()> {
        let key = workspace_id.to_key_string();
        let now = Utc::now().to_rfc3339();
        self.write(move |connection| -> Result<()> {
            connection.exec_bound::<(&str, &str, &str)>(sql! {
                UPDATE managed_workspaces
                SET name = ?, updated_at = ?
                WHERE workspace_id = ?
            })?((name.as_str(), now.as_str(), key.as_str()))?;
            Ok(())
        })
        .await
    }

    /// Add a project to a managed workspace, appending it at the end. The id is
    /// unchanged.
    pub async fn add_project(&self, workspace_id: ManagedWorkspaceId, path: PathBuf) -> Result<()> {
        let key = workspace_id.to_key_string();
        let path = path.to_string_lossy().into_owned();
        let position = self.projects(workspace_id)?.len() as i64;
        let now = Utc::now().to_rfc3339();
        self.write(move |connection| -> Result<()> {
            connection.exec_bound::<(&str, &str, i64)>(sql! {
                INSERT INTO managed_workspace_projects (workspace_id, path, position)
                VALUES (?, ?, ?)
                ON CONFLICT(workspace_id, path) DO NOTHING
            })?((key.as_str(), path.as_str(), position))?;
            connection.exec_bound::<(&str, &str)>(sql! {
                UPDATE managed_workspaces SET updated_at = ? WHERE workspace_id = ?
            })?((now.as_str(), key.as_str()))?;
            Ok(())
        })
        .await
    }

    /// Remove a project from a managed workspace. The id is unchanged.
    pub async fn remove_project(
        &self,
        workspace_id: ManagedWorkspaceId,
        path: PathBuf,
    ) -> Result<()> {
        let key = workspace_id.to_key_string();
        let path = path.to_string_lossy().into_owned();
        let now = Utc::now().to_rfc3339();
        self.write(move |connection| -> Result<()> {
            connection.exec_bound::<(&str, &str)>(sql! {
                DELETE FROM managed_workspace_projects
                WHERE workspace_id = ? AND path = ?
            })?((key.as_str(), path.as_str()))?;
            connection.exec_bound::<(&str, &str)>(sql! {
                UPDATE managed_workspaces SET updated_at = ? WHERE workspace_id = ?
            })?((now.as_str(), key.as_str()))?;
            Ok(())
        })
        .await
    }
}

fn parse_timestamp(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

/// The workspace that best represents `paths` by overlap, used when no
/// workspace is an exact or superset match.
///
/// A thread's saved paths drift from its workspace over time: worktrees are
/// added and removed, and the thread still names the old ones. Requiring a
/// subset would leave such a thread unattached, which hides it from the
/// sidebar. Accept a clear majority overlap (Jaccard similarity at least 1/2)
/// or a near superset (at least 80% of the smaller set), and require two shared
/// paths so one coincidence cannot move a thread to another project.
fn best_overlap_workspace(
    paths: &collections::HashSet<String>,
    by_workspace: &collections::HashMap<String, collections::HashSet<String>>,
) -> Option<String> {
    let mut candidates: Vec<(f64, usize, usize, &String)> = by_workspace
        .iter()
        .filter_map(|(workspace_id, members)| {
            let overlap = paths.intersection(members).count();
            if overlap < 2 {
                return None;
            }
            let union = paths.union(members).count();
            let jaccard = overlap as f64 / union as f64;
            let smaller = paths.len().min(members.len());
            if jaccard < 0.5 && overlap * 5 < smaller * 4 {
                return None;
            }
            Some((
                jaccard,
                overlap,
                members.len().saturating_sub(overlap),
                workspace_id,
            ))
        })
        .collect();

    candidates.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| a.3.cmp(b.3))
    });

    candidates
        .first()
        .map(|(_, _, _, workspace_id)| (*workspace_id).clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(paths: &[&str]) -> collections::HashSet<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    #[test]
    fn test_best_overlap_prefers_the_workspace_sharing_most_paths() {
        let mut by_workspace = collections::HashMap::default();
        by_workspace.insert("main".to_string(), set(&["a", "b", "c", "d"]));
        by_workspace.insert("test".to_string(), set(&["a", "b"]));

        assert_eq!(
            best_overlap_workspace(&set(&["a", "b", "c", "d", "stale"]), &by_workspace),
            Some("main".to_string())
        );
    }

    #[test]
    fn test_best_overlap_accepts_a_near_superset_with_stale_paths() {
        let mut by_workspace = collections::HashMap::default();
        by_workspace.insert("main".to_string(), set(&["a", "b", "c", "d", "e"]));
        // Four of the workspace's five folders survive, plus six stale paths;
        // Jaccard is below 1/2 but the overlap is a near superset.
        assert_eq!(
            best_overlap_workspace(
                &set(&["b", "c", "d", "e", "s1", "s2", "s3", "s4", "s5", "s6"]),
                &by_workspace,
            ),
            Some("main".to_string())
        );
    }

    #[test]
    fn test_best_overlap_ignores_a_single_shared_path() {
        let mut by_workspace = collections::HashMap::default();
        by_workspace.insert("main".to_string(), set(&["a", "b", "c"]));

        assert_eq!(
            best_overlap_workspace(&set(&["a", "unrelated"]), &by_workspace),
            None
        );
    }

    #[test]
    fn test_best_overlap_is_deterministic_on_a_tie() {
        let mut by_workspace = collections::HashMap::default();
        by_workspace.insert("b-workspace".to_string(), set(&["a", "b"]));
        by_workspace.insert("a-workspace".to_string(), set(&["a", "b"]));

        assert_eq!(
            best_overlap_workspace(&set(&["a", "b"]), &by_workspace),
            Some("a-workspace".to_string())
        );
    }
}
