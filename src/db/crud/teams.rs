//! Storage for teams of PeckBoard servers (`teams`, `team_log`,
//! `team_invites`). Plain rows in, plain rows out: every rule about who is
//! in a team lives in `service::teams` and the `peckboard-team-format`
//! crate, which replay the stored log.

use diesel::connection::Connection;
use diesel::prelude::*;

use crate::db::Db;
use crate::db::models::{TeamInviteRow, TeamRow};
use crate::db::schema::{team_invites, team_log, teams};

impl Db {
    /// Every team row, oldest first.
    pub async fn list_teams(&self) -> anyhow::Result<Vec<TeamRow>> {
        self.with_conn(|conn| {
            teams::table
                .order((teams::created_at.asc(), teams::id.asc()))
                .select(TeamRow::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn get_team(&self, id: &str) -> anyhow::Result<Option<TeamRow>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            teams::table
                .find(&id)
                .select(TeamRow::as_select())
                .first(conn)
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    /// Insert or fully replace a team row and, when `records` is given, its
    /// stored log — in one transaction, so the pinned head (`log_len`,
    /// `log_head`) and the stored records can never disagree.
    pub async fn save_team(
        &self,
        row: TeamRow,
        records: Option<Vec<String>>,
    ) -> anyhow::Result<()> {
        self.with_conn(move |conn| {
            conn.transaction::<_, anyhow::Error, _>(|conn| {
                diesel::insert_into(teams::table)
                    .values(&row)
                    .on_conflict(teams::id)
                    .do_update()
                    .set(&row)
                    .execute(conn)?;
                if let Some(records) = records {
                    diesel::delete(team_log::table.filter(team_log::team_id.eq(&row.id)))
                        .execute(conn)?;
                    for (seq, record) in records.iter().enumerate() {
                        diesel::insert_into(team_log::table)
                            .values((
                                team_log::team_id.eq(&row.id),
                                team_log::seq.eq(seq as i64),
                                team_log::record.eq(record),
                            ))
                            .execute(conn)?;
                    }
                }
                Ok(())
            })
        })
        .await
    }

    /// Forget a team (its log and this server's invites go with it).
    /// Returns whether a row was deleted.
    pub async fn delete_team(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(teams::table.find(&id))
                .execute(conn)
                .map(|n| n > 0)
                .map_err(Into::into)
        })
        .await
    }

    /// The stored membership log of a team, in order, one JSON record each.
    pub async fn team_log_records(&self, team_id: &str) -> anyhow::Result<Vec<String>> {
        let team_id = team_id.to_string();
        self.with_conn(move |conn| {
            team_log::table
                .filter(team_log::team_id.eq(&team_id))
                .order(team_log::seq.asc())
                .select(team_log::record)
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn add_team_invite(&self, row: TeamInviteRow) -> anyhow::Result<()> {
        self.with_conn(move |conn| {
            diesel::insert_into(team_invites::table)
                .values(&row)
                .execute(conn)
                .map(|_| ())
                .map_err(Into::into)
        })
        .await
    }

    /// This server's open invites for a team; expired ones are dropped
    /// first (`now` is RFC 3339 UTC to the second, the form they are stored in).
    pub async fn list_team_invites(
        &self,
        team_id: &str,
        now: &str,
    ) -> anyhow::Result<Vec<TeamInviteRow>> {
        let team_id = team_id.to_string();
        let now = now.to_string();
        self.with_conn(move |conn| {
            diesel::delete(team_invites::table.filter(team_invites::expires_at.le(&now)))
                .execute(conn)?;
            team_invites::table
                .filter(team_invites::team_id.eq(&team_id))
                .order(team_invites::created_at.asc())
                .select(TeamInviteRow::as_select())
                .load(conn)
                .map_err(Into::into)
        })
        .await
    }

    pub async fn delete_team_invite(&self, invite_id: &str) -> anyhow::Result<bool> {
        let invite_id = invite_id.to_string();
        self.with_conn(move |conn| {
            diesel::delete(team_invites::table.find(&invite_id))
                .execute(conn)
                .map(|n| n > 0)
                .map_err(Into::into)
        })
        .await
    }
}
