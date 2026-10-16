# Job queue: long transcriptions on the box (`/jobs`) (reference)

Hand the box a long recording and walk away. Jobs live in `$TT_CONFIG_DIR/jobs/<id>/`
(`job.json`, `input.<ext>`, transcripts), run oldest first one at a time, and survive the
app closing and the agent restarting: a job interrupted mid-run goes back in line and picks
up at the next chunk (`progress.json` holds the chunks done and their segments, written
before each chunk counts; a different cut of the recording starts over).

Each job remembers the paired device that started it (`started_by`, `started_by_name`;
none for the box itself, or a recording started on it). Members see their own jobs and the `shared`
ones; owners see every job. Someone else's job answers `404` everywhere.

## Routes (authed)

| Route | |
|---|---|
| `POST /jobs/transcribe?name=meeting.m4a&when=now\|tonight&documents=true&keep_audio=true&shared=true` | Body: the recording (anything ffmpeg reads; streamed to disk, ≤ 4 GB). `201` with the job. `documents` and `keep_audio` default to the household's data settings (both off); `shared` to off. |
| `GET /jobs` | Newest first, the ones this caller may see: `{id, kind, title, when, status, done, total, duration_s, created, started, finished, error, to_documents, keep_audio, restore, started_by?, started_by_name?, shared}` |
| `POST /jobs/{id}/share` `{shared}` | Share a job and its transcript with everyone on the box, or stop (its starter or an owner). Audited. |
| `POST /jobs/{id}/run` | The go-ahead to switch the box now. Follows who may start and stop models (`model_control`, `docs/reference/your-data.md`): with `owners`, a member's is `403`. The queue starting Whisper by itself when nothing is serving isn't refused. |
| `GET /jobs/{id}/result?format=txt\|srt\|json` | The transcript (also the partial one while running). |
| `DELETE /jobs/{id}` | Cancel a running job or delete a finished one. |

## How the runner decides (`jobs::plan`, every 5 s)

| Box state | Job | Plan |
|---|---|---|
| Whisper serving | any | **run** |
| idle / failed | any | **switch** to the `transcribe` mode (`crate::modes`) |
| loading or stopping | any | hold |
| serving something else | `now`, no go-ahead | **wait** (status `waiting`; the app offers "switch now") |
| serving something else | `now` + go-ahead, or `tonight` between 1:00 and 5:59 | **switch**, remembering the model in `restore` |

Whisper (whisper-large-v3) runs only on the whole box (P300x2). When the queue empties,
the runner starts the remembered model again. Lab runs (`lab_busy`) pause the queue.

## Transcription

Whisper's media server takes ≤ 300 s of audio per request (≤ 50 MB), so the runner cuts the
recording with ffmpeg into 270 s, 16 kHz mono PCM WAV pieces (~8.6 MB each), POSTs each as
multipart to `/v1/audio/transcriptions` (`response_format=verbose_json`,
`return_timestamps=true`), shifts each piece's segment times by its offset, and writes
`transcript.txt` (`[hh:mm:ss] text`, updated after every piece), `.srt` and `.json`. When it
finishes: the recording (`input.<ext>`) is deleted unless the job (or, when it didn't say,
the household's `keep_audio` setting) keeps it; the transcript is added to the shared Library
as `Transcript: <name>` only when the job asked (`documents=true`) or the household's
`transcripts_to_library` is on; and a `job_done` notification goes out. See
[your-data.md](your-data.md) for those settings, retention and export.

## Recording on the phone

Transcribe → Record records in place (the bottom bar turns into the recorder; an earlier full-screen recorder could fail to open next to the file picker) from the microphone (AAC, 16 kHz mono, 64 kb/s: about 30 MB an hour) into the
app's `Documents/Recordings`, keeps recording with the screen locked (`audio` background mode), and
uploads it as `Recording <date>.m4a` (or the name typed) through the same `POST /jobs/transcribe`.
The file is deleted only once the box has it; a failed or dismissed upload stays on the phone with a
"Send it again" button.


## Recording meetings on the box

`recorder.rs` records the box's default microphone and whatever its default
output is playing (PipeWire's `@DEFAULT_MONITOR@`), mixed to 16 kHz mono Opus,
straight into a new job directory. Stopping queues it as an ordinary
`transcribe` job that belongs to the device that started the recording (its starter and
owners see it; members' other devices don't, until it's shared). A recording started on the
box itself, or by Tori there, has no starter, so it's owners-only. Each recording can say
whether to keep the audio and whether the transcript goes to the Library; what it doesn't
say takes the household's data settings when the job finishes.

- `GET /record`: `{microphone, speakers, recording}`; while recording,
  `recording` has `seconds`, the devices in use and live levels (`mic_db`,
  `system_db`, dBFS over the last 100 ms).
- `POST /record/start` `{title?, mic?, system?, keep_audio?, documents?}`: both sides by
  default; the microphone is skipped when none is plugged in. Four hours at most.
- `POST /record/stop` `{discard?, keep_audio?, documents?}`: `{job}` once queued. A choice made
  at stop wins over one made at start.

The desktop app shows it in Studio → Transcribe, on Home while recording, and
in the tray ("Record a meeting").

## Research reports (`/research`, `src/research.rs`)

Research jobs are scoped the same way: each records the device that started it
(`started_by`, `started_by_name`) and `shared`. Members see their own and the shared ones;
owners see all; anything else is `404` (reading, `explain`, a follow-up's `parent`).
Renaming, stopping, deleting and sharing is for the starter or an owner (`403` for someone
a shared report is only shown to); `GET /research/{id}` says which with `can_change`.
Reports from before starters were kept, and ones the box started (a routine with nobody
behind it), are the owners' until shared. A routine run for a person starts research as
that person.

| Route | |
|---|---|
| `GET /research` | `{jobs, check, owner}`: the reports this caller may see, newest first. |
| `POST /research/{id}/share` `{shared}` | Share a report with everyone on the box, or stop (its starter or an owner). Audited (`research_share`). |
