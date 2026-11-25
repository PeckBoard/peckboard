---
title: Teams
nav_order: 7.7
---

# Teams

A team links your PeckBoard server with your coworkers' servers. There are
no accounts and nothing to sign up for: a member of a team is a **server**,
identified by a key that server created for itself and never shares.

Teams live under **Settings → Teams** and can be managed by admins.

> In Settings → Skills, a "team source" is a shared git repository of
> skills. That is a separate feature; a team of servers does not need one.

## Create a Team

1. Open **Settings → Teams** and choose **Create a team**.
2. Give the team a name, and give this server a name the others will see
   in the member list (for example "office server").

Your server is the team's first **owner**.

## Invite a Coworker's Server

1. In the team, choose **Invite a server**. You get an invite link.
2. Send the link to one coworker, any way you like. It works once and
   expires (after three days unless you pick another length, up to seven).
3. Your coworker opens **Settings → Teams → Join with an invite link** on
   their PeckBoard, pastes the link, and chooses **Ask to join**. Their page
   now shows a six-digit **verification code**.
4. The request appears under **Requests to join** on your page, with a
   code. Ask your coworker to read you theirs.
5. If the two codes match, approve. If they differ, decline: the request
   did not come from the server you invited.

The code is what protects you if the link was forwarded, copied out of a
chat, or read by someone else. Another server using the same link shows a
different code, so the person you invited can never confirm it.

Only the server that created an invite can approve the requests made with
it. If another admin's invite was used, approve on that admin's server.

## Roles

| Role   | Can do                                                  |
| ------ | ------------------------------------------------------- |
| Owner  | Everything: invite, remove anyone, change roles, rename |
| Admin  | Invite servers and remove plain members                 |
| Member | Leave the team                                          |

A team always keeps at least one owner. Make a second server an owner
before the only owner leaves.

## Leaving and Removing

- **Leave team** takes your own server out.
- An owner or admin can remove another server from the member list. That
  server finds out the next time it checks in and shows the team as
  removed.

Removing a server stops it from seeing anything new. It cannot take back
what that server already received.

## How It Stays Trustworthy Without Accounts

- **Every change is signed.** Creating the team, adding, removing and
  promoting a server are each signed by the server that made the change.
  Together they form the team's history.
- **Your server checks the history itself.** The hosted team service
  (`team.peckboard.com`) only stores and passes along that history. Each
  member server re-checks every signature and applies the role rules on
  its own, so the service cannot add a server to your team.
- **History can only grow.** Your server remembers the newest state it has
  verified and refuses anything that does not continue it. If it ever
  reports that the history does not match, do not ignore it.
- **The invite secret stays between the two servers.** It is in the link
  and on the inviting server; the team service only ever sees a
  fingerprint of it.

## If a Server Is Lost or Reinstalled

A server's identity is the key in its data folder. A server restored with
its data folder is the same member. A server reinstalled without it is a
new server: remove the old entry and invite the new one.
