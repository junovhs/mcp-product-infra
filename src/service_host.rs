//! Windowless Windows host for a supervised console-subsystem MCP server.
//!
//! Task Scheduler cannot request `CREATE_NO_WINDOW` for an executable action,
//! and its XML has no environment block. The companion binary reads this
//! configuration, creates the real server suspended and windowless, assigns it
//! to a kill-on-close Job Object, and only then resumes it. If Task Scheduler
//! ends the companion, Windows closes the job handle and terminates the whole
//! server tree rather than leaving an orphan behind.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// The complete child-process declaration consumed by `mcp-hub-supervisor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsServiceHostConfig {
    /// Absolute path to the product's console-subsystem executable.
    pub executable: String,
    /// Arguments passed to the product exactly, without a command shell.
    pub arguments: Vec<String>,
    /// Directory inherited by the product process.
    pub working_directory: String,
    /// Product-declared environment overrides added to the current user block.
    pub environment: Vec<(String, String)>,
}

impl WindowsServiceHostConfig {
    /// Read and validate one persisted service declaration.
    pub fn read(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("read service config {}: {error}", path.display()))?;
        let config: Self = serde_json::from_slice(&bytes)
            .map_err(|error| format!("parse service config {}: {error}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Serialize deterministically so an installer upgrade replaces one small,
    /// inspectable declaration rather than embedding shell syntax in task XML.
    pub fn to_json(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
        serde_json::to_vec_pretty(self)
            .map_err(|error| format!("serialize Windows service config: {error}"))
    }

    fn validate(&self) -> Result<(), String> {
        if self.executable.is_empty() {
            return Err("service executable is empty".to_string());
        }
        if self.working_directory.is_empty() {
            return Err("service working directory is empty".to_string());
        }
        for (key, _) in &self.environment {
            if key.is_empty() || key.contains('=') || key.contains('\0') {
                return Err(format!("invalid service environment key {key:?}"));
            }
        }
        if self
            .environment
            .iter()
            .any(|(_, value)| value.contains('\0'))
        {
            return Err("service environment value contains NUL".to_string());
        }
        Ok(())
    }
}

#[cfg(windows)]
mod windows {
    use super::WindowsServiceHostConfig;
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_FAILED};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, GetExitCodeProcess, ResumeThread, TerminateProcess, WaitForSingleObject,
        CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, INFINITE,
        PROCESS_INFORMATION, STARTUPINFOW,
    };

    struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        fn new(handle: HANDLE, operation: &str) -> Result<Self, String> {
            if handle.is_null() {
                Err(last_error(operation))
            } else {
                Ok(Self(handle))
            }
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    /// Run the configured hub until it exits. Killing this host closes the Job
    /// Object in the kernel, which kills the hub even though Rust destructors do
    /// not run when Task Scheduler terminates the host process.
    pub fn run(path: &Path) -> Result<u32, String> {
        let config = WindowsServiceHostConfig::read(path)?;
        let job = OwnedHandle::new(
            unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) },
            "CreateJobObjectW",
        )?;
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            )
        };
        if configured == 0 {
            return Err(last_error("SetInformationJobObject(KILL_ON_JOB_CLOSE)"));
        }

        let application = wide_null(OsStr::new(&config.executable));
        let mut command_line = command_line(&config);
        let current_directory = wide_null(OsStr::new(&config.working_directory));
        let environment = environment_block(&config.environment);
        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let created = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
                environment.as_ptr() as *const _,
                current_directory.as_ptr(),
                &startup,
                &mut process,
            )
        };
        if created == 0 {
            return Err(last_error("CreateProcessW(CREATE_NO_WINDOW)"));
        }
        let process_handle = OwnedHandle::new(process.hProcess, "CreateProcessW process handle")?;
        let thread_handle = OwnedHandle::new(process.hThread, "CreateProcessW thread handle")?;

        if unsafe { AssignProcessToJobObject(job.0, process_handle.0) } == 0 {
            unsafe {
                TerminateProcess(process_handle.0, 1);
            }
            return Err(last_error("AssignProcessToJobObject"));
        }
        if unsafe { ResumeThread(thread_handle.0) } == u32::MAX {
            unsafe {
                TerminateProcess(process_handle.0, 1);
            }
            return Err(last_error("ResumeThread"));
        }

        if unsafe { WaitForSingleObject(process_handle.0, INFINITE) } == WAIT_FAILED {
            return Err(last_error("WaitForSingleObject"));
        }
        let mut exit_code = 1;
        if unsafe { GetExitCodeProcess(process_handle.0, &mut exit_code) } == 0 {
            return Err(last_error("GetExitCodeProcess"));
        }
        Ok(exit_code)
    }

    fn command_line(config: &WindowsServiceHostConfig) -> Vec<u16> {
        let mut command = quote_windows_argument(&config.executable);
        for argument in &config.arguments {
            command.push(' ');
            command.push_str(&quote_windows_argument(argument));
        }
        wide_null(OsStr::new(&command))
    }

    fn quote_windows_argument(argument: &str) -> String {
        if !argument.is_empty()
            && !argument
                .chars()
                .any(|character| character.is_whitespace() || character == '"')
        {
            return argument.to_string();
        }
        let mut quoted = String::from("\"");
        let mut backslashes = 0;
        for character in argument.chars() {
            if character == '\\' {
                backslashes += 1;
            } else if character == '"' {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            } else {
                quoted.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                quoted.push(character);
            }
        }
        quoted.push_str(&"\\".repeat(backslashes * 2));
        quoted.push('"');
        quoted
    }

    fn environment_block(overrides: &[(String, String)]) -> Vec<u16> {
        let mut values: Vec<(OsString, OsString)> = std::env::vars_os().collect();
        for (key, value) in overrides {
            values.retain(|(existing, _)| !existing.to_string_lossy().eq_ignore_ascii_case(key));
            values.push((OsString::from(key), OsString::from(value)));
        }
        values.sort_by(|(left, _), (right, _)| {
            left.to_string_lossy()
                .to_ascii_lowercase()
                .cmp(&right.to_string_lossy().to_ascii_lowercase())
        });
        let mut block = Vec::new();
        for (key, value) in values {
            block.extend(key.encode_wide());
            block.push('=' as u16);
            block.extend(value.encode_wide());
            block.push(0);
        }
        block.push(0);
        block
    }

    fn wide_null(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn last_error(operation: &str) -> String {
        format!("{operation}: {}", io::Error::last_os_error())
    }

    #[cfg(test)]
    mod tests {
        use super::quote_windows_argument;

        #[test]
        fn windows_arguments_preserve_spaces_quotes_and_trailing_slashes() {
            assert_eq!(quote_windows_argument("plain"), "plain");
            assert_eq!(quote_windows_argument("two words"), "\"two words\"");
            assert_eq!(quote_windows_argument(""), "\"\"");
            assert_eq!(quote_windows_argument("a\\\"b"), "\"a\\\\\\\"b\"");
            assert_eq!(
                quote_windows_argument("C:\\dir with space\\"),
                "\"C:\\dir with space\\\\\""
            );
        }
    }
}

/// Run one persisted service configuration on Windows.
#[cfg(windows)]
pub fn run_windows_service_host(path: &Path) -> Result<u32, String> {
    windows::run(path)
}

/// The companion is Windows-only; other targets keep a buildable binary so
/// workspace-wide checks do not need target-specific package selection.
#[cfg(not(windows))]
pub fn run_windows_service_host(_path: &Path) -> Result<u32, String> {
    Err("mcp-hub-supervisor is supported only on Windows".to_string())
}

#[cfg(test)]
mod tests {
    use super::WindowsServiceHostConfig;

    #[test]
    fn config_round_trips_environment_without_shell_escaping() {
        let config = WindowsServiceHostConfig {
            executable: "C:\\Program Files\\Product & Co\\product.exe".to_string(),
            arguments: vec![
                "serve".to_string(),
                "--port".to_string(),
                "7977".to_string(),
            ],
            working_directory: "C:\\Users\\someone".to_string(),
            environment: vec![("PROD_MODE".to_string(), "hub & ready".to_string())],
        };
        let bytes = config.to_json().unwrap();
        let decoded: WindowsServiceHostConfig = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded, config);
        assert!(!String::from_utf8(bytes).unwrap().contains("cmd.exe"));
    }

    #[test]
    fn config_rejects_keys_that_cannot_form_an_environment_entry() {
        let config = WindowsServiceHostConfig {
            executable: "product.exe".to_string(),
            arguments: vec![],
            working_directory: "C:\\work".to_string(),
            environment: vec![("BAD=KEY".to_string(), "value".to_string())],
        };
        assert!(config.to_json().unwrap_err().contains("environment key"));
    }
}
