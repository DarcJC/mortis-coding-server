//! Tracing setup: a layered subscriber writing to stdout and, optionally, to a
//! plain-text log file.
//!
//! - **stdout** always gets a layer; ANSI colors are enabled only when stdout
//!   is an interactive terminal (so redirected/piped output and supervisor's
//!   captured `stdout.log` stay free of escape codes).
//! - **file** (when `[server].log_file` is set) gets a second, never-colored
//!   layer via a non-blocking writer. No rotation — a single stable path that
//!   operators can `tail`/retrieve. The returned [`WorkerGuard`] must be held
//!   for the program's lifetime so buffered lines flush on shutdown.
//!
//! Filter precedence: explicit `log_level` > `RUST_LOG` env var > `"info"`.

use std::io::IsTerminal;

use camino::Utf8Path;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::{
    EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt,
};

/// Resolve the env filter: explicit `log_level` > `RUST_LOG` > `"info"`.
fn filter(log_level: Option<&str>) -> EnvFilter {
    match log_level {
        Some(l) => EnvFilter::new(l),
        None => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
    }
}

/// Build a non-blocking writer for `path` (creating its parent directory),
/// returning the writer and its flush guard.
fn file_writer(path: &Utf8Path) -> anyhow::Result<(NonBlocking, WorkerGuard)> {
    let dir = path
        .parent()
        .filter(|p| !p.as_str().is_empty())
        .unwrap_or_else(|| Utf8Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("log_file has no file name: {path}"))?;
    std::fs::create_dir_all(dir)?;
    // `never` = a single file at `dir/name`, no date-stamped rotation.
    let appender = tracing_appender::rolling::never(dir.as_std_path(), name);
    Ok(tracing_appender::non_blocking(appender))
}

/// Initialize the global tracing subscriber. Returns a [`WorkerGuard`] when a
/// file sink is configured (hold it for the whole program lifetime).
pub fn init(
    log_file: Option<&Utf8Path>,
    log_level: Option<&str>,
) -> anyhow::Result<Option<WorkerGuard>> {
    let stdout_layer = fmt::layer()
        .with_ansi(std::io::stdout().is_terminal())
        .with_writer(std::io::stdout);

    match log_file {
        Some(path) => {
            let (writer, guard) = file_writer(path)?;
            let file_layer = fmt::layer().with_ansi(false).with_writer(writer);
            tracing_subscriber::registry()
                .with(filter(log_level))
                .with(stdout_layer)
                .with(file_layer)
                .init();
            Ok(Some(guard))
        }
        None => {
            tracing_subscriber::registry()
                .with(filter(log_level))
                .with(stdout_layer)
                .init();
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn file_sink_writes_plain_text() {
        let tmp = tempfile::tempdir().unwrap();
        let path = camino::Utf8PathBuf::from_path_buf(tmp.path().join("app.log")).unwrap();

        let (writer, guard) = file_writer(&path).unwrap();
        // Scoped subscriber (not global) so the test doesn't fight `init`'s
        // one-shot global default.
        let subscriber = tracing_subscriber::registry()
            .with(fmt::layer().with_ansi(false).with_writer(writer));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(answer = 42, "hello-from-test");
        });
        drop(guard); // block until the non-blocking writer flushes

        let content = std::fs::read_to_string(path.as_std_path()).unwrap();
        assert!(content.contains("hello-from-test"), "event must be written: {content:?}");
        assert!(
            !content.contains('\u{1b}'),
            "log file must not contain ANSI escape codes: {content:?}"
        );
    }
}
