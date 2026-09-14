a glade supplier: Gyld decision-stream verbs over an exchange surface, with
long-op output on a log surface.

`glade-gyld` is `glade-gwz`'s sibling and is modelled on it exactly. It attaches
to a glade node as an ordinary authority session (over the wire, via
`glade-client` — no node internals, P00-a), stands behind the declared
`(ws-razel, gyld.ops)` exchange surface, and runs **allow-listed** decision
stream verbs by invoking the Gyld hosts as subprocesses. It is the write path of
`gyld-wz/dev-docs/ui/GyldGrythPlugins.md` section 4.7, phase 4.

## Two roots, two very different rights

- `--gyld-root` is a **Gyld checkout and is read only**. The supplier runs the
  hosts out of its `scripts/` and seeds its own overlays tree from its
  `examples/`. It never writes a byte there, so the checkout's committed
  examples stay committed examples.
- `--bundle-root` is **app-owned storage the supplier owns outright**: the
  overlay modules it writes, the staging repository the hosts are pointed at,
  and one directory per build. The root is the app's, never derived from a
  request, and nothing is ever built over an existing build.

## Run

```sh
glade-gyld --node ws://127.0.0.1:9099 \
  --gyld-root /path/to/gyld-wz/gyld --bundle-root /path/to/data/files/gyld \
  [--share ws-razel] [--glade-id gyld.ops] [--output-id gyld.output] \
  [--streams-id gyld.streams] [--stream-id gyld.stream] \
  [--decisions-id gyld.decisions] [--lens-id gyld.lens] [--static-base /gyld] \
  [--principal gianni] [--python /opt/homebrew/bin/python3.13] \
  [--timeout-secs 600] [--max-output-bytes 1048576]
```

Attaches, serves, reattaches on link drop, and shuts down cleanly on
SIGTERM/SIGINT. The interpreter default is not decoration: the Gyld hosts need
Python 3.13 and the system `python3` is 3.10, on which they fail.

## The bundle root

```text
<bundle-root>/
  overlays/           the writable examples tree: one symlink per file of
                      <gyld-root>/examples, plus the overlay modules the
                      supplier writes. A written overlay always wins, because a
                      seed is only laid where no file exists.
  stage/examples  ->  ../overlays      (the hosts' `--repository <bundle-root>/stage`)
  builds/<stamp>/     one emitted bundle per build; never overwritten.
  latest.json         {"output_dir": "builds/<stamp>"} — swapped after a
                      successful build.
```

The staging repository exists because the Gyld capture hosts read every overlay
module from `<repository>/examples` and `manage_decision_streams.py` writes a
generated overlay to that same directory. Pointing `--repository` at the staging
root gives the hosts a writable examples tree that is entirely inside the bundle
root.

## Command surface (exchange `gyld.ops`)

Request payload — a small JSON envelope. `args` is a **typed object, never an
argv list**: nothing a requester writes reaches a command line as a flag.

```json
{ "verb": "answer",
  "args": { "stream": "keys-2026-09-13", "overlay": "\"\"\"Stream ...\"\"\"\n..." },
  "stream_output": false, "principal": "gianni" }
```

- `verb` selects one allow-listed verb, and each maps to exactly **one** Gyld
  host invocation.
- `stream_output` (also accepted as `stream`) routes the run's output to the log
  surface (below).
- `principal` attributes the run and falls back to `--principal`.

Response payload:

```json
{ "ok": true, "run_id": "run-3", "output_dir": "/…/builds/build-1789247615547",
  "exit": 0, "stdout": "…", "stderr": "…", "attributed_to": "gianni" }
```

`ok` = the verb succeeded (`exit == 0`), or a streaming run was accepted. A
refused verb, a bad envelope, a malformed stream id, a path that leaves the
bundle root, a spawn error and a timeout are all **failure as data**
(`{ "ok": false, "error": "…" }`). The **wire** `ExchangeRes.ok` is always
`true` — the exchange always produced a structured answer; the payload `ok`
carries the run's success. A failed run never reports an `output_dir`: nothing
was built, and the previous bundle stands untouched.

### The allow-list

| verb | args | one host invocation |
| --- | --- | --- |
| `list` | none | none: reads `streams.json` of the latest build |
| `answer` | `stream`, `overlay` | writes the overlay module, then `rebuild` |
| `ask` | `stream`, `overlay`, `question` | the same, with the question appended |
| `fork` | `parent`, `stream`, `note?`, `force?` | `manage_decision_streams.py fork PARENT NEW` |
| `link` | `parent`, `stream`, `note?`, `force?` | `manage_decision_streams.py link PARENT NEW` |
| `rebuild` | `built?` | `manage_decision_streams.py rebuild --bundle LATEST --output NEW` |
| `diff` | `left`, `right`, `force?` | `manage_decision_streams.py diff LEFT RIGHT --bundle LATEST` |

Everything else is refused as data. Excluded and why: `occurred`, `lens` and
`inspect` are named by section 4.7 but no Gyld host verb exists for them yet,
and the supplier only ever runs the hosts it can name.

Three guards stand between a request and a host, in this order: the verb
allow-list, the stream-id pattern (Gyld's own `[a-z][a-z0-9]*(-[a-z0-9]+)*`,
which no path separator, flag or shell metacharacter can satisfy), and path
containment inside the bundle root. Planning is pure: a refusal is produced with
no filesystem effect at all, and no refusal ever reaches a host.

Everything is bounded. A run is killed at `--timeout-secs`; stdout and stderr
each stop accumulating at `--max-output-bytes` and the answer says so; an
exported overlay module over 1 MiB is refused; a bundle document larger than the
output budget is refused with a pointer to fetch it over the static path rather
than returned on the exchange.

A **synchronous** mutating verb holds the exchange for as long as the run takes,
bounded by the timeout. A build is minutes of Python, so a UI sends one with
`stream_output: true` and follows the log surface instead.

## Long-op output (log `gyld.output`)

`stream_output: true` answers immediately with
`{ "ok": true, "run_id": "run-3", "done": false }`. The run's stdout and stderr
lines are appended as ops to the log surface **keyed by `run_id`**, in the
`gwz.output` record shape exactly, so one consumer folds both:

```json
{ "run_id": "run-3", "seq": 1, "principal": "gianni", "stream": "stdout", "line": "…" }
```

closed by a terminal marker:

```json
{ "run_id": "run-3", "seq": 7, "principal": "gianni", "stream": "end", "done": true, "exit": 0 }
```

A consumer subscribes `(share, gyld.output, run_id)` and folds the log to follow
the run.

## Results (value surfaces `gyld.streams`, `gyld.stream`, `gyld.decisions`, `gyld.lens`)

After a successful build **and once more when the supplier attaches**, the
supplier appends the current bundle's documents to value surfaces, so every
mount in the UI converges without a second round trip:

| surface | key | value |
| --- | --- | --- |
| `gyld.streams` | none | the build's `streams.json` |
| `gyld.stream` | stream id | that stream's `stream.json` |
| `gyld.decisions` | stream id | that stream's `decide-now.json` |
| `gyld.lens` | `<stream>/<perspective>` | a `{path, digest, bytes}` pointer |

The stream listing is the authority for which streams exist: a stream the bundle
does not list is not published, even if a directory for it is lying around.

Lens files are the large ones, so they travel as **pointer plus digest** (owner
ruling O5) and are fetched over HTTP from grazel's static path:

```json
{ "path": "/gyld/builds/build-1789247615547/streams/base/lenses/decisions.lens.json",
  "digest": "ba7816bf…", "bytes": 47911 }
```

`path` is the bundle-root-relative path under `--static-base`, which is the URL
grazel serves that file at. The consumer checks the digest; it never trusts the
pointer. A document over 256 KiB is left off its share with a logged note rather
than pushed through the fold: it is on the static path like any other large file.

Publication happens off the exchange's own thread — the answer already carried
the build directory — and a publication failure is logged, never fatal: a build
that succeeded stays a build that succeeded.

### At attach, not only after a build

The moment the supplier is serving it reads the bundle root and publishes the
build already there, reusing the one publication path rather than rebuilding
anything. A build a previous session of the same data directory left behind, or
one seeded by hand, is a build a mount should see; before this it sat there
unpublished and a glade root said `nothing has landed on gyld.streams for
streams.json` until somebody pressed Rebuild, even though `latest.json` named a
perfectly good build. Which build that is, is the same question every verb
asks: `latest.json` first, and failing that the newest `builds/` directory
holding a `streams.json`.

Every publication — after a build, at attach, and after the first build the
supplier makes for itself — logs one line:

```text
[gyld] glade-gyld: published builds/build-1789341052425 (5 streams)
```

The count is what the build's `streams.json` LISTS, which is the census figure
the stream manager shows. Publishing is idempotent **in the value**, not in the
op log: [`publications`] is a pure function of the build directory, so a second
publication of the same build appends the same bytes and every consumer folds to
exactly the value it already had. The node dedups by `(origin, seq)` and folds
`value` last-writer-wins by `(lamport, origin)` — never by content — so those
appends are new ops on the writer's chain. Re-attaching over the same build
therefore costs one op per document once per attach, and changes no value
anybody reads.

## Declarations and grazel

The surfaces and the `gyld.ops` service are declared in a **separate**
`grazel/apps/gyld-app.glade` (owner ruling O5), not in a grown
`grazel-app.glade`. `glade-node` has always accepted `--app FILE.glade` more
than once, so grazel passes both files and `grazel-app.glade` stays exactly as it
is, byte-identical in both of its homes. Registration is idempotent by diff, so
the `workspace ws-razel razel` entry both files declare registers once.

grazel spawns this supplier as a composed child exactly the way it spawns
`glade-gwz`, with one difference: the gyld leg is **default off**, and
`--gyld-supplier-bin` is the switch that both spawns the supplier and loads
`gyld-app.glade`. grazel serves the bundle root at `/gyld/`, which is what a
lens pointer's `path` names.

## Tests

```sh
cargo test                              # 38 unit + 8 integration
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The integration suite spawns the real `glade-node` binary booted with
`tests/fixtures/gyld-test-app.glade`, under a temp `GLADE_HOME` and `HOME`
(never the real `~/.glade`). Seven of its eight tests drive a **recording runner
double**, so the whole verb path is exercised with no interpreter and no Gyld
checkout in sight — the attach-time publication is checked with a runner that
panics if it is called at all, because that path runs no host. The eighth runs
one real subprocess,
`emit_decision_streams.py --help`, out of a Gyld checkout found at
`../../gyld-wz/gyld` or at `GLADE_GYLD_TEST_GYLD_ROOT`; it skips loudly when
that checkout or its interpreter is absent.
