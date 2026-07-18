// Cron-schedule types + a thin API client for the /schedules backend.
//
// Reuses the same backend base (CU_BACKEND) and bearer auth (authHeaders) as
// every other backend call in the app — see lib.ts. All actual occurrence /
// cron math is the backend's job; the frontend only renders a cron string as a
// human label (cronLabel, wrapping cronstrue).

import cronstrue from 'cronstrue'
import { CU_BACKEND, authHeaders } from './lib'

// A stored schedule. Field-for-field the backend contract (snake_case).
export interface Schedule {
  schedule_id: string
  name: string
  cron: string
  timezone: string
  task: string
  model: string
  pinned_set_id: string | null
  enabled: boolean
  require_confirmation: boolean
  snooze_minutes: number
  last_fired_at: string | null
  next_fire_at: string | null
  created_at: string
}

// GET /schedules/{id} augments a Schedule with the next few occurrences (ISO).
export type ScheduleDetail = Schedule & { next_occurrences: string[] }

// One entry per enabled schedule that has missed / due occurrences. Returned by
// GET /schedules/pending. `occurrences` are ISO timestamps ascending (may be
// several if the laptop was closed for days).
export interface PendingSchedule {
  schedule_id: string
  name: string
  task: string
  model: string
  pinned_set_id: string | null
  require_confirmation: boolean
  snooze_minutes: number
  occurrences: string[]
}

// Body accepted by POST /schedules.
export interface CreateScheduleBody {
  name: string
  cron: string
  timezone: string
  task: string
  model: string
  pinned_set_id?: string | null
  require_confirmation?: boolean
  snooze_minutes?: number
}

// Fields patchable via PATCH /schedules/{id} (any subset, including `enabled`).
export type SchedulePatch = Partial<
  Pick<
    Schedule,
    | 'name'
    | 'cron'
    | 'timezone'
    | 'task'
    | 'model'
    | 'pinned_set_id'
    | 'enabled'
    | 'require_confirmation'
    | 'snooze_minutes'
  >
>

// Shared JSON headers (bearer auth + content-type) for write calls.
function jsonHeaders(): Record<string, string> {
  return { ...authHeaders(), 'content-type': 'application/json' }
}

// Throw a readable error carrying the HTTP status for any non-2xx response.
async function ensureOk(resp: Response, what: string): Promise<void> {
  if (!resp.ok) {
    throw new Error(`${what} failed (HTTP ${resp.status})`)
  }
}

// POST /schedules → Schedule
export async function createSchedule(body: CreateScheduleBody): Promise<Schedule> {
  const resp = await fetch(`${CU_BACKEND}/schedules`, {
    method: 'POST',
    headers: jsonHeaders(),
    body: JSON.stringify(body),
  })
  await ensureOk(resp, 'Create schedule')
  return (await resp.json()) as Schedule
}

// GET /schedules → Schedule[]
export async function listSchedules(): Promise<Schedule[]> {
  const resp = await fetch(`${CU_BACKEND}/schedules`, { headers: authHeaders() })
  await ensureOk(resp, 'List schedules')
  const data = await resp.json()
  return Array.isArray(data) ? (data as Schedule[]) : ((data.schedules ?? []) as Schedule[])
}

// GET /schedules/{id} → Schedule & { next_occurrences }
export async function getSchedule(id: string): Promise<ScheduleDetail> {
  const resp = await fetch(`${CU_BACKEND}/schedules/${encodeURIComponent(id)}`, {
    headers: authHeaders(),
  })
  await ensureOk(resp, 'Get schedule')
  return (await resp.json()) as ScheduleDetail
}

// PATCH /schedules/{id} → Schedule
export async function patchSchedule(id: string, patch: SchedulePatch): Promise<Schedule> {
  const resp = await fetch(`${CU_BACKEND}/schedules/${encodeURIComponent(id)}`, {
    method: 'PATCH',
    headers: jsonHeaders(),
    body: JSON.stringify(patch),
  })
  await ensureOk(resp, 'Update schedule')
  return (await resp.json()) as Schedule
}

// DELETE /schedules/{id} → 204
export async function deleteSchedule(id: string): Promise<void> {
  const resp = await fetch(`${CU_BACKEND}/schedules/${encodeURIComponent(id)}`, {
    method: 'DELETE',
    headers: authHeaders(),
  })
  await ensureOk(resp, 'Delete schedule')
}

// GET /schedules/pending → PendingSchedule[]
export async function fetchPending(): Promise<PendingSchedule[]> {
  const resp = await fetch(`${CU_BACKEND}/schedules/pending`, { headers: authHeaders() })
  await ensureOk(resp, 'Fetch pending schedules')
  const data = await resp.json()
  return Array.isArray(data) ? (data as PendingSchedule[]) : ((data.pending ?? []) as PendingSchedule[])
}

// POST /schedules/{id}/fired {occurrence_ts} → Schedule
// Advances the schedule past that occurrence. Called after launching a run for
// it AND after skipping it.
export async function markFired(id: string, occurrenceTs: string): Promise<Schedule> {
  const resp = await fetch(`${CU_BACKEND}/schedules/${encodeURIComponent(id)}/fired`, {
    method: 'POST',
    headers: jsonHeaders(),
    body: JSON.stringify({ occurrence_ts: occurrenceTs }),
  })
  await ensureOk(resp, 'Mark schedule fired')
  return (await resp.json()) as Schedule
}

// Human-readable label for a cron string (e.g. "At 09:00, only on Monday").
// The frontend NEVER parses cron for occurrence math — this is display only.
// Falls back to the raw cron on any parse error.
export function cronLabel(cron: string): string {
  try {
    return cronstrue.toString(cron, { use24HourTimeFormat: true })
  } catch {
    return cron
  }
}
