use std::io::Write;
use std::process::{Command, Stdio};

use crate::application::{WebActionOutput, WebActionService};
use crate::error::Result;
use crate::platform::PlatformFuture;

#[derive(Debug, Clone, Copy, Default)]
pub struct NativeWebActionService;

impl NativeWebActionService {
    fn run<'a>(
        &self,
        args: Vec<String>,
        stdin: Option<String>,
    ) -> PlatformFuture<'a, Result<WebActionOutput>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || run_command(&args, stdin.as_deref()))
                .await
                .map_err(|error| crate::error::NarouError::Platform(format!("web action task failed: {error}")))?
        })
    }
}

impl WebActionService for NativeWebActionService {
    fn inspect<'a>(&'a self, targets: &'a [String]) -> PlatformFuture<'a, Result<WebActionOutput>> {
        self.run(std::iter::once("inspect".to_string()).chain(targets.iter().cloned()).collect(), None)
    }

    fn folder<'a>(&'a self, targets: &'a [String]) -> PlatformFuture<'a, Result<WebActionOutput>> {
        self.run(std::iter::once("folder".to_string()).chain(targets.iter().cloned()).collect(), None)
    }

    fn setting_burn<'a>(&'a self, targets: &'a [String]) -> PlatformFuture<'a, Result<WebActionOutput>> {
        self.run(
            std::iter::once("setting".to_string())
                .chain(std::iter::once("--burn".to_string()))
                .chain(targets.iter().cloned())
                .collect(),
            None,
        )
    }

    fn diff<'a>(&'a self, target: &'a str, number: &'a str) -> PlatformFuture<'a, Result<WebActionOutput>> {
        self.run(
            vec![
                "diff".to_string(),
                "--no-tool".to_string(),
                target.to_string(),
                "--number".to_string(),
                number.to_string(),
            ],
            None,
        )
    }

    fn diff_clean<'a>(&'a self, target: &'a str) -> PlatformFuture<'a, Result<WebActionOutput>> {
        self.run(vec!["diff".to_string(), "--clean".to_string(), target.to_string()], None)
    }

    fn diff_restore<'a>(&'a self, target: &'a str, version: i64) -> PlatformFuture<'a, Result<WebActionOutput>> {
        self.run(
            vec![
                "diff".to_string(),
                "--restore".to_string(),
                version.to_string(),
                target.to_string(),
            ],
            None,
        )
    }

    fn diff_merge<'a>(&'a self, target: &'a str, version: i64, sections: Option<&'a str>) -> PlatformFuture<'a, Result<WebActionOutput>> {
        let mut args = vec![
            "diff".to_string(),
            "--merge-from".to_string(),
            version.to_string(),
        ];
        if let Some(sections) = sections {
            args.push("--merge-sections".to_string());
            args.push(sections.to_string());
        }
        args.push(target.to_string());
        self.run(args, None)
    }

    fn csv_import<'a>(&'a self, csv: &'a str) -> PlatformFuture<'a, Result<WebActionOutput>> {
        self.run(vec!["csv".to_string(), "--import".to_string(), "-".to_string()], Some(csv.to_string()))
    }

    fn csv_download(&self) -> PlatformFuture<'_, Result<WebActionOutput>> {
        self.run(vec!["csv".to_string()], None)
    }
}

fn run_command(args: &[String], stdin: Option<&str>) -> Result<WebActionOutput> {
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .args(args)
        .current_dir(std::env::current_dir()?)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::compat::configure_web_subprocess_command(&mut command);
    let mut child = command.spawn()?;
    if let Some(input) = stdin {
        if let Some(child_stdin) = child.stdin.as_mut() {
            child_stdin.write_all(input.as_bytes())?;
        }
        drop(child.stdin.take());
    }
    let output = child.wait_with_output()?;
    Ok(WebActionOutput {
        success: output.status.success(),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}
