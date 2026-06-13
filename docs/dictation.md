# Editable composer dictation

Dictation turns microphone input into ordinary editable composer text. It does not start a live voice conversation, send a user turn, or invoke Codex tools automatically. Review and submit the text just as you would typed input.

This fork feature remains separate from upstream `/voice`, which starts or stops a live realtime WebRTC conversation. The upstream desktop requirements gate `in_app_dictation` does not enable TUI composer dictation.

## Configuration

Dictation is opt-in:

```toml
[features]
voice_transcription = true
```

The default shortcut is Alt+M. Customize or unbind it with `tui.keymap.composer.toggle_dictation` using the normal keymap syntax. When the feature is disabled or capture is unavailable in the build, its shortcut is neither reserved nor shown. Realtime's F8 toggle and Ctrl+X mute shortcuts remain separate.

Linux musl builds do not expose direct-capture dictation. Linux GNU builds require the ALSA runtime. The realtime voice helper and its packaged native runtime are independent of dictation's direct microphone capture.

## Recording and delivery

Press the shortcut to begin recording and press it again to stop. Completed chunks are transcribed while recording continues; results enter the recording's own composer placeholder in recording order, even if uploads finish out of order. Failed chunks are reported without discarding successfully transcribed text. There is no additional dictation-specific limit on the completed transcript's length.

Recording is split after at least 15 seconds when one second of silence is detected, or at 60 seconds per chunk. Stopping also submits the remaining short chunk. These are chunk boundaries, not a maximum total recording duration.

The upload pipeline and microphone ingress are bounded. If uploads cannot keep up, recording stops visibly and drains the accepted chunks and remaining partial chunk. A capture discontinuity is reported as a partial recording rather than silently presented as complete.

Deleting the placeholder or leaving its owning widget cancels that recording. Late results must not replace another draft or another thread's text. Realtime voice and dictation cannot capture the microphone concurrently; starting and stopping states also participate in this exclusion.

## Authentication

Dictation uses the selected browser ChatGPT login and the ChatGPT `/transcribe` HTTP endpoint, not the realtime WebSocket API. API keys, personal access tokens, externally supplied tokens, and other provider authentication modes are not substitutes for this browser login.

The recording captures its authentication manager, account, and endpoint before microphone acquisition. Its chunks use that same selection, including an OAuth refresh and a single retry after a 401 response. Logout or an account change prevents subsequent uploads; a recording never silently switches accounts.

Non-success responses retain their diagnostic body. Audio and authentication secrets are not included in ordinary diagnostic logs.
