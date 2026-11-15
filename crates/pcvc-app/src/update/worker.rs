//! Checks and installs on a background thread, one job at a time, with
//! results posted to the inbox.

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::mpsc;

use minisign_verify::PublicKey;

use super::download::{self, Progress};
use super::http::{Client, Policy};
use super::release::{self, CheckError, CheckOutcome, Endpoints, InstallKind, Release};
use super::verify::VerifyError;
use super::version::{self, ReleaseVersion};
use crate::inbox::{Inbound, Inbox};

#[derive(Debug, Clone)]
pub enum Event {
    Checked {
        manual: bool,
        result: Result<CheckOutcome, CheckError>,
    },
    Progress(Progress),
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
}

#[derive(Debug, Clone)]
pub struct Worker(mpsc::Sender<Job>);

impl Worker {
    pub fn spawn(context: Context, inbox: Inbox) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("updates".into())
            .spawn(move || {
                if let Some(dir) = &context.updates_dir {
                    download::clear(dir);
                }
                for job in rx {
                    let send = |event| inbox.send(Inbound::Update(event));
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
                            if let Err(e) = install(&context, release, &send) {
                                send(Event::InstallFailed(e));
                            }
                        }
                    }));
                    if ran.is_err() {
                        tracing::error!("the update worker panicked");
                        let event = match job {
                            Job::Check { manual } => Event::Checked {
                                manual,
                                result: Err(CheckError::Malformed("internal error".into())),
                            },
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
        Self(tx)
    }

    /// Returns false if the worker is not running.
    pub fn check(&self, manual: bool) -> bool {
        self.0.send(Job::Check { manual }).is_ok()
    }

    pub fn install(&self, release: Release) -> bool {
        self.0.send(Job::Install(Box::new(release))).is_ok()
    }
}

/// Downloads, verifies and starts the installer, then waits for it. The
/// installer stops this process when it gets to replacing the files, so
/// returning at all means it ended without installing.
fn install(
    context: &Context,
    release: &Release,
    send: &dyn Fn(Event) -> bool,
) -> Result<(), String> {
    if !context.kind.installs() {
        return Err("this copy of the app cannot install updates".into());
    }
    let key = context.key.as_ref().map_err(ToString::to_string)?;
    let dir = context
        .updates_dir
        .as_ref()
        .ok_or("there is no folder to download to")?;
    let verified =
        download::download_and_verify(&context.client, release, key.as_ref(), dir, &mut |p| {
            send(Event::Progress(p));
        })
        .map_err(|e| e.to_string())?;
    tracing::info!(
        "verified {} ({})",
        verified.path.display(),
        if verified.signed {
            "checksum and signature"
        } else {
            "checksum"
        }
    );
    let mut child = super::launch::start(&verified.path, context.kind)
        .map_err(|e| format!("could not start the installer: {e}"))?;
    send(Event::Launched);
    let status = child.wait().map_err(|e| e.to_string())?;
    drop(verified);
    Err(match status.code() {
        Some(code) => format!("the installer ended without updating (exit code {code})"),
        None => "the installer ended without updating".to_owned(),
    })
}
