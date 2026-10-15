# Job queue: long transcriptions on the box (`/jobs`) (reference)

Hand the box a long recording and walk away. Jobs live in `$TT_CONFIG_DIR/jobs/<id>/`
(`job.json`, `input.<ext>`, transcripts), run oldest first one at a time, and survive the
app closing and the agent restarting: a job interrupted mid-run goes back in line and picks
up at the next chunk (`progress.json` holds the chunks done and their segments, written
before each chunk counts; a different cut of the recording starts over).

Each job remembers the paired device that started it (`started_by`, `started_by_name`;
none for the box itself and the recorder). Members see their own jobs and the `shared`
ones; owners see every job. Someone else's job answers `404` everywhere.

## Routes (authed)

| Route | |
|---|---|
| `POST /jobs/transcribe?name=meeting.m4a&when=now\|tonight&documents=true&keep_audio=true&shared=true` | Body: the recording (anything ffmpeg reads; streamed to disk, ≤ 4 GB). `201` with the job. `documents` and `keep_audio` default to the household's data settings (both off); `shared` to off. |
| `GET /jobs` | Newest first, the ones this caller may see: `{id, kind, title, when, status, done, total, duration_s, created, started, finished, error, to_documents, keep_audio, restore, started_by?, started_by_name?, shared}` |
| `POST /jobs/{id}/share` `{shared}` | Share a job and its transcript with everyone on the box, or stop (its starter or an owner). Audited. |
| `POST /jobs/{id}/run` | The owner's go-ahead to switch the box now. |
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
`transcribe` job. The recorder doesn't say who started it or what they chose, so its jobs
are owners-only (until shared) and take the household's audio and Library defaults.

- `GET /record`: `{microphone, speakers, recording}`; while recording,
  `recording` has `seconds`, the devices in use and live levels (`mic_db`,
  `system_db`, dBFS over the last 100 ms).
- `POST /record/start` `{title?, mic?, system?}`: both sides by default; the
  microphone is skipped when none is plugged in. Four hours at most.
- `POST /record/stop` `{discard?}`: `{job}` once queued.

The desktop app shows it in Studio → Transcribe, on Home while recording, and
in the tray ("Record a meeting").
