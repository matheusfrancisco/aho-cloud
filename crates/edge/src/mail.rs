//! Magic-link delivery. The edge only needs "send this link to this address"; [`SmtpMailer`]
//! is the real adapter (lettre, pooled, STARTTLS/TLS per the SMTP URL).

use futures::future::BoxFuture;
use lettre::message::Mailbox;
use lettre::message::header::ContentType;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

/// One magic-link email.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MagicLinkMail {
    pub to: String,
    /// Hostname the link signs in to (shown to the user).
    pub host: String,
    pub link: String,
    pub expires_in_secs: u64,
}

impl MagicLinkMail {
    /// Plain-text body (short lines, so it travels as 7-bit text).
    pub fn text(&self) -> String {
        format!(
            "Sign in to {host}:\n\n{link}\n\nThe link works once and expires in {mins} minutes.\n\
             If you did not ask to sign in, ignore this email.\n",
            host = self.host,
            link = self.link,
            mins = self.expires_in_secs / 60,
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MailError {
    #[error("invalid address `{0}`")]
    Address(String),
    #[error("building message: {0}")]
    Build(String),
    #[error("smtp: {0}")]
    Transport(String),
}

/// Sends magic links.
pub trait Mailer: Send + Sync {
    fn send_magic_link<'a>(&'a self, mail: &'a MagicLinkMail) -> BoxFuture<'a, Result<(), MailError>>;
}

/// SMTP delivery via lettre.
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl SmtpMailer {
    /// `url` as lettre parses it: `smtps://user:pass@host:465`, `smtp://host:587?tls=required`,
    /// `smtp://host:25` (plaintext, local relays only). `from` is `Name <addr>` or `addr`.
    pub fn from_url(url: &str, from: &str) -> Result<Self, MailError> {
        let from: Mailbox = from.parse().map_err(|_| MailError::Address(from.to_owned()))?;
        let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(url).map_err(|e| MailError::Transport(e.to_string()))?.build();
        Ok(Self { transport, from })
    }
}

impl Mailer for SmtpMailer {
    fn send_magic_link<'a>(&'a self, mail: &'a MagicLinkMail) -> BoxFuture<'a, Result<(), MailError>> {
        Box::pin(async move {
            let to: Mailbox = mail.to.parse().map_err(|_| MailError::Address(mail.to.clone()))?;
            let message = Message::builder()
                .from(self.from.clone())
                .to(to)
                .subject(format!("Sign in to {}", mail.host))
                .header(ContentType::TEXT_PLAIN)
                .body(mail.text())
                .map_err(|e| MailError::Build(e.to_string()))?;
            self.transport.send(message).await.map_err(|e| MailError::Transport(e.to_string()))?;
            Ok(())
        })
    }
}
