use std::collections::HashSet;

use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::StateRuntime;
use crate::AgentSendMode;
use crate::AgentSendScope;
use crate::AgentSendSetting;

fn setting_from_row(scope: AgentSendScope, row: &SqliteRow) -> anyhow::Result<AgentSendSetting> {
    let mode = match row.try_get::<&str, _>("mode")? {
        "enabled" => AgentSendMode::Enabled,
        "disabled" => AgentSendMode::Disabled,
        _ => anyhow::bail!("invalid stored agent send setting"),
    };
    Ok(AgentSendSetting {
        scope,
        mode,
        revision: row.try_get("revision")?,
    })
}

impl StateRuntime {
    /// Reads authoritative settings in first-requested scope order from one snapshot.
    ///
    /// Missing scopes are omitted, not interpreted as disabled. Repeated scopes appear once.
    /// This does not resolve ancestry or make authorization decisions.
    pub async fn read_agent_send_settings(
        &self,
        scopes: Vec<AgentSendScope>,
    ) -> anyhow::Result<Vec<AgentSendSetting>> {
        let mut tx = self.pool.begin().await?;
        let mut seen = HashSet::new();
        let mut settings = Vec::new();
        for scope in scopes {
            if !seen.insert(scope) {
                continue;
            }
            let row = match scope {
                AgentSendScope::Directed {
                    sender_thread_id,
                    receiver_thread_id,
                } => {
                    sqlx::query(
                        "SELECT mode, revision FROM agent_directed_send_settings
                         WHERE sender_thread_id = ? AND receiver_thread_id = ?",
                    )
                    .bind(sender_thread_id.to_string())
                    .bind(receiver_thread_id.to_string())
                    .fetch_optional(&mut *tx)
                    .await?
                }
                AgentSendScope::Subtree {
                    supervisor_thread_id,
                } => {
                    sqlx::query(
                        "SELECT mode, revision FROM agent_subtree_send_settings
                         WHERE supervisor_thread_id = ?",
                    )
                    .bind(supervisor_thread_id.to_string())
                    .fetch_optional(&mut *tx)
                    .await?
                }
            };
            if let Some(row) = row {
                settings.push(setting_from_row(scope, &row)?);
            }
        }
        tx.commit().await?;
        Ok(settings)
    }

    /// Atomically replaces one explicit setting, with last-committed-write-wins semantics.
    ///
    /// Revision starts at one and increases even when the supplied mode is unchanged. The
    /// caller owns permission/admission serialization; persistence grants no lifecycle rights.
    pub async fn replace_agent_send_setting(
        &self,
        scope: AgentSendScope,
        mode: AgentSendMode,
    ) -> anyhow::Result<AgentSendSetting> {
        let stored_mode = match mode {
            AgentSendMode::Enabled => "enabled",
            AgentSendMode::Disabled => "disabled",
        };
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = match scope {
            AgentSendScope::Directed {
                sender_thread_id,
                receiver_thread_id,
            } => {
                sqlx::query(
                    "INSERT INTO agent_directed_send_settings
                     (sender_thread_id, receiver_thread_id, mode, revision) VALUES (?, ?, ?, 1)
                     ON CONFLICT (sender_thread_id, receiver_thread_id) DO UPDATE
                     SET mode = excluded.mode, revision = agent_directed_send_settings.revision + 1
                     RETURNING mode, revision",
                )
                .bind(sender_thread_id.to_string())
                .bind(receiver_thread_id.to_string())
                .bind(stored_mode)
                .fetch_one(&mut *tx)
                .await?
            }
            AgentSendScope::Subtree {
                supervisor_thread_id,
            } => {
                sqlx::query(
                    "INSERT INTO agent_subtree_send_settings
                     (supervisor_thread_id, mode, revision) VALUES (?, ?, 1)
                     ON CONFLICT (supervisor_thread_id) DO UPDATE
                     SET mode = excluded.mode, revision = agent_subtree_send_settings.revision + 1
                     RETURNING mode, revision",
                )
                .bind(supervisor_thread_id.to_string())
                .bind(stored_mode)
                .fetch_one(&mut *tx)
                .await?
            }
        };
        let setting = setting_from_row(scope, &row)?;
        tx.commit().await?;
        Ok(setting)
    }
}
