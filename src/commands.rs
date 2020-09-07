//! The command-line options for the executable.

use crate::config::{Config, ConfigFile, Size};
use crate::errors::{Result, ResultExt};
use crate::git::Repo;
use crate::mono::Mono;
use crate::output::{Output, ProjLine};
use crate::state::StateRead;
use crate::vcs::{VcsLevel, VcsRange};
use error_chain::bail;
use std::path::{Path, PathBuf};

pub fn early_info() -> Result<EarlyInfo> {
  let vcs = VcsRange::detect()?.max();
  let root = Repo::find_working_dir(".", vcs, true)?;
  let file = ConfigFile::from_dir(&root)?;
  let project_count = file.projects().len();

  Ok(EarlyInfo::new(project_count, root))
}

/// Environment information gathered even before we set the CLI options.
pub struct EarlyInfo {
  project_count: usize,
  working_dir: PathBuf
}

impl EarlyInfo {
  pub fn new(project_count: usize, working_dir: PathBuf) -> EarlyInfo { EarlyInfo { project_count, working_dir } }
  pub fn project_count(&self) -> usize { self.project_count }
  pub fn working_dir(&self) -> &Path { &self.working_dir }
}

pub fn check(pref_vcs: Option<VcsRange>) -> Result<()> {
  let mono = build(pref_vcs, VcsLevel::None, VcsLevel::Local, VcsLevel::None, VcsLevel::Smart)?;
  let output = Output::new();
  let mut output = output.check();

  mono.check()?;
  output.write_done()?;

  output.commit()
}

pub fn get(
  pref_vcs: Option<VcsRange>, wide: bool, versonly: bool, prev: bool, id: Option<&str>, name: Option<&str>
) -> Result<()> {
  let mono = build(pref_vcs, VcsLevel::None, VcsLevel::Local, VcsLevel::None, VcsLevel::Smart)?;

  if prev {
    get_using_cfg(&mono.config().slice_to_prev(mono.repo())?, wide, versonly, id, name)
  } else {
    get_using_cfg(mono.config(), wide, versonly, id, name)
  }
}

fn get_using_cfg<R: StateRead>(
  cfg: &Config<R>, wide: bool, versonly: bool, id: Option<&str>, name: Option<&str>
) -> Result<()> {
  let output = Output::new();
  let mut output = output.projects(wide, versonly);

  let ensure = || bad!("No such project.");

  let reader = cfg.state_read();
  if let Some(id) = id {
    let id = id.parse()?;
    output.write_project(ProjLine::from(cfg.get_project(&id).ok_or_else(&ensure)?, reader)?)?;
  } else if let Some(name) = name {
    let id = cfg.find_unique(name)?;
    output.write_project(ProjLine::from(cfg.get_project(&id).ok_or_else(&ensure)?, reader)?)?;
  } else {
    if cfg.projects().len() != 1 {
      bail!("No solo project.");
    }
    let id = cfg.projects().get(0).unwrap().id();
    output.write_project(ProjLine::from(cfg.get_project(&id).ok_or_else(&ensure)?, reader)?)?;
  }

  output.commit()
}

pub fn show(pref_vcs: Option<VcsRange>, wide: bool, prev: bool) -> Result<()> {
  let mono = build(pref_vcs, VcsLevel::None, VcsLevel::Local, VcsLevel::None, VcsLevel::Smart)?;

  if prev {
    show_using_cfg(&mono.config().slice_to_prev(mono.repo())?, wide)
  } else {
    show_using_cfg(mono.config(), wide)
  }
}

fn show_using_cfg<R: StateRead>(cfg: &Config<R>, wide: bool) -> Result<()> {
  let output = Output::new();
  let mut output = output.projects(wide, false);
  let reader = cfg.state_read();
  output.write_projects(cfg.projects().iter().map(|p| ProjLine::from(p, reader)))?;
  output.commit()
}

pub fn set(pref_vcs: Option<VcsRange>, id: Option<&str>, name: Option<&str>, value: &str) -> Result<()> {
  let mut mono = build(pref_vcs, VcsLevel::None, VcsLevel::None, VcsLevel::None, VcsLevel::Smart)?;

  if let Some(id) = id {
    mono.set_by_id(&id.parse()?, value)?;
  } else if let Some(name) = name {
    mono.set_by_name(name, value)?;
  } else {
    mono.set_by_only(value)?;
  }

  mono.commit()
}

pub fn diff(pref_vcs: Option<VcsRange>) -> Result<()> {
  let mono = build(pref_vcs, VcsLevel::None, VcsLevel::Local, VcsLevel::Local, VcsLevel::Smart)?;
  let output = Output::new();
  let mut output = output.diff();

  let analysis = mono.diff()?;

  output.write_analysis(analysis)?;
  output.commit()
}

pub fn files(pref_vcs: Option<VcsRange>) -> Result<()> {
  let mono = build(pref_vcs, VcsLevel::None, VcsLevel::Smart, VcsLevel::Local, VcsLevel::Smart)?;
  let output = Output::new();
  let mut output = output.files();

  output.write_files(mono.keyed_files()?)?;
  output.commit()
}

pub fn log(pref_vcs: Option<VcsRange>) -> Result<()> {
  let mut mono = build(pref_vcs, VcsLevel::None, VcsLevel::Smart, VcsLevel::Local, VcsLevel::Smart)?;
  let output = Output::new();
  let mut output = output.log();

  let plan = mono.build_plan()?;

  if plan.incrs().is_empty() {
    output.write_empty()?;
    return output.commit();
  }

  for (id, (.., change_log)) in plan.incrs() {
    if let Some(wrote) = mono.write_change_log(id, change_log)? {
      output.write_logged(wrote)?;
    }
  }

  mono.commit()?;
  output.commit()
}

pub fn changes(pref_vcs: Option<VcsRange>) -> Result<()> {
  let mono = build(pref_vcs, VcsLevel::None, VcsLevel::Smart, VcsLevel::Local, VcsLevel::Smart)?;
  let output = Output::new();
  let mut output = output.changes();

  output.write_changes(mono.changes()?)?;
  output.commit()
}

pub fn plan(pref_vcs: Option<VcsRange>) -> Result<()> {
  let mono = build(pref_vcs, VcsLevel::None, VcsLevel::Smart, VcsLevel::Local, VcsLevel::Smart)?;
  let output = Output::new();
  let mut output = output.plan();

  output.write_plan(mono.build_plan()?)?;
  output.commit(&mono)
}

pub fn run(pref_vcs: Option<VcsRange>, all: bool, dry: bool) -> Result<()> {
  let mut mono = build(pref_vcs, VcsLevel::None, VcsLevel::Smart, VcsLevel::Local, VcsLevel::Smart)?;
  let output = Output::new();
  let mut output = output.run();
  let plan = mono.build_plan()?;

  if let Err((should, is)) = mono.check_branch() {
    bail!("Branch name \"{}\"\" doesn't match \"{}\".", is, should);
  }

  if plan.incrs().is_empty() {
    output.write_empty()?;
    return output.commit();
  }

  for (id, (size, change_log)) in plan.incrs() {
    if let Some(wrote) = mono.write_change_log(id, change_log)? {
      output.write_logged(wrote)?;
    }

    let proj = mono.get_project(id)?;
    let name = proj.name().to_string();
    let curt_config = mono.config();
    let prev_config = curt_config.slice_to_prev(mono.repo())?;
    let curt_vers = curt_config
      .get_value(id)
      .chain_err(|| format!("Unable to find project {} value.", id))?
      .unwrap_or_else(|| panic!("No such project {}.", id));
    let prev_vers = prev_config.get_value(id).chain_err(|| format!("Unable to find prev {} value.", id))?;

    if size == &Size::Empty {
      output.write_no_change(all, name.clone(), prev_vers.clone(), curt_vers.clone())?;
    } else if let Some(prev_vers) = prev_vers {
      let target = size.apply(&prev_vers)?;
      if Size::less_than(&curt_vers, &target)? {
        proj.verify_restrictions(&target)?;
        mono.set_by_id(id, &target)?;
        output.write_changed(name.clone(), prev_vers.clone(), curt_vers.clone(), target.clone())?;
      } else {
        proj.verify_restrictions(&curt_vers)?;
        mono.forward_by_id(id, &curt_vers)?;
        output.write_forward(all, name.clone(), prev_vers.clone(), curt_vers.clone(), target.clone())?;
      }
    } else {
      proj.verify_restrictions(&curt_vers)?;
      mono.forward_by_id(id, &curt_vers)?;
      output.write_new(all, name.clone(), curt_vers.clone())?;
    }
  }

  if !dry {
    mono.commit()?;
    output.write_commit()?;
  } else {
    output.write_dry()?;
  }

  output.write_done()?;
  output.commit()?;
  Ok(())
}

fn build(
  user_pref_vcs: Option<VcsRange>, my_pref_lo: VcsLevel, my_pref_hi: VcsLevel, my_reqd_lo: VcsLevel,
  my_reqd_hi: VcsLevel
) -> Result<Mono> {
  let vcs = combine_vcs(user_pref_vcs, my_pref_lo, my_pref_hi, my_reqd_lo, my_reqd_hi)?;
  Mono::here(vcs.max())
}

fn combine_vcs(
  user_pref_vcs: Option<VcsRange>, my_pref_lo: VcsLevel, my_pref_hi: VcsLevel, my_reqd_lo: VcsLevel,
  my_reqd_hi: VcsLevel
) -> Result<VcsRange> {
  let pref_vcs = user_pref_vcs.unwrap_or_else(move || VcsRange::new(my_pref_lo, my_pref_hi));
  let reqd_vcs = VcsRange::new(my_reqd_lo, my_reqd_hi);
  VcsRange::detect_and_combine(&pref_vcs, &reqd_vcs)
}
