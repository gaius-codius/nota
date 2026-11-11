# The settings file

nota's clean-up job asks a language model, the **local decider**, which
terms the transcript got wrong. It runs on your machine, in llama.cpp's
`llama-server`. The clean-up job is still being built: this version reads
the settings below, but doesn't start the decider yet. For now you tell nota where that server and its model are,
in a settings file. A later version will download its own.

## Where the file is

The file is `settings.toml` in nota's config folder:

- `$XDG_CONFIG_HOME/nota/settings.toml`, if `XDG_CONFIG_HOME` is set to an
  absolute path
- otherwise `~/.config/nota/settings.toml`

nota doesn't create it. Without the file, the local decider is simply not
set up, and nota doesn't start a server.

nota reads the file each time it needs the decider, so a change takes
effect the next time, with no need to restart nota.

## A complete example

Copy this into the settings file (see above) and change the paths to yours:

```toml
[local_decider]
program = "~/llama.cpp/build/bin/llama-server"
model = "~/models/decider.gguf"
device = "Vulkan0"
# socket_dir = "/run/user/1000/nota"
```

The file holds one table, `[local_decider]`, with the four keys below. All
of them are text, in quotes.

## The keys

### `program`

The `llama-server` to run. Either:

- a path to it, such as `"~/llama.cpp/build/bin/llama-server"`, or
- a bare name with no `/`, such as `"llama-server"`, which nota looks for in
  the folders on your `PATH`. Folders on `PATH` that aren't absolute are
  skipped.

It must be a file you're allowed to run.

### `model`

The model file the server loads, a `.gguf` file. A path.

### `device`

Where the model runs:

- `"cpu"` (any case) runs it on the CPU alone.
- One GPU's name runs all of it on that GPU. Use the name
  `llama-server --list-devices` prints, such as `"Vulkan0"` or `"CUDA0"`.
  Name only one GPU: a list such as `"Vulkan0,Vulkan1"` isn't accepted, and
  neither is `"none"`.

Without `device`, the model runs on the CPU.

### `socket_dir`

The folder for the socket nota talks to the server through. A path.

Without it, nota uses its folder in your runtime folder,
`$XDG_RUNTIME_DIR/nota` (often `/run/user/<your user id>/nota`). Most people
never need to set it.

nota creates the folder if it's missing, readable only by you. If the folder
is already there, it must be readable only by you (no group or other
permissions, as `chmod 700` leaves it), or the server won't start.

## Paths

`program` (when it has a `/`), `model` and `socket_dir` are paths. Each must
be either:

- absolute, starting with `/`, such as `"/opt/models/decider.gguf"`, or
- in your home folder, starting with `~/`, such as `"~/models/decider.gguf"`.

Other relative paths (`"models/decider.gguf"`, `"./llama-server"`) aren't
accepted, and nor is `~user/`. Environment variables such as `$HOME` aren't
expanded.

## When the decider isn't set up

If there's no settings file, or the file doesn't give both `program` and
`model`, the local decider is **not set up**. That isn't an error: nota just
doesn't start a server. To set it up, give both keys.

## When the decider is unavailable

When the file is there but the decider can't be used, nota says why, in one
of the messages below. `<file>` stands for the settings file's path, and the
other `<…>` for what the message names. No server is started until it's
fixed.

| Message | What to fix |
|---|---|
| no home directory to find the settings in | nota found no home folder. Set `HOME` to your home folder. |
| the settings `<file>` can't be read: `<reason>` | Make the file readable by you (often `chmod 644 <file>`), or fix what the reason names. |
| the settings `<file>` are too big: over 65536 bytes | The file can be at most 64 KiB. It needs only a few lines: remove the rest. |
| there's no llama-server to run at `<path>` | `program` names a path with nothing there, or a file you can't run. Fix the path, or make the file runnable (`chmod +x <path>`). |
| `<name>` isn't in any PATH folder | `program` is a bare name, and no folder on `PATH` has a runnable file by that name. Give its full path instead, or add its folder to `PATH`. |
| the model isn't at `<path>` | `model` names a path with no file there. Fix the path. |
| no folder for the server's socket: set socket_dir, or XDG_RUNTIME_DIR | There's no runtime folder to put the socket in. Set `socket_dir` to a folder that only you can read, or log in through a session that sets `XDG_RUNTIME_DIR`. |
| nota's own program can't be found: `<reason>` | nota starts the server through its own program, and couldn't find it, often because it was replaced or removed while running. Restart nota. |
| the settings `<file>` aren't usable: `<reason>` | The file has a mistake. The reason says which; see the next table. |

These are the reasons the last message gives:

| Reason | What to fix |
|---|---|
| not UTF-8 text | Save the file as UTF-8. |
| not TOML: `<detail>` | The file isn't valid TOML. The detail names the line. Check the quotes around each value. |
| nota has no setting `<key>` | The file has a table or key outside `[local_decider]`. Move a key such as `program` under the `[local_decider]` line, check the table's spelling, or remove it. |
| local_decider isn't a table | `local_decider` is written as a value. Write it as the table `[local_decider]`, with the keys on the lines below. |
| nota has no setting local_decider.`<key>` | That key isn't one of `program`, `model`, `device` and `socket_dir`. Check its spelling, or remove it. |
| local_decider.`<key>` isn't text | Put the value in quotes. |
| local_decider.program is empty | Give the program's path or name, or remove the key. |
| local_decider.`<key>` starts with ~/ but there's no home directory | nota found no home folder. Set `HOME`, or give an absolute path. |
| local_decider.`<key>` must be an absolute path or start with ~/: `<text>` | Write the path in full from `/`, or from `~/` (see "Paths"). |
| local_decider.device must be cpu or one GPU's name, as llama-server lists it: `<text>` | Write `"cpu"`, or one name from `llama-server --list-devices`, with no commas. |
