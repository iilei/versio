//! A monorepo can read and alter the current state of all projects.

use crate::analyze::{analyze, Analysis};
use crate::config::{Config, ConfigFile, Project, ProjectId, Size};
use crate::either::{IterEither2 as E2, IterEither3 as E3};
use crate::errors::Result;
use crate::git::{CommitInfoBuf, FullPr, Repo, Slice};
use crate::github::{changes, line_commits_head, Changes};
use crate::state::{CurrentState, OldTags, StateRead, StateWrite};
use crate::vcs::VcsLevel;
use chrono::{DateTime, FixedOffset};
use error_chain::bail;
use std::cmp::max;
use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::identity;
use std::iter;
use std::path::{Path, PathBuf};

pub struct Mono {
  current: Config<CurrentState>,
  next: StateWrite,
  last_commits: HashMap<ProjectId, String>,
  repo: Repo
}

impl Mono {
  pub fn here(vcs: VcsLevel) -> Result<Mono> { Mono::open(".", vcs) }

  pub fn open<P: AsRef<Path>>(dir: P, vcs: VcsLevel) -> Result<Mono> {
    let repo = Repo::open(dir.as_ref(), vcs)?;
    let root = repo.working_dir()?;

    // A little dance to construct a state and config.
    let file = ConfigFile::from_dir(root)?;
    let tag_prefixes = file.projects().iter().filter_map(|p| p.tag_prefix().as_ref().map(|s| s.as_str()));
    let old_tags = find_old_tags(tag_prefixes, file.prev_tag(), &repo)?;
    let state = CurrentState::new(root.to_path_buf(), old_tags);
    let current = Config::new(state, file);

    // TODO: last_commits can be expensive to create: only create them when we build a plan and/or commit?
    //  - we commit often: perhaps only use a real last_commits when we're commiting a plan?
    //  - could `last_commits` be created as part of generating the plan?
    let last_commits = find_last_commits(&current, &repo)?;
    let next = StateWrite::new();

    Ok(Mono { current, next, last_commits, repo })
  }

  pub fn commit(&mut self) -> Result<()> { self.next.commit(&self.repo, self.current.prev_tag(), &self.last_commits) }

  pub fn projects(&self) -> &[Project] { self.current.projects() }

  pub fn get_project(&self, id: ProjectId) -> Result<&Project> {
    self.current.get_project(id).ok_or_else(|| bad!("No such project {}", id))
  }

  pub fn get_named_project(&self, name: &str) -> Result<&Project> {
    let id = self.current.find_unique(name)?;
    self.get_project(id)
  }

  pub fn diff(&self) -> Result<Analysis> {
    let prev_config = self.current.slice_to_prev(&self.repo)?;

    let curt_annotate = self.current.annotate()?;
    let prev_annotate = prev_config.annotate()?;

    Ok(analyze(prev_annotate, curt_annotate))
  }

  pub fn writer(&mut self) -> &mut StateWrite { &mut self.next }
  pub fn reader(&self) -> &dyn StateRead { self.current.state_read() }

  pub fn set_by_id(&mut self, id: ProjectId, val: &str) -> Result<()> {
    self.do_project(id, move |p, n| p.set_value(n, val))
  }

  pub fn set_by_name(&mut self, name: &str, val: &str) -> Result<()> {
    let id = self.current.find_unique(name)?;
    self.set_by_id(id, val)
  }

  pub fn forward_by_id(&mut self, id: ProjectId, val: &str) -> Result<()> {
    self.do_project(id, move |p, n| p.forward_tag(n, val))
  }

  pub fn write_change_log(&mut self, id: ProjectId, change_log: &ChangeLog) -> Result<Option<PathBuf>> {
    self.do_project(id, move |p, n| p.write_change_log(n, change_log))
  }

  fn do_project<F, T>(&mut self, id: ProjectId, f: F) -> Result<T>
  where
    F: FnOnce(&Project, &mut StateWrite) -> Result<T>
  {
    let proj = self.current.get_project(id).ok_or_else(|| bad!("No such project {}", id))?;
    f(proj, &mut self.next)
  }

  pub fn check(&self) -> Result<()> {
    if !self.current.is_configured()? {
      bail!("Project is not configured.");
    }

    for project in self.current.projects() {
      project.check(self.current.state_read())?;
    }
    Ok(())
  }

  pub fn keyed_files<'a>(&'a self) -> Result<impl Iterator<Item = Result<(String, String)>> + 'a> {
    let changes = self.changes()?;
    let prs = changes.into_groups().into_iter().map(|(_, v)| v).filter(|pr| !pr.best_guess());

    let mut vec = Vec::new();
    for pr in prs {
      vec.push(pr_keyed_files(&self.repo, pr));
    }

    Ok(vec.into_iter().flatten())
  }

  pub fn build_plan(&self) -> Result<Plan> {
    let mut plan = PlanBuilder::create(&self.repo, self.current.file())?;

    // Consider the grouped, unsquashed commits to determine project sizing and changelogs.
    for pr in self.changes()?.groups().values() {
      plan.start_pr(pr)?;
      for commit in pr.included_commits() {
        plan.start_commit(commit.clone())?;
        for file in commit.files() {
          plan.start_file(file)?;
          plan.finish_file()?;
        }
        plan.finish_commit()?;
      }
      plan.finish_pr()?;
    }

    // Some projects might depend on other projects.
    plan.handle_deps()?;

    // Sort projects by earliest closed date, mark duplicate commits.
    plan.sort_and_dedup()?;

    Ok(plan.build())
  }

  pub fn changes(&self) -> Result<Changes> {
    let base = self.current.prev_tag().to_string();
    let head = self.repo.branch_name()?.to_string();
    changes(&self.repo, base, head)
  }
}

/// Find the last covering commit ID, if any, for each current project.
fn find_last_commits(current: &Config<CurrentState>, repo: &Repo) -> Result<HashMap<ProjectId, String>> {
  let prev_spec = current.prev_tag();

  let mut last_commits = LastCommitBuilder::create(repo, &current)?;

  // Consider the in-line commits to determine the last commit (if any) for each project.
  for commit in line_commits_head(repo, prev_spec)? {
    last_commits.start_line_commit(&commit)?;
    for file in commit.files() {
      last_commits.start_line_file(file)?;
      last_commits.finish_line_file()?;
    }
    last_commits.finish_line_commit()?;
  }

  last_commits.build()
}

fn pr_keyed_files<'a>(repo: &'a Repo, pr: FullPr) -> impl Iterator<Item = Result<(String, String)>> + 'a {
  let head_oid = match pr.head_oid() {
    Some(oid) => *oid,
    None => return E3::C(iter::empty())
  };

  let iter = repo.commits_between(pr.base_oid(), head_oid).map(move |cmts| {
    cmts
      .filter_map(move |cmt| match cmt {
        Ok(cmt) => {
          if pr.has_exclude(&cmt.id()) {
            None
          } else {
            match cmt.files() {
              Ok(files) => {
                let kind = cmt.kind();
                Some(E2::A(files.map(move |f| Ok((kind.clone(), f)))))
              }
              Err(e) => Some(E2::B(iter::once(Err(e))))
            }
          }
        }
        Err(e) => Some(E2::B(iter::once(Err(e))))
      })
      .flatten()
  });

  match iter {
    Ok(iter) => E3::A(iter),
    Err(e) => E3::B(iter::once(Err(e)))
  }
}

pub struct Plan {
  incrs: HashMap<ProjectId, (Size, ChangeLog)>, // proj ID, incr size, change log
  ineffective: Vec<LoggedPr>                    // PRs that didn't apply to any project
}

impl Plan {
  pub fn incrs(&self) -> &HashMap<ProjectId, (Size, ChangeLog)> { &self.incrs }
  pub fn ineffective(&self) -> &[LoggedPr] { &self.ineffective }
}

pub struct ChangeLog {
  entries: Vec<(LoggedPr, Size)>
}

impl ChangeLog {
  pub fn empty() -> ChangeLog { ChangeLog { entries: Vec::new() } }
  pub fn entries(&self) -> &[(LoggedPr, Size)] { &self.entries }
  pub fn add_entry(&mut self, pr: LoggedPr, size: Size) { self.entries.push((pr, size)); }
  pub fn is_empty(&self) -> bool { self.entries.is_empty() }
}

pub struct LoggedPr {
  number: u32,
  closed_at: DateTime<FixedOffset>,
  commits: Vec<LoggedCommit>
}

impl LoggedPr {
  pub fn empty(number: u32, closed_at: DateTime<FixedOffset>) -> LoggedPr {
    LoggedPr { number, commits: Vec::new(), closed_at }
  }

  pub fn capture(pr: &FullPr) -> LoggedPr {
    LoggedPr { number: pr.number(), closed_at: *pr.closed_at(), commits: Vec::new() }
  }

  pub fn number(&self) -> u32 { self.number }
  pub fn closed_at(&self) -> &DateTime<FixedOffset> { &self.closed_at }
  pub fn commits(&self) -> &[LoggedCommit] { &self.commits }
}

pub struct LoggedCommit {
  oid: String,
  message: String,
  size: Size,
  applies: bool,
  duplicate: bool
}

impl LoggedCommit {
  pub fn new(oid: String, message: String, size: Size) -> LoggedCommit {
    LoggedCommit { oid, message, size, applies: false, duplicate: false }
  }

  pub fn applies(&self) -> bool { self.applies }
  pub fn duplicate(&self) -> bool { self.duplicate }
  pub fn included(&self) -> bool { self.applies && !self.duplicate }
  pub fn oid(&self) -> &str { &self.oid }
  pub fn message(&self) -> &str { &self.message }
  pub fn size(&self) -> Size { self.size }
}

struct PlanBuilder<'s> {
  on_pr_sizes: HashMap<ProjectId, LoggedPr>,
  on_ineffective: Option<LoggedPr>,
  on_commit: Option<CommitInfoBuf>,
  prev: Slicer<'s>,
  current: &'s ConfigFile,
  incrs: HashMap<ProjectId, (Size, ChangeLog)>, // proj ID, incr size, change log
  ineffective: Vec<LoggedPr>                    // PRs that didn't apply to any project
}

impl<'s> PlanBuilder<'s> {
  fn create(repo: &'s Repo, current: &'s ConfigFile) -> Result<PlanBuilder<'s>> {
    let prev = Slicer::init(repo);
    let builder = PlanBuilder {
      on_pr_sizes: HashMap::new(),
      on_ineffective: None,
      on_commit: None,
      prev,
      current,
      incrs: HashMap::new(),
      ineffective: Vec::new()
    };
    Ok(builder)
  }

  pub fn start_pr(&mut self, pr: &FullPr) -> Result<()> {
    self.on_pr_sizes = self.current.projects().iter().map(|p| (p.id(), LoggedPr::capture(pr))).collect();
    self.on_ineffective = Some(LoggedPr::capture(pr));
    Ok(())
  }

  pub fn finish_pr(&mut self) -> Result<()> {
    let mut found = false;
    for (proj_id, logged_pr) in self.on_pr_sizes.drain() {
      let (size, change_log) = self.incrs.entry(proj_id).or_insert((Size::None, ChangeLog::empty()));
      let pr_size = logged_pr.commits.iter().filter(|c| c.applies).map(|c| c.size).max();
      if let Some(pr_size) = pr_size {
        found = true;
        *size = max(*size, pr_size);
        change_log.add_entry(logged_pr, pr_size);
      }
    }

    let ineffective = self.on_ineffective.take().unwrap();
    if !found {
      self.ineffective.push(ineffective);
    }

    Ok(())
  }

  pub fn start_commit(&mut self, commit: CommitInfoBuf) -> Result<()> {
    let id = commit.id().to_string();
    let kind = commit.kind().to_string();
    let summary = commit.summary().to_string();
    self.on_commit = Some(commit);
    self.prev.slice_to(id.clone())?;

    for (proj_id, logged_pr) in &mut self.on_pr_sizes {
      if let Some(cur_project) = self.current.get_project(*proj_id) {
        let size = cur_project.size(&self.current.sizes(), &kind)?;
        logged_pr.commits.push(LoggedCommit::new(id.clone(), summary.clone(), size));
      }
    }

    Ok(())
  }

  pub fn finish_commit(&mut self) -> Result<()> { Ok(()) }

  pub fn start_file(&mut self, path: &str) -> Result<()> {
    let commit = self.on_commit.as_ref().ok_or_else(|| bad!("Not on a commit"))?;
    let commit_id = commit.id();

    for prev_project in self.prev.file()?.projects() {
      if let Some(logged_pr) = self.on_pr_sizes.get_mut(&prev_project.id()) {
        if prev_project.does_cover(path)? {
          let LoggedCommit { applies, .. } = logged_pr.commits.iter_mut().find(|c| c.oid == commit_id).unwrap();
          *applies = true;
        }
      }
    }
    Ok(())
  }

  pub fn finish_file(&mut self) -> Result<()> { Ok(()) }

  pub fn handle_deps(&mut self) -> Result<()> {
    // Use a modified Kahn's algorithm to traverse deps in order.
    let mut queue: VecDeque<(ProjectId, Size)> = VecDeque::new();

    let mut dependents: HashMap<ProjectId, HashSet<ProjectId>> = HashMap::new();
    for project in self.current.projects() {
      for dep in project.depends() {
        dependents.entry(*dep).or_insert_with(HashSet::new).insert(project.id());
      }

      if project.depends().is_empty() {
        if let Some((size, ..)) = self.incrs.get(&project.id()) {
          queue.push_back((project.id(), *size));
        } else {
          queue.push_back((project.id(), Size::None))
        }
      }
    }

    while let Some((id, size)) = queue.pop_front() {
      let val = &mut self.incrs.entry(id).or_insert((Size::None, ChangeLog::empty())).0;
      *val = max(*val, size);

      let depds: Option<HashSet<ProjectId>> = dependents.get(&id).cloned();
      if let Some(depds) = depds {
        for depd in depds {
          dependents.get_mut(&id).unwrap().remove(&depd);
          let val = &mut self.incrs.entry(depd).or_insert((Size::None, ChangeLog::empty())).0;
          *val = max(*val, size);

          if dependents.values().all(|ds| !ds.contains(&depd)) {
            queue.push_back((depd, *val));
          }
        }
      }
    }

    Ok(())
  }

  pub fn sort_and_dedup(&mut self) -> Result<()> {
    for (.., change_log) in self.incrs.values_mut() {
      change_log.entries.sort_by_key(|(pr, _)| *pr.closed_at());

      let mut seen_commits = HashSet::new();
      for (pr, size) in &mut change_log.entries {
        for LoggedCommit { oid, duplicate, .. } in &mut pr.commits {
          if seen_commits.contains(oid) {
            *duplicate = true;
          }
          seen_commits.insert(oid.clone());
        }
        *size = pr.commits().iter().filter(|c| c.included()).map(|c| c.size).max().unwrap_or(Size::None);
      }
    }
    Ok(())
  }

  pub fn build(self) -> Plan { Plan { incrs: self.incrs, ineffective: self.ineffective } }
}

struct LastCommitBuilder<'s, C: StateRead> {
  on_line_commit: Option<String>,
  last_commits: HashMap<ProjectId, String>,
  prev: Slicer<'s>,
  current: &'s Config<C>
}

impl<'s, C: StateRead> LastCommitBuilder<'s, C> {
  fn create(repo: &'s Repo, current: &'s Config<C>) -> Result<LastCommitBuilder<'s, C>> {
    let prev = Slicer::init(repo);
    let builder = LastCommitBuilder { on_line_commit: None, last_commits: HashMap::new(), prev, current };
    Ok(builder)
  }

  pub fn start_line_commit(&mut self, commit: &CommitInfoBuf) -> Result<()> {
    let id = commit.id().to_string();
    self.on_line_commit = Some(id.clone());
    self.prev.slice_to(id)?;
    Ok(())
  }

  pub fn finish_line_commit(&mut self) -> Result<()> { Ok(()) }

  pub fn start_line_file(&mut self, path: &str) -> Result<()> {
    let commit_id = self.on_line_commit.as_ref().ok_or_else(|| bad!("Not on a line commit"))?;

    for prev_project in self.prev.file()?.projects() {
      let proj_id = prev_project.id();
      if self.current.get_project(proj_id).is_some() && prev_project.does_cover(path)? {
        self.last_commits.insert(proj_id, commit_id.clone());
      }
    }
    Ok(())
  }

  pub fn finish_line_file(&mut self) -> Result<()> { Ok(()) }

  pub fn build(self) -> Result<HashMap<ProjectId, String>> { Ok(self.last_commits) }
}

enum Slicer<'r> {
  Orig(&'r Repo),
  Slice((Slice<'r>, ConfigFile))
}

impl<'r> Slicer<'r> {
  pub fn init(repo: &'r Repo) -> Slicer<'r> { Slicer::Orig(repo) }

  pub fn file(&self) -> Result<&ConfigFile> {
    match self {
      Slicer::Slice((_, file)) => Ok(file),
      _ => err!("Slicer not sliced")
    }
  }

  pub fn slice(&self, id: String) -> Slice<'r> {
    match self {
      Slicer::Orig(repo) => repo.slice(id),
      Slicer::Slice((slice, _)) => slice.slice(id)
    }
  }

  pub fn slice_to(&mut self, id: String) -> Result<()> {
    let prev = self.slice(id);
    let file = ConfigFile::from_slice(&prev)?;
    *self = Slicer::Slice((prev, file));
    Ok(())
  }
}

fn find_old_tags<'s, I: Iterator<Item = &'s str>>(prefixes: I, prev_tag: &str, repo: &Repo) -> Result<OldTags> {
  let mut by_prefix_id = HashMap::new(); // Map<prefix, Map<oid, Vec<tag>>>

  for tag_prefix in prefixes {
    let fnmatch = if tag_prefix.is_empty() {
      // TODO: narrow to v[[digit:*]].[[digit:*]].[[digit:*]] or however it's supposed to work.
      "v*".to_string()
    } else {
      // tag_prefix must be alphanum + '-', so no escaping necessary
      // TODO: narrow to v[[digit:*]].[[digit:*]].[[digit:*]] or however it's supposed to work.
      format!("{}-v*", tag_prefix)
    };
    for tag in repo.tag_names(Some(fnmatch.as_str()))?.iter().filter_map(identity) {
      let hash = repo.revparse_oid(&format!("{}^{{}}", tag))?;
      let by_id = by_prefix_id.entry(tag_prefix.to_string()).or_insert_with(HashMap::new);

      // TODO: if adding to non-empty list, sort by tag timestamp (make these annotated and use
      // `Tag.tagger().when()` ?), latest first
      by_id.entry(hash).or_insert_with(Vec::new).push(tag.to_string());
    }
  }

  let mut by_prefix = HashMap::new();
  let mut not_after = HashMap::new();
  let mut not_after_walk = HashMap::new();
  for commit_oid in repo.commits_to_head(prev_tag)?.map(|c| c.map(|c| c.id())) {
    let commit_oid = commit_oid?;
    for (prefix, by_id) in &mut by_prefix_id {
      let not_after_walk = not_after_walk.entry(prefix.clone()).or_insert_with(Vec::new);
      not_after_walk.push(commit_oid.clone());
      if let Some(tags) = by_id.remove(&commit_oid) {
        let old_tags = by_prefix.entry(prefix.clone()).or_insert_with(Vec::new);
        let best_ind = old_tags.len();
        old_tags.extend_from_slice(&tags);
        let not_after_by_oid = not_after.entry(prefix.clone()).or_insert_with(HashMap::new);
        for later_commit_oid in not_after_walk.drain(..) {
          not_after_by_oid.insert(later_commit_oid, best_ind);
        }
      }
    }
  }

  Ok(OldTags::new(by_prefix, not_after))
}