import { useCallback, useEffect, useState } from 'react'
import List from '../List'
import EmptyState from '../EmptyState'
import HelpTip from '../HelpTip'
import ConfirmDialog from '../ConfirmDialog'
import RenameModal from '../RenameModal'
import { MenuButton, type MenuItem } from '../Dropdown'
import { copyText } from '../../lib/clipboard'
import {
  ROLE_LABEL,
  approveRequest,
  canChangeRole,
  canManage,
  canRemove,
  forgetTeam,
  formatWhen,
  getTeams,
  leaveTeam,
  listInvites,
  listRequests,
  refreshTeam,
  refreshTeams,
  rejectRequest,
  removeMember,
  renameTeam,
  setMemberRole,
  withdrawInvite,
  type JoinRequest,
  type Team,
  type TeamInvite,
  type TeamMember,
  type TeamRole,
  type TeamsOverview,
} from '../../store/teams'
import { CreateTeamModal, InviteModal, JoinTeamModal } from './TeamDialogs'
import '../../styles/teams.css'

/** How often a pending join asks whether it was approved. */
const PENDING_POLL_MS = 10_000
const ROLES: TeamRole[] = ['owner', 'admin', 'member']
const noop = () => {}

/**
 * Settings → Teams: link this server with coworkers' servers. A member of a
 * team is a server (identified by its key), not a person. Not the skills
 * "team source", which is a shared git repo of skills.
 *
 * Local state comes from `GET /api/teams`; opening the page (and Refresh)
 * asks the team service for news. Any `team-update` event reloads it.
 */
export default function TeamsSection() {
  const [overview, setOverview] = useState<TeamsOverview | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [refreshError, setRefreshError] = useState<string | null>(null)
  const [refreshing, setRefreshing] = useState(false)
  // Bumped on every team-update so open teams refetch requests and invites.
  const [tick, setTick] = useState(0)
  const [dialog, setDialog] = useState<'create' | 'join' | null>(null)

  const load = useCallback(
    () =>
      getTeams()
        .then((o) => {
          setOverview(o)
          setLoadError(null)
        })
        .catch((e: Error) => setLoadError(e.message)),
    [],
  )

  const refresh = useCallback(() => {
    setRefreshing(true)
    setRefreshError(null)
    return refreshTeams()
      .catch((e: Error) => setRefreshError(e.message))
      .then(load)
      .then(() => setTick((t) => t + 1))
      .finally(() => setRefreshing(false))
  }, [load])

  useEffect(() => {
    void load().then(refresh)
  }, [load, refresh])

  useEffect(() => {
    const onUpdate = () => {
      void load()
      setTick((t) => t + 1)
    }
    window.addEventListener('peckboard:team-update', onUpdate)
    return () => window.removeEventListener('peckboard:team-update', onUpdate)
  }, [load])

  const teams = overview?.teams ?? []
  const pending = teams.filter((t) => t.state === 'pending')
  const members = teams.filter((t) => t.state === 'member')
  const past = teams.filter((t) => t.state === 'left' || t.state === 'removed')
  // Stable, so a pending team's poll timer isn't reset on every render.
  const reload = useCallback(() => {
    void load()
    setTick((t) => t + 1)
  }, [load])

  return (
    <section className="settings-section teams-section" data-testid="teams-section">
      <div className="settings-section-head">
        <h3>Teams</h3>
        {teams.length > 0 && (
          <div className="acct-row-actions teams-actions">
            <button
              type="button"
              className="btn-secondary btn-sm"
              onClick={() => void refresh()}
              disabled={refreshing}
              data-testid="teams-refresh"
            >
              {refreshing ? 'Checking…' : 'Refresh'}
            </button>
            <button
              type="button"
              className="btn-secondary btn-sm"
              onClick={() => setDialog('join')}
              disabled={!overview}
              data-testid="teams-join"
            >
              Join with an invite link
            </button>
            <button
              type="button"
              className="btn-primary btn-sm"
              onClick={() => setDialog('create')}
              disabled={!overview}
              data-testid="teams-create"
            >
              + Create a team
            </button>
          </div>
        )}
      </div>
      <p className="form-hint">
        Link this server with your coworkers&rsquo; servers. In a team of servers each member is a
        server, not a person, and nobody needs an account.
      </p>
      {overview && (
        <p className="teams-identity">
          <span>This server&rsquo;s key</span>
          <code className="team-fingerprint" data-testid="teams-fingerprint">
            {overview.server_fingerprint}
          </code>
          <HelpTip
            title="Server key"
            body="Every member of a team is a server, identified by its key. Coworkers see this short form next to this server's name, so they can tell it apart from any other server."
            testId="teams-key-help"
          />
        </p>
      )}
      {loadError && (
        <div className="form-error" role="alert" data-testid="teams-load-error">
          <span>{loadError}</span>{' '}
          <button type="button" className="btn-secondary btn-sm" onClick={() => void load()}>
            Retry
          </button>
        </div>
      )}
      {refreshError && (
        <p className="team-warning" role="alert" data-testid="teams-refresh-error">
          Could not check with the team service: {refreshError}
        </p>
      )}

      {overview && teams.length === 0 && (
        <EmptyState
          title="No teams yet"
          body="A team of servers links this Peckboard with your coworkers' Peckboards. Create one and invite them, or join a team with a link a coworker sent you."
          actionLabel="Create a team"
          onAction={() => setDialog('create')}
          actionTestId="teams-empty-create"
          testId="teams-empty"
        >
          <button
            type="button"
            className="btn-secondary teams-empty-join"
            onClick={() => setDialog('join')}
            data-testid="teams-empty-join"
          >
            Join with an invite link
          </button>
        </EmptyState>
      )}
      {!overview && !loadError && <div className="list-view-empty">Loading…</div>}

      {pending.map((t) => (
        <PendingTeam key={t.id} team={t} onChanged={reload} />
      ))}
      {members.map((t) => (
        <MemberTeam key={t.id} team={t} tick={tick} onChanged={reload} />
      ))}
      {past.length > 0 && <PastTeams teams={past} onChanged={reload} />}

      {dialog === 'create' && overview && (
        <CreateTeamModal
          defaultLabel={overview.default_label}
          onClose={() => setDialog(null)}
          onCreated={() => {
            setDialog(null)
            reload()
          }}
        />
      )}
      {dialog === 'join' && overview && (
        <JoinTeamModal
          defaultLabel={overview.default_label}
          onClose={() => setDialog(null)}
          onJoined={() => {
            setDialog(null)
            reload()
          }}
        />
      )}
    </section>
  )
}

/** A join this server asked for: the code to read out, waiting for approval. */
function PendingTeam({ team, onChanged }: { team: Team; onChanged: () => void }) {
  const [checking, setChecking] = useState(false)
  const [cancelling, setCancelling] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const check = useCallback(
    (quiet: boolean) => {
      if (!quiet) {
        setChecking(true)
        setError(null)
      }
      return refreshTeam(team.id)
        .then(onChanged)
        .catch((e: Error) => {
          if (!quiet) setError(e.message)
        })
        .finally(() => {
          if (!quiet) setChecking(false)
        })
    },
    [team.id, onChanged],
  )

  useEffect(() => {
    const id = window.setInterval(() => void check(true), PENDING_POLL_MS)
    return () => window.clearInterval(id)
  }, [check])

  const cancel = () => {
    setCancelling(true)
    setError(null)
    forgetTeam(team.id)
      .then(onChanged)
      .catch((e: Error) => setError(e.message))
      .finally(() => setCancelling(false))
  }

  return (
    <div className="team-card team-card-pending" data-testid="team-pending" data-team-id={team.id}>
      <div className="team-card-head">
        <h4 className="team-card-name" data-testid="team-name">
          {team.name}
        </h4>
        <span className="list-view-tag">Waiting for approval</span>
      </div>
      <p className="team-code-label">Your verification code</p>
      <div className="team-code team-code-large" data-testid="team-pending-code">
        {team.pending_code ?? '…'}
      </div>
      <p className="team-code-instruction">
        Read this code to the person who invited you. They approve the request once it matches.
      </p>
      {team.last_error && (
        <p className="team-warning" role="alert" data-testid="team-last-error">
          {team.last_error}
        </p>
      )}
      {error && (
        <p className="form-error" role="alert" data-testid="team-pending-error">
          {error}
        </p>
      )}
      <div className="team-card-actions">
        <button
          type="button"
          className="btn-secondary btn-sm"
          onClick={cancel}
          disabled={cancelling || checking}
          data-testid="team-pending-cancel"
        >
          {cancelling ? 'Cancelling…' : 'Cancel request'}
        </button>
        <button
          type="button"
          className="btn-primary btn-sm"
          onClick={() => void check(false)}
          disabled={checking || cancelling}
          data-testid="team-pending-check"
        >
          {checking ? 'Checking…' : 'Check again'}
        </button>
      </div>
    </div>
  )
}

/** A team this server is in: members, and for owners/admins invites and requests. */
function MemberTeam({
  team,
  tick,
  onChanged,
}: {
  team: Team
  tick: number
  onChanged: () => void
}) {
  const manager = canManage(team.role)
  const owners = team.members.filter((m) => m.role === 'owner').length
  const [invites, setInvites] = useState<TeamInvite[]>([])
  const [requests, setRequests] = useState<JoinRequest[] | null>(null)
  const [requestsError, setRequestsError] = useState<string | null>(null)
  const [actionError, setActionError] = useState<string | null>(null)
  const [inviting, setInviting] = useState(false)
  const [renaming, setRenaming] = useState(false)
  const [leaving, setLeaving] = useState(false)
  const [removing, setRemoving] = useState<TeamMember | null>(null)
  const [approving, setApproving] = useState<JoinRequest | null>(null)
  const [busy, setBusy] = useState(false)
  const [dialogError, setDialogError] = useState<string | null>(null)
  const [declining, setDeclining] = useState<string | null>(null)

  const loadExtras = useCallback(() => {
    if (!manager) return
    listInvites(team.id)
      .then(setInvites)
      .catch(() => setInvites([]))
    listRequests(team.id)
      .then((r) => {
        setRequests(r)
        setRequestsError(null)
      })
      .catch((e: Error) => setRequestsError(e.message))
  }, [team.id, manager])

  useEffect(() => {
    loadExtras()
  }, [loadExtras, tick])

  /** Run a dialog action: keep the dialog open with the error on failure. */
  const runDialog = (p: Promise<unknown>, close: () => void) => {
    setBusy(true)
    setDialogError(null)
    p.then(() => {
      close()
      onChanged()
    })
      .catch((e: Error) => setDialogError(e.message))
      .finally(() => setBusy(false))
  }

  const changeRole = (m: TeamMember, role: TeamRole) => {
    if (role === m.role) return
    setActionError(null)
    setMemberRole(team.id, m.key, role)
      .then(onChanged)
      .catch((e: Error) => setActionError(e.message))
  }

  const memberMenu = (m: TeamMember): MenuItem[] => [
    {
      label: 'Role',
      hint: ROLE_LABEL[m.role],
      hidden: !canChangeRole(team.role, m, owners),
      testId: 'team-member-role',
      submenu: ROLES.map((r) => ({
        label: ROLE_LABEL[r],
        active: r === m.role,
        onSelect: () => changeRole(m, r),
        testId: `team-member-role-${r}`,
      })),
    },
    {
      label: 'Remove from team',
      danger: true,
      hidden: !canRemove(team.role, m),
      onSelect: () => setRemoving(m),
      testId: 'team-member-remove',
    },
  ]

  const teamMenu: MenuItem[] = [
    {
      label: 'Rename',
      hidden: team.role !== 'owner',
      onSelect: () => setRenaming(true),
      testId: 'team-rename',
    },
    { divider: true, hidden: team.role !== 'owner' },
    { label: 'Leave team', danger: true, onSelect: () => setLeaving(true), testId: 'team-leave' },
  ]

  const decline = (r: JoinRequest) => {
    setDeclining(r.request_id)
    setActionError(null)
    rejectRequest(team.id, r.request_id)
      .then(() => {
        loadExtras()
        onChanged()
      })
      .catch((e: Error) => setActionError(e.message))
      .finally(() => setDeclining(null))
  }

  const withdraw = (inv: TeamInvite) => {
    setActionError(null)
    withdrawInvite(team.id, inv.invite_id)
      .then(loadExtras)
      .catch((e: Error) => setActionError(e.message))
  }

  return (
    <div className="team-card" data-testid="team-card" data-team-id={team.id}>
      <div className="team-card-head">
        <h4 className="team-card-name" data-testid="team-name">
          {team.name}
        </h4>
        {team.role && (
          <span className="list-view-tag" data-testid="team-role">
            You are {ROLE_LABEL[team.role].toLowerCase()}
          </span>
        )}
        <span className="team-card-spacer" />
        <MenuButton items={teamMenu} ariaLabel="Team menu" testId="team-menu" />
      </div>
      {team.last_error && (
        <p className="team-warning" role="alert" data-testid="team-last-error">
          {team.last_error}
        </p>
      )}
      {actionError && (
        <p className="form-error" role="alert" data-testid="team-action-error">
          {actionError}
        </p>
      )}

      {manager && (
        <div className="team-block" data-testid="team-requests">
          <div className="team-block-head">
            <h5>Requests to join</h5>
          </div>
          {requestsError && (
            <p className="team-warning" role="alert" data-testid="team-requests-error">
              {requestsError}
            </p>
          )}
          {requests && requests.length === 0 && (
            <p className="form-hint" data-testid="team-requests-empty">
              No one is waiting to join.
            </p>
          )}
          {requests?.map((r) => (
            <div
              key={r.request_id}
              className="team-request"
              data-testid="team-request"
              data-request-id={r.request_id}
            >
              <div className="team-request-who">
                <span className="team-request-label">{r.label}</span>
                <code className="team-fingerprint" title={r.key}>
                  {r.fingerprint}
                </code>
                <span className="team-muted">asked {formatWhen(r.created_at)}</span>
              </div>
              {r.can_approve && r.genuine && r.code && (
                <div className="team-request-code-wrap">
                  <span className="team-code-label">Verification code</span>
                  <span className="team-code" data-testid="team-request-code">
                    {r.code}
                  </span>
                </div>
              )}
              {!r.can_approve && (
                <p className="form-hint" data-testid="team-request-elsewhere">
                  This request used an invite made on another server. Approve it on the server that
                  sent the invite.
                </p>
              )}
              {r.can_approve && !r.genuine && (
                <p className="team-warning" role="alert" data-testid="team-request-not-genuine">
                  This request did not come from someone holding your invite link. Do not approve
                  it. Decline it and send your coworker a new invite.
                </p>
              )}
              <div className="team-card-actions">
                <button
                  type="button"
                  className="btn-secondary btn-sm"
                  onClick={() => decline(r)}
                  disabled={declining === r.request_id}
                  data-testid="team-request-decline"
                >
                  {declining === r.request_id ? 'Declining…' : 'Decline'}
                </button>
                {r.can_approve && r.genuine && (
                  <button
                    type="button"
                    className="btn-primary btn-sm"
                    onClick={() => {
                      setDialogError(null)
                      setApproving(r)
                    }}
                    data-testid="team-request-approve"
                  >
                    Approve
                  </button>
                )}
              </div>
            </div>
          ))}
        </div>
      )}

      <div className="team-block">
        <div className="team-block-head">
          <h5>
            Members <span className="team-muted">({team.members.length})</span>
          </h5>
        </div>
        <div data-testid="team-members">
          <List<TeamMember>
            items={team.members}
            getKey={(m) => m.key}
            bodyClassName="list-view-rows"
            onActivate={noop}
            getMenuItems={memberMenu}
            renderItem={(m) => (
              <>
                <span className="list-view-name" data-testid="team-member-name">
                  {m.label}
                </span>
                <span className="list-view-meta">
                  {m.is_self && (
                    <span className="list-view-tag team-self-tag" data-testid="team-member-self">
                      This server
                    </span>
                  )}
                  <span className="list-view-tag" data-testid="team-member-role-tag">
                    {ROLE_LABEL[m.role]}
                  </span>
                  <code className="team-fingerprint" title={m.key}>
                    {m.fingerprint}
                  </code>
                </span>
              </>
            )}
          />
        </div>
      </div>

      {manager && (
        <div className="team-block">
          <div className="team-block-head">
            <h5>Invites</h5>
            <button
              type="button"
              className="btn-primary btn-sm"
              onClick={() => setInviting(true)}
              data-testid="team-invite"
            >
              Invite a server
            </button>
          </div>
          {invites.length === 0 ? (
            <p className="form-hint">No open invites.</p>
          ) : (
            <div data-testid="team-invites">
              <List<TeamInvite>
                items={invites}
                getKey={(i) => i.invite_id}
                bodyClassName="list-view-rows"
                onActivate={(i) => void copyText(i.link)}
                getMenuItems={(i) => [
                  { label: 'Copy link', onSelect: () => void copyText(i.link) },
                  { divider: true },
                  {
                    label: 'Withdraw',
                    danger: true,
                    onSelect: () => withdraw(i),
                    testId: 'team-invite-withdraw',
                  },
                ]}
                renderItem={(i) => (
                  <>
                    <span className="list-view-name">Invite link</span>
                    <span className="list-view-meta">
                      <span>Made {formatWhen(i.created_at)}</span>
                      <span>Expires {formatWhen(i.expires_at)}</span>
                    </span>
                  </>
                )}
              />
              <p className="form-hint">Click an invite to copy its link again.</p>
            </div>
          )}
        </div>
      )}

      {inviting && (
        <InviteModal
          teamId={team.id}
          teamName={team.name}
          onClose={() => setInviting(false)}
          onCreated={loadExtras}
        />
      )}
      {renaming && (
        <RenameModal
          title="Rename team"
          label="Team name"
          initialValue={team.name}
          onSubmit={(name) => renameTeam(team.id, name).then(onChanged)}
          onClose={() => setRenaming(false)}
        />
      )}
      {approving && (
        <ConfirmDialog
          title="Approve this server?"
          message={`Approve only if the person you invited read you this code: ${approving.code}.`}
          confirmLabel="Approve"
          busy={busy}
          busyLabel="Approving…"
          error={dialogError}
          testId="team-approve-confirm"
          confirmTestId="team-approve-confirm-button"
          onConfirm={() =>
            runDialog(approveRequest(team.id, approving.request_id), () => setApproving(null))
          }
          onCancel={() => !busy && setApproving(null)}
        />
      )}
      {leaving && (
        <ConfirmDialog
          title="Leave team"
          message={`This server leaves "${team.name}". To come back, it needs a new invite.`}
          confirmLabel="Leave team"
          danger
          busy={busy}
          busyLabel="Leaving…"
          error={dialogError}
          testId="team-leave-confirm"
          onConfirm={() => runDialog(leaveTeam(team.id), () => setLeaving(false))}
          onCancel={() => {
            if (busy) return
            setLeaving(false)
            setDialogError(null)
          }}
        />
      )}
      {removing && (
        <ConfirmDialog
          title="Remove server"
          message={`Remove "${removing.label}" from "${team.name}"? It needs a new invite to come back.`}
          confirmLabel="Remove"
          danger
          busy={busy}
          busyLabel="Removing…"
          error={dialogError}
          testId="team-remove-confirm"
          onConfirm={() => runDialog(removeMember(team.id, removing.key), () => setRemoving(null))}
          onCancel={() => {
            if (busy) return
            setRemoving(null)
            setDialogError(null)
          }}
        />
      )}
    </div>
  )
}

/** Teams this server left or was removed from. */
function PastTeams({ teams, onChanged }: { teams: Team[]; onChanged: () => void }) {
  const [error, setError] = useState<string | null>(null)
  const forget = (t: Team) => {
    setError(null)
    forgetTeam(t.id)
      .then(onChanged)
      .catch((e: Error) => setError(e.message))
  }
  return (
    <div className="team-block team-past" data-testid="teams-past">
      <div className="team-block-head">
        <h5>No longer a member</h5>
      </div>
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
      <List<Team>
        items={teams}
        getKey={(t) => t.id}
        bodyClassName="list-view-rows"
        onActivate={noop}
        getMenuItems={(t) => [
          {
            label: 'Remove from list',
            danger: true,
            onSelect: () => forget(t),
            testId: 'team-forget',
          },
        ]}
        renderItem={(t) => (
          <>
            <span className="list-view-name team-muted">{t.name}</span>
            <span className="list-view-meta">
              <span>
                {t.last_error ||
                  (t.state === 'left'
                    ? 'This server left the team.'
                    : 'This server was removed from the team.')}
              </span>
            </span>
          </>
        )}
      />
    </div>
  )
}
