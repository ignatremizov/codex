# Non-interactive mode

For information about non-interactive mode, see [this documentation](https://developers.openai.com/codex/noninteractive).

Prompt-bearing `codex exec resume <UUID-or-name> <prompt>` now sends ordinary input
through the shared queue. A loaded owner uses its existing auth and environment;
an unloaded independent thread resumes under the CLI-selected auth profile.
Active regular turns are steered, idle/interrupted targets start through normal
admission, and unsupported active tasks keep the input pending.

```sh
codex exec resume <thread-id> 'Review the deployment' -o result.txt
codex schedule --thread <thread-id> --at 'in 2h' --message 'Review the deployment'
```

Shared resume supports text, stdin (`-` or a piped prompt), `--last`/`--all`,
`--json`, and `-o`. It uses the receiving thread's saved settings. Explicit images,
model/permission/directory overrides, `-c`, output schemas, and other execution
overrides are rejected before input acceptance. Promptless and fork operations
keep their existing entry points. Remote selection for exec remains unchanged;
the `schedule` command supports the existing `--remote` connection flags.

The input ID is printed to stderr so observation can be resumed with
`codex schedule result <id> --wait`. Output belongs to its exact receiving turn;
a steered turn may answer other input too. JSONL returns standard thread/turn
and final-message events after completion. It does not stream tool progress from
another server, and unavailable usage uses exec's existing zero-valued fallback.
Human output writes only the final answer to stdout.

Ctrl+C ends observation. If this input started its turn, the client attempts an
ordinary interrupt through its connection; it does not interrupt a turn it only
steered. For a different hosting server, use that thread's interrupt controls.
Neither a client disconnect nor a saved result pins the thread. Completed
independent threads follow normal inactive/unsubscribed unloading.

See [scheduled thread execution](scheduled-thread-execution-spec.md) for the
storage, auth, lifecycle and qualification contract.
