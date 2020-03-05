use crate::either::IterEither as E;
use crate::error::Result;
use git2::build::CheckoutBuilder;
use git2::{
  AnnotatedCommit, AutotagOption, Blob, Cred, Diff, DiffOptions, FetchOptions, Oid, Reference, ReferenceType, Remote,
  RemoteCallbacks, Repository, RepositoryState, Status, StatusOptions
};
use std::env::var;
use std::io::{stdout, Write};
use std::path::{Path, PathBuf};

const PREV_TAG_NAME: &str = "versio-prev";

pub struct FetchResults {
  pub fetch_branch: String,
  pub commit_oid: Option<Oid>
}

pub fn prev_blob<P: AsRef<Path>>(repo: &Repository, path: P) -> Result<Option<Blob>> {
  let path_string = path.as_ref().to_string_lossy();
  let obj = repo.revparse_single(&format!("{}:{}", PREV_TAG_NAME, &path_string)).ok();
  obj.map(|obj| obj.into_blob().map_err(|e| versio_error!("Not a file: {} : {:?}", path_string, e))).transpose()
}

pub fn pull_ff_only(repo: &Repository, remote_name: Option<&str>, remote_branch: Option<&str>) -> Result<()> {
  let fetch_results = fetch(&repo, remote_name, remote_branch)?;
  merge_after_fetch(repo, &fetch_results)
}

pub fn fetch(repo: &Repository, remote_name: Option<&str>, remote_branch: Option<&str>) -> Result<FetchResults> {
  let (remote_name, fetch_branch) = get_name_and_branch(repo, remote_name, remote_branch)?;

  let state = repo.state();
  if state != RepositoryState::Clean {
    return versio_err!("Can't pull: repository {:?} isn't clean.", state);
  }

  let mut remote = repo.find_remote(&remote_name)?;
  let fetch_commit: Option<AnnotatedCommit> = do_fetch(&repo, &[&fetch_branch], &mut remote)?;
  Ok(FetchResults { fetch_branch, commit_oid: fetch_commit.map(|c| c.id()) })
}

pub fn merge_after_fetch(repo: &Repository, fetch_results: &FetchResults) -> Result<()> {
  if let Some(fetch_commit_oid) = &fetch_results.commit_oid {
    let fetch_commit = repo.find_annotated_commit(*fetch_commit_oid)?;

    let mut status_opts = StatusOptions::new();
    status_opts.include_ignored(false);
    status_opts.include_untracked(true);
    status_opts.exclude_submodules(false);
    if repo.statuses(Some(&mut status_opts))?.iter().any(|s| s.status() != Status::CURRENT) {
      return versio_err!("Can't pull: repository isn't current.");
    }

    do_merge(repo, &fetch_results.fetch_branch, &fetch_commit)?;
  }

  Ok(())
}

fn do_fetch<'a>(repo: &'a Repository, refs: &[&str], remote: &'a mut Remote) -> Result<Option<AnnotatedCommit<'a>>> {
  let mut cb = RemoteCallbacks::new();

  cb.credentials(|_url, username_from_url, _allowed_types| {
    Cred::ssh_key(
      username_from_url.unwrap(),
      None,
      Path::new(&format!("{}/.ssh/id_rsa", var("HOME").unwrap())),
      Some("unVm7JekaHpvyefTJMHK")
    )
  });

  cb.transfer_progress(|stats| {
    if stats.received_objects() == stats.total_objects() {
      print!("Resolving deltas {}/{}\r", stats.indexed_deltas(), stats.total_deltas());
    } else if stats.total_objects() > 0 {
      print!(
        "Received {}/{} objects ({}) in {} bytes\r",
        stats.received_objects(),
        stats.total_objects(),
        stats.indexed_objects(),
        stats.received_bytes()
      );
    }
    stdout().flush().unwrap();
    true
  });

  let mut fo = FetchOptions::new();
  fo.remote_callbacks(cb);

  fo.download_tags(AutotagOption::All);
  println!("Fetching {} for repo", remote.name().unwrap());
  remote.fetch(refs, Some(&mut fo), None)?;

  let stats = remote.stats();
  if stats.local_objects() > 0 {
    println!(
      "\rReceived {}/{} objects in {} bytes (used {} local objects)",
      stats.indexed_objects(),
      stats.total_objects(),
      stats.received_bytes(),
      stats.local_objects()
    );
  } else {
    println!(
      "\rReceived {}/{} objects in {} bytes",
      stats.indexed_objects(),
      stats.total_objects(),
      stats.received_bytes()
    );
  }

  let fetch_head = repo.find_reference("FETCH_HEAD").ok();
  Ok(fetch_head.map(|fetch_head| repo.reference_to_annotated_commit(&fetch_head)).transpose()?)
}

fn do_merge<'a>(repo: &'a Repository, remote_branch: &str, fetch_commit: &AnnotatedCommit<'a>) -> Result<()> {
  let analysis = repo.merge_analysis(&[fetch_commit])?;

  if analysis.0.is_fast_forward() {
    println!("Updating branch (fast forward)");
    let refname = format!("refs/heads/{}", remote_branch);
    match repo.find_reference(&refname) {
      Ok(mut r) => Ok(fast_forward(repo, &mut r, fetch_commit)?),
      Err(_) => {
        // Probably pulling in an empty repo; just set the reference to the commit directly.
        let message = format!("Setting {} to {}", remote_branch, fetch_commit.id());
        repo.reference(&refname, fetch_commit.id(), true, &message)?;
        repo.set_head(&refname)?;
        Ok(
          repo.checkout_head(Some(
            CheckoutBuilder::default().allow_conflicts(false).conflict_style_merge(false).force()
          ))?
        )
      }
    }
  } else if analysis.0.is_normal() {
    versio_err!("Can't pull: would not be a fast-forward.")
  } else {
    println!("Up to date.");
    Ok(())
  }
}

fn fast_forward(repo: &Repository, lb: &mut Reference, rc: &AnnotatedCommit) -> Result<()> {
  let name = match lb.name() {
    Some(s) => s.to_string(),
    None => String::from_utf8_lossy(lb.name_bytes()).to_string()
  };

  let msg = format!("Fast-forward: {} -> {:.7}", name, rc.id());
  println!("{}", msg);

  lb.set_target(rc.id(), &msg)?;
  repo.set_head(&name)?;
  // 'force' required to update the working directory; safe becaused we checked that it's clean.
  repo.checkout_head(Some(CheckoutBuilder::default().force()))?;
  Ok(())
}

pub fn get_name_and_branch(repo: &Repository, name: Option<&str>, branch: Option<&str>) -> Result<(String, String)> {
  let remote_name: String = name.map(|s| s.to_string()).unwrap_or_else(|| {
    let remote_name = "origin";
    println!("No remote provided: using {}", remote_name);
    remote_name.to_string()
  });

  let remote_branch = branch.map(|b| Ok(b.to_string())).unwrap_or_else(|| {
    let head_ref = repo.find_reference("HEAD").map_err(|e| versio_error!("Couldn't resolve head: {:?}.", e))?;
    if head_ref.kind() != Some(ReferenceType::Symbolic) {
      return versio_err!("Not on a branch.");
    } else {
      let mut branch_name = head_ref.symbolic_target().ok_or_else(|| versio_error!("Branch is not named."))?;
      if branch_name.starts_with("refs/heads/") {
        branch_name = &branch_name[11 ..];
      } else {
        return versio_err!("Current {} is not a branch.", branch_name);
      }
      println!("No branch provided: using {}", branch_name);
      Ok(branch_name.to_string())
    }
  })?;

  Ok((remote_name, remote_branch))
}

pub fn get_changed_since<'a>(repo: &'a Repository) -> Result<impl Iterator<Item = Result<(String, String)>> + 'a> {
  let mut revwalk = repo.revwalk()?;
  if let Ok(prev_spec) = repo.revparse_single(PREV_TAG_NAME) {
    revwalk.hide(prev_spec.id())?;
  } else {
    println!("\"{}\" not found, searching all history.", PREV_TAG_NAME);
  }
  let head_spec = repo.revparse_single("HEAD")?;
  revwalk.push(head_spec.id())?;

  macro_rules! try1 {
    ($e:expr) => {
      match $e {
        Ok(t) => t,
        Err(e) => return E::A(std::iter::once(Err(crate::error::Error::from(e))))
      }
    };
  }

  Ok(revwalk.flat_map(move |id| {
    let id = try1!(id);
    let commit = try1!(repo.find_commit(id));
    let summary = commit.summary().unwrap_or("-");
    let kind = match summary.char_indices().find(|(_, c)| *c == ':' || *c == '(').map(|(i, _)| i) {
      Some(i) => &summary[0 .. i].trim(),
      None => "-"
    };
    let kind = kind.to_string();

    if commit.parents().len() == 1 {
      let parent = try1!(commit.parent(0));
      let mut diffopts = DiffOptions::new();
      let ptree = try1!(parent.tree());
      let ctree = try1!(commit.tree());
      let diff = try1!(repo.diff_tree_to_tree(Some(&ptree), Some(&ctree), Some(&mut diffopts)));
      let iter = DeltaIter::new(diff);
      E::B(iter.map(move |path| Ok((kind.clone(), path.to_string_lossy().into_owned()))))
    } else {
      E::C(std::iter::empty())
    }
  }))
}

struct DeltaIter<'repo> {
  diff: Diff<'repo>,
  len: usize,
  on: usize,
  on_new: bool
}

impl<'repo> Iterator for DeltaIter<'repo> {
  type Item = PathBuf;

  fn next(&mut self) -> Option<PathBuf> {
    while let None = self.current() {
      if self.advance() {
        break;
      }
    }

    let current = self.current().map(|p| p.to_path_buf());
    self.advance();
    current
  }
}

impl<'repo> DeltaIter<'repo> {
  pub fn new(diff: Diff<'repo>) -> DeltaIter<'repo> {
    let len = diff.deltas().len();
    DeltaIter { diff, len, on: 0, on_new: false }
  }

  fn current(&self) -> Option<&Path> {
    let delta = self.diff.get_delta(self.on);
    if self.on_new {
      delta.and_then(|delta| delta.new_file().path())
    } else {
      delta.and_then(|delta| delta.old_file().path())
    }
  }

  fn advance(&mut self) -> bool {
    if self.on >= self.len {
      return true;
    }

    if self.on_new {
      self.on_new = false;
      self.on += 1;
    } else {
      let old_path = self.diff.get_delta(self.on).and_then(|dl| dl.old_file().path());
      let new_path = self.diff.get_delta(self.on).and_then(|dl| dl.new_file().path());
      if old_path == new_path {
        self.on += 1;
      } else {
        self.on_new = true;
      }
    }

    self.on >= self.len
  }
}
