# Notification Lifecycle Hardening Design

## Scope

Harden now-playing notification cancellation, terminal shutdown, native submission, and platform identity without changing playback behavior or introducing system registration side effects.

## Lifecycle

Notification replacement remains a capacity-one watch update: producers never wait, and superseded requests collapse to the latest value. An operation timeout requests cooperative cancellation. The worker then owns cleanup only until a supplied terminal deadline. At that deadline it stops awaiting and deliberately detaches the remaining operation.

Blocking native or image work runs on a dedicated detached operating-system thread, not Tokio's blocking pool. The thread owns its inputs until it returns. This makes forced cancellation claims unnecessary: a timed-out caller can stop awaiting, and Tokio runtime destruction does not wait forever for a non-cooperative blocking closure.

After ownership transfers at commit, validated temporary artwork is copied into a bounded app-private notification cache by the same detached operation that performs native submission. Its directory is mode 0700 and files are mode 0600 on Unix. Successful current and previous artwork are retained; normal replacement prunes older entries, accepted entries survive process cleanup for deferred platform loading, and the next startup removes leftovers. A serialized commit permit prevents an unresolved native submission from creating an unbounded sequence of cached files.

## Native submission commit

Cancellation is honored until ownership transfers to the dedicated operating-system thread. Spawning that thread with the serialization permit, request, and attachment is the commit point; cache promotion and the platform call both occur within that owned operation. Once spawning succeeds, the operation and attachment remain alive through native acceptance or failure even if the awaiting async task is cancelled or detached.

On macOS this matches `mac-usernotifications`: polling `show_async` dispatches a worker closure before awaiting its completion channel, so dropping the future cannot retract the queued request. Linux sends no explicit replacement ID; an arbitrary XDG server-global ID must not be reused.

## Windows identity and artwork

The Windows backend does not use PowerShell's AppUserModelID. Microsoft documents that unpackaged desktop toasts require an installed Start-menu shortcut carrying the matching AppUserModelID. A process-only AppUserModelID controls taskbar identity and is not a replacement for toast registration. Runtime shortcut or registry registration would be surprising external mutation, so Windows is unavailable by default and accepts only an optional, validated `notifications.windows_aum_id` that the user or installer has already registered.

Windows notifications are text-only because the crate source proves only that `Image::new_local` embeds a file URL, not when Windows finishes resolving it. The generic committed-submission seam and cache tests cover attachment ownership for the macOS and Linux artwork paths without making an unsupported Windows artwork claim.

## Tests

- A permanently non-cooperative notifier cannot exceed notification-worker or runtime cleanup deadlines.
- Replacement stays nonblocking and capacity-one while prior cleanup is pending.
- Cancellation before the ownership-transfer/thread-spawn commit prevents submission; cancellation after it leaves the owned promotion, submission, permit, and attachment alive until release.
- Linux request construction has no arbitrary replacement ID.
- Windows policy exposes unsupported identity and never references PowerShell.
