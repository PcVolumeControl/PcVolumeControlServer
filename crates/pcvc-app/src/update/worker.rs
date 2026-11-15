//! Checks and installs on a background thread, one job at a time, with
//! results posted to the inbox.

use std::cell::Cell;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::process::{Child, ExitStatus};
use std::sync::mpsc;
use std::time::Duration;

use minisign_verify::PublicKey;

use super::download::{self, Progress};
use super::http::{Client, Policy};
use super::release::{self, CheckError, CheckOutcome, Endpoints, InstallKind, Release};
use super::verify::VerifyError;
use super::version::{self, ReleaseVersion};
use crate::inbox::{Inbound, Inbox};

/// Longest wait for the app to step aside before the installer starts
/// anyway.
const HAND_OVER_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub enum Event {
    Checked {
        manual: bool,
        result: Result<CheckOutcome, CheckError>,
    },
    Progress(Progress),
    /// The installer is verified and about to run. The app steps aside
    /// (see [`Worker::proceed`]) before it starts.
    Verified,
    /// The installer is running. It stops this process to replace it, and
    /// starts the new version afterwards.
    Launched,
    InstallFailed(String),
}

enum Job {
    Check { manual: bool },
    Install(Box<Release>),
}

/// Everything the worker needs, fixed at startup.
pub struct Context {
    pub client: Client,
    pub endpoints: Endpoints,
    pub current: ReleaseVersion,
    pub kind: InstallKind,
    pub key: Result<Option<PublicKey>, VerifyError>,
    /// Where installers are downloaded; `None` without a home directory.
    pub updates_dir: Option<PathBuf>,
}

impl Context {
    pub fn github(kind: InstallKind, updates_dir: Option<PathBuf>) -> Self {
        Self {
            client: Client::new(Policy::github()),
            endpoints: Endpoints::github(),
            current: version::current(),
            kind,
            key: super::verify::signing_key(),
            updates_dir,
        }
    }

    /// Whether installs need a signature; a broken built-in key counts, so
    /// installs fail rather than skip it.
    pub fn signature_required(&self) -> bool {
        !matches!(self.key, Ok(None))
    }
}

#[derive(Debug, Clone)]
pub struct Worker {
    jobs: mpsc::Sender<Job>,
    go: mpsc::Sender<bool>,
}

impl Worker {
    pub fn spawn(context: Context, inbox: Inbox) -> Self {
        let (jobs, rx) = mpsc::channel::<Job>();
        let (go, go_rx) = mpsc::channel::<bool>();
        let spawned = std::thread::Builder::new()
            .name("updates".into())
            .spawn(move || {
                for job in std::iter::once(None).chain(rx.into_iter().map(Some)) {
                    // Also after starting, as the installer that started
                    // this process may still have held its file.
                    if let Some(dir) = &context.updates_dir {
                        download::clear(dir);
                    }
                    let Some(job) = job else { continue };
                    let send = |event| inbox.send(Inbound::Update(event));
                    let launched = Cell::new(false);
                    let ran = std::panic::catch_unwind(AssertUnwindSafe(|| match &job {
                        Job::Check { manual } => {
                            let result = release::check(
                                &context.client,
                                &context.endpoints,
                                &context.current,
                                context.kind,
                            );
                            send(Event::Checked {
                                manual: *manual,
                                result,
                            });
                        }
                        Job::Install(release) => {
                            install(&context, release, &go_rx, &send, &launched);
                        }
                    }));
                    if ran.is_err() {
                        tracing::error!("the update worker panicked");
                        let event = match job {
                            Job::Check { manual } => Event::Checked {
                                manual,
                                result: Err(CheckError::Malformed("internal error".into())),
                            },
                            // A running installer still stops the app.
                            Job::Install(_) if launched.get() => continue,
                            Job::Install(_) => Event::InstallFailed("internal error".into()),
                        };
                        if !send(event) {
                            return;
                        }
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::error!("cannot start the update thread: {e}");
        }
        Self { jobs, go }
    }

    /// Returns false if the worker is not running.
    pub fn check(&self, manual: bool) -> bool {
        self.jobs.send(Job::Check { manual }).is_ok()
    }

    pub fn install(&self, release: Release) -> bool {
        self.jobs.send(Job::Install(Box::new(release))).is_ok()
    }

    /// Answers [`Event::Verified`]: `true` once the app has stepped aside,
    /// `false` to abandon the install.
    pub fn proceed(&self, go: bool) {
        let _ = self.go.send(go);
    }
}

/// Downloads and verifies the installer, waits for the app to step aside,
/// starts the installer and waits for it. The installer stops this process
/// when it gets to replacing the files, so its ending at all means it
/// ended without installing.
fn install(
    context: &Context,
    release: &Release,
    go: &mpsc::Receiver<bool>,
    send: &dyn Fn(Event) -> bool,
    launched: &Cell<bool>,
) {
    let fail = |message: String| {
        send(Event::InstallFailed(message));
    };
    if !context.kind.installs() {
        return fail("this copy of the app cannot install updates".into());
    }
    let key = match &context.key {
        Ok(key) => key,
        Err(e) => return fail(e.to_string()),
    };
    let Some(dir) = &context.updates_dir else {
        return fail("there is no folder to download to".into());
    };
    let verified =
        match download::download_and_verify(&context.client, release, key.as_ref(), dir, &mut |p| {
            send(Event::Progress(p));
        }) {
            Ok(verified) => verified,
            Err(e) => return fail(e.to_string()),
        };
    tracing::info!(
        "verified {} ({})",
        verified.path.display(),
        if verified.signed {
            "checksum and signature"
        } else {
            "checksum"
        }
    );

    while go.try_recv().is_ok() {}
    send(Event::Verified);
    match go.recv_timeout(HAND_OVER_TIMEOUT) {
        Ok(true) => {}
        Ok(false) | Err(mpsc::RecvTimeoutError::Disconnected) => {
            return fail("the install was cancelled".into());
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            tracing::warn!("the app did not step aside in time; starting the installer anyway");
        }
    }
    let mut child = match super::launch::start(&verified.path, context.kind) {
        Ok(child) => child,
        Err(e) => return fail(format!("could not start the installer: {e}")),
    };
    launched.set(true);
    send(Event::Launched);
    let status = wait(&mut child);
    drop(verified);
    fail(match status.code() {
        Some(code) => format!("the installer ended without updating (exit code {code})"),
        None => "the installer ended without updating".to_owned(),
    });
}

/// Waits for the installer however long it takes: while it runs the app
/// must stay aside, and the installer ends the app itself.
fn wait(child: &mut Child) -> ExitStatus {
    loop {
        match child.wait() {
            Ok(status) => return status,
            Err(e) => {
                tracing::warn!("waiting for the installer failed: {e}");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}
