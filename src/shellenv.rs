use std::borrow::BorrowMut;
use std::env;
use std::fmt::Display;
use nix::sys::signal::{self, SigHandler, Signal};
use nix::unistd::{gethostname, getpgid, getpid, tcgetpgrp, tcsetpgrp, Pid, User};
use nix::NixPath;
use std::collections::{HashSet,HashMap};
use std::ffi::CString;
use std::fs::File;
use std::io::Read;
use std::os::fd::{BorrowedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bitflags::bitflags;
use log::{debug, info, trace};

use crate::event::{ShellError, ShellErrorFull};
use crate::execute::{NodeWalker, RshWait};
use crate::interp::expand::expand_var;
use crate::interp::helper;
use crate::interp::parse::{descend, Node, Span};
use crate::RshResult;

bitflags! {
	#[derive(Debug,Copy,Clone)]
	pub struct JobFlags: i8 {
		const LONG      = 0b00000001;
		const PIDS      = 0b00000010;
		const NEW_ONLY  = 0b00000100;
		const RUNNING   = 0b00001000;
		const STOPPED   = 0b00010000;
		const INIT      = 0b00100000;
	}
}

#[derive(Debug,Clone)]
pub struct Job {
	job_id: i32,
	pids: Vec<Pid>,
	commands: Vec<String>,
	pgid: Pid,
	statuses: Vec<RshWait>,
	active: bool
}

impl Job {
	pub fn new(job_id: i32, pids: Vec<Pid>, commands: Vec<String>, pgid: Pid) -> Self {
		let num_pids = pids.len();
		Self { job_id, pgid, pids, commands, statuses: vec![RshWait::Running;num_pids], active: true }
	}
	pub fn is_active(&self) -> bool {
		self.active
	}
	pub fn update_status(&mut self, pid_index: usize, new_stat: RshWait) {
			if pid_index < self.statuses.len() {
					self.statuses[pid_index] = new_stat;
			} else {
					eprintln!("Error: Invalid pid_index {} for statuses", pid_index);
					// Alternatively, return a Result to signal the error.
			}
	}
	pub fn pids(&self) -> &[Pid] {
		&self.pids
	}
	pub fn pgid(&self) -> &Pid {
		&self.pgid
	}
	pub fn get_proc_statuses(&self) -> &[RshWait] {
		&self.statuses
	}
	pub fn id(&self) -> i32 {
		self.job_id
	}
	pub fn commands(&self) -> Vec<String> {
		self.commands.clone()
	}
	pub fn deactivate(&mut self) {
		self.active = false;
	}
	pub fn signal_proc(&self, sig: Signal) -> RshResult<()> {
		if self.pids().len() == 1 {
			let pid = *self.pids().first().unwrap();
				signal::kill(pid, sig).map_err(|_| ShellError::from_io())
		} else {
			signal::killpg(self.pgid, sig).map_err(|_| ShellError::from_io())
		}
	}
	pub fn print(&self, current: Option<i32>, flags: JobFlags) -> String {
		let long = flags.contains(JobFlags::LONG);
		let init = flags.contains(JobFlags::INIT);
		let pids = flags.contains(JobFlags::PIDS);
		let mut output = String::new();

		const GREEN: &str = "\x1b[32m";
		const RED: &str = "\x1b[31m";
		const CYAN: &str = "\x1b[35m";
		const RESET: &str = "\x1b[0m";

		// Add job ID and status
		let symbol = if current.is_some_and(|cur| cur == self.job_id) {
			"+"
		} else if current.is_some_and(|cur| cur == self.job_id + 1) {
			"-"
		} else {
			" "
		};
		output.push_str(&format!("[{}]{} ", self.job_id, symbol));
		let padding_num = symbol.len() + self.job_id.to_string().len() + 3;
		let padding: String = " ".repeat(padding_num);

		// Add commands and PIDs
		for (i, cmd) in self.commands.iter().enumerate() {
			let pid = if pids || init {
				let mut pid = self.pids.get(i).unwrap().to_string();
				pid.push(' ');
				pid
			} else {
				"".to_string()
			};
			let cmd = cmd.clone();
			let mut status0 = if init { "".into() } else { self.statuses.get(i).unwrap().to_string() };
			if status0.len() < 6 && !status0.is_empty() {
				// Pad out the length so that formatting is uniform
				let diff = 6 - status0.len();
				let pad = " ".repeat(diff);
				status0.push_str(&pad);
			}
			let status1 = format!("{}{}",pid,status0);
			let status2 = format!("{}\t{}",status1,cmd);
			let mut status_final = if status0.starts_with("done") {
				format!("{}{}{}",GREEN,status2,RESET)
			} else if status0.starts_with("exit") {
				format!("{}{}{}",RED,status2,RESET)
			} else {
				format!("{}{}{}",CYAN,status2,RESET)
			};
			if i != self.commands.len() - 1 {
				status_final.push_str(" |");
			}
			status_final.push('\n');
			let status_line = if long {
				// Long format includes PIDs
				format!(
						"{}{} {}",
						if i != 0 { padding.clone() } else { "".into() },
						self.pids().get(i).unwrap(),
						status_final
				)
			} else {
				format!(
						"{}{}",
						if i != 0 { padding.clone() } else { "".into() },
						status_final
				)
			};
			output.push_str(&status_line);
		}

		output
	}

}

bitflags! {
	#[derive(Debug,Copy,Clone,PartialEq)]
	pub struct EnvFlags: u32 {
		// Guard conditions against infinite alias/var/function recursion
		const NO_ALIAS         = 0b00000000000000000000000000000001;
		const NO_VAR           = 0b00000000000000000000000000000010;
		const NO_FUNC          = 0b00000000000000000000000000000100;

		// Context
		const IN_FUNC          = 0b00000000000000000000000000001000; // Enables the `return` builtin
		const INTERACTIVE      = 0b00000000000000000000000000010000;
		const CLEAN            = 0b00000000000000000000000000100000; // Do not inherit env vars from parent
		const NO_RC            = 0b00000000000000000000000001000000;

		// Options set by 'set' command
		const EXPORT_ALL_VARS  = 0b00000000000000000000000010000000; // set -a
		const REPORT_JOBS_ASAP = 0b00000000000000000000000100000000; // set -b
		const EXIT_ON_ERROR    = 0b00000000000000000000001000000000; // set -e
		const NO_GLOB          = 0b00000000000000000000010000000000; // set -f
		const HASH_CMDS        = 0b00000000000000000000100000000000; // set -h
		const ASSIGN_ANYWHERE  = 0b00000000000000000001000000000000; // set -k
		const ENABLE_JOB_CTL   = 0b00000000000000000010000000000000; // set -m
		const NO_EXECUTE       = 0b00000000000000000100000000000000; // set -n
		const ENABLE_RSHELL    = 0b00000000000000001000000000000000; // set -r
		const EXIT_AFTER_EXEC  = 0b00000000000000010000000000000000; // set -t
		const UNSET_IS_ERROR   = 0b00000000000000100000000000000000; // set -u
		const PRINT_INPUT      = 0b00000000000001000000000000000000; // set -v
		const STACK_TRACE      = 0b00000000000010000000000000000000; // set -x
		const EXPAND_BRACES    = 0b00000000000100000000000000000000; // set -B
		const NO_OVERWRITE     = 0b00000000001000000000000000000000; // set -C
		const INHERIT_ERR      = 0b00000000010000000000000000000000; // set -E
		const HIST_SUB         = 0b00000000100000000000000000000000; // set -H
		const NO_CD_SYMLINKS   = 0b00000001000000000000000000000000; // set -P
		const INHERIT_RET      = 0b00000010000000000000000000000000; // set -T
	}
}

impl PartialEq for ShellEnv {
    fn eq(&self, other: &Self) -> bool {
        // Compare the inner value of `output_buffer`
        let self_output = self.output_buffer.lock().unwrap();
        let other_output = other.output_buffer.lock().unwrap();

        *self_output == *other_output
            && self.flags == other.flags
            && self.env_vars == other.env_vars
            && self.variables == other.variables
            && self.aliases == other.aliases
            && self.shopts == other.shopts
            && self.functions == other.functions
            && self.parameters == other.parameters
            && self.last_input == other.last_input
						&& self.shell_is_fg == other.shell_is_fg
    }
}

#[derive(Clone,Debug)]
pub struct JobTable {
	fg: Option<Job>,
	jobs: HashMap<i32,Job>,
	curr_job: Option<i32>,
	updated_since_check: Vec<i32>,
}

impl JobTable {
	fn new() -> Self {
		Self {
			fg: None,
			jobs: HashMap::new(),
			curr_job: None,
			updated_since_check: Vec::new()
		}
	}
	pub fn curr_job(&self) -> Option<i32> {
		self.curr_job
	}
	pub fn mark_updated(&mut self, id: i32) {
		self.updated_since_check.push(id)
	}
	pub fn print_jobs(&mut self, flags: &JobFlags) {
		let mut jobs = if flags.contains(JobFlags::NEW_ONLY) {
			self.jobs
				.values()
				.filter(|job| self.updated_since_check.contains(&job.job_id))
				.collect::<Vec<&Job>>()
		} else {
			self.jobs.values().collect::<Vec<&Job>>()
		};
		self.updated_since_check.clear();
		jobs.sort_by_key(|job| job.job_id);
		for job in jobs {
			let id = job.job_id;
			// Filter jobs based on flags
			if flags.contains(JobFlags::RUNNING) && !matches!(job.statuses.get(id as usize).unwrap(), RshWait::Running) {
				continue;
			}
			if flags.contains(JobFlags::STOPPED) && !matches!(job.statuses.get(id as usize).unwrap(), RshWait::Stopped {..}) {
				continue;
			}
			// Print the job in the selected format
			println!("{}", job.print(self.curr_job, *flags));
		}
	}
}

#[derive(Debug,Clone)]
pub struct ShellEnv {
	flags: EnvFlags,
	output_buffer: Arc<Mutex<String>>,
	env_vars: HashMap<String, String>,
	variables: HashMap<String, String>,
	aliases: HashMap<String, String>,
	shopts: HashMap<String,usize>,
	functions: HashMap<String, Box<Node>>,
	parameters: HashMap<String, String>,
	last_input: Option<String>,
	pub job_table: JobTable,
	shell_is_fg: bool,
}

impl ShellEnv {
	// Constructor
	pub fn new(flags: EnvFlags) -> Self {
		let mut open_fds = HashSet::new();
		let shopts = init_shopts();
		// TODO: probably need to find a way to initialize env vars that doesnt rely on a parent process
		let clean_env = flags.contains(EnvFlags::CLEAN);
		let env_vars = Self::init_env_vars(clean_env);
		open_fds.insert(0);
		open_fds.insert(1);
		open_fds.insert(2);
		let mut shellenv = Self {
			flags: EnvFlags::empty(),
			output_buffer: Arc::new(Mutex::new(String::new())),
			env_vars,
			variables: HashMap::new(),
			aliases: HashMap::new(),
			shopts,
			functions: HashMap::new(),
			parameters: HashMap::new(),
			last_input: None,
			job_table: JobTable::new(),
			shell_is_fg: true
		};
		if !flags.contains(EnvFlags::NO_RC) {
			let runtime_commands_path = &expand_var(&shellenv, "${HOME}/.rshrc".into());
			let runtime_commands_path = Path::new(runtime_commands_path);
			if runtime_commands_path.exists() {
				if let Err(e) = shellenv.source_file(runtime_commands_path.to_path_buf()) {
					let err = ShellErrorFull::from(shellenv.get_last_input(),e);
					eprintln!("Failed to source ~/.rshrc: {}",err);
				}
			} else {
				eprintln!("Warning: Runtime commands file '{}' not found.", runtime_commands_path.display());
			}
		}
		shellenv
	}

	pub fn new_job(&mut self, pids: Vec<Pid>, commands: Vec<String>, pgid: Pid, fg: bool) {
		let job_id = if fg {
			0
		} else {
			self.job_table.jobs.len() + 1
		};
		let job = Job::new(job_id as i32,pids,commands,pgid);
		println!("{}",job.print(Some(job_id as i32), JobFlags::INIT));
		if job_id >= 1 {
			self.job_table.jobs.insert(job_id as i32, job);
			self.set_curr_job(job_id as i32);
		} else {
			self.set_fg_job(job);
		}
	}

	pub fn borrow_jobs(&mut self) -> &mut HashMap<i32,Job> {
		&mut self.job_table.jobs
	}

	pub fn set_fg_job(&mut self, job: Job) {
		self.job_table.fg = Some(job)
	}

	pub fn update_curr_job(&mut self) {
		let jobs = &self.job_table.jobs;
		let mut jobs = jobs.values().collect::<Vec<&Job>>();
		jobs.sort_by_key(|job| job.job_id);
		let mut most_recent_still_running: Option<i32> = None;
		for job in jobs {
			if job.is_active() {
				most_recent_still_running = Some(job.job_id);
			}
		}
		self.job_table.curr_job = most_recent_still_running;
	}

	pub fn set_curr_job(&mut self, job_id: i32) {
		if !self.job_table.jobs.is_empty() {
			self.job_table.curr_job = Some(job_id);
		}
	}

	fn init_env_vars(clean: bool) -> HashMap<String,String> {
		let pathbuf_to_string = |pb: Result<PathBuf, std::io::Error>| pb.unwrap_or_default().to_string_lossy().to_string();
		// First, inherit any env vars from the parent process if clean bit not set
		let mut env_vars = HashMap::new();
		if !clean {
			env_vars = std::env::vars().collect::<HashMap<String,String>>();
		}
		let home;
		let username;
		let uid;
		if let Some(user) = User::from_uid(nix::unistd::Uid::current()).ok().flatten() {
			home = user.dir;
			username = user.name;
			uid = user.uid;
		} else {
			home = PathBuf::new();
			username = "unknown".into();
			uid = 0.into();
		}
		let home = pathbuf_to_string(Ok(home));
		let hostname = gethostname().map(|hname| hname.to_string_lossy().to_string()).unwrap_or_default();

		env_vars.insert("HOSTNAME".into(), hostname.clone());
		env::set_var("HOSTNAME", hostname);
		env_vars.insert("UID".into(), uid.to_string());
		env::set_var("UID", uid.to_string());
		env_vars.insert("TMPDIR".into(), "/tmp".into());
		env::set_var("TMPDIR", "/tmp");
		env_vars.insert("TERM".into(), "xterm-256color".into());
		env::set_var("TERM", "xterm-256color");
		env_vars.insert("LANG".into(), "en_US.UTF-8".into());
		env::set_var("LANG", "en_US.UTF-8");
		env_vars.insert("USER".into(), username.clone());
		env::set_var("USER", username.clone());
		env_vars.insert("LOGNAME".into(), username.clone());
		env::set_var("LOGNAME", username);
		env_vars.insert("PWD".into(), pathbuf_to_string(std::env::current_dir()));
		env::set_var("PWD", pathbuf_to_string(std::env::current_dir()));
		env_vars.insert("OLDPWD".into(), pathbuf_to_string(std::env::current_dir()));
		env::set_var("OLDPWD", pathbuf_to_string(std::env::current_dir()));
		env_vars.insert("HOME".into(), home.clone());
		env::set_var("HOME", home.clone());
		env_vars.insert("SHELL".into(), pathbuf_to_string(std::env::current_exe()));
		env::set_var("SHELL", pathbuf_to_string(std::env::current_exe()));
		env_vars.insert("HIST_FILE".into(),format!("{}/.rsh_hist",home));
		env::set_var("HIST_FILE",format!("{}/.rsh_hist",home));
		env_vars
	}

	pub fn mod_flags<F>(&mut self, transform: F) where F: FnOnce(&mut EnvFlags) {
		transform(&mut self.flags)
	}

	pub fn is_interactive(&self) -> bool {
		self.flags.contains(EnvFlags::INTERACTIVE)
	}

	pub fn set_last_input(&mut self, input: &str) {
		self.last_input = Some(input.to_string())
	}

	pub fn get_last_input(&mut self) -> String {
		self.last_input.clone().unwrap_or_default()
	}

	pub fn source_profile(&mut self) -> RshResult<()> {
		let home = self.get_variable("HOME").unwrap();
		let path = PathBuf::from(format!("{}/.rsh_profile",home));
		self.source_file(path)
	}

	pub fn source_file(&mut self, path: PathBuf) -> RshResult<()> {
		let mut file = File::open(&path).map_err(|_| ShellError::from_io())?;
		let mut buffer = String::new();
		file.read_to_string(&mut buffer).map_err(|_| ShellError::from_io())?;
		self.last_input = Some(buffer.clone());


		let state = descend(&buffer, self)?;
		let new_env = self.clone();
		let mut walker = NodeWalker::new(state.ast, new_env);
		let code = walker.start_walk()?;
		if let RshWait::Fail { code, cmd, span } = code {
			if code == 127 {
				if let Some(cmd) = cmd {
					let err = ShellErrorFull::from(walker.shellenv.get_last_input(),ShellError::from_no_cmd(&format!("Command not found: {}",cmd), span));
					eprintln!("{}", err);
				}
			}
		}
		let new_env = walker.deconstruct();
		self.replace(new_env);
		Ok(())
	}

	pub fn replace(&mut self, other: ShellEnv) {
		let ShellEnv {
			flags,
			output_buffer,
			env_vars,
			variables,
			aliases,
			shopts,
			functions,
			parameters,
			last_input,
			job_table,
			shell_is_fg
		} = other;

		self.flags = flags;
		self.output_buffer= output_buffer;
		self.env_vars= env_vars;
		self.variables= variables;
		self.aliases= aliases;
		self.shopts= shopts;
		self.functions= functions;
		self.parameters= parameters;
		self.last_input= last_input;
		self.job_table = job_table;
		self.shell_is_fg = shell_is_fg;
	}

	pub fn change_dir(&mut self, path: &Path, span: Span) -> RshResult<()> {
		let result = env::set_current_dir(path);
		match result {
			Ok(_) => {
				let old_pwd = self.env_vars.remove("PWD").unwrap_or_default();
				self.export_variable("OLDPWD".into(), old_pwd);
				let new_dir = env::current_dir().unwrap().to_string_lossy().to_string();
				self.export_variable("PWD".into(), new_dir);
				Ok(())
			}
			Err(e) => Err(ShellError::from_execf(&e.to_string(), 1, span))
		}
	}

	pub fn set_flags(&mut self, flags: EnvFlags) {
		self.flags |= flags;
		self.update_flags_param();
	}

	pub fn unset_flags(&mut self, flags:EnvFlags) {
		self.flags &= !flags;
		self.update_flags_param();
	}

	pub fn update_flags_param(&mut self) {
		let mut flag_list = "abefhkmnrptuvxBCEHPT".chars();
		let mut flag_string = String::new();
		while let Some(ch) = flag_list.next() {
			match ch {
				'a' if self.flags.contains(EnvFlags::EXPORT_ALL_VARS) => flag_string.push(ch),
				'b' if self.flags.contains(EnvFlags::REPORT_JOBS_ASAP) => flag_string.push(ch),
				'e' if self.flags.contains(EnvFlags::EXIT_ON_ERROR) => flag_string.push(ch),
				'f' if self.flags.contains(EnvFlags::NO_GLOB) => flag_string.push(ch),
				'h' if self.flags.contains(EnvFlags::HASH_CMDS) => flag_string.push(ch),
				'k' if self.flags.contains(EnvFlags::ASSIGN_ANYWHERE) => flag_string.push(ch),
				'm' if self.flags.contains(EnvFlags::ENABLE_JOB_CTL) => flag_string.push(ch),
				'n' if self.flags.contains(EnvFlags::NO_EXECUTE) => flag_string.push(ch),
				'r' if self.flags.contains(EnvFlags::ENABLE_RSHELL) => flag_string.push(ch),
				't' if self.flags.contains(EnvFlags::EXIT_AFTER_EXEC) => flag_string.push(ch),
				'u' if self.flags.contains(EnvFlags::UNSET_IS_ERROR) => flag_string.push(ch),
				'v' if self.flags.contains(EnvFlags::PRINT_INPUT) => flag_string.push(ch),
				'x' if self.flags.contains(EnvFlags::STACK_TRACE) => flag_string.push(ch),
				'B' if self.flags.contains(EnvFlags::EXPAND_BRACES) => flag_string.push(ch),
				'C' if self.flags.contains(EnvFlags::NO_OVERWRITE) => flag_string.push(ch),
				'E' if self.flags.contains(EnvFlags::INHERIT_ERR) => flag_string.push(ch),
				'H' if self.flags.contains(EnvFlags::HIST_SUB) => flag_string.push(ch),
				'P' if self.flags.contains(EnvFlags::NO_CD_SYMLINKS) => flag_string.push(ch),
				'T' if self.flags.contains(EnvFlags::INHERIT_RET) => flag_string.push(ch),
				_ => { /* Do nothing */ }
			}
		}
		self.set_parameter("-".into(), flag_string);
	}

	pub fn handle_exit_status(&mut self, wait_status: RshWait) {
		match wait_status {
			RshWait::Success { .. } => self.set_parameter("?".into(), "0".into()),
			RshWait::Fail { code, cmd: _, span: _ } => self.set_parameter("?".into(), code.to_string()),
			_ => unimplemented!()
		}
	}

	pub fn set_interactive(&mut self, interactive: bool) {
		if interactive {
			self.flags |= EnvFlags::INTERACTIVE
		} else {
			self.flags &= !EnvFlags::INTERACTIVE
		}
	}

	pub fn get_env_vars(&self) -> &HashMap<String,String> {
		&self.env_vars
	}

	pub fn get_shopts(&self) -> &HashMap<String,usize> {
		&self.shopts
	}

	pub fn get_shopt(&self, shopt: &str) -> Option<&usize> {
		self.shopts.get(shopt)
	}

	pub fn get_params(&self) -> &HashMap<String,String> {
		&self.parameters
	}

	pub fn set_params(&mut self, new_params: HashMap<String,String>) {
		self.parameters = new_params;
	}

	// Getters and Setters for `variables`
	pub fn get_variable(&self, key: &str) -> Option<String> {
		if let Some(value) = self.variables.get(key) {
			Some(value.to_string())
		} else if let Some(value) = self.env_vars.get(key) {
			Some(value.to_string())
		} else {
			self.get_parameter(key).map(|val| val.to_string())
		}
	}

	/// For C FFI calls
	pub fn get_cvars(&self) -> Vec<CString> {
		self.env_vars
			.iter()
			.map(|(key, value)| {
				let env_pair = format!("{}={}", key, value);
				CString::new(env_pair).unwrap() })
			.collect::<Vec<CString>>()
	}

	pub fn set_variable(&mut self, key: String, mut value: String) {
		debug!("inserted var: {} with value: {}",key,value);
		if value.starts_with('"') && value.ends_with('"') {
			value = value.strip_prefix('"').unwrap().into();
			value = value.strip_suffix('"').unwrap().into();
		}
		self.variables.insert(key.clone(), value);
		trace!("testing variable get: {} = {}", key, self.get_variable(key.as_str()).unwrap())
	}

	pub fn set_shopt(&mut self, key: &str, value: usize) {
		self.shopts.insert(key.into(),value);
	}

	pub fn export_variable(&mut self, key: String, value: String) {
		let value = value.trim_matches(|ch| ch == '"').to_string();
		if value.as_str() == "" {
			self.variables.remove(&key);
			self.env_vars.remove(&key);
		} else {
			self.variables.insert(key.clone(),value.clone());
			self.env_vars.insert(key,value);
		}
	}

	pub fn remove_variable(&mut self, key: &str) -> Option<String> {
		self.variables.remove(key)
	}

	// Getters and Setters for `aliases`
	pub fn get_alias(&self, key: &str) -> Option<&String> {
		if self.flags.contains(EnvFlags::NO_ALIAS) {
			return None
		}
		self.aliases.get(key)
	}

	pub fn set_alias(&mut self, key: String, mut value: String) -> Result<(), String> {
		if self.get_function(key.as_str()).is_some() {
			return Err(format!("The name `{}` is already being used as a function",key))
		}
		if value.starts_with('"') && value.ends_with('"') {
			value = value.strip_prefix('"').unwrap().into();
			value = value.strip_suffix('"').unwrap().into();
		}
		self.aliases.insert(key, value);
		Ok(())
	}

	pub fn remove_alias(&mut self, key: &str) -> Option<String> {
		self.aliases.remove(key)
	}

	// Getters and Setters for `functions`
	pub fn get_function(&self, name: &str) -> Option<Box<Node>> {
		self.functions.get(name).cloned()
	}

	pub fn set_function(&mut self, name: String, body: Box<Node>) -> RshResult<()> {
		if self.get_alias(name.as_str()).is_some() {
			return Err(ShellError::from_parse(format!("The name `{}` is already being used as an alias",name).as_str(), body.span()))
		}
		self.functions.insert(name, body);
		Ok(())
	}

	pub fn remove_function(&mut self, name: &str) -> Option<Box<Node>> {
		self.functions.remove(name)
	}

	// Getters and Setters for `parameters`
	pub fn get_parameter(&self, key: &str) -> Option<&String> {
		if key == "*" {
			// Return the un-split parameter string
			return self.parameters.get("@")
		}
		self.parameters.get(key)
	}

	pub fn set_parameter(&mut self, key: String, value: String) {
		if key.chars().next().unwrap().is_ascii_digit() {
			let mut pos_params = self.parameters.remove("@").unwrap_or_default();
			if pos_params.is_empty() {
				pos_params = value.clone();
			} else {
				pos_params = format!("{} {}",pos_params,value);
			}
			self.parameters.insert("@".into(),pos_params.clone());
			let num_params = pos_params.split(' ').count();
			self.parameters.insert("#".into(),num_params.to_string());
		}
		self.parameters.insert(key, value);
	}

	pub fn clear_pos_parameters(&mut self) {
		let mut index = 1;
		while let Some(_value) = self.get_parameter(index.to_string().as_str()) {
			self.parameters.remove(index.to_string().as_str());
			index += 1;
		}
	}

	// Utility method to clear the environment
	pub fn clear(&mut self) {
		self.variables.clear();
		self.aliases.clear();
		self.functions.clear();
		self.parameters.clear();
	}
}

fn init_shopts() -> HashMap<String,usize> {
	let mut shopts = HashMap::new();
	shopts.insert("dotglob".into(),0);
	shopts.insert("trunc_prompt_path".into(),4);
	shopts.insert("int_comments".into(),1);
	shopts.insert("autocd".into(),1);
	shopts.insert("hist_ignore_dupes".into(),1);
	shopts.insert("max_hist".into(),1000);
	shopts.insert("edit_mode".into(),1);
	shopts.insert("comp_limit".into(),100);
	shopts.insert("auto_hist".into(),1);
	shopts.insert("prompt_highlight".into(),1);
	shopts.insert("tab_stop".into(),4);
	shopts.insert("bell_style".into(),1);
	shopts
}
