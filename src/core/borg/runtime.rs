use super::{process, BorgEnvironment, Error, Operation, Result};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

pub struct Limits {
    pub timeout: Duration,
    pub listing_bytes: usize,
    pub extraction_bytes: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            listing_bytes: 16 * 1024 * 1024,
            extraction_bytes: 1024 * 1024 * 1024,
        }
    }
}

/// Fixed executable and closed environment/operations. No executable setter.
/// Output is provisional until successful return (including final revalidation).
/// The callback must return promptly; arbitrary blocking caller code cannot be
/// preempted by this synchronous API. Parser failure must return Err.
pub struct Runtime;
impl Runtime {
    pub fn list(
        &self,
        operation: &Operation<'_>,
        environment: &BorgEnvironment<'_>,
        limits: &Limits,
        cancel: &Cancellation,
    ) -> Result<Vec<u8>> {
        if operation.is_extract() {
            return Err(Error::InvalidInput);
        }
        let mut bytes = Vec::new();
        self.execute(operation, environment, limits, cancel, &mut |chunk| {
            bytes.extend_from_slice(chunk);
            Ok(())
        })?;
        Ok(bytes)
    }

    /// Streams one selected file; never returns or buffers the entire plaintext.
    pub fn extract(
        &self,
        operation: &Operation<'_>,
        environment: &BorgEnvironment<'_>,
        limits: &Limits,
        cancel: &Cancellation,
        mut consume: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        if !operation.is_extract() {
            return Err(Error::InvalidInput);
        }
        self.execute(operation, environment, limits, cancel, &mut consume)
    }

    fn execute(
        &self,
        op: &Operation<'_>,
        env: &BorgEnvironment<'_>,
        limits: &Limits,
        cancel: &Cancellation,
        consume: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        let started = Instant::now();
        op.snapshot.revalidate()?;
        op.snapshot.child_policy_ready()?;
        env.state.validate_source(op.snapshot.pin())?;
        // Probe using the same finite env, pin, descriptor and child lifecycle.
        // Probe never opens a repository; no general command API is exported.
        self.profile(op, env, limits, cancel, started)?;
        op.snapshot.revalidate()?;
        let remaining = remaining(limits, started)?;
        let result = process::run(
            process::Invocation {
                executable: Path::new("/usr/bin/borg"),
                argv: &op.argv(),
                pin: op.snapshot.pin(),
            },
            env,
            &remaining,
            cancel,
            if op.is_extract() {
                limits.extraction_bytes
            } else {
                limits.listing_bytes as u64
            },
            consume,
        );
        let post = op
            .snapshot
            .revalidate()
            .and_then(|_| env.state.validate_source(op.snapshot.pin()));
        result.and(post)
    }

    fn profile(
        &self,
        op: &Operation<'_>,
        env: &BorgEnvironment<'_>,
        limits: &Limits,
        cancel: &Cancellation,
        started: Instant,
    ) -> Result<()> {
        for (args, required) in [
            (vec!["--version"], vec!["borg 1.4.4\n"]),
            (
                vec!["list", "--help"],
                vec![
                    "--json",
                    "--json-lines",
                    "--consider-part-files",
                    "--bypass-lock",
                    "--format",
                ],
            ),
            (
                vec!["extract", "--help"],
                vec!["--stdout", "--consider-part-files", "--bypass-lock"],
            ),
        ] {
            let mut bytes = Vec::new();
            op.snapshot.revalidate()?;
            let remaining = remaining(limits, started)?;
            process::run(
                process::Invocation {
                    executable: Path::new("/usr/bin/borg"),
                    argv: &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                    pin: op.snapshot.pin(),
                },
                env,
                &remaining,
                cancel,
                128 * 1024,
                &mut |chunk| {
                    bytes.extend_from_slice(chunk);
                    Ok(())
                },
            )?;
            op.snapshot.revalidate()?;
            let text = std::str::from_utf8(&bytes).map_err(|_| Error::UnsupportedBorg)?;
            if (args == ["--version"] && text != "borg 1.4.4\n")
                || required.iter().any(|s| !text.contains(s))
            {
                return Err(Error::UnsupportedBorg);
            }
        }
        Ok(())
    }
}

fn remaining(limits: &Limits, started: Instant) -> Result<Limits> {
    let timeout = limits
        .timeout
        .checked_sub(started.elapsed())
        .filter(|v| !v.is_zero())
        .ok_or(Error::Timeout)?;
    Ok(Limits {
        timeout,
        listing_bytes: limits.listing_bytes,
        extraction_bytes: limits.extraction_bytes,
    })
}
