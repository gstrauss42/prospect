//! Outgoing email on its own thread, so a slow mail provider never holds up a request or a run.
//! Every attempt is logged in the `emails` table; a failed email is logged and forgotten — it
//! never fails anything else.

use std::sync::Mutex;
use std::sync::mpsc::{Sender, channel};

use crate::util::lock;
use crate::{app, db, util};

pub struct Job {
    pub user_id: Option<i64>,
    /// "welcome", "digest" or "reset".
    pub kind: &'static str,
    pub email: mailer::Email,
}

pub struct Queue {
    tx: Mutex<Option<Sender<Job>>>,
}

impl Queue {
    /// Start the mail thread.
    pub fn start() -> Queue {
        let (tx, rx) = channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("mail".into())
            .spawn(move || {
                for job in rx {
                    deliver(job);
                }
            });
        match spawned {
            Ok(_) => Queue {
                tx: Mutex::new(Some(tx)),
            },
            Err(e) => {
                error!("couldn't start the mail thread, emails are off: {e}");
                Queue {
                    tx: Mutex::new(None),
                }
            }
        }
    }

    pub fn send(&self, job: Job) {
        let sent = lock(&self.tx)
            .as_ref()
            .map(|tx| tx.send(job).is_ok())
            .unwrap_or(false);
        if !sent {
            warn!("an email couldn't be queued");
        }
    }
}

fn deliver(job: Job) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        app().mailer.send(&job.email)
    }));
    let (delivery, error) = match result {
        Ok(Ok(mailer::Delivery::Mailgun { id })) => (Some(format!("mailgun:{id}")), None),
        Ok(Ok(mailer::Delivery::Outbox { path })) => (
            Some(format!(
                "outbox:{}",
                path.file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default()
            )),
            None,
        ),
        Ok(Err(e)) => (None, Some(e.to_string())),
        Err(_) => (None, Some("the mailer panicked".to_string())),
    };
    match &error {
        None => info!(
            "email {} sent ({})",
            job.kind,
            delivery.as_deref().unwrap_or("")
        ),
        Some(e) => warn!("email {} failed: {e}", job.kind),
    }
    let logged = db::conn().and_then(|c| {
        c.execute(
            "INSERT INTO emails(user_id, kind, to_addr, subject, delivery, error, created_at) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (job.user_id, job.kind, &job.email.to, &job.email.subject, delivery, error, util::now()),
        )
    });
    if let Err(e) = logged {
        // The account may have been deleted in the meantime (foreign key); nothing to do.
        debug!("couldn't log an email: {e}");
    }
}
