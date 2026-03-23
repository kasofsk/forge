use std::collections::{HashSet, VecDeque};

use anyhow::{Context, Result};
use indradb::{
    AllVertexQuery, Database, Edge, Identifier, Json, QueryExt, QueryOutputValue, RocksdbDatastore,
    SpecificEdgeQuery, SpecificVertexQuery, Vertex,
};
use uuid::Uuid;
use workflow_types::{ActivityEntry, Job, JobState};

/// UUID v5 namespace for deriving deterministic vertex IDs from job keys.
const JOB_NS: Uuid = Uuid::NAMESPACE_URL;

pub struct TaskGraph {
    db: Database<RocksdbDatastore>,
    job_type: Identifier,
    dep_type: Identifier,
    job_prop: Identifier,
}

impl TaskGraph {
    pub fn open(path: &str) -> Result<Self> {
        let db = RocksdbDatastore::new_db(path).context("open RocksDB datastore")?;
        Ok(Self {
            db,
            job_type: Identifier::new("job").unwrap(),
            dep_type: Identifier::new("depends_on").unwrap(),
            job_prop: Identifier::new("job").unwrap(),
        })
    }

    /// Derive a deterministic UUID v5 from a job key `owner/repo/number`.
    pub fn job_uuid(key: &str) -> Uuid {
        Uuid::new_v5(&JOB_NS, key.as_bytes())
    }

    /// Upsert a job vertex and store the full job as a JSON property.
    pub fn upsert_job(&self, job: &Job) -> Result<()> {
        let id = Self::job_uuid(&job.key());
        let vertex = Vertex::with_id(id, self.job_type);
        let _ = self.db.create_vertex(&vertex);

        let value = Json::new(serde_json::to_value(job)?);
        self.db
            .set_properties(SpecificVertexQuery::single(id), self.job_prop, &value)
            .context("set job property")?;
        Ok(())
    }

    /// Update only the state field of a stored job.
    pub fn set_state(&self, job_key: &str, state: &JobState) -> Result<()> {
        if let Some(mut job) = self.get_job(job_key)? {
            job.state = state.clone();
            self.upsert_job(&job)?;
        }
        Ok(())
    }

    /// Append an activity entry to a job's timeline.
    pub fn append_activity(&self, job_key: &str, entry: ActivityEntry) -> Result<()> {
        if let Some(mut job) = self.get_job(job_key)? {
            job.activities.push(entry);
            self.upsert_job(&job)?;
        }
        Ok(())
    }

    /// Update an existing activity entry on a job. The `mutator` is called
    /// with a mutable reference to each activity until one is matched.
    pub fn update_activity(
        &self,
        job_key: &str,
        mutator: impl FnOnce(&mut Vec<ActivityEntry>) -> bool,
    ) -> Result<()> {
        if let Some(mut job) = self.get_job(job_key)? {
            if mutator(&mut job.activities) {
                self.upsert_job(&job)?;
            }
        }
        Ok(())
    }

    /// Get a job by key, or None if not found.
    pub fn get_job(&self, job_key: &str) -> Result<Option<Job>> {
        let id = Self::job_uuid(job_key);
        let q = SpecificVertexQuery::single(id)
            .properties()
            .context("build properties query")?
            .name(self.job_prop);
        let results = self.db.get(q).context("get job")?;
        Ok(extract_first_job(results))
    }

    /// Get all stored jobs, optionally filtered by state.
    pub fn get_all_jobs(&self, state_filter: Option<&JobState>) -> Result<Vec<Job>> {
        let q = AllVertexQuery
            .properties()
            .context("build all-vertex properties query")?;
        let results = self.db.get(q).context("get all jobs")?;
        let mut jobs = extract_jobs(results);
        if let Some(state) = state_filter {
            jobs.retain(|j| &j.state == state);
        }
        Ok(jobs)
    }

    /// Sync dependency edges for a job, enforcing DAG invariants.
    ///
    /// Returns a list of dep_keys that were **rejected** because adding them
    /// would introduce a cycle. Callers should surface this as a warning
    /// (e.g. a Forgejo comment) so authors can fix the dep list.
    ///
    /// Edge direction: `job_key --depends_on--> dep_key`.
    pub fn sync_deps(&self, job_key: &str, dep_keys: &[String]) -> Result<Vec<String>> {
        let from_id = Self::job_uuid(job_key);

        // Current outbound `depends_on` edges from this vertex
        let current_edges: Vec<Edge> = {
            let q = SpecificVertexQuery::single(from_id)
                .outbound()
                .context("build outbound query")?
                .t(self.dep_type);
            let results = self.db.get(q).context("get outbound edges")?;
            edges_from_output(results)
        };

        let desired_dep_ids: Vec<Uuid> = dep_keys.iter().map(|k| Self::job_uuid(k)).collect();

        // Remove stale edges
        for edge in &current_edges {
            if !desired_dep_ids.contains(&edge.inbound_id) {
                let q = SpecificEdgeQuery::single(edge.clone());
                let _ = self.db.delete(q);
            }
        }

        let current_dep_ids: Vec<Uuid> = current_edges.iter().map(|e| e.inbound_id).collect();

        let mut rejected = Vec::new();

        // Add new edges; enforce DAG before each insertion
        for (dep_key, dep_id) in dep_keys.iter().zip(&desired_dep_ids) {
            if current_dep_ids.contains(dep_id) {
                continue; // edge already exists, nothing to do
            }

            // Self-dependency or cycle check
            if self.would_create_cycle(from_id, *dep_id)? {
                tracing::warn!(job_key, dep_key, "rejected dep edge: would create a cycle");
                rejected.push(dep_key.clone());
                continue;
            }

            // Ensure the dep vertex exists as a stub so the edge can be created
            let stub = Vertex::with_id(*dep_id, self.job_type);
            let _ = self.db.create_vertex(&stub);

            if self.get_job(dep_key)?.is_none() {
                if let Some(stub_job) = stub_job_from_key(dep_key) {
                    let value = Json::new(serde_json::to_value(&stub_job)?);
                    let _ = self.db.set_properties(
                        SpecificVertexQuery::single(*dep_id),
                        self.job_prop,
                        &value,
                    );
                }
            }

            let edge = Edge::new(from_id, self.dep_type, *dep_id);
            let _ = self.db.create_edge(&edge);
        }

        Ok(rejected)
    }

    /// Return the keys of all jobs that have a `depends_on` edge to `job_key`.
    pub fn get_dependents(&self, job_key: &str) -> Result<Vec<String>> {
        let id = Self::job_uuid(job_key);

        let q = SpecificVertexQuery::single(id)
            .inbound()
            .context("build inbound query")?
            .t(self.dep_type);
        let results = self.db.get(q).context("get inbound edges")?;
        let dependent_ids: Vec<Uuid> = edges_from_output(results)
            .into_iter()
            .map(|e| e.outbound_id)
            .collect();

        if dependent_ids.is_empty() {
            return Ok(vec![]);
        }

        let q = SpecificVertexQuery::new(dependent_ids)
            .properties()
            .context("build dep properties query")?;
        let results = self.db.get(q).context("get dependents")?;
        Ok(extract_jobs(results).into_iter().map(|j| j.key()).collect())
    }

    /// Check declared dep numbers directly by vertex lookup. Returns true only
    /// if every dep vertex exists in the graph AND has state Done. This is safe
    /// against the race where dep edges haven't been synced yet — if a dep
    /// vertex doesn't exist, we return false.
    pub fn all_declared_deps_done(
        &self,
        owner: &str,
        repo: &str,
        dep_numbers: &[u64],
    ) -> Result<bool> {
        for dep_num in dep_numbers {
            let dep_key = format!("{owner}/{repo}/{dep_num}");
            match self.get_job(&dep_key)? {
                Some(j) if j.state == JobState::Done => {}
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    /// Returns true if every direct dependency of `job_key` is Done.
    /// Revoked deps are terminal but do NOT satisfy this check — they block.
    pub fn all_deps_done(&self, job_key: &str) -> Result<bool> {
        let id = Self::job_uuid(job_key);

        let q = SpecificVertexQuery::single(id)
            .outbound()
            .context("build outbound query")?
            .t(self.dep_type);
        let results = self.db.get(q).context("get outbound edges")?;
        let dep_ids: Vec<Uuid> = edges_from_output(results)
            .into_iter()
            .map(|e| e.inbound_id)
            .collect();

        if dep_ids.is_empty() {
            return Ok(true);
        }

        let dep_count = dep_ids.len();
        let q = SpecificVertexQuery::new(dep_ids)
            .properties()
            .context("build dep properties query")?;
        let results = self.db.get(q).context("get dep states")?;
        let jobs = extract_jobs(results);

        Ok(jobs.len() == dep_count && jobs.iter().all(|j| j.state == JobState::Done))
    }

    // ── DAG enforcement ───────────────────────────────────────────────────────

    /// Returns `true` if adding the edge `from_id → to_id` would create a cycle.
    ///
    /// A cycle would exist if there is already a directed path from `to_id`
    /// back to `from_id` through `depends_on` edges, or if `from_id == to_id`.
    ///
    /// Uses iterative BFS to avoid stack overflow on deep graphs.
    fn would_create_cycle(&self, from_id: Uuid, to_id: Uuid) -> Result<bool> {
        if from_id == to_id {
            return Ok(true);
        }

        // BFS from `to_id` following outbound depends_on edges.
        // If we reach `from_id`, the proposed edge closes a cycle.
        let mut visited: HashSet<Uuid> = HashSet::new();
        let mut queue: VecDeque<Uuid> = VecDeque::new();
        queue.push_back(to_id);

        while let Some(current) = queue.pop_front() {
            if !visited.insert(current) {
                continue;
            }

            let q = SpecificVertexQuery::single(current)
                .outbound()
                .context("would_create_cycle: outbound query")?
                .t(self.dep_type);
            let results = self.db.get(q).context("would_create_cycle: get edges")?;

            for edge in edges_from_output(results) {
                if edge.inbound_id == from_id {
                    return Ok(true);
                }
                queue.push_back(edge.inbound_id);
            }
        }

        Ok(false)
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn extract_jobs(output: Vec<QueryOutputValue>) -> Vec<Job> {
    output
        .into_iter()
        .flat_map(|v| match v {
            QueryOutputValue::VertexProperties(props) => props
                .into_iter()
                .flat_map(|vp| {
                    vp.props
                        .into_iter()
                        .filter_map(|np| serde_json::from_value::<Job>((*np.value.0).clone()).ok())
                })
                .collect::<Vec<_>>(),
            _ => vec![],
        })
        .collect()
}

fn extract_first_job(output: Vec<QueryOutputValue>) -> Option<Job> {
    extract_jobs(output).into_iter().next()
}

fn edges_from_output(output: Vec<QueryOutputValue>) -> Vec<Edge> {
    output
        .into_iter()
        .flat_map(|v| match v {
            QueryOutputValue::Edges(edges) => edges,
            _ => vec![],
        })
        .collect()
}

fn stub_job_from_key(key: &str) -> Option<Job> {
    let parts: Vec<&str> = key.splitn(3, '/').collect();
    if parts.len() != 3 {
        return None;
    }
    let number: u64 = parts[2].parse().ok()?;
    Some(Job {
        repo_owner: parts[0].to_string(),
        repo_name: parts[1].to_string(),
        number,
        title: String::new(),
        state: JobState::Blocked,
        assignees: vec![],
        dependency_numbers: vec![],
        priority: 50,
        timeout_secs: None,
        capabilities: vec![],
        max_retries: 3,
        worker_type: None,
        platform: vec![],
        review: Default::default(),
        activities: vec![],
        retry_attempt: 0,
        is_rework: false,
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn open_tmp() -> TaskGraph {
        let dir = tempdir().unwrap();
        TaskGraph::open(dir.path().to_str().unwrap()).unwrap()
    }

    fn make_job(owner: &str, repo: &str, number: u64) -> Job {
        Job {
            repo_owner: owner.to_string(),
            repo_name: repo.to_string(),
            number,
            title: format!("Job #{number}"),
            state: JobState::OnDeck,
            assignees: vec![],
            dependency_numbers: vec![],
            priority: 50,
            timeout_secs: None,
            capabilities: vec![],
            max_retries: 3,
            worker_type: None,
            platform: vec![],
            review: Default::default(),
            activities: vec![],
            retry_attempt: 0,
            is_rework: false,
        }
    }

    #[test]
    fn test_job_uuid_deterministic() {
        let id1 = TaskGraph::job_uuid("owner/repo/42");
        let id2 = TaskGraph::job_uuid("owner/repo/42");
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_job_uuid_distinct() {
        let id1 = TaskGraph::job_uuid("owner/repo/42");
        let id2 = TaskGraph::job_uuid("owner/repo/43");
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_dag_self_dep_rejected() {
        let g = open_tmp();
        let job = make_job("o", "r", 1);
        g.upsert_job(&job).unwrap();
        let rejected = g.sync_deps("o/r/1", &["o/r/1".to_string()]).unwrap();
        assert_eq!(rejected, vec!["o/r/1"]);
    }

    #[test]
    fn test_dag_direct_cycle_rejected() {
        // A → B and then B → A should be rejected
        let g = open_tmp();
        let a = make_job("o", "r", 1);
        let b = make_job("o", "r", 2);
        g.upsert_job(&a).unwrap();
        g.upsert_job(&b).unwrap();

        // A depends on B (ok)
        let rejected = g.sync_deps("o/r/1", &["o/r/2".to_string()]).unwrap();
        assert!(rejected.is_empty());

        // Now B wants to depend on A — would form A→B→A cycle
        let rejected = g.sync_deps("o/r/2", &["o/r/1".to_string()]).unwrap();
        assert_eq!(rejected, vec!["o/r/1"]);
    }

    #[test]
    fn test_dag_transitive_cycle_rejected() {
        // A→B, B→C, then C→A should be rejected
        let g = open_tmp();
        for n in 1..=3 {
            g.upsert_job(&make_job("o", "r", n)).unwrap();
        }
        assert!(g
            .sync_deps("o/r/1", &["o/r/2".to_string()])
            .unwrap()
            .is_empty());
        assert!(g
            .sync_deps("o/r/2", &["o/r/3".to_string()])
            .unwrap()
            .is_empty());

        // C→A would close the cycle
        let rejected = g.sync_deps("o/r/3", &["o/r/1".to_string()]).unwrap();
        assert_eq!(rejected, vec!["o/r/1"]);
    }

    #[test]
    fn test_dag_valid_diamond() {
        // A→B, A→C, B→D, C→D is a valid DAG (diamond)
        let g = open_tmp();
        for n in 1..=4 {
            g.upsert_job(&make_job("o", "r", n)).unwrap();
        }
        assert!(g
            .sync_deps("o/r/1", &["o/r/2".to_string(), "o/r/3".to_string()])
            .unwrap()
            .is_empty());
        assert!(g
            .sync_deps("o/r/2", &["o/r/4".to_string()])
            .unwrap()
            .is_empty());
        assert!(g
            .sync_deps("o/r/3", &["o/r/4".to_string()])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_all_deps_done_empty() {
        let g = open_tmp();
        g.upsert_job(&make_job("o", "r", 1)).unwrap();
        assert!(g.all_deps_done("o/r/1").unwrap());
    }

    #[test]
    fn test_all_deps_done_with_pending() {
        let g = open_tmp();
        let a = make_job("o", "r", 1); // on-deck
        let mut b = make_job("o", "r", 2);
        b.state = JobState::Done;
        g.upsert_job(&a).unwrap();
        g.upsert_job(&b).unwrap();

        // A depends on B; B is Done → all deps done
        g.sync_deps("o/r/1", &["o/r/2".to_string()]).unwrap();
        assert!(g.all_deps_done("o/r/1").unwrap());

        // If B were on-deck, not all done
        let mut b2 = make_job("o", "r", 3);
        b2.state = JobState::OnDeck;
        g.upsert_job(&b2).unwrap();
        g.sync_deps("o/r/1", &["o/r/2".to_string(), "o/r/3".to_string()])
            .unwrap();
        assert!(!g.all_deps_done("o/r/1").unwrap());
    }
}
