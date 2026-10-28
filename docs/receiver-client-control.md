# Native receiver control

The private `receive --control-stdio` mode lets the native Mac client own the
receiver process. Use `--peer <saved-device-id>` to resolve managed credentials;
the diagnostic `--identity` option remains available to isolated tests. Neither
control mode nor saved-peer selection enables OS input: `--inject` is still an
explicit separate choice.

The inherited stdin accepts one bounded JSON line, `{"op":"stop"}`, or EOF.
Both request cooperative stop through the same flag as the existing signal and
emergency paths. Unknown commands, extra fields, malformed or overlong messages
stop with an error. This channel cannot arm the receiver or change its trust.
Receiver-side pause is not implemented; the first client integration should offer
start and stop, with the sender controlling enable/pause as before.

The inherited stdout emits bounded JSON lines every approximately 50 ms:

```json
{"protocol":1,"state":"listening","listen_endpoint":"0.0.0.0:24800","error":null}
```

States describe actual core lifecycle:

| State | Meaning |
| --- | --- |
| `starting` | Adapter and listener readiness have not been established. |
| `listening` | Listener is bound; there is no authenticated active connection. Discovery startup may still be in progress. |
| `paused` | Authenticated transport exists and receiver input is disabled. |
| `sharing` | The receiver has successfully processed remote enable; the UI must also account for its chosen DrySink/injection mode. |
| `stopping` | Stop was requested; cleanup is not yet confirmed. |
| `stopped` | Receiver, discovery, input-owner and log cleanup completed successfully. |
| `error` | Final cleanup or control failed; successful release must not be inferred. |

The UI must drain stdout throughout the session and redirect stderr to a file or
the null device (or continuously drain it). Repeated session diagnostics are
suppressed in controlled mode, but startup failures and emergency worker
diagnostics may still use stderr. An undrained stderr pipe is not a supported
client configuration because it can block Rust's synchronous diagnostics.
A failed stdout write or a two-second
lack of pipe-write progress requests stop. This is backpressure detection, not an
acknowledgement that the UI rendered a status. The output queue is bounded.
The final frame has a bounded delivery wait; failed final delivery causes a
nonzero process exit. Detailed diagnostic errors remain in the session log;
the status channel returns fixed generic messages and no credential paths/keys.

Status observations are snapshots rather than a durable event stream. Connection
and enabled notifications come directly from authenticated session processing,
not from tailing buffered metrics. A failed release does not publish a successful
pause/disconnect. Only the outer CLI publishes a final result after all cleanup.

The native client should close stdin when its owning window/session closes, wait
for final status and process exit, and report an unconfirmed outcome if it must
force termination. A silent peer can keep the existing handshake call occupied
for up to its three-second deadline. Discovery teardown, worker shutdown and log
flush can add time; closing stdin is a stop request, not an immediate release
guarantee. Process-tree kills or OS/native-call failures remain outside this
cooperative cleanup guarantee.

The opt-in `receiver_stdio` test target uses the actual CLI with localhost and a
DrySink. It does not inject OS input. Native UI ownership, real Mac key release,
Escape behavior and visible motion still require attended device validation.
