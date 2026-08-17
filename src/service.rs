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
}

impl Service {
    /// Declare a product's service. `description` becomes the unit's
    /// `Description=` line, which is what `systemctl --user status` shows.
    pub const fn new(hub: Hub, description: &'static str) -> Self {
        Self { hub, description }
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
        let text = exe.display().to_string();
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

    /// Materialize the unit, reload systemd, and enable + start it now.
    pub fn install(&self, port: u16) -> Result<ServiceOutcome, String> {
        if let Some(reason) = self.systemd_user_available() {
            return Ok(ServiceOutcome::Unsupported(reason));
        }
        let name = self.hub.name();
        let exe = std::env::current_exe()
            .map_err(|error| format!("cannot resolve the running {name} binary: {error}"))?;
        let working_dir = directories::BaseDirs::new()
            .map(|dirs| dirs.home_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("/"));
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
            warning: self.durability_warning(&exe),
        })
    }

    /// Stop, disable, and remove the unit.
    pub fn uninstall(&self) -> Result<ServiceOutcome, String> {
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

    /// The description is the product's, not a generic one: it is what the user
    /// sees in `systemctl --user status`.
    #[test]
    fn the_products_description_reaches_the_unit() {
        assert!(SERVICE
            .unit_contents("/usr/bin/testprod", 7977, "/home/x")
            .contains("Description=Testprod MCP hub (loopback HTTP)"));
    }
}
