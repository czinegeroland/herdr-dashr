//! Running the commands pull sources are made of.

use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// The argv for a user-given command: through `cmd /c` on Windows, where
/// `aws`, `az`, `gcloud` and `npm` are `.cmd` files a bare spawn misses.
pub fn shell_argv(command: &[String]) -> Vec<String> {
    if cfg!(windows) {
        ["cmd".to_owned(), "/c".to_owned()]
            .into_iter()
            .chain(command.iter().cloned())
            .collect()
    } else {
        command.to_vec()
    }
}

/// What a finished command printed.
#[derive(Debug)]
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Runs `argv` with extra environment to completion within `timeout`.
/// A failure carries the last line of stderr.
pub fn run(argv: &[String], env: &[(String, String)], timeout: Duration) -> Result<Output, String> {
    let (program, args) = argv.split_first().ok_or("empty command")?;
    let mut child = Command::new(program)
        .args(args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{program}: {error}"))?;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out = thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(pipe) = stdout.as_mut() {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    });
    let err = thread::spawn(move || {
        let mut text = String::new();
        if let Some(pipe) = stderr.as_mut() {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program} took longer than {}s", timeout.as_secs()));
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(error) => return Err(error.to_string()),
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if status.success() {
        Ok(Output { stdout, stderr })
    } else {
        Err(format!(
            "{program} exited with {status}: {}",
            last_line(&stderr)
        ))
    }
}

/// The last non-empty line, at most 300 characters.
pub fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .chars()
        .take(300)
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn runs_with_environment_and_reports_failures() {
        let argv: Vec<String> = ["sh", "-c", "printf %s \"$X\"; echo oops >&2"]
            .map(String::from)
            .to_vec();
        let out = run(&argv, &[("X".into(), "hi".into())], Duration::from_secs(5)).unwrap();
        assert_eq!(out.stdout, b"hi");
        assert_eq!(out.stderr.trim(), "oops");
        let failing: Vec<String> = ["sh", "-c", "echo first >&2; echo why >&2; exit 3"]
            .map(String::from)
            .to_vec();
        assert!(
            run(&failing, &[], Duration::from_secs(5))
                .unwrap_err()
                .ends_with("why")
        );
        let slow: Vec<String> = ["sleep", "5"].map(String::from).to_vec();
        assert!(
            run(&slow, &[], Duration::from_millis(200))
                .unwrap_err()
                .contains("longer")
        );
        assert!(run(&["no-such-program-xyz".into()], &[], Duration::from_secs(1)).is_err());
    }
}
