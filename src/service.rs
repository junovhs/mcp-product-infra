//! Install the platform supervisor that keeps a product's `serve` hub resident.
//!
//! The HTTP hub makes a host unable to take a server down by crashing, but only
//! while the server happens to be running. Started by hand, it dies at the next
//! reboot and every host config then points at a dead port — which surfaces to
//! the user as "MCP server unavailable" with nothing naming the cause. The unit
//! text lives in the binary rather than in a file someone wrote once on one
//! machine, so it versions with the product.
//!
//! Product-neutral: a product supplies its [`Hub`] and a one-line description,
//! and everything else — the quoting rules, the restart bounds, the hardening,
//! and the honesty about what systemd actually reported — is shared.

use crate::http::Hub;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What an install/uninstall attempt actually did, so the caller reports facts
/// rather than assuming success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceOutcome {
    Installed {
        unit_path: PathBuf,
        enabled: String,
        active: String,
        /// Set when the unit points at a location that may not survive, e.g. a
        /// build directory. Reported rather than silently accepted.
        warning: Option<String>,
    },
    Uninstalled {
        unit_path: PathBuf,
        removed: bool,
    },
    /// This platform has no supported supervisor. Reported explicitly: silently
    /// doing nothing would leave the user believing the service is installed.
    Unsupported(String),
}

/// One product's resident-hub service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Service {
    hub: Hub,
    description: &'static str,
    env: &'static [(&'static str, &'static str)],
}

impl Service {
    /// Declare a product's service. `description` becomes the unit's
    /// `Description=` line, which is what `systemctl --user status` shows.
    pub const fn new(hub: Hub, description: &'static str) -> Self {
        Self {
            hub,
            description,
            env: &[],
        }
    }

    /// Declare environment variables the unit sets for the served process, in
    /// the order given.
    ///
    /// A `static` slice rather than an owned collection so a product can still
    /// declare its whole service as a `const`, and so what the unit will set is
    /// visible in code instead of inherited from whatever shell ran the install.
    /// This exists because a product may gate its own `serve` command behind an
    /// environment variable: the systemd user manager inherits nothing from the
    /// operator's shell, so without this the installed unit is rejected by the
    /// product's own gate at every start.
    pub const fn with_env(mut self, env: &'static [(&'static str, &'static str)]) -> Self {
        self.env = env;
        self
    }

    /// The unit name is part of the user's interface — they type it into
    /// `systemctl --user status` — so it is derived from the product name and
    /// nothing else, and stays stable for the life of the product.
    pub fn unit_name(&self) -> String {
        format!("{}-serve.service", self.hub.name())
    }

    /// Where the systemd --user unit lives.
    pub fn unit_path(&self) -> Result<PathBuf, String> {
        let dirs = directories::BaseDirs::new().ok_or("cannot resolve a home directory")?;
        Ok(dirs
            .config_dir()
            .join("systemd")
            .join("user")
            .join(self.unit_name()))
    }

    /// Render the unit. Pure so it can be asserted on without touching systemd.
    ///
    /// `Restart=always` is the point of the whole file, but it is rate-bounded: a
    /// genuinely broken binary should fail visibly instead of spinning forever.
    pub fn unit_contents(&self, exe: &str, port: u16, working_dir: &str) -> String {
        let exe = systemd_value(exe);
        let working_dir = systemd_value(working_dir);
        let name = self.hub.name();
        let description = self.description;
        // Empty when nothing is declared, so a product that declares no variables
        // gets a unit byte-identical to one from before this existed.
        let environment: String = self
            .env
            .iter()
            .map(|(key, value)| {
                format!("Environment={}\n", systemd_value(&format!("{key}={value}")))
            })
            .collect();
        format!(
            "[Unit]\n\
             # Keep the hub resident so agent hosts always have a URL to connect to.\n\
             # Without this a reboot leaves every host config pointing at a dead port,\n\
             # which presents as \"MCP server unavailable\" with no obvious cause.\n\
             Description={description}\n\
             After=default.target\n\
             \n\
             # Rate-bound the restarts configured below. These two keys belong to this\n\
             # section; systemd ignores them in the service section, which would\n\
             # silently leave a crash loop unbounded.\n\
             StartLimitIntervalSec=60\n\
             StartLimitBurst=5\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={exe} serve --port {port}\n\
             WorkingDirectory={working_dir}\n\
             {environment}\
             \n\
             # A host crash must not take this down; neither should a crash of its own.\n\
             Restart=always\n\
             RestartSec=2\n\
             \n\
             # A loopback-only listener holding a bearer token gets no more of the\n\
             # system than it needs. PrivateTmp is safe: a product's own lock state\n\
             # lives under its repository, never in /tmp.\n\
             NoNewPrivileges=true\n\
             PrivateTmp=true\n\
             ProtectSystem=full\n\
             \n\
             StandardOutput=journal\n\
             StandardError=journal\n\
             SyslogIdentifier={name}-serve\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n"
        )
    }

    /// The Scheduled Task name, which is what a user types into
    /// `schtasks /Query /TN`. Derived from the product name alone — like the unit
    /// name — so two products can never overwrite each other's supervisor.
    pub fn task_name(&self) -> String {
        format!("{}-serve", self.hub.name())
    }

    /// The task as Task Scheduler addresses it: a name in the root folder.
    pub fn task_path(&self) -> PathBuf {
        PathBuf::from(format!("\\{}", self.task_name()))
    }

    /// What the task actually launches.
    ///
    /// A Scheduled Task has no equivalent of systemd's `Environment=`: the XML
    /// schema simply cannot carry environment variables. A product that gates its
    /// own `serve` behind one (semmap does) would therefore install a task that
    /// its own gate rejects at every start — the precise failure the systemd
    /// `Environment=` support was added to fix. So when variables are declared the
    /// action is wrapped in `cmd.exe /c set ... && <exe>`, and when none are
    /// declared the executable is invoked directly, leaving the common case free
    /// of a wrapper process.
    fn windows_action(&self, exe: &str, port: u16) -> (String, String) {
        if self.env.is_empty() {
            return (exe.to_string(), format!("serve --port {port}"));
        }
        let assignments: String = self
            .env
            .iter()
            .map(|(key, value)| format!("set \"{key}={value}\" && "))
            .collect();
        (
            "cmd.exe".to_string(),
            format!("/c {assignments}\"{exe}\" serve --port {port}"),
        )
    }

    /// Render the Scheduled Task registration. Pure, so the XML can be asserted on
    /// without touching Task Scheduler.
    ///
    /// Two triggers, because they do different jobs. The `LogonTrigger` starts the
    /// hub at login, which is what makes it survive a reboot. The repeating
    /// `TimeTrigger` is the `Restart=always` equivalent.
    ///
    /// It is emphatically NOT `RestartOnFailure`, which is the intuitive choice and
    /// the wrong one: that setting governs the task failing to *start*, not the
    /// launched program exiting. Killing the supervised hub with it configured left
    /// the task sitting at `Ready` with `Last Result: -1` and nothing ever came
    /// back. A trigger repeating every minute retries instead, and
    /// `MultipleInstancesPolicy=IgnoreNew` makes each retry a no-op while the hub
    /// is healthy — so the pair behaves as "start it, and start it again whenever
    /// it is not running". `RestartOnFailure` is kept for the case it does cover,
    /// bounded at 5 like `StartLimitBurst`.
    ///
    /// The repetition must hang off a `TimeTrigger` with a `StartBoundary` in the
    /// past. A `<Repetition>` nested in the `LogonTrigger` is accepted by
    /// `schtasks /Create` and then silently discarded — `schtasks /Query /V` shows
    /// `Repeat: Every: N/A` — which is a supervisor that reports success and
    /// supervises nothing.
    ///
    /// `ExecutionTimeLimit` of `PT0S` means "no limit", without which Task
    /// Scheduler stops a long-lived server after three days.
    ///
    /// `user_id` scopes both the trigger and the principal to one account, and it
    /// is not optional: a `LogonTrigger` carrying no `UserId` fires for *every*
    /// user of the machine, and registering that is an administrative act. Without
    /// it `schtasks /Create` fails with a bare "Access is denied" on a normal
    /// account — which would make this backend useless for exactly the per-user,
    /// admin-free install it exists to provide.
    pub fn task_xml(&self, exe: &str, port: u16, working_dir: &str, user_id: &str) -> String {
        let (command, arguments) = self.windows_action(exe, port);
        let command = xml_text(&command);
        let arguments = xml_text(&arguments);
        let working_dir = xml_text(working_dir);
        let description = xml_text(self.description);
        let user_id = xml_text(user_id);
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
             <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
             \x20 <RegistrationInfo>\n\
             \x20   <Description>{description}</Description>\n\
             \x20 </RegistrationInfo>\n\
             \x20 <Triggers>\n\
             \x20   <LogonTrigger>\n\
             \x20     <Enabled>true</Enabled>\n\
             \x20     <UserId>{user_id}</UserId>\n\
             \x20   </LogonTrigger>\n\
             \x20   <TimeTrigger>\n\
             \x20     <StartBoundary>2000-01-01T00:00:00</StartBoundary>\n\
             \x20     <Repetition>\n\
             \x20       <Interval>PT1M</Interval>\n\
             \x20     </Repetition>\n\
             \x20   </TimeTrigger>\n\
             \x20 </Triggers>\n\
             \x20 <Principals>\n\
             \x20   <Principal id=\"Author\">\n\
             \x20     <UserId>{user_id}</UserId>\n\
             \x20     <LogonType>InteractiveToken</LogonType>\n\
             \x20     <RunLevel>LeastPrivilege</RunLevel>\n\
             \x20   </Principal>\n\
             \x20 </Principals>\n\
             \x20 <Settings>\n\
             \x20   <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n\
             \x20   <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n\
             \x20   <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n\
             \x20   <AllowHardTerminate>true</AllowHardTerminate>\n\
             \x20   <StartWhenAvailable>true</StartWhenAvailable>\n\
             \x20   <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>\n\
             \x20   <IdleSettings>\n\
             \x20     <StopOnIdleEnd>false</StopOnIdleEnd>\n\
             \x20     <RestartOnIdle>false</RestartOnIdle>\n\
             \x20   </IdleSettings>\n\
             \x20   <AllowStartOnDemand>true</AllowStartOnDemand>\n\
             \x20   <Enabled>true</Enabled>\n\
             \x20   <Hidden>true</Hidden>\n\
             \x20   <RunOnlyIfIdle>false</RunOnlyIfIdle>\n\
             \x20   <WakeToRun>false</WakeToRun>\n\
             \x20   <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\n\
             \x20   <Priority>7</Priority>\n\
             \x20   <RestartOnFailure>\n\
             \x20     <Interval>PT1M</Interval>\n\
             \x20     <Count>5</Count>\n\
             \x20   </RestartOnFailure>\n\
             \x20 </Settings>\n\
             \x20 <Actions Context=\"Author\">\n\
             \x20   <Exec>\n\
             \x20     <Command>{command}</Command>\n\
             \x20     <Arguments>{arguments}</Arguments>\n\
             \x20     <WorkingDirectory>{working_dir}</WorkingDirectory>\n\
             \x20   </Exec>\n\
             \x20 </Actions>\n\
             </Task>\n"
        )
    }

    /// The launchd job label, which is what a user passes to `launchctl`.
    pub fn plist_label(&self) -> String {
        format!("com.{}.serve", self.hub.name())
    }

    /// Where the per-user LaunchAgent lives. `~/Library/LaunchAgents` is the
    /// per-user location; `/Library/LaunchDaemons` would be machine-wide and needs
    /// root, which a per-user product install must not require.
    pub fn plist_path(&self) -> Result<PathBuf, String> {
        let dirs = directories::BaseDirs::new().ok_or("cannot resolve a home directory")?;
        Ok(dirs
            .home_dir()
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{}.plist", self.plist_label())))
    }

    /// Render the LaunchAgent. Pure, so it can be asserted on from any platform —
    /// which is the only way this is covered at all, since macOS cannot be
    /// executed on the machine this was written on.
    ///
    /// `KeepAlive` is the `Restart=always` equivalent; `ThrottleInterval` is the
    /// rate bound, and launchd's floor is 10 seconds regardless of a lower value.
    pub fn plist_contents(&self, exe: &str, port: u16, working_dir: &str) -> String {
        let label = xml_text(&self.plist_label());
        let environment = if self.env.is_empty() {
            String::new()
        } else {
            let entries: String = self
                .env
                .iter()
                .map(|(key, value)| {
                    format!(
                        "\x20   <key>{}</key>\n\x20   <string>{}</string>\n",
                        xml_text(key),
                        xml_text(value)
                    )
                })
                .collect();
            format!("\x20 <key>EnvironmentVariables</key>\n\x20 <dict>\n{entries}\x20 </dict>\n")
        };
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \x20 <key>Label</key>\n\
             \x20 <string>{label}</string>\n\
             \x20 <key>ProgramArguments</key>\n\
             \x20 <array>\n\
             \x20   <string>{exe}</string>\n\
             \x20   <string>serve</string>\n\
             \x20   <string>--port</string>\n\
             \x20   <string>{port}</string>\n\
             \x20 </array>\n\
             \x20 <key>WorkingDirectory</key>\n\
             \x20 <string>{working_dir}</string>\n\
             {environment}\
             \x20 <key>RunAtLoad</key>\n\
             \x20 <true/>\n\
             \x20 <key>KeepAlive</key>\n\
             \x20 <true/>\n\
             \x20 <key>ThrottleInterval</key>\n\
             \x20 <integer>2</integer>\n\
             </dict>\n\
             </plist>\n",
            exe = xml_text(exe),
            working_dir = xml_text(working_dir)
        )
    }

    /// Is a systemd user session actually available? Having the binary on PATH is
    /// not enough — in a container or over a bare ssh session there may be no user
    /// manager to talk to, and installing into one that cannot run is a false
    /// success.
    fn systemd_user_available(&self) -> Option<String> {
        if !cfg!(target_os = "linux") {
            return Some(format!(
                "{} is only supported on Linux with systemd; install a supervisor for this platform by hand",
                self.unit_name()
            ));
        }
        match systemctl(&["--user", "is-system-running"]) {
            // Any state at all means a user manager answered; `degraded` still runs units.
            Ok(output) if !output.stdout.is_empty() || output.status.success() => None,
            Ok(_) | Err(_) => Some(
                "no systemd --user session is available here, so nothing was installed".to_string(),
            ),
        }
    }

    /// Does this executable live somewhere that will still exist later? A binary
    /// run out of a build directory or a temp dir would leave an enabled unit
    /// pointing at a path that disappears, which fails silently at the next boot.
    fn durability_warning(&self, exe: &Path) -> Option<String> {
        // Normalize separators before matching: the markers below are written with
        // forward slashes, so on Windows — where a build path is
        // `...\target\debug\...` — none of them would ever fire, and the warning
        // would be silently dead on the one platform whose install this was added
        // alongside.
        let text = exe.display().to_string().replace('\\', "/");
        let name = self.hub.name();
        // The worktree marker is built from the product's own dot-directory
        // rather than a bare `/worktrees/`: a bare match would warn about any
        // stable install that merely happens to sit under a directory with that
        // name, and a warning that fires on a durable path teaches the operator
        // to ignore it.
        let worktrees = format!("/.{name}/worktrees/");
        let fragile = ["/target/", "/tmp/", "/var/tmp/", worktrees.as_str()];
        fragile
            .iter()
            .find(|marker| text.contains(**marker))
            .map(|_| {
                format!(
                    "the unit will start {text}, which is a build/temporary location — \
                 reinstall {name} and re-run `{name} serve --install-service` so the \
                 service points at the installed binary"
                )
            })
    }

    /// Install and start the supervisor that keeps this product's hub resident.
    ///
    /// Dispatches to the platform's own per-user supervisor. Every backend is
    /// compiled on every platform — the choice is a runtime `cfg!`, not a
    /// `#[cfg]` — so a change to the macOS renderer still type-checks and is still
    /// unit-tested on a Windows or Linux machine. Only the OS commands differ.
    pub fn install(&self, port: u16) -> Result<ServiceOutcome, String> {
        let name = self.hub.name();
        let exe = std::env::current_exe()
            .map_err(|error| format!("cannot resolve the running {name} binary: {error}"))?;
        let working_dir = directories::BaseDirs::new()
            .map(|dirs| dirs.home_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("/"));
        if cfg!(target_os = "linux") {
            self.install_systemd(port, &exe, &working_dir)
        } else if cfg!(target_os = "windows") {
            self.install_windows(port, &exe, &working_dir)
        } else if cfg!(target_os = "macos") {
            self.install_launchd(port, &exe, &working_dir)
        } else {
            Ok(ServiceOutcome::Unsupported(format!(
                "no per-user supervisor is implemented for this platform, so {name} \
                 will not survive a reboot; start it with `{name} serve`"
            )))
        }
    }

    /// Stop, disable, and remove the supervisor this product installed.
    pub fn uninstall(&self) -> Result<ServiceOutcome, String> {
        if cfg!(target_os = "linux") {
            self.uninstall_systemd()
        } else if cfg!(target_os = "windows") {
            self.uninstall_windows()
        } else if cfg!(target_os = "macos") {
            self.uninstall_launchd()
        } else {
            Ok(ServiceOutcome::Unsupported(
                "no per-user supervisor is implemented for this platform".to_string(),
            ))
        }
    }

    /// Register the Scheduled Task from XML and start it now.
    ///
    /// The XML is written UTF-16LE with a BOM: `schtasks /Create /XML` rejects a
    /// UTF-8 file whose declaration says UTF-16, and reads the encoding from the
    /// bytes rather than the declaration.
    fn install_windows(
        &self,
        port: u16,
        exe: &Path,
        working_dir: &Path,
    ) -> Result<ServiceOutcome, String> {
        let task = self.task_name();
        let xml = self.task_xml(
            &exe.display().to_string(),
            port,
            &working_dir.display().to_string(),
            &windows_user_id(),
        );
        let xml_file = std::env::temp_dir().join(format!("{task}.xml"));
        std::fs::write(&xml_file, utf16le_with_bom(&xml))
            .map_err(|error| format!("write {}: {error}", xml_file.display()))?;

        // /F replaces an existing registration, which is what makes re-running the
        // install an upgrade rather than an error.
        let created = schtasks(&[
            "/Create",
            "/TN",
            &task,
            "/XML",
            &xml_file.display().to_string(),
            "/F",
        ])?;
        let _ = std::fs::remove_file(&xml_file);
        if !created.status.success() {
            return Err(format!(
                "schtasks /Create /TN {task} failed: {}",
                command_message(&created)
            ));
        }
        let run = schtasks(&["/Run", "/TN", &task])?;
        if !run.status.success() {
            return Err(format!(
                "the task registered but schtasks /Run /TN {task} failed, so the hub is \
                 not up yet: {}",
                command_message(&run)
            ));
        }
        let status = schtasks_status(&task);
        Ok(ServiceOutcome::Installed {
            unit_path: self.task_path(),
            enabled: status.clone(),
            active: status,
            warning: self.durability_warning(exe),
        })
    }

    fn uninstall_windows(&self) -> Result<ServiceOutcome, String> {
        let task = self.task_name();
        // Ending a task that is not running also "fails", so its result is only
        // advisory; deletion is the step that has to succeed.
        let _ = schtasks(&["/End", "/TN", &task]);
        let deleted = schtasks(&["/Delete", "/TN", &task, "/F"])?;
        let missing = command_message(&deleted).contains("cannot find");
        if !deleted.status.success() && !missing {
            return Err(format!(
                "schtasks /Delete /TN {task} failed, so the supervisor may still be \
                 registered: {}",
                command_message(&deleted)
            ));
        }
        Ok(ServiceOutcome::Uninstalled {
            unit_path: self.task_path(),
            removed: deleted.status.success(),
        })
    }

    /// Write the LaunchAgent and load it now.
    fn install_launchd(
        &self,
        port: u16,
        exe: &Path,
        working_dir: &Path,
    ) -> Result<ServiceOutcome, String> {
        let path = self.plist_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        std::fs::write(
            &path,
            self.plist_contents(
                &exe.display().to_string(),
                port,
                &working_dir.display().to_string(),
            ),
        )
        .map_err(|error| format!("write {}: {error}", path.display()))?;

        let target = format!("gui/{}", current_uid());
        // Replacing an existing job requires removing it first; bootout failing
        // because nothing was loaded is the normal first-install case.
        let _ = launchctl(&["bootout", &target, &path.display().to_string()]);
        let loaded = launchctl(&["bootstrap", &target, &path.display().to_string()])?;
        if !loaded.status.success() {
            return Err(format!(
                "launchctl bootstrap {target} {} failed: {}",
                path.display(),
                command_message(&loaded)
            ));
        }
        let label = self.plist_label();
        let _ = launchctl(&["enable", &format!("{target}/{label}")]);
        let status = match launchctl(&["print", &format!("{target}/{label}")]) {
            Ok(output) if output.status.success() => "loaded".to_string(),
            Ok(output) => command_message(&output),
            Err(error) => error,
        };
        Ok(ServiceOutcome::Installed {
            unit_path: path,
            enabled: status.clone(),
            active: status,
            warning: self.durability_warning(exe),
        })
    }

    fn uninstall_launchd(&self) -> Result<ServiceOutcome, String> {
        let path = self.plist_path()?;
        let target = format!("gui/{}", current_uid());
        let booted_out = launchctl(&["bootout", &target, &path.display().to_string()]);
        if let Ok(output) = &booted_out {
            if !output.status.success() && path.exists() {
                let message = command_message(output);
                // "No such process" is the already-unloaded case, not a failure.
                if !message.contains("No such process") && !message.contains("not find") {
                    return Err(format!(
                        "launchctl bootout failed, so the agent may still be running: {message}"
                    ));
                }
            }
        }
        let removed = match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(format!("remove {}: {error}", path.display())),
        };
        Ok(ServiceOutcome::Uninstalled {
            unit_path: path,
            removed,
        })
    }

    /// Materialize the unit, reload systemd, and enable + start it now.
    fn install_systemd(
        &self,
        port: u16,
        exe: &Path,
        working_dir: &Path,
    ) -> Result<ServiceOutcome, String> {
        if let Some(reason) = self.systemd_user_available() {
            return Ok(ServiceOutcome::Unsupported(reason));
        }
        let path = self.unit_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        std::fs::write(
            &path,
            self.unit_contents(
                &exe.display().to_string(),
                port,
                &working_dir.display().to_string(),
            ),
        )
        .map_err(|error| format!("write {}: {error}", path.display()))?;

        let unit_name = self.unit_name();
        let reload = systemctl(&["--user", "daemon-reload"])?;
        if !reload.status.success() {
            return Err(format!(
                "systemctl --user daemon-reload failed: {}",
                String::from_utf8_lossy(&reload.stderr).trim()
            ));
        }
        let enable = systemctl(&["--user", "enable", "--now", &unit_name])?;
        if !enable.status.success() {
            return Err(format!(
                "systemctl --user enable --now {unit_name} failed: {}",
                String::from_utf8_lossy(&enable.stderr).trim()
            ));
        }
        Ok(ServiceOutcome::Installed {
            unit_path: path,
            enabled: query(&["--user", "is-enabled", &unit_name]),
            active: query(&["--user", "is-active", &unit_name]),
            warning: self.durability_warning(exe),
        })
    }

    /// Stop, disable, and remove the unit.
    fn uninstall_systemd(&self) -> Result<ServiceOutcome, String> {
        if let Some(reason) = self.systemd_user_available() {
            return Ok(ServiceOutcome::Unsupported(reason));
        }
        let path = self.unit_path()?;
        let unit_name = self.unit_name();
        // Disabling before removing the file is what lets systemd drop its symlinks;
        // deleting first would strand them in default.target.wants.
        let disable = systemctl(&["--user", "disable", "--now", &unit_name])?;
        // A unit that was never installed also "fails" to disable, so distinguish
        // that from a real failure by whether the unit file is actually there.
        if !disable.status.success() && path.exists() {
            return Err(format!(
                "systemctl --user disable --now {unit_name} failed, so the service may still be running: {}",
                String::from_utf8_lossy(&disable.stderr).trim()
            ));
        }
        let removed = match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(format!("remove {}: {error}", path.display())),
        };
        let reload = systemctl(&["--user", "daemon-reload"])?;
        if !reload.status.success() {
            return Err(format!(
                "removed {} but systemctl --user daemon-reload failed: {}",
                path.display(),
                String::from_utf8_lossy(&reload.stderr).trim()
            ));
        }
        Ok(ServiceOutcome::Uninstalled {
            unit_path: path,
            removed,
        })
    }
}

/// Quote a path for a unit file.
///
/// Three escapes, each for a different systemd rule. `%` is the specifier prefix
/// and must be doubled. `$` triggers environment expansion in `ExecStart=` — and
/// it does so inside quotes too, so quoting alone does not save a path
/// containing `$HOME` or `${x}`; it must be doubled to `$$`. Whitespace or quote
/// characters need the whole value quoted or it silently becomes several
/// arguments.
fn systemd_value(raw: &str) -> String {
    let escaped = raw.replace('%', "%%").replace('$', "$$");
    if escaped
        .chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\\' || c == '\'')
    {
        format!("\"{}\"", escaped.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        escaped
    }
}

/// Escape a value for XML text content.
///
/// Both the Scheduled Task registration and the LaunchAgent are XML, and both
/// carry filesystem paths chosen by whoever installed the product. A path
/// containing `&` or `<` — legal on Windows and macOS alike — would otherwise
/// produce a document the OS rejects, or worse, one it misreads.
fn xml_text(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Encode as UTF-16LE with a byte-order mark.
///
/// `schtasks /Create /XML` determines the encoding from the file's bytes, not
/// from the XML declaration, and rejects a file whose declaration says UTF-16
/// while the bytes are UTF-8 with "The task XML is malformed".
fn utf16le_with_bom(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// The current user's numeric id, for launchctl's `gui/<uid>` domain target.
fn current_uid() -> String {
    // Only ever called on macOS, where `id -u` is always present.
    match Command::new("id").arg("-u").output() {
        Ok(output) => String::from_utf8_lossy(&output.stdout).trim().to_string(),
        Err(_) => String::new(),
    }
}

/// The account the task is registered for, as `DOMAIN\user`.
///
/// Read from the environment rather than shelled out for: `whoami` renders an
/// Entra/AzureAD account as `AzureAD+user`, which Task Scheduler does not accept,
/// while `USERDOMAIN`/`USERNAME` give the `AzureAD\user` form it does. Falls back
/// to the bare user name when no domain is set, which is the local-account case.
fn windows_user_id() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() && !user.is_empty() => format!("{domain}\\{user}"),
        _ => user,
    }
}

fn schtasks(args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("schtasks")
        .args(args)
        .output()
        .map_err(|error| format!("schtasks {}: {error}", args.join(" ")))
}

fn launchctl(args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("launchctl")
        .args(args)
        .output()
        .map_err(|error| format!("launchctl {}: {error}", args.join(" ")))
}

/// What a command actually said, preferring stderr and falling back to stdout —
/// schtasks reports several failures on stdout.
fn command_message(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        stderr
    }
}

/// The task's state as Task Scheduler reports it, so the caller states a fact
/// rather than assuming the registration took.
fn schtasks_status(task: &str) -> String {
    match schtasks(&["/Query", "/TN", task, "/FO", "LIST"]) {
        Ok(output) => String::from_utf8_lossy(&output.stdout)
            .lines()
            .find(|line| line.trim_start().starts_with("Status:"))
            .map(|line| {
                line.split_once(':')
                    .map_or("", |(_, v)| v)
                    .trim()
                    .to_string()
            })
            .unwrap_or_else(|| command_message(&output)),
        Err(error) => error,
    }
}

fn systemctl(args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|error| format!("systemctl {}: {error}", args.join(" ")))
}

/// A systemctl query whose non-zero exit is itself the answer (`is-enabled`
/// exits non-zero for a disabled unit), so the status text is what matters.
fn query(args: &[&str]) -> String {
    match systemctl(args) {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if text.is_empty() {
                String::from_utf8_lossy(&output.stderr).trim().to_string()
            } else {
                text
            }
        }
        Err(error) => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HUB: Hub = Hub::new("testprod", 7977);
    const SERVICE: Service = Service::new(HUB, "Testprod MCP hub (loopback HTTP)");

    #[test]
    fn the_unit_starts_the_running_binary_on_the_requested_port() {
        let unit = SERVICE.unit_contents("/opt/testprod/bin/testprod", 7977, "/home/someone");
        assert!(unit.contains("ExecStart=/opt/testprod/bin/testprod serve --port 7977"));
        assert!(unit.contains("WorkingDirectory=/home/someone"));
        // The restart guarantee is the entire reason this file exists.
        assert!(unit.contains("Restart=always"));
        // ...but bounded, so a broken binary cannot spin forever.
        assert!(unit.contains("StartLimitBurst=5"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn a_non_default_port_reaches_the_unit() {
        let unit = SERVICE.unit_contents("/usr/bin/testprod", 8123, "/root");
        assert!(unit.contains("serve --port 8123"));
        assert!(!unit.contains("7977"));
    }

    #[test]
    fn the_unit_lands_in_the_systemd_user_directory() {
        let path = SERVICE
            .unit_path()
            .expect("a home directory in the test environment");
        assert!(
            path.ends_with("systemd/user/testprod-serve.service"),
            "{path:?}"
        );
    }

    #[test]
    fn restart_limits_live_in_the_unit_section_where_systemd_reads_them() {
        let unit = SERVICE.unit_contents("/usr/bin/testprod", 7977, "/home/someone");
        let unit_section = &unit[unit.find("[Unit]").unwrap()..unit.find("[Service]").unwrap()];
        // Under [Service] these keys are silently ignored, leaving a crash loop
        // unbounded while the file still looks like it has a bound.
        assert!(unit_section.contains("StartLimitIntervalSec=60"), "{unit}");
        assert!(unit_section.contains("StartLimitBurst=5"), "{unit}");
        let service_section = &unit[unit.find("[Service]").unwrap()..];
        assert!(!service_section.contains("StartLimit"), "{unit}");
    }

    #[test]
    fn awkward_paths_cannot_break_the_unit_file() {
        let unit = SERVICE.unit_contents("/opt/my prod/bin/testprod", 7977, "/home/some one");
        assert!(
            unit.contains("ExecStart=\"/opt/my prod/bin/testprod\" serve --port 7977"),
            "a path with a space must stay one argument: {unit}"
        );
        assert!(
            unit.contains("WorkingDirectory=\"/home/some one\""),
            "{unit}"
        );
        // `%` is systemd's specifier prefix and must be doubled to survive.
        let percent = SERVICE.unit_contents("/opt/100%prod/testprod", 7977, "/home/x");
        assert!(percent.contains("/opt/100%%prod/testprod"), "{percent}");
        // `$` is expanded in ExecStart even inside quotes, so quoting is not
        // enough on its own — it has to be doubled.
        let dollar = SERVICE.unit_contents("/opt/${HOME}/testprod", 7977, "/home/x");
        assert!(dollar.contains("/opt/$${HOME}/testprod"), "{dollar}");
    }

    #[test]
    fn a_build_directory_binary_is_flagged_as_not_durable() {
        assert!(SERVICE
            .durability_warning(Path::new("/home/x/prod/target/debug/testprod"))
            .is_some());
        assert!(SERVICE
            .durability_warning(Path::new(
                "/home/x/prod/.testprod/worktrees/SERV-02/testprod"
            ))
            .is_some());
        // The installed location must not warn.
        assert!(SERVICE
            .durability_warning(Path::new("/home/x/.cargo/bin/testprod"))
            .is_none());
        // A durable path that merely contains the word must not warn: a warning
        // that fires on a stable install teaches the operator to ignore it.
        assert!(SERVICE
            .durability_warning(Path::new("/srv/worktrees/bin/testprod"))
            .is_none());
    }

    /// Two products must not collide on one unit name, or installing the second
    /// would silently overwrite the first's supervisor.
    #[test]
    fn each_product_gets_its_own_unit_name_and_identifier() {
        let other = Service::new(Hub::new("otherprod", 7988), "Otherprod MCP hub");
        assert_eq!(SERVICE.unit_name(), "testprod-serve.service");
        assert_eq!(other.unit_name(), "otherprod-serve.service");
        assert_ne!(SERVICE.unit_path().unwrap(), other.unit_path().unwrap());
        assert!(SERVICE
            .unit_contents("/usr/bin/testprod", 7977, "/home/x")
            .contains("SyslogIdentifier=testprod-serve"));
        assert!(other
            .unit_contents("/usr/bin/otherprod", 7988, "/home/x")
            .contains("SyslogIdentifier=otherprod-serve"));
    }

    /// A product that gates its own `serve` behind an environment variable needs
    /// the unit to set it: the systemd user manager inherits nothing from the
    /// shell that ran the install, so without this the service is rejected by
    /// that product's own gate at every start.
    #[test]
    fn declared_environment_reaches_the_unit_in_order() {
        let service = SERVICE.with_env(&[("PROD_CLI", "1"), ("PROD_MODE", "hub")]);
        let unit = service.unit_contents("/usr/bin/testprod", 7977, "/home/x");
        assert!(unit.contains("Environment=PROD_CLI=1"), "{unit}");
        assert!(unit.contains("Environment=PROD_MODE=hub"), "{unit}");
        let first = unit.find("PROD_CLI").unwrap();
        let second = unit.find("PROD_MODE").unwrap();
        assert!(
            first < second,
            "declaration order must be preserved: {unit}"
        );
        // They belong to the service section, where systemd reads them.
        let service_section = &unit[unit.find("[Service]").unwrap()..];
        assert!(service_section.contains("Environment=PROD_CLI=1"), "{unit}");
    }

    /// An awkward value must be escaped exactly as ExecStart's is, or it silently
    /// becomes several arguments or a systemd specifier.
    #[test]
    fn an_awkward_environment_value_is_escaped_like_every_other_unit_value() {
        let service = SERVICE.with_env(&[("PROD_DIR", "/opt/my prod/100%")]);
        let unit = service.unit_contents("/usr/bin/testprod", 7977, "/home/x");
        assert!(
            unit.contains("Environment=\"PROD_DIR=/opt/my prod/100%%\""),
            "{unit}"
        );
    }

    /// Ishoo declares no variables, so its installed unit must be untouched by
    /// this feature existing.
    #[test]
    fn a_service_with_no_environment_emits_no_environment_line() {
        let unit = SERVICE.unit_contents("/usr/bin/testprod", 7977, "/home/x");
        assert!(!unit.contains("Environment="), "{unit}");
    }

    /// The description is the product's, not a generic one: it is what the user
    /// sees in `systemctl --user status`.
    #[test]
    fn the_products_description_reaches_the_unit() {
        assert!(SERVICE
            .unit_contents("/usr/bin/testprod", 7977, "/home/x")
            .contains("Description=Testprod MCP hub (loopback HTTP)"));
    }

    // ---- Windows Scheduled Task -------------------------------------------

    #[test]
    fn the_task_starts_the_running_binary_on_the_requested_port() {
        let xml = SERVICE.task_xml(
            "C:\\bin\\testprod.exe",
            7977,
            "C:\\Users\\someone",
            "DOM\\me",
        );
        assert!(
            xml.contains("<Command>C:\\bin\\testprod.exe</Command>"),
            "{xml}"
        );
        assert!(
            xml.contains("<Arguments>serve --port 7977</Arguments>"),
            "{xml}"
        );
        assert!(
            xml.contains("<WorkingDirectory>C:\\Users\\someone</WorkingDirectory>"),
            "{xml}"
        );
        // The restart guarantee is the entire reason this registration exists, and
        // on Windows it is the repeating trigger that provides it: RestartOnFailure
        // does NOT restart a task whose launched program exited, which was proven
        // by killing the supervised hub and watching the task go back to Ready.
        assert!(xml.contains("<Repetition>"), "{xml}");
        assert!(xml.contains("<Interval>PT1M</Interval>"), "{xml}");
        // The repetition must hang off the TimeTrigger: nested in the LogonTrigger
        // it is accepted and then silently discarded, leaving no keep-alive at all.
        let time_trigger =
            &xml[xml.find("<TimeTrigger>").unwrap()..xml.find("</TimeTrigger>").unwrap()];
        assert!(time_trigger.contains("<Repetition>"), "{time_trigger}");
        assert!(time_trigger.contains("<StartBoundary>"), "{time_trigger}");
        let logon_trigger =
            &xml[xml.find("<LogonTrigger>").unwrap()..xml.find("</LogonTrigger>").unwrap()];
        assert!(!logon_trigger.contains("<Repetition>"), "{logon_trigger}");
        // Each retry must be a no-op while the hub is healthy, or the repetition
        // would pile up a new server every minute.
        assert!(
            xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"),
            "{xml}"
        );
        assert!(xml.contains("<RestartOnFailure>"), "{xml}");
        // ...bounded, exactly as StartLimitBurst bounds the systemd unit.
        assert!(xml.contains("<Count>5</Count>"), "{xml}");
        // Without this Task Scheduler stops a long-lived server after three days.
        assert!(
            xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"),
            "{xml}"
        );
        // It has to come back after a reboot, which is the whole point.
        assert!(xml.contains("<LogonTrigger>"), "{xml}");
    }

    /// A Scheduled Task cannot carry environment variables, so a product that
    /// gates its own `serve` behind one (semmap does) must still get them — or the
    /// installed task is rejected by that product's gate at every start.
    #[test]
    fn declared_environment_reaches_the_task_through_a_command_wrapper() {
        let service = SERVICE.with_env(&[("PROD_CLI", "1"), ("PROD_MODE", "hub")]);
        let xml = service.task_xml("C:\\bin\\testprod.exe", 7977, "C:\\Users\\x", "DOM\\me");
        assert!(xml.contains("<Command>cmd.exe</Command>"), "{xml}");
        let args = &xml[xml.find("<Arguments>").unwrap()..xml.find("</Arguments>").unwrap()];
        assert!(args.contains("set &quot;PROD_CLI=1&quot;"), "{args}");
        assert!(args.contains("set &quot;PROD_MODE=hub&quot;"), "{args}");
        assert!(
            args.find("PROD_CLI").unwrap() < args.find("PROD_MODE").unwrap(),
            "declaration order must be preserved: {args}"
        );
        assert!(
            args.contains("&amp;&amp;"),
            "the shell operator must be XML-escaped or the document is malformed: {args}"
        );
        assert!(
            args.contains("testprod.exe&quot; serve --port 7977"),
            "{args}"
        );
    }

    /// A product declaring nothing must get no wrapper process at all.
    #[test]
    fn a_task_with_no_environment_invokes_the_binary_directly() {
        let xml = SERVICE.task_xml("C:\\bin\\testprod.exe", 7977, "C:\\Users\\x", "DOM\\me");
        assert!(!xml.contains("cmd.exe"), "{xml}");
    }

    /// A path containing XML metacharacters is legal on Windows and must not be
    /// able to produce a document Task Scheduler rejects or misreads.
    #[test]
    fn awkward_paths_cannot_break_the_task_xml() {
        let xml = SERVICE.task_xml("C:\\a&b\\<prod>.exe", 7977, "C:\\Users\\o'brien", "DOM\\me");
        assert!(xml.contains("C:\\a&amp;b\\&lt;prod&gt;.exe"), "{xml}");
        assert!(xml.contains("o&apos;brien"), "{xml}");
        assert!(
            !xml.contains("<prod>"),
            "raw angle brackets must not survive: {xml}"
        );
    }

    /// A `LogonTrigger` with no `UserId` fires for every account on the machine,
    /// which Windows treats as an administrative registration and refuses with a
    /// bare "Access is denied" on a normal user. Both the trigger and the
    /// principal must name the account, or this backend cannot install at all
    /// without elevation — which was confirmed against `schtasks /Create` on a
    /// non-elevated account before this was written.
    #[test]
    fn the_task_is_scoped_to_one_account_so_it_installs_without_elevation() {
        let xml = SERVICE.task_xml(
            "C:\\bin\\testprod.exe",
            7977,
            "C:\\Users\\x",
            "AzureAD\\dev",
        );
        let triggers = &xml[xml.find("<Triggers>").unwrap()..xml.find("</Triggers>").unwrap()];
        assert!(
            triggers.contains("<UserId>AzureAD\\dev</UserId>"),
            "the trigger must name the account: {triggers}"
        );
        let principals =
            &xml[xml.find("<Principals>").unwrap()..xml.find("</Principals>").unwrap()];
        assert!(
            principals.contains("<UserId>AzureAD\\dev</UserId>"),
            "the principal must name the account: {principals}"
        );
        // LeastPrivilege: a loopback listener has no business running elevated.
        assert!(
            principals.contains("<RunLevel>LeastPrivilege</RunLevel>"),
            "{principals}"
        );
    }

    #[test]
    fn each_product_gets_its_own_task_name() {
        let other = Service::new(Hub::new("otherprod", 7988), "Otherprod MCP hub");
        assert_eq!(SERVICE.task_name(), "testprod-serve");
        assert_eq!(other.task_name(), "otherprod-serve");
        assert_ne!(SERVICE.task_path(), other.task_path());
    }

    #[test]
    fn the_task_xml_is_utf16_with_a_bom_because_schtasks_reads_the_bytes() {
        let bytes = utf16le_with_bom("<Task/>");
        assert_eq!(&bytes[..2], &[0xFF, 0xFE], "byte-order mark first");
        assert_eq!(&bytes[2..4], &[b'<', 0x00], "little-endian UTF-16");
    }

    // ---- macOS LaunchAgent -------------------------------------------------

    #[test]
    fn the_agent_starts_the_running_binary_on_the_requested_port() {
        let plist = SERVICE.plist_contents("/usr/local/bin/testprod", 7977, "/Users/someone");
        assert!(
            plist.contains("<string>/usr/local/bin/testprod</string>"),
            "{plist}"
        );
        assert!(plist.contains("<string>--port</string>"), "{plist}");
        assert!(plist.contains("<string>7977</string>"), "{plist}");
        assert!(plist.contains("<string>/Users/someone</string>"), "{plist}");
        // KeepAlive is launchd's Restart=always; RunAtLoad is what survives reboot.
        assert!(plist.contains("<key>KeepAlive</key>"), "{plist}");
        assert!(plist.contains("<key>RunAtLoad</key>"), "{plist}");
        assert!(plist.contains("<key>ThrottleInterval</key>"), "{plist}");
    }

    #[test]
    fn declared_environment_reaches_the_agent_in_order() {
        let service = SERVICE.with_env(&[("PROD_CLI", "1"), ("PROD_MODE", "hub")]);
        let plist = service.plist_contents("/usr/local/bin/testprod", 7977, "/Users/x");
        assert!(plist.contains("<key>EnvironmentVariables</key>"), "{plist}");
        assert!(plist.contains("<key>PROD_CLI</key>"), "{plist}");
        assert!(plist.contains("<string>hub</string>"), "{plist}");
        assert!(
            plist.find("PROD_CLI").unwrap() < plist.find("PROD_MODE").unwrap(),
            "{plist}"
        );
    }

    #[test]
    fn an_agent_with_no_environment_omits_the_dictionary_entirely() {
        let plist = SERVICE.plist_contents("/usr/local/bin/testprod", 7977, "/Users/x");
        assert!(!plist.contains("EnvironmentVariables"), "{plist}");
    }

    #[test]
    fn awkward_paths_cannot_break_the_plist() {
        let plist = SERVICE.plist_contents("/opt/a&b/<prod>", 7977, "/Users/x");
        assert!(plist.contains("/opt/a&amp;b/&lt;prod&gt;"), "{plist}");
    }

    #[test]
    fn each_product_gets_its_own_agent_label() {
        let other = Service::new(Hub::new("otherprod", 7988), "Otherprod MCP hub");
        assert_eq!(SERVICE.plist_label(), "com.testprod.serve");
        assert_eq!(other.plist_label(), "com.otherprod.serve");
    }

    #[test]
    fn the_agent_lands_in_the_per_user_launchagents_directory() {
        let path = SERVICE
            .plist_path()
            .expect("a home directory in the test environment");
        assert!(
            path.ends_with("Library/LaunchAgents/com.testprod.serve.plist"),
            "{path:?}"
        );
    }

    /// The fragile-path markers are written with forward slashes, so without
    /// normalization the durability warning would be dead on Windows — the one
    /// platform whose backend was added alongside it.
    #[test]
    fn a_windows_build_directory_binary_is_flagged_as_not_durable() {
        assert!(SERVICE
            .durability_warning(Path::new(
                "C:\\Users\\x\\prod\\target\\release\\testprod.exe"
            ))
            .is_some());
        assert!(SERVICE
            .durability_warning(Path::new(
                "C:\\Users\\x\\prod\\.testprod\\worktrees\\SERV-06\\testprod.exe"
            ))
            .is_some());
        assert!(SERVICE
            .durability_warning(Path::new("C:\\Users\\x\\.cargo\\bin\\testprod.exe"))
            .is_none());
    }
}
