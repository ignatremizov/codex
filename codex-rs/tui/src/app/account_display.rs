//! Rehydrate email omitted from account-update notifications without blocking the UI.
//!
//! A notification immediately clears the previous identity. Each subsequent account read is
//! correlated with that boundary so a slow old read cannot restore another account's email.

use super::App;
use crate::app_event::AppEvent;
use crate::app_server_session::AppServerSession;
use codex_app_server_protocol::Account;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::GetAccountParams;
use codex_app_server_protocol::GetAccountResponse;
use codex_app_server_protocol::RequestId;
use std::time::Duration;
use uuid::Uuid;

impl App {
    pub(super) fn refresh_account_email(&mut self, session: &AppServerSession) {
        self.account_email_request_id = None;
        if !self.chat_widget.has_chatgpt_account() {
            return;
        }
        let request_id = Uuid::new_v4();
        self.account_email_request_id = Some(request_id);
        let handle = session.request_handle();
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_secs(/*secs*/ 10),
                handle.request_typed::<GetAccountResponse>(ClientRequest::GetAccount {
                    request_id: RequestId::String(format!("status-account-{request_id}")),
                    params: GetAccountParams {
                        refresh_token: false,
                    },
                }),
            )
            .await;
            let email = match result {
                Ok(Ok(response)) => match response.account {
                    Some(Account::Chatgpt { email, .. }) => email,
                    Some(Account::ApiKey {} | Account::AmazonBedrock { .. }) | None => None,
                },
                Ok(Err(error)) => {
                    tracing::debug!(%error, "could not refresh status account email");
                    None
                }
                Err(_) => {
                    tracing::debug!("status account email refresh timed out");
                    None
                }
            };
            tx.send(AppEvent::AccountEmailLoaded { request_id, email });
        });
    }
}
