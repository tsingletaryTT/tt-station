# Your data: settings, retention, export and import (reference)

`crates/tt-station-agentd/src/export.rs` (and `boot.rs` for the power-cut setting). The
desktop app shows all of it to owners in Settings → Your data.

## Settings (`data-settings.json`)

| Field | Default | |
|---|---|---|
| `keep_audio` | off | Keep a recording after it's transcribed, when its job didn't say. |
| `transcripts_to_library` | off | Add finished transcripts to the shared Library, when the job didn't say. |
| `retention_days` | none | Delete transcripts and research reports this many days after they finished (1–3650). Checked 3 minutes after start, then hourly; unfinished work is never touched; each sweep that deletes something is audited (`retention`). A transcript already added to the Library stays there. |
| `restart_last_model` | off | Start the model that was serving again after the box boots (below). |
| `model_control` | `anyone` | Who may start, stop and switch the served model: `anyone` or `owners` (`model_control.rs`; the desktop app shows it in Settings → Models). With `owners`, members get `403` "Only owners can start or stop models on this box." from every route and tool that starts, stops or switches serving; see `docs/security-faq.md`. |

- `GET /data/settings` (authed): `{settings, owner, may_control_models}`.
- `PUT /data/settings` (owners): any of the fields; `retention_days: null` turns it off. Audited (`data_settings`); a change of `model_control` is also audited on its own (`model_control`, `anyone -> owners`).

## Start the last model when the box boots

The agent notes what is serving every 30 s (`last-serving.json`: the model, and whether it
was still serving). At start it reads the note first, waits 45 s for a running model to be
adopted, and starts the noted model through the ordinary serve path (`begin_serve`, which
claims the start) only when: the setting is on; the note says the model was still serving
(one that was stopped isn't brought back); the box has been up at most 15 minutes (a boot,
not an agent restart, after which the model is still running and is adopted); nothing is
serving or starting; nothing else holds the chips; and no job is waiting. Audited as a
`model_switch` by the box. The decision is `boot::decide`.

## Export (`POST /export`, owners)

A `.tar.gz` download (`tt-station-<box>-<date>.tar.gz`) holding, at the same paths as in the
config directory, an allowlist (`export::classify`):

| In | Not in |
|---|---|
| Research reports (`research/<id>/`: job, plan, report, sources, cited, events), finished only | Paired-device tokens (`agentd-tokens.json`), app keys, `desktop-token` |
| Finished transcripts (`jobs/<id>/`: job, txt, srt, json); the recording only if kept | TLS key and certificate, `notify.json` |
| Memories, people (names only: which device is whose stays behind), schedules (now mode-switching routines) | The local secret, `secrets.json` (HF token), app keys under `apps/` and `coding/` |
| Routines and their finished runs | Code projects, conversations (`events.jsonl`), the sandbox's credentials |
| Preferences: model, favorites, idle, embeddings, voice, data settings | Images (only their prompts), metrics, logs, models, downloads, the audit log |
| Evaluations (and their raw trials), finished only | Anything not on the allowlist, and any path that even looks like a secret (`export::looks_secret`) |
| Code sessions' `session.json` | |
| Prompts of images made in chat (`creations/<id>.txt`) | |

`manifest.json` (last in the archive) has `format: "tt-station-export"`, `format_version`
(1), `agent_version`, `created`, `box`, `counts` per kind, and every file's `path`, `kind`,
`bytes` and `sha256`. Audited (`export`).

## Import (`POST /import[?dry_run=true]`, owners)

Body: an export (≤ 6 GB), streamed to a private temp file and read twice: once to check it
and work out what would change, and (without `dry_run`) once to write.

- Refused before anything is written (`422`): a path that isn't plain (absolute, `..`, `.`,
  empty parts, backslashes, NUL), a link or special file, a file twice, a file over 64 MB
  (recordings: 4 GB), over 32 GB or 100,000 entries in all, a file whose SHA-256 doesn't
  match the manifest, no manifest, or a newer `format_version`.
- Skipped and listed: anything not on the allowlist (secrets included), JSON that doesn't
  parse, and unfinished work (a job, report, run or evaluation that wasn't finished: an
  import never starts anything).
- People, memories, schedules and routines merge by id into what the box has (the box's
  own limits apply; a routine keeps only real actions in `allowed` and works out its next
  run here). Schedules from an older export become mode-switching routines, as on any box. Everything else is written file by file, atomically, replacing a file at the
  same path. Imported transcripts appear in `GET /jobs` straight away.
- The answer: `{dry_run, preview: {agent_version, format_version, created, box, categories:
  {<kind>: {added, overwritten, unchanged}}, overwritten: [...], skipped: [...], bytes}}`.
  Reports, transcripts, evaluations and sessions count once each; merged kinds count entries.
- A real import is audited (`import`, `ok` or `refused`).
