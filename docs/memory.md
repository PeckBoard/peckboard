---
title: Memory
nav_order: 7.6
---

# Memory

Agents in PeckBoard keep two kinds of notes, so they do not start from
nothing every time a conversation is cleared or a new session begins.

## Long-Term Memory

Long-term memory belongs to a **folder**. Every session that works in the
folder starts out knowing it, and it never expires.

It holds what should not have to be rediscovered:

- **Architecture**: how the system is put together.
- **Design decisions**: what was decided, and why.
- **Code structure**: how the code is organised.
- **Project layout**: where things live.
- **Conventions**: working rules and your preferences.

Agents add to it as they learn or decide something of that kind. You can
read, add, edit and delete notes yourself: open a session's menu, choose
**Memory**, and switch to the **Long-term (folder)** tab.

## Short-Term Memory

Short-term memory belongs to one **session**. It is about the work in
progress: the current task, where it stands, what is still open.

A short-term note is deleted 24 hours after it was last written. Past
half that time it fades to a one-line mention; an agent that still needs
the note rewrites it, which restarts the clock. Finished work is simply
left to expire.

An admin can change how long short-term notes last under **Settings →
Chat → Agent Memory**. The new length applies to notes written from then
on.

Notes written before short-term memory existed have no expiry and stay
until you or the agent remove them.

## Which Agents Use It

All of them. The notes and the rules for using them are part of what every
session is told when it starts, whichever provider runs it.

## What Not to Put in Memory

Passwords, tokens and keys. Long-term notes are shared with every session
in the folder; values that look like secrets are masked when an agent
writes one.
